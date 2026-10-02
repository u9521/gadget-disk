//! GadgetDisk 核心：磁盘镜像的布局、容量与对齐计算，以及镜像创建。
//!
//! 本 crate **完全可在主机测试**，不接触 configfs、loop 或 Android 特有接口。
//! 规格来源：[docs/disk-image-format.md](../../../docs/disk-image-format.md)。

pub mod create;
pub mod directory;
pub mod fat;
pub mod fs;
pub mod fsinfo;
pub mod layout;
pub mod partition;
pub mod partitions;
pub mod partspec;

pub use create::{CreatedImage, create_image, ensure_target_free};
pub use fs::{CreatedPartition, FilesystemType, FormatPlan, FormattedVolume, Formatter};
pub use fsinfo::{available_bytes, nearest_existing_ancestor};
pub use layout::{
    ALIGNMENT_BYTES, DEFAULT_SIZE_BYTES, ImageLayout, ImageMode, MIN_EXFAT_BYTES, MIN_EXT4_BYTES,
    MIN_FAT32_BYTES,
};
pub use partition::{MbrHeader, PartitionTable};
pub use partitions::{PartitionEntry, PartitionScan, read_partitions};
pub use partspec::{GptPartitionType, MbrPartitionType, PartitionKind, PartitionSpec};

/// 核心错误。
///
/// 变体与 [docs/protocol.md](../../../docs/protocol.md) 的稳定错误码一一对应，
/// 由 gdd 直接映射为 `ErrorResponse.code`。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 某个**分区**的容量低于其文件系统的下限。
    ///
    /// 带上 `row` 与 `filesystem`：只说"容量低于下限"会让用户在一个多分区镜像里
    /// 找不到是哪一行。早先缺陷正是这条信息缺失——它还借用了 `no_space`，读起来
    /// 像"磁盘放不下"，而实际是"这一行的文件系统装不进这么小的区间"。
    #[error(
        "partition {row} ({filesystem}) is {requested} bytes, which is below the {filesystem} \
         minimum of {minimum} bytes"
    )]
    SizeBelowMinimum {
        /// 分区在用户填写顺序中的行号（从 1 起）。
        row: usize,
        /// 该分区实际要使用的文件系统（线格式名）。
        filesystem: &'static str,
        /// 实际申请（或只能获得）的字节数。
        requested: u64,
        /// 该文件系统的下限。
        minimum: u64,
    },

    #[error(
        "not enough space on the target filesystem: need {needed} bytes, {available} available"
    )]
    NoSpace { needed: u64, available: u64 },

    #[error("image path is not a regular file: {0}")]
    NotRegularFile(String),

    #[error("image not found: {0}")]
    NotFound(String),

    #[error("unrecognized disk layout: {0}")]
    UnsupportedLayout(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("destination already exists: {0}")]
    AlreadyExists(String),

    #[error("unsupported filesystem: {0}")]
    UnsupportedFilesystem(String),

    #[error("GPT operation failed: {0}")]
    Gpt(#[from] gpt::GptError),

    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("post-create verification failed: {0}")]
    VerifyFailed(String),
}

/// 磁盘镜像计算、分区表生成与 FAT32 格式化的操作结果类型。
pub type Result<T> = std::result::Result<T, CoreError>;

/// 测试辅助：创建临时目录承载镜像文件。
///
/// 不引入 `tempfile` 依赖（目标设备与 CI 均无需），进程退出前由调用方清理。
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 在系统临时目录下创建一个全新的空目录。
    pub fn temp_dir(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "gadgetdisk-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 创建一个预分配 `bytes` 长度的镜像文件，返回路径与**可读写**句柄。
    ///
    /// 句柄必须是读写打开的：`StreamSlice` 与 GPT 读写都需要同时读写。
    pub fn temp_image(tag: &str, bytes: u64) -> (PathBuf, std::fs::File) {
        let dir = temp_dir(tag);
        let path = dir.join("image.img");
        {
            let file = std::fs::File::create(&path).expect("创建镜像文件");
            file.set_len(bytes).expect("预分配镜像");
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("以读写方式打开镜像");
        (path, file)
    }

    /// 清理 [`temp_image`] / [`temp_dir`] 产生的目录。
    pub fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::remove_dir_all(parent).ok();
        }
    }
}
