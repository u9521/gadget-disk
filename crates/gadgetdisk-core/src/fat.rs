//! FAT32 格式化与校验。
//!
//! 规格与实测依据见 [docs/disk-image-format.md](../../../docs/disk-image-format.md)。
//!
//! **强制项**：
//! 1. 显式指定 [`fatfs::FatType::Fat32`]。`fatfs` 默认按容量自动选择，
//!    实测会把约 32.5 MiB（65525 簇）以下的区间静默格式化为 FAT16。
//! 2. 用 [`fscommon::StreamSlice`] 限定分区区间，不要对整盘格式化，
//!    否则会覆盖分区表。

use std::fs::File;

use fatfs::{FatType, FormatVolumeOptions, FsOptions};
use fscommon::StreamSlice;

use crate::layout::{SECTOR_BYTES, VOLUME_LABEL};
use crate::{CoreError, Result};

/// 卷标（11 字节，含尾随空格）。
pub const LABEL: [u8; 11] = VOLUME_LABEL;

/// 已格式化的 FAT32 卷信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fat32Volume {
    /// 每簇字节数。
    pub cluster_bytes: u32,
    /// 总簇数。
    pub total_clusters: u32,
    /// 卷标（去除尾随空格）。
    pub label: String,
}

/// 在 `[start_byte, end_byte)` 区间内格式化 FAT32。
///
/// 区间由分区表决定：`raw` 为 `[0, image_bytes)`，
/// `gpt`/`mbr` 为 `[partition_offset, partition_offset + partition_size)`。
pub fn format_fat32(file: File, start_byte: u64, end_byte: u64) -> Result<Fat32Volume> {
    format_fat32_labeled(file, start_byte, end_byte, &LABEL)
}

/// 同 [`format_fat32`]，但可指定卷标。
///
/// 卷标必须是**恰好 11 字节**（不足补空格、超出截断）——这是 FAT 目录项里
/// 卷标字段的固定宽度，`fatfs` 不会替我们补全。
pub fn format_fat32_labeled(
    file: File,
    start_byte: u64,
    end_byte: u64,
    label: &[u8; 11],
) -> Result<Fat32Volume> {
    if end_byte <= start_byte {
        return Err(CoreError::InvalidArgument(format!(
            "invalid partition range: start={start_byte} end={end_byte}"
        )));
    }

    let slice = StreamSlice::new(file, start_byte, end_byte)?;
    let options = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .volume_label(*label);
    fatfs::format_volume(slice, options)?;

    Ok(Fat32Volume {
        // 实际簇大小需重新打开卷才能读到，由 `verify_fat32` 填充。
        cluster_bytes: 0,
        total_clusters: 0,
        label: String::from_utf8_lossy(label).trim_end().to_string(),
    })
}

/// 把任意卷标规整为 FAT 目录项要求的 11 字节。
///
/// - 非 ASCII 字符用 `?` 代替（FAT 卷标为 OEM 代码页，UTF-8 中文放进去
///   会被 Host 显示成乱码，不如显式替换）；
/// - 超出 11 字节则截断。
pub fn normalize_label(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    for (slot, byte) in out.iter_mut().zip(label.bytes()) {
        *slot = if byte.is_ascii_graphic() || byte == b' ' {
            byte.to_ascii_uppercase()
        } else {
            b'?'
        };
    }
    out
}

/// FAT32 格式化器，**仅供 core 内部测试使用**。
///
/// ## 为什么不再对外提供
///
/// FAT32 的格式化现在是**独立二进制** `mkfs.vfat`
/// （`crates/gadgetdisk-mkfsvfat`，随模块安装到 `bin/`）。设备上原本不存在该工具
/// （AVD 实测：无 dosfstools、toybox 亦无 `mkfs`），故随模块分发。这样做的好处是
/// 三种文件系统走**同一条**格式化路径（外部进程 + loop 设备），流程里没有"内置特例"。
///
/// 保留本实现的原因只有一个：`gadgetdisk-core` 的编排逻辑需要在主机上被测试，
/// 而主机不一定装了 `mkfs.vfat`。它**不参与运行期格式化**——生产路径由
/// `gadgetdisk-cli` 的 `MkfsFormatter` 承担。
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FatfsFormatter;

#[cfg(test)]
impl crate::fs::Formatter for FatfsFormatter {
    fn format(
        &self,
        image_path: &std::path::Path,
        plan: &crate::fs::FormatPlan,
    ) -> Result<crate::fs::FormattedVolume> {
        use crate::fs::{FilesystemType, FormattedVolume};

        if plan.filesystem != FilesystemType::Fat32 {
            return Err(CoreError::UnsupportedFilesystem(format!(
                "测试格式化器只支持 FAT32，收到 {}",
                plan.filesystem
            )));
        }

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(image_path)?;
        let label = normalize_label(&plan.label);
        let volume = format_fat32_labeled(file, plan.start_bytes, plan.end_bytes, &label)?;

        Ok(FormattedVolume {
            filesystem: FilesystemType::Fat32,
            label: volume.label,
            tool: None,
        })
    }
}

/// 打开已格式化的卷并读取其真实参数。
pub fn verify_fat32(file: File, start_byte: u64, end_byte: u64) -> Result<Fat32Volume> {
    let slice = StreamSlice::new(file, start_byte, end_byte)?;
    let fs = fatfs::FileSystem::new(slice, FsOptions::new())?;

    if fs.fat_type() != FatType::Fat32 {
        return Err(CoreError::VerifyFailed(format!(
            "filesystem type is not FAT32: {:?}",
            fs.fat_type()
        )));
    }

    let stats = fs.stats()?;
    let total_clusters = stats.total_clusters();
    if u64::from(total_clusters) < MIN_FAT32_CLUSTERS {
        return Err(CoreError::VerifyFailed(format!(
            "FAT32 cluster count {total_clusters} is below spec minimum {MIN_FAT32_CLUSTERS}"
        )));
    }

    Ok(Fat32Volume {
        cluster_bytes: stats.cluster_size(),
        total_clusters,
        label: fs.volume_label(),
    })
}

/// FAT32 规范要求的簇数下限。
pub const MIN_FAT32_CLUSTERS: u64 = 65525;

/// 读取分区引导扇区的关键字段（用于创建后自检，不依赖 crate 内部状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    /// `510..512` 是否为 `55 AA`。
    pub has_signature: bool,
    /// `3..11` 的 OEM 标识。
    pub oem: [u8; 8],
    /// `82..90` 的文件系统类型字符串。
    pub fs_type: [u8; 8],
    /// 每扇区字节数。
    pub bytes_per_sector: u16,
}

/// 读取 `offset` 处的 FAT32 引导扇区。
pub fn read_boot_sector(file: &mut File, offset: u64) -> Result<BootSector> {
    use std::io::{Read, Seek, SeekFrom};

    let mut buf = [0u8; 512];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut buf)?;

    Ok(BootSector {
        has_signature: buf[510..512] == [0x55, 0xAA],
        oem: buf[3..11].try_into().expect("OEM 字段长度固定"),
        fs_type: buf[82..90].try_into().expect("FS 类型字段长度固定"),
        bytes_per_sector: u16::from_le_bytes([buf[11], buf[12]]),
    })
}

/// 校验引导扇区符合 FAT32 预期。
pub fn check_boot_sector(file: &mut File, offset: u64) -> Result<BootSector> {
    let sector = read_boot_sector(file, offset)?;
    if !sector.has_signature {
        return Err(CoreError::VerifyFailed(format!(
            "missing boot sector signature 0x55AA at offset {offset}"
        )));
    }
    // 提升到 u64 比较，而不是把 `SECTOR_BYTES` 截断成 u16：后者会被
    // `clippy::cast_possible_truncation` 判为可能截断。
    if u64::from(sector.bytes_per_sector) != SECTOR_BYTES {
        return Err(CoreError::VerifyFailed(format!(
            "unexpected bytes per sector: {}",
            sector.bytes_per_sector
        )));
    }
    if &sector.fs_type != b"FAT32   " {
        return Err(CoreError::VerifyFailed(format!(
            "unexpected filesystem type string in boot sector: {:?}",
            String::from_utf8_lossy(&sector.fs_type)
        )));
    }
    Ok(sector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::MIN_FAT32_BYTES;
    use crate::testutil;

    #[test]
    fn formats_fat32_at_requested_range() {
        let size = 64 * 1024 * 1024;
        let (path, file) = testutil::temp_image("fat32-ok", size);
        let start = 1024 * 1024;
        let end = size;

        format_fat32(file, start, end).unwrap();

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let sector = check_boot_sector(&mut file, start).unwrap();
        assert!(sector.has_signature);
        assert_eq!(&sector.fs_type, b"FAT32   ");

        let volume = verify_fat32(file, start, end).unwrap();
        assert!(u64::from(volume.total_clusters) >= MIN_FAT32_CLUSTERS);
        assert_eq!(volume.label, "GADGETDISK");

        testutil::cleanup(&path);
    }

    #[test]
    fn format_rejects_inverted_range() {
        let size = 64 * 1024 * 1024;
        let (path, file) = testutil::temp_image("fat32-bad", size);
        let err = format_fat32(file, size, 0).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        testutil::cleanup(&path);
    }

    #[test]
    fn format_does_not_touch_bytes_before_start() {
        use std::io::{Read, Seek, SeekFrom, Write};

        let size = 64 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("fat32-guard", size);
        // 在分区表位置写哨兵，格式化后必须保留。
        file.seek(SeekFrom::Start(510)).unwrap();
        file.write_all(&[0x55, 0xAA]).unwrap();
        file.seek(SeekFrom::Start(520)).unwrap();
        file.write_all(b"MBR-GUARD").unwrap();
        drop(file);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let start = 1024 * 1024;
        format_fat32(file, start, size).unwrap();

        let mut file = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(510)).unwrap();
        let mut probe = [0u8; 19];
        file.read_exact(&mut probe).unwrap();
        assert_eq!(&probe[0..2], &[0x55, 0xAA]);
        assert_eq!(&probe[10..19], b"MBR-GUARD");

        testutil::cleanup(&path);
    }

    #[test]
    fn explicit_fat_type_does_not_force_fat32_below_threshold() {
        // 实测（fatfs 0.3.6）：即使显式传 `FatType::Fat32`，当区间小到
        // 簇数不足以构成 FAT32 时，`format_volume` **仍会成功返回**，
        // 但实际文件系统退化为 FAT16（约 32.5 MiB / 65525 簇以下触发）。
        //
        // 这正是 `verify_fat32` 的簇数下限校验必须存在的原因：
        // 只检查「格式化是否报错」会漏掉静默降级。
        let size = 8 * 1024 * 1024;
        let (path, file) = testutil::temp_image("fat32-small", size);
        format_fat32(file, 0, size).expect("格式化本身会成功");

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let slice = StreamSlice::new(file, 0, size).unwrap();
        let fs = fatfs::FileSystem::new(slice, FsOptions::new()).unwrap();
        assert_eq!(
            fs.fat_type(),
            FatType::Fat16,
            "小容量下 fatfs 会静默降级为 FAT16"
        );
        drop(fs);

        // 因此校验层必须拒绝它。
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let err = verify_fat32(file, 0, size).unwrap_err();
        assert!(matches!(err, CoreError::VerifyFailed(_)));

        testutil::cleanup(&path);
    }

    #[test]
    fn verify_rejects_non_fat32_volume() {
        let size = 8 * 1024 * 1024;
        let (path, file) = testutil::temp_image("fat32-verify", size);
        // 用自动选择（默认 Fat16）格式化，校验必须失败。
        let slice = StreamSlice::new(file, 0, size).unwrap();
        fatfs::format_volume(slice, FormatVolumeOptions::new()).unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let err = verify_fat32(file, 0, size).unwrap_err();
        assert!(matches!(err, CoreError::VerifyFailed(_)));

        testutil::cleanup(&path);
    }

    #[test]
    fn minimum_size_yields_valid_fat32() {
        let size = MIN_FAT32_BYTES;
        let (path, file) = testutil::temp_image("fat32-min", size);
        format_fat32(file, 0, size).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let volume = verify_fat32(file, 0, size).unwrap();
        assert!(u64::from(volume.total_clusters) >= MIN_FAT32_CLUSTERS);
        testutil::cleanup(&path);
    }
}
