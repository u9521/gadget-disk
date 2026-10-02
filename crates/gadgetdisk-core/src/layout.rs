//! 镜像布局与容量常量。
//!
//! 全部数值来自 [docs/disk-image-format.md](../../../docs/disk-image-format.md) 的实测记录，
//! 集中在此处以便调参并保留依据。

use crate::{CoreError, Result};

/// 分区起始对齐：1 MiB。
///
/// 依据：`gpt` crate 默认对齐会把分区起点放在 LBA 34（约 17 KiB），
/// 部分宿主系统与工具判为不规范磁盘。实测 `Some(2048)` → `first_lba = 2048`。
pub const ALIGNMENT_BYTES: u64 = 1024 * 1024;

/// 对齐扇区数（512 字节扇区下的 [`ALIGNMENT_BYTES`]）。
pub const ALIGNMENT_SECTORS: u64 = ALIGNMENT_BYTES / 512;

/// 逻辑扇区大小。`gpt` 与 MBR 构造均假定 512 字节。
pub const SECTOR_BYTES: u64 = 512;

/// [`SECTOR_BYTES`] 的 `usize` 形式，供数组长度与切片下标使用。
///
/// 存在的意义是让「512 一定能放进 `usize`」在**类型层面**成立：直接写
/// `SECTOR_BYTES as usize` 会被 `clippy::cast_possible_truncation` 判为在 32 位
/// 目标上可能截断（它看不见常量取值），而本仓库把该 lint 设为阻塞。
///
/// **必须写字面量 `512`，不能写 `SECTOR_BYTES as usize`**——后者只是把告警换个
/// 位置，在 32 位目标上照样触发。两者取值必须保持一致（512 字节扇区是
/// [docs/disk-image-format.md](../../../docs/disk-image-format.md) 记录的既定事实）。
pub const SECTOR_BYTES_USIZE: usize = 512;

/// 单分区 FAT32 下限：33 MiB。
///
/// 依据：FAT32 规范要求至少 65525 个簇。宿主实测（`target/debug/mkfsvfat`）：
/// **34077184 字节（32.50 MiB）**产生 512 字节簇、恰好 **65525** 个簇并通过
/// [`crate::fat::MIN_FAT32_CLUSTERS`] 自检；再小一个字节即报
/// `Cannot select FAT type - unfortunate disk size`，32 MiB 则被判为 Fat16。
/// 33 MiB 是在该硬边界之上留出余量的取值。
///
/// 这里**只**约束单个 FAT32 分区，不再充当整个镜像的下限——镜像容量本身没有
/// 下限（见 [`crate::partspec::resolve_sizes`]）。
pub const MIN_FAT32_BYTES: u64 = 33 * 1024 * 1024;

/// 单分区 exFAT 下限：1 MiB。
///
/// **待验证假设**：设备端 `mkfs.exfat`（exfatprogs）的真实下限未实测（宿主没有
/// 该工具）。该值只用于前端与 core 的**提前拦截**，明显过小的分区不必等到格式化
/// 才失败；真正可行与否仍以 `mkfs.exfat` 的返回为准。
pub const MIN_EXFAT_BYTES: u64 = 1024 * 1024;

/// 单分区 ext4 下限：2 MiB。
///
/// 依据：宿主实测 `mke2fs` 在 1 MiB 时报
/// `Filesystem too small for a journal`，2 MiB 可正常建立文件系统。
pub const MIN_EXT4_BYTES: u64 = 2 * 1024 * 1024;

/// 默认容量：4 GiB（兼顾常见 U 盘场景与创建耗时）。
pub const DEFAULT_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 卷标固定 11 字节，不足补空格。
pub const VOLUME_LABEL: [u8; 11] = *b"GADGETDISK ";

/// 镜像的磁盘布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageLayout {
    /// 无分区表；整个镜像即一个 FAT32 卷。
    Raw,
    /// GPT 分区表 + 单个 BASIC Data 分区。
    #[default]
    Gpt,
    /// MBR 分区表 + 单个类型 `0x0C`（FAT32 LBA）分区。
    Mbr,
}

impl ImageLayout {
    /// 线格式名称，与 [docs/protocol.md](../../../docs/protocol.md) 的 `layout` 字段一致。
    pub const fn as_str(self) -> &'static str {
        match self {
            ImageLayout::Raw => "raw",
            ImageLayout::Gpt => "gpt",
            ImageLayout::Mbr => "mbr",
        }
    }

    /// 从线格式名称解析。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "raw" => Some(ImageLayout::Raw),
            "gpt" => Some(ImageLayout::Gpt),
            "mbr" => Some(ImageLayout::Mbr),
            _ => None,
        }
    }

    /// 该布局是否带分区表（即分区起点非 0）。
    pub const fn has_partition_table(self) -> bool {
        !matches!(self, ImageLayout::Raw)
    }

    /// 分区起始字节偏移；`raw` 布局为 0。
    pub const fn partition_offset_bytes(self) -> u64 {
        match self {
            ImageLayout::Raw => 0,
            ImageLayout::Gpt | ImageLayout::Mbr => ALIGNMENT_BYTES,
        }
    }
}

/// USB 设备模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageMode {
    /// 可读写 U 盘（默认）。
    #[default]
    Rw,
    /// 只读（写保护）。
    Ro,
    /// 光驱；建议配合 `.iso` 且只读。
    Cdrom,
}

impl ImageMode {
    /// 线格式名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            ImageMode::Rw => "rw",
            ImageMode::Ro => "ro",
            ImageMode::Cdrom => "cdrom",
        }
    }

    /// 从线格式名称解析。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rw" => Some(ImageMode::Rw),
            "ro" => Some(ImageMode::Ro),
            "cdrom" => Some(ImageMode::Cdrom),
            _ => None,
        }
    }

    /// configfs `lun.N/ro` 属性取值。
    pub const fn ro_attr(self) -> u8 {
        match self {
            ImageMode::Rw => 0,
            ImageMode::Ro | ImageMode::Cdrom => 1,
        }
    }

    /// configfs `lun.N/cdrom` 属性取值。
    pub const fn cdrom_attr(self) -> u8 {
        match self {
            ImageMode::Cdrom => 1,
            ImageMode::Rw | ImageMode::Ro => 0,
        }
    }

    /// 该模式是否要求只读打开后端文件。
    pub const fn is_read_only(self) -> bool {
        matches!(self, ImageMode::Ro | ImageMode::Cdrom)
    }
}

/// 创建镜像前的容量预检结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeCheck {
    /// 实际将使用的镜像容量。
    pub image_bytes: u64,
    /// 分区区间可用容量（`raw` 布局等于 `image_bytes`）。
    pub partition_bytes: u64,
}

/// 校验并归整请求容量。
///
/// **镜像容量本身没有下限**：真正有下限的是「某个分区在其所选文件系统下能否成立」
/// （见 [`crate::fs::FilesystemType::minimum_bytes`]），而那取决于分区数量、布局
/// 与每个分区的文件系统意图，在只看总容量的地方判不出来。这里只拒绝 `0`——它无法
/// 产生任何可用镜像，属于参数错误而不是"容量太小"。
///
/// - `requested == 0` 返回 `InvalidArgument`；
/// - 其余值对齐到 1 MiB 的整数倍，避免分区区间出现半扇区尾巴。
pub fn check_size(requested: u64) -> Result<SizeCheck> {
    if requested == 0 {
        return Err(CoreError::InvalidArgument(
            "image size must be greater than 0 bytes".into(),
        ));
    }
    let image_bytes = align_up(requested, ALIGNMENT_BYTES);
    Ok(SizeCheck {
        image_bytes,
        // 预留 GPT 首尾头部与分区项数组的空间由 partition 模块精确求得，
        // 这里只给出保守的上界估计。
        partition_bytes: image_bytes.saturating_sub(ALIGNMENT_BYTES),
    })
}

/// 向上对齐到 `align` 的整数倍（`align` 必须非 0）。
pub const fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    let remainder = value % align;
    if remainder == 0 {
        value
    } else {
        value + (align - remainder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_rounds_to_multiple() {
        assert_eq!(align_up(0, 1024), 0);
        assert_eq!(align_up(1024, 1024), 1024);
        assert_eq!(align_up(1025, 1024), 2048);
        assert_eq!(align_up(1, ALIGNMENT_BYTES), ALIGNMENT_BYTES);
    }

    #[test]
    fn align_up_with_zero_align_is_identity() {
        // 防除零：契约要求 align 非 0，但函数必须不 panic。
        assert_eq!(align_up(1234, 0), 1234);
    }

    #[test]
    fn default_capacity_matches_spec() {
        assert_eq!(DEFAULT_SIZE_BYTES, 4 * 1024 * 1024 * 1024);
        assert_eq!(MIN_FAT32_BYTES, 33 * 1024 * 1024);
        assert_eq!(MIN_EXFAT_BYTES, 1024 * 1024);
        assert_eq!(MIN_EXT4_BYTES, 2 * 1024 * 1024);
        assert_eq!(ALIGNMENT_SECTORS, 2048);
    }

    #[test]
    fn fat32_floor_admits_the_measured_cluster_boundary() {
        // 实测边界：34077184 字节（32.50 MiB）恰好产生 65525 个簇；33 MiB 必须
        // 在它之上——下限低于该硬边界会让"通过校验的镜像"在格式化阶段被自检拒绝。
        //
        // 断言包在 `const {}` 里让它**编译期**求值：两个操作数都是常量，运行期比较
        // 会被 clippy 判为 `assertions_on_constants`。
        const MEASURED_MINIMUM: u64 = 34_077_184;
        const { assert!(MIN_FAT32_BYTES >= MEASURED_MINIMUM) };
    }

    #[test]
    fn layout_wire_names_round_trip() {
        for layout in [ImageLayout::Raw, ImageLayout::Gpt, ImageLayout::Mbr] {
            assert_eq!(ImageLayout::parse(layout.as_str()), Some(layout));
        }
        assert_eq!(ImageLayout::parse("unknown"), None);
    }

    #[test]
    fn layout_partition_offsets_follow_spec() {
        assert_eq!(ImageLayout::Raw.partition_offset_bytes(), 0);
        assert_eq!(ImageLayout::Gpt.partition_offset_bytes(), ALIGNMENT_BYTES);
        assert_eq!(ImageLayout::Mbr.partition_offset_bytes(), ALIGNMENT_BYTES);
        assert!(!ImageLayout::Raw.has_partition_table());
        assert!(ImageLayout::Gpt.has_partition_table());
    }

    #[test]
    fn mode_maps_to_configfs_attributes() {
        assert_eq!(ImageMode::Rw.ro_attr(), 0);
        assert_eq!(ImageMode::Rw.cdrom_attr(), 0);
        assert!(!ImageMode::Rw.is_read_only());

        assert_eq!(ImageMode::Ro.ro_attr(), 1);
        assert_eq!(ImageMode::Ro.cdrom_attr(), 0);
        assert!(ImageMode::Ro.is_read_only());

        // 光驱必须只读，且 cdrom 置位。
        assert_eq!(ImageMode::Cdrom.ro_attr(), 1);
        assert_eq!(ImageMode::Cdrom.cdrom_attr(), 1);
        assert!(ImageMode::Cdrom.is_read_only());
    }

    #[test]
    fn mode_wire_names_round_trip() {
        for mode in [ImageMode::Rw, ImageMode::Ro, ImageMode::Cdrom] {
            assert_eq!(ImageMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ImageMode::parse("nope"), None);
    }

    #[test]
    fn check_size_rejects_zero() {
        // 容量 0 会产出零长度文件，属于参数错误而非"容量太小"。
        let err = check_size(0).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "得到 {err:?}");
    }

    #[test]
    fn check_size_has_no_image_level_floor() {
        // 镜像容量本身没有下限：1 字节被接受并对齐到 1 MiB。真正要拦的是
        // 「某个分区在其文件系统下太小」，那由 `partspec::resolve_sizes` 判定。
        let check = check_size(1).unwrap();
        assert_eq!(check.image_bytes, ALIGNMENT_BYTES);
    }

    #[test]
    fn check_size_aligns_up_to_mib() {
        let check = check_size(ALIGNMENT_BYTES + 1).unwrap();
        assert_eq!(check.image_bytes % ALIGNMENT_BYTES, 0);
        assert_eq!(check.image_bytes, ALIGNMENT_BYTES * 2);
    }
}
