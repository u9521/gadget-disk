//! FAT32 格式化核心。
//!
//! 规格与实测依据见 [docs/disk-image-format.md](../../../docs/disk-image-format.md)。
//!
//! ## 两个强制项（都是实测结论，不是风格偏好）
//!
//! 1. **必须显式指定 [`fatfs::FatType::Fat32`]**。`fatfs` 默认按容量自动选择，
//!    实测 8–256 MiB 的区间都会静默产生 **FAT16**。
//! 2. **必须用 [`fscommon::StreamSlice`] 限定区间**，严禁对整盘格式化——
//!    那会覆盖分区表。

use std::fs::File;
use std::path::Path;

use fatfs::{FatType, FormatVolumeOptions, FsOptions};
use fscommon::StreamSlice;

/// 格式化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fat32Volume {
    /// 每簇字节数。
    pub cluster_bytes: u32,
    /// 总簇数。
    pub total_clusters: u32,
    /// 卷标（去除尾随空格）。
    pub label: String,
}

/// 格式化错误。
#[derive(Debug)]
pub enum FormatError {
    /// 区间非法。
    InvalidRange(String),
    /// 容量不足以形成合法 FAT32。
    TooSmall(String),
    /// I/O 或底层 crate 失败。
    Io(String),
    /// 格式化后的自检失败（例如 `fatfs` 静默降级成 FAT16）。
    VerifyFailed(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::InvalidRange(m) => write!(f, "invalid range: {m}"),
            FormatError::TooSmall(m) => write!(f, "not enough space: {m}"),
            FormatError::Io(m) => write!(f, "I/O error: {m}"),
            FormatError::VerifyFailed(m) => write!(f, "post-format verification failed: {m}"),
        }
    }
}

impl std::error::Error for FormatError {}

/// 把任意卷标规整为 FAT 目录项要求的 11 字节。
///
/// - 非 ASCII 可打印字符用 `?` 代替：FAT 卷标是 OEM 代码页，UTF-8 中文放进去
///   在 Host 上会显示成乱码，不如显式替换；
/// - 字母转大写（FAT 惯例）；
/// - 超出 11 字节截断。
pub fn normalize_label(label: &str) -> [u8; crate::LABEL_LEN] {
    let mut out = [b' '; crate::LABEL_LEN];
    for (slot, byte) in out.iter_mut().zip(label.bytes()) {
        *slot = if byte.is_ascii_graphic() || byte == b' ' {
            byte.to_ascii_uppercase()
        } else {
            b'?'
        };
    }
    out
}

/// 在 `[start_byte, end_byte)` 区间内格式化 FAT32。
///
/// 区间由调用方决定：整盘为 `[offset, offset + size)`；分区则由 loop 设备
/// 表达偏移，此时区间为 `[0, partition_size)`。
///
/// **不含自检**：`file` 的所有权在此被 `StreamSlice` 消耗，无法再读回。
/// 调用方应在格式化后调用 [`verify_path`] 确认没有静默降级成 FAT16。
pub fn format_fat32(
    file: File,
    start_byte: u64,
    end_byte: u64,
    label: &[u8; crate::LABEL_LEN],
) -> Result<Fat32Volume, FormatError> {
    if end_byte <= start_byte {
        return Err(FormatError::InvalidRange(format!(
            "start={start_byte} end={end_byte}"
        )));
    }

    let slice =
        StreamSlice::new(file, start_byte, end_byte).map_err(|e| FormatError::Io(e.to_string()))?;

    // 显式 Fat32：默认按容量自动选择会静默产生 FAT16（实测）。
    let options = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .volume_label(*label);

    fatfs::format_volume(slice, options).map_err(|e| FormatError::Io(e.to_string()))?;

    Ok(Fat32Volume {
        // 真实簇大小需重新打开卷才能读到，由 `verify_path` 填充。
        cluster_bytes: 0,
        total_clusters: 0,
        label: String::from_utf8_lossy(label).trim_end().to_string(),
    })
}

/// 打开已格式化的卷并读取真实参数（用于自检）。
pub fn verify_path(
    path: &Path,
    start_byte: u64,
    end_byte: u64,
) -> Result<Fat32Volume, FormatError> {
    let file = File::open(path).map_err(|e| FormatError::Io(e.to_string()))?;
    let slice =
        StreamSlice::new(file, start_byte, end_byte).map_err(|e| FormatError::Io(e.to_string()))?;
    let fs = fatfs::FileSystem::new(slice, FsOptions::new())
        .map_err(|e| FormatError::VerifyFailed(e.to_string()))?;

    if fs.fat_type() != FatType::Fat32 {
        return Err(FormatError::VerifyFailed(format!(
            "filesystem type is not FAT32: {:?} (the partition/image size is too small; volume was downgraded)",
            fs.fat_type()
        )));
    }

    let stats = fs
        .stats()
        .map_err(|e| FormatError::VerifyFailed(e.to_string()))?;
    let total_clusters = stats.total_clusters();
    if u64::from(total_clusters) < crate::MIN_FAT32_CLUSTERS {
        return Err(FormatError::VerifyFailed(format!(
            "FAT32 cluster count {total_clusters} is below the spec minimum {}",
            crate::MIN_FAT32_CLUSTERS
        )));
    }

    Ok(Fat32Volume {
        cluster_bytes: stats.cluster_size(),
        total_clusters,
        label: fs.volume_label(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_image(tag: &str, bytes: u64) -> (std::path::PathBuf, File) {
        let dir = std::env::temp_dir().join(format!(
            "gd-mkfsvfat-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img");
        {
            let f = File::create(&path).unwrap();
            f.set_len(bytes).unwrap();
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (path, file)
    }

    #[test]
    fn normalize_label_pads_to_eleven_bytes() {
        assert_eq!(normalize_label("ABC"), *b"ABC        ");
        assert_eq!(normalize_label(""), *b"           ");
        // 正好 11 字节。
        assert_eq!(normalize_label("ABCDEFGHIJK"), *b"ABCDEFGHIJK");
    }

    #[test]
    fn normalize_label_uppercases_and_replaces_non_ascii() {
        assert_eq!(normalize_label("mydisk"), *b"MYDISK     ");
        // 非 ASCII 替换为 `?`，而不是写入会被 Host 显示成乱码的 UTF-8 字节。
        let out = normalize_label("中文");
        assert_eq!(&out[..2], b"??");
    }

    #[test]
    fn normalize_label_truncates_overlong() {
        assert_eq!(normalize_label("ABCDEFGHIJKLMNOP"), *b"ABCDEFGHIJK");
    }

    #[test]
    fn formats_a_whole_image_as_fat32() {
        let (path, file) = temp_image("whole", 64 * 1024 * 1024);
        let label = normalize_label("TESTDISK");
        format_fat32(file, 0, 64 * 1024 * 1024, &label).unwrap();

        // 自检必须通过——这是"没有静默降级成 FAT16"的唯一证据。
        let vol = verify_path(&path, 0, 64 * 1024 * 1024).unwrap();
        assert_eq!(vol.label, "TESTDISK");
        assert!(u64::from(vol.total_clusters) >= crate::MIN_FAT32_CLUSTERS);

        // 引导扇区签名与 FS 类型字符串。
        use std::io::{Read, Seek, SeekFrom};
        let mut f = File::open(&path).unwrap();
        let mut boot = [0u8; 512];
        f.read_exact(&mut boot).unwrap();
        assert_eq!(&boot[510..512], &[0x55, 0xAA]);
        assert_eq!(&boot[82..90], b"FAT32   ");
        assert_eq!(&boot[71..82], b"TESTDISK   ");
        let _ = f.seek(SeekFrom::Start(0));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn formats_a_partition_range_within_an_image() {
        // 模拟"映像里 1 MiB 处的一个分区"。
        let (path, file) = temp_image("part", 128 * 1024 * 1024);
        let start = 1024 * 1024;
        let end = 65 * 1024 * 1024;
        let label = normalize_label("PART");
        format_fat32(file, start, end, &label).unwrap();

        let vol = verify_path(&path, start, end).unwrap();
        assert_eq!(vol.label, "PART");

        // 区间外（含 0 处）不得被写入：那是分区表所在位置。
        use std::io::Read;
        let mut f = File::open(&path).unwrap();
        let mut head = [0u8; 512];
        f.read_exact(&mut head).unwrap();
        assert_eq!(head, [0u8; 512], "分区表区域被格式化污染");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_inverted_range() {
        let (path, file) = temp_image("inverted", 16 * 1024 * 1024);
        let label = normalize_label("X");
        let err = format_fat32(file, 4096, 1024, &label).unwrap_err();
        assert!(matches!(err, FormatError::InvalidRange(_)));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn too_small_volume_is_detected_by_verify_not_silently_fat16() {
        // 这正是"静默降级"防线：小于 65525 簇时 fatfs 会产出 FAT16，
        // 自检必须**报错**而不是当成成功。
        let (path, file) = temp_image("small", 16 * 1024 * 1024);
        let label = normalize_label("SMALL");
        // 格式化本身可能"成功"（fatfs 不报错）。
        let _ = format_fat32(file, 0, 16 * 1024 * 1024, &label);

        let result = verify_path(&path, 0, 16 * 1024 * 1024);
        assert!(
            result.is_err(),
            "16 MiB 的卷应被自检判为非法 FAT32，得到 {result:?}"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
