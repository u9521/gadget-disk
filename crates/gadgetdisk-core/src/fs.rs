//! 文件系统类型与格式化抽象。
//!
//! ## 为什么要有这一层
//!
//! 规格要求「格式化使用系统带的 `mkfs`，不自己在 CLI 里实现」。但 `mkfs` 是
//! **外部进程**，而 `gadgetdisk-core` 的既定边界是「不接触平台接口、完全可在
//! 主机测试」（见 `crates/gadgetdisk-core/src/lib.rs` 的模块文档）。两者要同时成立，
//! 只能把「如何格式化」抽象成 [`Formatter`] trait：
//!
//! - 本 crate 定义 trait 与**纯逻辑**（类型名、探测候选的解析、命令构造）；
//! - 实际 `fork/exec` 由 CLI 层实现（`crates/gadgetdisk-cli/src/mkfs.rs`）；
//! - 测试用内存替身，不需要真的装 `mkfs`。
//!
//! ## AVD 实测结论（决定了本模块的设计）
//!
//! 在 x86_64 / Android 17.1 AVD（`u:r:su:s0`）上的实测：
//!
//! | 文件系统 | 系统工具 | 分区内偏移方式 |
//! |---|---|---|
//! | FAT32 | **不存在**（无 `mkfs.vfat`、无 dosfstools、toybox 亦无） | — |
//! | exFAT | `/system/bin/mkfs.exfat`（exfatprogs 1.3.2） | **无 offset 选项**，须经 loop 设备 |
//! | ext4 | `/system/bin/mkfs.ext4` → `mke2fs`（1.47.2） | `-E offset=<字节>` |
//!
//! 因此：
//!
//! 1. **FAT32 必然回退到纯 Rust 的 `fatfs`**——设备上没有它的 `mkfs`。
//!    这不是「降级」，而是唯一可行路径；
//! 2. exFAT 与 ext4 需要**不同的偏移策略**，由 [`FormatPlan`] 表达，
//!    执行细节留在 CLI 层。
//!
//! 这些结论的原始记录见 [docs/disk-image-format.md](../../../docs/disk-image-format.md)。

use std::fmt;

/// 文件系统类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FilesystemType {
    /// FAT32。由模块自带的 `mkfs.vfat`（或系统工具）格式化。
    #[default]
    Fat32,
    /// exFAT。系统工具为 `mkfs.exfat`。
    ExFat,
    /// ext4。系统工具为 `mkfs.ext4`（通常是指向 `mke2fs` 的符号链接）。
    Ext4,
}

impl FilesystemType {
    /// 线格式名称，与 REST 契约的 `filesystem` 字段一致。
    pub const fn as_str(self) -> &'static str {
        match self {
            FilesystemType::Fat32 => "fat32",
            FilesystemType::ExFat => "exfat",
            FilesystemType::Ext4 => "ext4",
        }
    }

    /// 从线格式名称解析。
    ///
    /// 接受若干常见别名（`vfat`、`fat`、`ext2`/`ext3` 等），因为宿主与用户
    /// 都可能用别名表达同一件事；**静默接受别名比报错更友好**，且映射是明确的。
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "fat32" | "vfat" | "fat" | "msdos" => Some(FilesystemType::Fat32),
            "exfat" => Some(FilesystemType::ExFat),
            "ext4" | "ext3" | "ext2" => Some(FilesystemType::Ext4),
            _ => None,
        }
    }

    /// 该类型是否由外部 `mkfs` 进程格式化。
    ///
    /// 现在三种类型都是 `true`：FAT32 由**模块自带的** `mkfs.vfat` 处理
    /// （设备上原本没有该工具，故随模块分发），另外两种用系统工具。
    pub const fn uses_external_mkfs(self) -> bool {
        // 三种文件系统一律经外部 mkfs 进程，格式化路径完全统一。
        matches!(
            self,
            FilesystemType::Fat32 | FilesystemType::ExFat | FilesystemType::Ext4
        )
    }

    /// 该文件系统能成立的**单分区容量下限**（字节）。
    ///
    /// 下限是**每个分区、按其实际文件系统**判定的，不是镜像级属性：同一个镜像里
    /// 一个 33 MiB 的 FAT32 与一个 2 MiB 的 ext4 都合法，而"分区≤镜像"这种整盘
    /// 判据说不出是哪一个不成立，也解释不了为什么 64 MiB 的镜像装不下 32 MiB 的
    /// 分区（早先缺陷的症状）。
    ///
    /// 取值依据见 [`crate::layout`] 中各常量；exFAT 一项是**待验证假设**。
    pub const fn minimum_bytes(self) -> u64 {
        match self {
            FilesystemType::Fat32 => crate::layout::MIN_FAT32_BYTES,
            FilesystemType::ExFat => crate::layout::MIN_EXFAT_BYTES,
            FilesystemType::Ext4 => crate::layout::MIN_EXT4_BYTES,
        }
    }

    /// 系统 `mkfs` 的候选程序名（按优先级）。
    ///
    /// 探测实现应依次在 `PATH` 与已知目录下查找这些名字。
    /// FAT32 优先查找模块自带或系统的 `mkfs.vfat` / `mkfs.fat`。
    pub const fn mkfs_candidates(self) -> &'static [&'static str] {
        match self {
            // 模块自带 `bin/mkfs.vfat`（设备上原本不存在）。
            FilesystemType::Fat32 => &["mkfs.vfat", "mkfs.fat"],
            FilesystemType::ExFat => &["mkfs.exfat"],
            FilesystemType::Ext4 => &["mkfs.ext4", "mke2fs"],
        }
    }

    /// 挂载时内核使用的文件系统类型名（供 UI 与诊断展示）。
    pub const fn kernel_fs_name(self) -> &'static str {
        match self {
            FilesystemType::Fat32 => "vfat",
            FilesystemType::ExFat => "exfat",
            FilesystemType::Ext4 => "ext4",
        }
    }

    /// 全部类型（供 UI 枚举与测试遍历）。
    pub const ALL: [FilesystemType; 3] = [
        FilesystemType::Fat32,
        FilesystemType::ExFat,
        FilesystemType::Ext4,
    ];
}

impl fmt::Display for FilesystemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 格式化一个分区区间的执行计划。
///
/// 由 CLI 层翻译为具体命令；本 crate 只表达**意图**，不构造命令行字符串，
/// 避免把 shell 语义引入可在主机穷举测试的纯逻辑层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatPlan {
    /// 文件系统类型。
    pub filesystem: FilesystemType,
    /// 区间起始字节偏移。
    pub start_bytes: u64,
    /// 区间结束字节偏移（不含）。
    pub end_bytes: u64,
    /// 卷标。
    pub label: String,
}

impl FormatPlan {
    /// 为一个分区区间构造计划。
    pub fn new(
        filesystem: FilesystemType,
        start_bytes: u64,
        end_bytes: u64,
        label: impl Into<String>,
    ) -> Self {
        Self {
            filesystem,
            start_bytes,
            end_bytes,
            label: label.into(),
        }
    }

    /// 区间容量字节数。
    pub const fn size_bytes(&self) -> u64 {
        self.end_bytes.saturating_sub(self.start_bytes)
    }

    /// 该区间是否从 0 开始（即整盘格式化，不需要偏移处理）。
    pub const fn is_whole_disk(&self) -> bool {
        self.start_bytes == 0
    }
}

/// 一次已验证成功的格式化结果。
///
/// **不记录簇大小等细节**：ext4/exFAT 的细节读取需要按类型各自实现，
/// 本模块只保证「格式化已由可信工具完成」，具体校验交由
/// [`FormatVerify`] 的按类型策略处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedVolume {
    /// 文件系统类型。
    pub filesystem: FilesystemType,
    /// 卷标（回读确认后的值）。
    pub label: String,
    /// 实际使用的工具路径；内置实现为 `None`。
    pub tool: Option<String>,
}

/// 格式化执行器。
///
/// 生产实现是 CLI 层的 `MkfsFormatter`（外部 `mkfs` 进程 + loop 设备）。
pub trait Formatter {
    /// 按 `plan` 格式化。
    fn format(
        &self,
        image_path: &std::path::Path,
        plan: &FormatPlan,
    ) -> crate::Result<FormattedVolume>;
}

/// 单个分区的最终创建结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPartition {
    /// 1 起序号。
    pub index: u32,
    /// 分区名（GPT 已写入镜像；MBR 仅具展示意义）。
    pub name: String,
    /// GPT 分区类型（GPT 布局下有值）。
    pub gpt_type: Option<crate::partspec::GptPartitionType>,
    /// MBR 分区类型（MBR 布局下有值）。
    pub mbr_type: Option<crate::partspec::MbrPartitionType>,
    /// 起始字节偏移。
    pub offset_bytes: u64,
    /// 容量字节数。
    pub size_bytes: u64,
    /// 该分区的文件系统；`None` 表示**未格式化**（只写了分区表）。
    pub filesystem: Option<FilesystemType>,
    /// 格式化结果；与 `filesystem` 同为 `None` 时表示未格式化。
    pub volume: Option<FormattedVolume>,
}

impl CreatedPartition {
    /// 该分区在主分区 / 逻辑分区上的归属。
    ///
    /// 与 [`crate::partitions::PartitionEntry::kind`] 同一规则：由序号推出
    /// （MBR 主分区占 1–4，逻辑分区从 5 起），不额外存状态，避免两处不一致。
    pub const fn kind(&self) -> crate::partspec::PartitionKind {
        if self.index > 4 {
            crate::partspec::PartitionKind::Logical
        } else {
            crate::partspec::PartitionKind::Primary
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_wire_names_round_trip() {
        for fs in FilesystemType::ALL {
            assert_eq!(FilesystemType::parse(fs.as_str()), Some(fs));
        }
        assert_eq!(FilesystemType::parse("nope"), None);
    }

    #[test]
    fn filesystem_aliases_map_to_canonical_types() {
        assert_eq!(FilesystemType::parse("vfat"), Some(FilesystemType::Fat32));
        assert_eq!(FilesystemType::parse("FAT"), Some(FilesystemType::Fat32));
        assert_eq!(FilesystemType::parse("Ext4"), Some(FilesystemType::Ext4));
        assert_eq!(FilesystemType::parse("ext2"), Some(FilesystemType::Ext4));
        assert_eq!(FilesystemType::parse("EXFAT"), Some(FilesystemType::ExFat));
    }

    #[test]
    fn default_filesystem_is_fat32() {
        // 缺省必须与既有行为一致：旧调用方不传 filesystem 时仍得到 FAT32。
        assert_eq!(FilesystemType::default(), FilesystemType::Fat32);
    }

    #[test]
    fn every_filesystem_goes_through_an_external_mkfs() {
        // 格式化路径统一：三种文件系统都是外部进程，没有"内置特例"。
        for fs in FilesystemType::ALL {
            assert!(fs.uses_external_mkfs(), "{fs} 应走外部 mkfs");
            assert!(!fs.mkfs_candidates().is_empty(), "{fs} 应有工具候选");
        }
    }

    #[test]
    fn mkfs_candidates_match_device_reality() {
        assert_eq!(FilesystemType::ExFat.mkfs_candidates(), &["mkfs.exfat"]);
        // mke2fs 是 mkfs.ext4 的真实目标（AVD 上 mkfs.ext4 是它的符号链接）。
        assert_eq!(
            FilesystemType::Ext4.mkfs_candidates(),
            &["mkfs.ext4", "mke2fs"]
        );
        // FAT32 由模块自带（设备上原本没有）。
        assert_eq!(
            FilesystemType::Fat32.mkfs_candidates(),
            &["mkfs.vfat", "mkfs.fat"]
        );
    }

    #[test]
    fn kernel_names_match_mount_expectations() {
        assert_eq!(FilesystemType::Fat32.kernel_fs_name(), "vfat");
        assert_eq!(FilesystemType::ExFat.kernel_fs_name(), "exfat");
        assert_eq!(FilesystemType::Ext4.kernel_fs_name(), "ext4");
    }

    #[test]
    fn plan_carries_range_and_filesystem() {
        // 偏移能力差异已不再由 `FormatPlan` 表达——执行层一律经 loop，
        // 因此这里只断言区间与类型的表达正确。
        let whole = FormatPlan::new(FilesystemType::Fat32, 0, 1024, "X");
        assert!(whole.is_whole_disk());
        assert_eq!(whole.size_bytes(), 1024);

        let part = FormatPlan::new(FilesystemType::Ext4, 1048576, 2097152, "X");
        assert!(!part.is_whole_disk());
        assert_eq!(part.start_bytes, 1048576);
        assert_eq!(part.size_bytes(), 1048576);
    }

    #[test]
    fn plan_size_is_exclusive_end_minus_start() {
        let plan = FormatPlan::new(FilesystemType::Fat32, 1048576, 1048576 + 4096, "X");
        assert_eq!(plan.size_bytes(), 4096);
    }

    #[test]
    fn plan_size_saturates_on_inverted_range() {
        // 防下溢：非法区间不该 panic，交由调用方校验。
        let plan = FormatPlan::new(FilesystemType::Fat32, 4096, 1024, "X");
        assert_eq!(plan.size_bytes(), 0);
    }
}
