//! 分块上传：把 WebView 选中的镜像**逐块**写进 `tmp/`，完成后原子改名到 `images/`。
//!
//! 规格见 [docs/image-upload-and-import.md](../../../../docs/image-upload-and-import.md)。
//!
//! ## 为什么是分块而不是「客户端给个路径」
//!
//! KernelSU 的文件选择器（`onShowFileChooser`）用 `ACTION_GET_CONTENT` 取文件，
//! 结果只以 `content://` URI 的形式交回 WebView；**URI 与文件系统路径都不会进入
//! JS 面**（Chromium 把 URI 留在 browser 进程，renderer 只拿到可读句柄与
//! 显示名/大小）。因此「选中文件 → 把路径交给后端去复制」这条路在架构上不存在，
//! 只能由 WebView 把字节读出来传给后端。
//!
//! 上传字节**不经过 `gdd`**（直接写文件系统），所以它不受 gdd 的协议帧上限约束——
//! 这也正是 HTTP 层不再对上传体设 1 MiB 上限的依据（见 `http::RequestBody`）。
//!
//! ## 状态机
//!
//! ```text
//! begin ──▶ （Client 反复 chunk）──▶ commit ──▶ images/<name>（原子改名）
//!   │                                   │
//!   └──────────── abort ────────────────┴──▶ 清理 tmp/<upload_id>.part
//! ```
//!
//! ## 两条不变量
//!
//! 1. **`begin` 必须登记一个 running job。** `serve` 判空闲退出时看的就是
//!    「有没有 running job」；不登记的话，一次大镜像上传会被空闲回收**从中间掐断**。
//! 2. **块必须顺序追加。** `offset` 与当前长度不符即拒绝，绝不 `seek` 补洞——
//!    空洞文件会变成一个「看起来完整、实际缺一大段」的镜像。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gadgetdisk_gdd::paths::DataDirs;

use crate::job::{JobError, JobRegistry, PROGRESS_STRIDE, check_space};

/// 一次进行中的上传。
#[derive(Debug, Clone)]
struct Upload {
    /// 目标镜像文件名（`images/` 下的键）。
    dest_name: String,
    /// 已写入字节数。**分块偏移的唯一基准**：`chunk` 要求客户端给的 `offset`
    /// 恰好等于它，从而保证「只顺序追加、绝不留空洞」。
    written: u64,
}

/// 进行中上传的注册表。
///
/// 与 [`JobRegistry`] 的分工：后者是**面向客户端**的任务状态（可轮询、决定
/// `in_use`），前者是**服务端自己**的分块续写状态（当前偏移、暂存路径）。
/// 两者由 `job_id` 关联——一次上传同时存在于两张表里。
#[derive(Debug, Clone, Default)]
pub struct UploadRegistry {
    inner: Arc<Mutex<HashMap<String, Upload>>>,
    counter: Arc<AtomicU64>,
}

impl UploadRegistry {
    /// 新建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 暂存文件路径：`tmp/<upload_id>.part`。
    ///
    /// 放在 `tmp/` 而不是直接写 `images/`：镜像只有在 `commit` 时才**原子出现**，
    /// 中断不会留下一个会被误当作完整镜像的半成品。
    fn part_path(dirs: &DataDirs, upload_id: &str) -> PathBuf {
        dirs.tmp().join(format!("{upload_id}.part"))
    }

    /// 受理一次上传：校验 + 登记 + 建暂存文件。
    ///
    /// 返回 `upload_id`。校验全部在**写入任何字节之前**完成，让「目标名非法 /
    /// 同名已存在 / 空间不足」立刻暴露，而不是传了几 GiB 之后才失败。
    pub fn begin(
        &self,
        dirs: &DataDirs,
        jobs: &JobRegistry,
        dest_name: &str,
        declared_size: u64,
    ) -> Result<String, JobError> {
        // 目标名必须落在 `images/` 内（拒绝 `..`、`/` 等）。
        let dest = dirs
            .image_path(dest_name)
            .ok_or_else(|| JobError::InvalidDestination(dest_name.to_string()))?;

        // 同名不静默覆盖。
        if dest.exists() {
            return Err(JobError::DestinationExists(dest.display().to_string()));
        }

        // 空间预检。声明大小为 0 时跳过——那时没有依据，交由写入过程自然失败
        // （`ENOSPC`），总好过用一个错误的数字去拒绝一个合法请求。
        if declared_size > 0 {
            check_space(&dirs.tmp(), declared_size)?;
        }

        let upload_id = self.next_id();

        // **关键**：登记 running job，否则 `serve` 会在上传期间判空闲而退出。
        jobs.register(upload_id.clone(), dest_name.to_string(), declared_size)?;

        let part = Self::part_path(dirs, &upload_id);
        // 建空文件：让后续 `chunk` 的 `offset` 校验有一个真实基准，
        // 也让「这段字节数必须等于已写长度」这条不变量立刻可查。
        File::create(&part)?;

        let mut map = self
            .inner
            .lock()
            .map_err(|_| JobError::Io(std::io::Error::other("upload registry lock poisoned")))?;
        map.insert(
            upload_id.clone(),
            Upload {
                dest_name: dest_name.to_string(),
                written: 0,
            },
        );

        Ok(upload_id)
    }

    /// 追加一块。`offset` 必须**恰好**等于已写长度。
    pub fn chunk(
        &self,
        dirs: &DataDirs,
        jobs: &JobRegistry,
        upload_id: &str,
        offset: u64,
        mut body: impl Read,
    ) -> Result<u64, JobError> {
        let (expected, dest_name) = {
            let map = self.inner.lock().map_err(|_| {
                JobError::Io(std::io::Error::other("upload registry lock poisoned"))
            })?;
            let Some(upload) = map.get(upload_id) else {
                return Err(JobError::Unknown(upload_id.to_string()));
            };
            (upload.written, upload.dest_name.clone())
        };
        let _ = dest_name;

        // 顺序追加，**不 seek**：允许任意 offset 会在文件中间留下空洞，
        // 而那会变成一个「大小看起来对、内容其实缺一段」的镜像。
        if offset != expected {
            return Err(JobError::OutOfOrder {
                expected,
                got: offset,
            });
        }

        let part = Self::part_path(dirs, upload_id);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    JobError::Unknown(upload_id.to_string())
                } else {
                    JobError::Io(err)
                }
            })?;

        let mut buffer = vec![0u8; 64 * 1024];
        let mut written_now: u64 = 0;
        let mut last_report: u64 = 0;
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            written_now += read as u64;

            // 边写边报进度：`chunk` 是**一个** HTTP 请求，但可能很大，
            // 期间客户端仍要能看到进度前进。
            if written_now - last_report >= PROGRESS_STRIDE {
                let total = expected + written_now;
                jobs.update_progress(upload_id, total);
                self.set_written(upload_id, total)?;
                last_report = written_now;
            }
        }
        // **不在这里 fsync**。每块落盘一次要 ~42ms（实测），而分块正是为了
        // 降低单请求开销——每块都 fsync 会把收益吃回去大半。
        //
        // 安全性没有实质损失：这里写的是 `tmp/<id>.part`，**未 commit 前它不是
        // 任何合法镜像**。中途掉电只丢这个半成品（崩溃后也不会被误当成完整镜像），
        // 而 `commit` 会先 `sync_all` 再原子改名，保证「改名成功即内容已落盘」。
        // 换言之：把耐久性从「每块」挪到「提交时」，中间态本来就不需要耐久。

        let total = expected + written_now;
        jobs.update_progress(upload_id, total);
        self.set_written(upload_id, total)?;
        Ok(total)
    }

    /// 收尾：原子改名到 `images/<dest_name>`，并把 job 标记为完成。
    pub fn commit(
        &self,
        dirs: &DataDirs,
        jobs: &JobRegistry,
        upload_id: &str,
    ) -> Result<String, JobError> {
        let upload = {
            let mut map = self.inner.lock().map_err(|_| {
                JobError::Io(std::io::Error::other("upload registry lock poisoned"))
            })?;
            map.remove(upload_id)
                .ok_or_else(|| JobError::Unknown(upload_id.to_string()))?
        };

        let part = Self::part_path(dirs, upload_id);
        let dest = dirs
            .image_path(&upload.dest_name)
            .ok_or_else(|| JobError::InvalidDestination(upload.dest_name.clone()))?;

        // 再查一次同名：上传期间可能有别的路径（`create`）抢先建了同名镜像。
        if dest.exists() {
            std::fs::remove_file(&part).ok();
            jobs.fail(
                upload_id,
                format!("destination already exists: {}", dest.display()),
            );
            return Err(JobError::DestinationExists(dest.display().to_string()));
        }

        // **落盘一次，然后才改名**。
        //
        // 这是整个上传唯一的耐久性屏障（`chunk` 刻意不 fsync，见其注释）：
        // 先把暂存文件刷到稳定存储，再原子改名——因此「改名成功」就等价于
        // 「内容已在盘上」。顺序不能颠倒：先改名再 fsync 的话，掉电可能留下
        // 一个名字正确、内容不完整的镜像，那是最坏的失败形态。
        if let Err(err) = std::fs::File::open(&part).and_then(|f| f.sync_all()) {
            std::fs::remove_file(&part).ok();
            jobs.fail(upload_id, format!("failed to write to disk: {err}"));
            return Err(JobError::Io(err));
        }

        if let Err(err) = std::fs::rename(&part, &dest) {
            std::fs::remove_file(&part).ok();
            jobs.fail(upload_id, format!("atomic rename failed: {err}"));
            return Err(JobError::Io(err));
        }

        jobs.update_progress(upload_id, upload.written);
        jobs.finish(upload_id);
        Ok(upload.dest_name)
    }

    /// 放弃上传：删暂存文件并把 job 标记为失败。
    pub fn abort(
        &self,
        dirs: &DataDirs,
        jobs: &JobRegistry,
        upload_id: &str,
    ) -> Result<(), JobError> {
        let existed = {
            let mut map = self.inner.lock().map_err(|_| {
                JobError::Io(std::io::Error::other("upload registry lock poisoned"))
            })?;
            map.remove(upload_id).is_some()
        };

        std::fs::remove_file(Self::part_path(dirs, upload_id)).ok();
        if existed {
            jobs.fail(upload_id, "upload canceled".to_string());
        }
        Ok(())
    }

    /// 该上传的目标名（供互斥检查复用）。
    pub fn dest_of(&self, upload_id: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|map| map.get(upload_id).map(|u| u.dest_name.clone()))
    }

    /// 是否还有进行中的上传。
    pub fn is_empty(&self) -> bool {
        self.inner.lock().map(|map| map.is_empty()).unwrap_or(true)
    }

    fn set_written(&self, upload_id: &str, written: u64) -> Result<(), JobError> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| JobError::Io(std::io::Error::other("upload registry lock poisoned")))?;
        if let Some(upload) = map.get_mut(upload_id) {
            upload.written = written;
        }
        Ok(())
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("upload-{}-{}", std::process::id(), n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> (DataDirs, PathBuf) {
        let root = std::env::temp_dir().join(format!("gd-upload-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        (dirs, root)
    }

    #[test]
    fn chunked_upload_reassembles_byte_for_byte() {
        let (dirs, root) = setup("reassemble");
        let jobs = JobRegistry::new();
        let uploads = UploadRegistry::new();

        // 刻意跨块切分，验证拼接边界。
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let id = uploads
            .begin(&dirs, &jobs, "disk.img", payload.len() as u64)
            .unwrap();

        let mut offset = 0usize;
        for piece in payload.chunks(777) {
            let written = uploads
                .chunk(
                    &dirs,
                    &jobs,
                    &id,
                    offset as u64,
                    std::io::Cursor::new(piece),
                )
                .unwrap();
            offset += piece.len();
            assert_eq!(written, offset as u64);
        }

        uploads.commit(&dirs, &jobs, &id).unwrap();
        let got = std::fs::read(dirs.image_path("disk.img").unwrap()).unwrap();
        assert_eq!(got, payload, "分块拼接结果必须与源逐字节一致");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_of_order_chunk_is_rejected_and_leaves_no_hole() {
        let (dirs, root) = setup("out-of-order");
        let jobs = JobRegistry::new();
        let uploads = UploadRegistry::new();

        let id = uploads.begin(&dirs, &jobs, "disk.img", 100).unwrap();
        uploads
            .chunk(&dirs, &jobs, &id, 0, std::io::Cursor::new(b"abcde"))
            .unwrap();

        // 跳到 offset 50：中间会形成空洞，必须拒绝而不是 seek 过去。
        let err = uploads
            .chunk(&dirs, &jobs, &id, 50, std::io::Cursor::new(b"xyz"))
            .unwrap_err();
        assert!(matches!(
            err,
            JobError::OutOfOrder {
                expected: 5,
                got: 50
            }
        ));

        // 暂存文件仍只有 5 字节，没有空洞。
        let part = UploadRegistry::part_path(&dirs, &id);
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 5);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_is_atomic_and_abort_cleans_up() {
        let (dirs, root) = setup("commit-abort");
        let jobs = JobRegistry::new();
        let uploads = UploadRegistry::new();

        // commit 之前目标必须**不存在**（半成品不得可见）。
        let id = uploads.begin(&dirs, &jobs, "a.img", 3).unwrap();
        assert!(!dirs.image_path("a.img").unwrap().exists());
        uploads
            .chunk(&dirs, &jobs, &id, 0, std::io::Cursor::new(b"abc"))
            .unwrap();
        assert!(
            !dirs.image_path("a.img").unwrap().exists(),
            "commit 前不得出现目标"
        );
        uploads.commit(&dirs, &jobs, &id).unwrap();
        assert!(dirs.image_path("a.img").unwrap().exists());

        // abort 后暂存清干净。
        let id2 = uploads.begin(&dirs, &jobs, "b.img", 3).unwrap();
        uploads
            .chunk(&dirs, &jobs, &id2, 0, std::io::Cursor::new(b"xy"))
            .unwrap();
        uploads.abort(&dirs, &jobs, &id2).unwrap();
        assert!(!UploadRegistry::part_path(&dirs, &id2).exists());
        assert!(!dirs.image_path("b.img").unwrap().exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn begin_rejects_existing_destination() {
        let (dirs, root) = setup("dup");
        let jobs = JobRegistry::new();
        let uploads = UploadRegistry::new();

        std::fs::write(dirs.image_path("taken.img").unwrap(), b"x").unwrap();
        let err = uploads.begin(&dirs, &jobs, "taken.img", 1).unwrap_err();
        assert!(matches!(err, JobError::DestinationExists(_)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn begin_registers_running_job_so_serve_will_not_idle_exit() {
        // 回归：不登记 running job 的话，`serve` 会认为空闲并把上传从中间掐断。
        let (dirs, root) = setup("keeps-alive");
        let jobs = JobRegistry::new();
        let uploads = UploadRegistry::new();

        let id = uploads.begin(&dirs, &jobs, "big.img", 1024).unwrap();
        assert!(
            jobs.is_importing("big.img"),
            "上传期间目标名必须处于 importing"
        );
        assert!(jobs.running_count() > 0, "有上传时不得判为空闲");

        uploads.abort(&dirs, &jobs, &id).unwrap();
        assert_eq!(jobs.running_count(), 0, "上传结束后应可空闲退出");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_upload_id_is_reported() {
        let (dirs, root) = setup("unknown");
        let jobs = JobRegistry::new();
        let uploads = UploadRegistry::new();

        let err = uploads
            .chunk(&dirs, &jobs, "nope", 0, std::io::Cursor::new(b""))
            .unwrap_err();
        assert!(matches!(err, JobError::Unknown(_)));
        assert!(matches!(
            uploads.commit(&dirs, &jobs, "nope").unwrap_err(),
            JobError::Unknown(_)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }
}
