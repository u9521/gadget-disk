//! 长任务（job）子系统：镜像导入。
//!
//! 规格见 [docs/protocol.md](../../../../docs/protocol.md) 的「长任务（job）」与
//! [docs/image-upload-and-import.md](../../../../docs/image-upload-and-import.md)：
//!
//! - 必须防止同一目标路径被两个 job 写入；
//! - 进度以 `PROGRESS_STRIDE` 为步长上报，供 WebUI 轮询；
//! - **有 running job 时 `serve` 不判空闲退出**——这是「大镜像上传不被
//!   空闲回收掐断」所依赖的唯一判据（见 `serve::has_running_jobs`）。
//!
//! 本模块只负责**任务状态**；实际的字节搬运（分块续写、原子改名）在
//! [`crate::upload`]，因为那部分有自己的一套不变量（顺序偏移、暂存文件）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gadgetdisk_proto::{JobState, JobStatus};

use gadgetdisk_gdd::paths::DataDirs;

/// 复制缓冲区大小。
///
/// 64 KiB：在 `/sdcard` FUSE 与 ext4 上都能取得良好吞吐，同时让进度上报
/// 足够频繁（见 [`PROGRESS_STRIDE`]）。
pub const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// 每写入多少字节更新一次进度。
///
/// 依据：`exec` 没有字节级回调，UI 只能轮询 `JobStatus`。以 4 MiB 为步长，
/// 既不会让 4 GiB 级复制产生过多锁竞争，又能让进度足够细（≥1024 次更新）。
pub const PROGRESS_STRIDE: u64 = 4 * 1024 * 1024;

/// job 相关错误。
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// 底层 IO 失败。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 目标空间不足。
    #[error("not enough space: need {needed} bytes, {available} available")]
    NoSpace {
        /// 需要的字节数。
        needed: u64,
        /// 可用的字节数。
        available: u64,
    },

    /// 目标已存在且未允许覆盖。
    #[error("destination already exists: {0}")]
    DestinationExists(String),

    /// 目标名非法。
    #[error("invalid destination name: {0}")]
    InvalidDestination(String),

    /// 没有该 job。
    #[error("no such job: {0}")]
    Unknown(String),

    /// 分块上传的偏移不连续。
    ///
    /// 单独一个变体（而不是复用 `Io`）：这是**客户端**的错，要回 400 并说明
    /// 期望值，而不是一个笼统的 500。也借此钉住「只许顺序追加」这条不变量。
    #[error("chunks out of order: expected offset {expected}, got {got}")]
    OutOfOrder {
        /// 服务端当前已写入的长度。
        expected: u64,
        /// 客户端声明的偏移。
        got: u64,
    },
}

/// 一个导入任务的状态记录。
#[derive(Debug, Clone)]
pub struct JobRecord {
    /// 任务 id。
    pub id: String,
    /// 目标镜像键（`images/` 下的文件名）。
    pub key: String,
    /// 当前状态。
    pub status: JobStatus,
}

/// job 注册表：跨连接共享。
#[derive(Debug, Clone, Default)]
pub struct JobRegistry {
    inner: Arc<Mutex<HashMap<String, JobRecord>>>,
}

impl JobRegistry {
    /// 新建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 查询任务状态。
    pub fn status(&self, id: &str) -> Option<JobStatus> {
        self.inner
            .lock()
            .ok()
            .and_then(|map| map.get(id).map(|record| record.status.clone()))
    }

    /// 该镜像是否正在被导入。
    pub fn is_importing(&self, key: &str) -> bool {
        self.inner
            .lock()
            .map(|map| {
                map.values()
                    .any(|record| record.key == key && record.status.state == JobState::Running)
            })
            .unwrap_or(false)
    }

    /// 当前运行中的任务数。
    pub fn running_count(&self) -> usize {
        self.inner
            .lock()
            .map(|map| {
                map.values()
                    .filter(|record| record.status.state == JobState::Running)
                    .count()
            })
            .unwrap_or(0)
    }

    /// 登记一个即将开始的任务。
    pub fn register(&self, id: String, key: String, total: u64) -> Result<(), JobError> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| JobError::Io(std::io::Error::other("job registry lock poisoned")))?;

        // 防止同一目标路径被两个 job 写入。
        if map
            .values()
            .any(|r| r.key == key && r.status.state == JobState::Running)
        {
            return Err(JobError::DestinationExists(key));
        }

        map.insert(
            id.clone(),
            JobRecord {
                id,
                key,
                status: JobStatus {
                    state: JobState::Running,
                    bytes_done: 0,
                    bytes_total: total,
                    error: None,
                },
            },
        );
        Ok(())
    }

    /// 更新进度。
    pub fn update_progress(&self, id: &str, done: u64) {
        if let Ok(mut map) = self.inner.lock()
            && let Some(record) = map.get_mut(id)
        {
            record.status.bytes_done = done;
        }
    }

    /// 标记完成。
    pub fn finish(&self, id: &str) {
        if let Ok(mut map) = self.inner.lock()
            && let Some(record) = map.get_mut(id)
        {
            record.status.bytes_done = record.status.bytes_total;
            record.status.state = JobState::Done;
        }
    }

    /// 标记失败。
    ///
    /// `pub` 是给「登记成功、但后台线程没能起来」这类调用方用的：那种情况下
    /// job 已经登记为 `running`，必须有人把它收尾，否则会留下一条永远
    /// `running` 的记录，而 `serve` 判空闲时看的就是它——`serve` 将**永不退出**。
    pub fn fail(&self, id: &str, message: String) {
        if let Ok(mut map) = self.inner.lock()
            && let Some(record) = map.get_mut(id)
        {
            record.status.state = JobState::Failed;
            record.status.error = Some(message);
        }
    }
}

/// 空间预检：目标文件系统可用字节是否够放 `needed`。
///
/// 上传与创建共用：两者都要在**写入任何字节之前**回答「放得下吗」。
/// 失败时回 `NoSpace` 并带上两个具体数值，让界面能说清「还差多少」，
/// 而不是给一句「空间不足」。
pub fn check_space(dir: &Path, needed: u64) -> Result<(), JobError> {
    // `rustix::fs::statvfs` 返回 Result 且字段是 u64，不需要裸 libc 调用或手工零初始化。
    let stat = rustix::fs::statvfs(dir).map_err(std::io::Error::from)?;

    let available = stat.f_bavail * stat.f_frsize;
    if available < needed {
        return Err(JobError::NoSpace { needed, available });
    }
    Ok(())
}

// ---------------------------------------------------------------- 镜像元信息

/// 去掉可能的前缀，把 `create` 的目标解析为 `images/` 下的路径。
///
/// 若调用方给的是裸文件名，则落到 `images/`；若给的是完整路径，
/// 则**原样返回**（调用方负责越界校验，见 `rest::resolve_image_path`）。
///
/// 之所以放在这里而不是 `serve`：它是「镜像管理」这组操作的共同前置，
/// `create`/`delete`/`list` 都要用，放在 job 模块避免 `serve` 与 CLI 各写一份。
pub fn resolve_target(dirs: &DataDirs, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return path;
    }
    dirs.images().join(path)
}

/// 读取修改时间（Unix 秒）。
pub fn mtime_secs(metadata: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 探测镜像布局。
///
/// `raw` 与带分区表的镜像靠首扇区区分：
/// - 偏移 512 处为 `EFI PART` → GPT；
/// - 偏移 446 处分区项类型 `0x0C` 且偏移 510 处 `55AA` → MBR；
/// - 否则若偏移 510 处为 `55AA` 且 `82..90` 是 FAT 类型串 → `raw`；
/// - 都不符合 → `unknown`。
pub fn detect_layout(path: &Path) -> gadgetdisk_proto::ImageLayout {
    use gadgetdisk_proto::ImageLayout;
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return ImageLayout::Unknown;
    };
    let mut header = [0u8; 512];
    if file.read_exact(&mut header).is_err() {
        return ImageLayout::Unknown;
    }

    // GPT：LBA1 处为 "EFI PART"。
    let mut gpt_sig = [0u8; 8];
    if file.seek(SeekFrom::Start(512)).is_ok()
        && file.read_exact(&mut gpt_sig).is_ok()
        && &gpt_sig == b"EFI PART"
    {
        return ImageLayout::Gpt;
    }

    let has_boot_sig = header[510..512] == [0x55, 0xAA];
    let mbr_type = header[446 + 4];

    // MBR：有引导签名 + 分区项类型为 FAT32 LBA，且分区起点在合理范围。
    if has_boot_sig && mbr_type == 0x0C {
        let first_lba = u32::from_le_bytes([header[454], header[455], header[456], header[457]]);
        if first_lba >= 1 {
            return ImageLayout::Mbr;
        }
    }

    // raw：整盘就是一个 FAT 卷，偏移 510 处为 55AA 且类型字段是 FAT 串。
    if has_boot_sig && &header[82..85] == b"FAT" {
        return ImageLayout::Raw;
    }

    ImageLayout::Unknown
}

/// 把核心错误映射为稳定错误码。
pub fn core_error_code(err: &gadgetdisk_core::CoreError) -> gadgetdisk_proto::ErrorCode {
    use gadgetdisk_core::CoreError;
    use gadgetdisk_proto::ErrorCode;
    match err {
        CoreError::SizeBelowMinimum { .. } => ErrorCode::SizeBelowMinimum,
        CoreError::NoSpace { .. } => ErrorCode::NoSpace,
        CoreError::NotRegularFile(_) => ErrorCode::NotRegularFile,
        CoreError::NotFound(_) => ErrorCode::ImageNotFound,
        CoreError::UnsupportedLayout(_) => ErrorCode::UnsupportedLayout,
        CoreError::InvalidArgument(_) => ErrorCode::InvalidArgument,
        // 同名阻断要回一个**专属**错误码：WebUI 据此给出"改名或先删除"的
        // 具体指引，而不是笼统的"参数非法"。
        CoreError::AlreadyExists(_) => ErrorCode::AlreadyExists,
        // 缺少某个文件系统的 mkfs 工具时，语义上属于"内核/系统不支持"，
        // 与既有的 filesystem_unsupported 一致（loop 挂载侧同码）。
        CoreError::UnsupportedFilesystem(_) => ErrorCode::FilesystemUnsupported,
        CoreError::VerifyFailed(_) | CoreError::Gpt(_) | CoreError::Io(_) => ErrorCode::Internal,
    }
}

/// 把 job 错误映射为稳定错误码。
pub fn import_error_code(err: &JobError) -> gadgetdisk_proto::ErrorCode {
    use gadgetdisk_proto::ErrorCode;
    match err {
        JobError::NoSpace { .. } => ErrorCode::NoSpace,
        // 同名目标是**冲突**（409），不是「镜像正被占用」——上传路径下没有
        // 「谁占着它」可言，用 `ImageInUse` 会让提示误导用户去找占用方。
        JobError::DestinationExists(_) => ErrorCode::AlreadyExists,
        // 乱序分块是客户端把 offset 算错了，必须回 400 而不是笼统 500。
        JobError::InvalidDestination(_) | JobError::Unknown(_) | JobError::OutOfOrder { .. } => {
            ErrorCode::InvalidArgument
        }
        JobError::Io(_) => ErrorCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn setup(tag: &str) -> (DataDirs, PathBuf) {
        let root = testutil::temp_dir(tag);
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        (dirs, root)
    }

    #[test]
    fn importing_flag_tracks_running_jobs() {
        let registry = JobRegistry::new();
        assert!(!registry.is_importing("a.img"));

        registry
            .register("job-1".into(), "a.img".into(), 10)
            .unwrap();
        assert!(registry.is_importing("a.img"));
        assert_eq!(registry.running_count(), 1);

        registry.finish("job-1");
        assert!(!registry.is_importing("a.img"));
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn unknown_job_status_is_none() {
        let registry = JobRegistry::new();
        assert!(registry.status("nope").is_none());
    }

    #[test]
    fn concurrent_job_to_same_destination_is_rejected() {
        // 同一目标名不得被两个 job 同时写：否则两个上传会互相覆盖，
        // 得到的是一个「两半拼起来」的镜像。
        let registry = JobRegistry::new();
        registry
            .register("job-1".into(), "same.img".into(), 10)
            .unwrap();
        let err = registry
            .register("job-2".into(), "same.img".into(), 10)
            .unwrap_err();
        assert!(matches!(err, JobError::DestinationExists(_)));
    }

    #[test]
    fn failed_job_stops_counting_as_running() {
        // `serve` 判空闲退出看的是 running 数：失败必须让它归零，
        // 否则一次失败的上传会让 `serve` **永不退出**。
        let registry = JobRegistry::new();
        registry
            .register("job-1".into(), "a.img".into(), 10)
            .unwrap();
        assert_eq!(registry.running_count(), 1);
        registry.fail("job-1", "boom".into());
        assert_eq!(registry.running_count(), 0);
        assert_eq!(registry.status("job-1").unwrap().state, JobState::Failed);
    }

    #[test]
    fn space_check_reports_shortage() {
        let (dirs, _root) = setup("job-space");
        // 要一个不可能满足的量：必须报 NoSpace 并带上数值。
        let err = check_space(&dirs.tmp(), u64::MAX).unwrap_err();
        assert!(matches!(err, JobError::NoSpace { .. }));
    }
}
