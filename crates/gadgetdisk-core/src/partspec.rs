//! 分区规格：用户对「要建哪些分区、每个分区多大、什么类型、叫什么名字」的意图。
//!
//! 规格与实测依据见 [docs/disk-image-format.md](../../../docs/disk-image-format.md)。
//!
//! ## 为什么单独一个模块
//!
//! [`crate::partition`] 负责**把意图写成字节**，本模块负责**表达与校验意图**。
//! 两者分开的直接好处是：分区规格的校验（数量上限、名称长度、容量下限）是纯函数，
//! 可以在不碰磁盘的情况下穷举测试——而「写坏分区表」这类错误在真机上代价很高。
//!
//! ## GPT 与 MBR 的能力差异（重要）
//!
//! | 能力 | GPT | MBR |
//! |---|---|---|
//! | 分区数量 | 受分区项数组大小限制（本模块取 [`MAX_PARTITIONS`]） | 最多 4 个主分区，或 3 主 + 若干逻辑分区 |
//! | 分区类型 | 128 位类型 GUID | 8 位类型字节 |
//! | 分区名 | 有（UTF-16，≤36 字符） | **没有** |
//!
//! MBR **不存在分区名字段**，因此 [`PartitionSpec::name`] 在 MBR 布局下**只用于
//! UI 展示，不会被写入镜像**。这一点必须由 UI 明确告知用户，而不是静默丢弃：
//! 用户填了名字却在 Host 上看不到，属于「静默失败」。
//!
//! ## 逻辑分区需要扩展分区容器（重要）
//!
//! MBR 的 4 个分区项槽位是**主分区**槽位。要放第 5 个及以后的分区，必须占用一个
//! 槽位作为**扩展分区**容器，其内部再以 EBR（Extended Boot Record）链描述
//! **逻辑分区**。因此合法的槽位组合是「4 主」或「N 主 + 1 容器（N ≤ 3）」。
//!
//! 用户表达「这个分区是逻辑分区」时用 [`PartitionKind::Logical`]；想**预留**一片
//! 空间以后再放逻辑分区时，用 [`PartitionKind::Extended`] 显式声明一个**空容器**。
//! 无论哪种来源，容器的**位置与对齐**都由 [`resolve_mbr_layout`] 推出。因此：
//!
//! - [`MbrPartitionType::Extended`] **仍不出现在 UI 预设里**——容器的类型字节
//!   恒为 `0x05`，不是用户可选的「分区类型」；
//! - 容器由[`PartitionKind::Extended`]表达：**位置与对齐**由 [`resolve_mbr_layout`]
//!   推出，**存在性与容量**由用户决定。
//!
//! 早先的实现把 `mbr:extended` 直接暴露为可选类型，但写入侧不生成任何 EBR——
//! 用户得到的是一块**占着主分区槽位却什么都挂不上的空壳**，容量被白白吃掉。
//! 这是静默失败，不是「尚未实现」，故本次一并修正。
//!
//! ## 扩展容器的两种来源
//!
//! | 来源 | 何时出现 | 容量由谁决定 |
//! |---|---|---|
//! | 隐式（自动） | 存在逻辑分区时必然有 | 求解器按 EBR 链推导 |
//! | 显式（用户声明） | 用户把某行归属设为扩展分区 | 用户填的容量（`0` = 占满剩余） |
//!
//! 两者**可以并存**：显式声明容量的同时又放了逻辑分区。此时容量以**求解器按
//! EBR 链推导的结果为准**，显式容量被忽略——否则「容器要多大」会有两个来源打架。
//! 该规则由 [`resolve_mbr_layout`] 实现，并在文档与 Note 里写明。
//!
//! 显式容器允许里面**暂时没有逻辑分区**（「先预留一片空间」）。此时的区间从对齐
//! 边界起、按用户容量向下取，不产生任何 EBR。

use crate::fs::FilesystemType;
use crate::{CoreError, Result};

/// GPT 布局允许的分区数量上限。
///
/// 与 [`crate::partitions::MAX_PARTITION_ENTRIES`]（读取侧上界）保持一致：
/// 写入侧允许的数量若超过读取侧上界，自己创建的镜像会读不全。
pub const MAX_PARTITIONS: usize = 128;

/// MBR 主分区数量上限（首扇区分区项数组固定 4 项）。
pub const MBR_MAX_PRIMARY: usize = 4;

/// MBR 逻辑分区数量上限。
///
/// 规格上 EBR 链可以任意长，但链越长越容易被其他工具判为不规范，且遍历成本
/// 与损坏风险都随之上升。64 远超实际使用场景（3 主 + 61 逻辑已是极端配置）。
pub const MBR_MAX_LOGICAL: usize = 64;

/// 扩展分区容器的容量下限：1 MiB（一个对齐单位）。
///
/// 容器里没有文件系统，只需放得下 EBR（一个扇区）与至少一个扇区的逻辑分区。
/// 取一个对齐单位而不是一个扇区，是因为**分区起始必须对齐**：容器起点之后紧邻
/// 的 EBR 与其后的逻辑分区都要落在对齐边界上，小于一个对齐单位的容器放不下任何
/// 合法逻辑分区，建出来注定是个用不上的空壳。
pub const MIN_EXTENDED_BYTES: u64 = 1024 * 1024;

/// 把归属描述成短名（用于错误信息，避免对用户说「第 3 个分区」却指的是容器）。
const fn describe_kind(kind: PartitionKind) -> &'static str {
    match kind {
        PartitionKind::Primary => "primary",
        PartitionKind::Logical => "logical",
        PartitionKind::Extended => "extended container",
    }
}

/// GPT 分区名长度上限（UTF-16 码元数）。
///
/// UEFI 规定分区名为 36 个 UTF-16 码元（含结尾 NUL）。`gpt` crate 在写入时
/// 会按此截断或报错，故这里**提前拒绝**而不是让它静默截断。
pub const GPT_NAME_MAX_UTF16: usize = 36;

/// GPT 分区类型。
///
/// **与 MBR 类型完全独立**（见 [`MbrPartitionType`]）。两者曾共用一个枚举，后果是
/// `fat32_lba` 与 `microsoft_basic` 映射到同一个 GPT GUID（都是 BASIC Data），
/// 既无法区分、也只能表达 4 种类型，还需要 `supports_mbr()` 这类"按布局过滤"的
/// 补丁。拆开后每个布局拥有自己完整的类型空间。
///
/// 读取侧（[`crate::partitions`]）本就分开维护 MBR 字节表与 GPT GUID 表，
/// 拆分后两侧模型终于一致。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GptPartitionType {
    /// EFI System Partition（`C12A7328-F81F-11D2-BA4B-00A0C93EC93B`）。
    EfiSystem,
    /// Microsoft Basic Data（`EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`）。**默认**。
    #[default]
    MicrosoftBasic,
    /// Microsoft Reserved（`E3C9E316-0B5C-4DB8-817D-F92DF00215AE`）。
    MicrosoftReserved,
    /// Windows Recovery Environment（`DE94BBA4-06D1-4D40-A16A-BFD50179D6AC`）。
    WindowsRecovery,
    /// Linux filesystem data（`0FC63DAF-8483-4772-8E79-3D69D8477DE4`）。
    LinuxFilesystem,
    /// Linux swap（`0657FD6D-A4AB-43C4-84E5-0933C84B4F4F`）。
    LinuxSwap,
    /// Linux LVM（`E6D6D379-F507-44C2-A23C-238F2A3DF928`）。
    LinuxLvm,
    /// Linux RAID（`A19D880F-05FC-4D3B-A006-743F0F84911E`）。
    LinuxRaid,
    /// BIOS boot partition（`21686148-6449-6E6F-744E-656564454649`）。
    BiosBoot,
    /// 自定义类型 GUID。
    ///
    /// 用途：用户的特殊分区类型不在预设表里。UEFI 允许任意 GUID 作为类型，
    /// 因此这里如实放行，而不是逼用户去改代码。
    Custom(uuid::Uuid),
}

impl GptPartitionType {
    /// 该类型的 GPT 类型 GUID。
    pub fn guid(&self) -> uuid::Uuid {
        use uuid::Uuid;
        match self {
            GptPartitionType::EfiSystem => {
                Uuid::parse_str("C12A7328-F81F-11D2-BA4B-00A0C93EC93B").expect("内置 GUID 合法")
            }
            GptPartitionType::MicrosoftBasic => {
                Uuid::parse_str("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7").expect("内置 GUID 合法")
            }
            GptPartitionType::MicrosoftReserved => {
                Uuid::parse_str("E3C9E316-0B5C-4DB8-817D-F92DF00215AE").expect("内置 GUID 合法")
            }
            GptPartitionType::WindowsRecovery => {
                Uuid::parse_str("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC").expect("内置 GUID 合法")
            }
            GptPartitionType::LinuxFilesystem => {
                Uuid::parse_str("0FC63DAF-8483-4772-8E79-3D69D8477DE4").expect("内置 GUID 合法")
            }
            GptPartitionType::LinuxSwap => {
                Uuid::parse_str("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F").expect("内置 GUID 合法")
            }
            GptPartitionType::LinuxLvm => {
                Uuid::parse_str("E6D6D379-F507-44C2-A23C-238F2A3DF928").expect("内置 GUID 合法")
            }
            GptPartitionType::LinuxRaid => {
                Uuid::parse_str("A19D880F-05FC-4D3B-A006-743F0F84911E").expect("内置 GUID 合法")
            }
            GptPartitionType::BiosBoot => {
                Uuid::parse_str("21686148-6449-6E6F-744E-656564454649").expect("内置 GUID 合法")
            }
            GptPartitionType::Custom(guid) => *guid,
        }
    }

    /// 线格式名称。
    ///
    /// 预设类型用短名；自定义类型回**规范化的 GUID 字面量**，使「写入什么就读回
    /// 什么」成立（否则自定义值无法在往返中表达）。
    pub fn as_wire(&self) -> String {
        match self {
            GptPartitionType::EfiSystem => "gpt:efi_system".to_string(),
            GptPartitionType::MicrosoftBasic => "gpt:microsoft_basic".to_string(),
            GptPartitionType::MicrosoftReserved => "gpt:microsoft_reserved".to_string(),
            GptPartitionType::WindowsRecovery => "gpt:windows_recovery".to_string(),
            GptPartitionType::LinuxFilesystem => "gpt:linux_filesystem".to_string(),
            GptPartitionType::LinuxSwap => "gpt:linux_swap".to_string(),
            GptPartitionType::LinuxLvm => "gpt:linux_lvm".to_string(),
            GptPartitionType::LinuxRaid => "gpt:linux_raid".to_string(),
            GptPartitionType::BiosBoot => "gpt:bios_boot".to_string(),
            // 大写下发：GUID 惯例是大写，回读时大小写不敏感解析。
            GptPartitionType::Custom(guid) => format!("gpt:{}", guid.to_string().to_uppercase()),
        }
    }

    /// 人类可读标签（供 UI 展示）。
    pub fn label(&self) -> String {
        match self {
            GptPartitionType::EfiSystem => "EFI System".to_string(),
            GptPartitionType::MicrosoftBasic => "Microsoft basic data".to_string(),
            GptPartitionType::MicrosoftReserved => "Microsoft reserved".to_string(),
            GptPartitionType::WindowsRecovery => "Windows recovery".to_string(),
            GptPartitionType::LinuxFilesystem => "Linux filesystem".to_string(),
            GptPartitionType::LinuxSwap => "Linux swap".to_string(),
            GptPartitionType::LinuxLvm => "Linux LVM".to_string(),
            GptPartitionType::LinuxRaid => "Linux RAID".to_string(),
            GptPartitionType::BiosBoot => "BIOS boot".to_string(),
            GptPartitionType::Custom(guid) => {
                format!("custom ({})", guid.to_string().to_uppercase())
            }
        }
    }

    /// 从线格式名称解析。
    ///
    /// 接受 `gpt:<短名>` 与 `gpt:<GUID 字面量>` 两种形式。**不接受不带前缀的名字**
    /// ——那正是过去两种布局混用的来源。
    pub fn parse(value: &str) -> Option<Self> {
        let rest = value.strip_prefix("gpt:")?;
        match rest.to_ascii_lowercase().as_str() {
            "efi_system" => Some(GptPartitionType::EfiSystem),
            "microsoft_basic" => Some(GptPartitionType::MicrosoftBasic),
            "microsoft_reserved" => Some(GptPartitionType::MicrosoftReserved),
            "windows_recovery" => Some(GptPartitionType::WindowsRecovery),
            "linux_filesystem" | "linux" => Some(GptPartitionType::LinuxFilesystem),
            "linux_swap" => Some(GptPartitionType::LinuxSwap),
            "linux_lvm" => Some(GptPartitionType::LinuxLvm),
            "linux_raid" => Some(GptPartitionType::LinuxRaid),
            "bios_boot" => Some(GptPartitionType::BiosBoot),
            other => uuid::Uuid::parse_str(other)
                .ok()
                .map(GptPartitionType::Custom),
        }
    }

    /// UI 预设列表（不含 `Custom`：它由输入框表达）。
    pub fn presets() -> Vec<GptPartitionType> {
        vec![
            GptPartitionType::MicrosoftBasic,
            GptPartitionType::EfiSystem,
            GptPartitionType::MicrosoftReserved,
            GptPartitionType::WindowsRecovery,
            GptPartitionType::LinuxFilesystem,
            GptPartitionType::LinuxSwap,
            GptPartitionType::LinuxLvm,
            GptPartitionType::LinuxRaid,
            GptPartitionType::BiosBoot,
        ]
    }
}

/// MBR 分区类型（首扇区分区项 `+4` 的一个字节）。
///
/// **与 GPT 类型完全独立**：MBR 只有 8 位类型空间，语义与 GPT 的 128 位 GUID
/// 没有对应关系（同一个"FAT32"在 GPT 里是 BASIC Data，在 MBR 里是 `0x0C`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MbrPartitionType {
    /// `0x0C` — FAT32 (LBA)。**默认**（与既有单分区实现一致）。
    #[default]
    Fat32Lba,
    /// `0x0B` — FAT32（CHS 寻址；现代系统应优先用 `0x0C`）。
    Fat32Chs,
    /// `0x0E` — FAT16 (LBA)。
    Fat16Lba,
    /// `0x07` — NTFS / exFAT。
    NtfsExfat,
    /// `0x83` — Linux。
    Linux,
    /// `0x82` — Linux swap。
    LinuxSwap,
    /// `0x8E` — Linux LVM。
    LinuxLvm,
    /// `0xEF` — EFI System Partition。
    EfiSystem,
    /// `0x05` — 扩展分区（容纳逻辑分区的容器）。
    ///
    /// **不由用户作为「类型」选择**：容器由 [`PartitionKind::Extended`] 表达，
    /// 其类型字节恒为本值。`parse()` 仍接受 `mbr:extended` 以兼容存量请求，
    /// 但它不在 [`MbrPartitionType::presets`] 里。
    Extended,
    /// `0x00` — 空项（不占用分区项）。
    Empty,
    /// 自定义类型字节。
    Custom(u8),
}

impl MbrPartitionType {
    /// 类型字节。
    pub const fn byte(self) -> u8 {
        match self {
            MbrPartitionType::Fat32Lba => 0x0C,
            MbrPartitionType::Fat32Chs => 0x0B,
            MbrPartitionType::Fat16Lba => 0x0E,
            MbrPartitionType::NtfsExfat => 0x07,
            MbrPartitionType::Linux => 0x83,
            MbrPartitionType::LinuxSwap => 0x82,
            MbrPartitionType::LinuxLvm => 0x8E,
            MbrPartitionType::EfiSystem => 0xEF,
            MbrPartitionType::Extended => 0x05,
            MbrPartitionType::Empty => 0x00,
            MbrPartitionType::Custom(byte) => byte,
        }
    }

    /// 线格式名称（一律带 `mbr:` 前缀）。
    pub fn as_wire(self) -> String {
        match self {
            MbrPartitionType::Fat32Lba => "mbr:fat32_lba".to_string(),
            MbrPartitionType::Fat32Chs => "mbr:fat32_chs".to_string(),
            MbrPartitionType::Fat16Lba => "mbr:fat16_lba".to_string(),
            MbrPartitionType::NtfsExfat => "mbr:ntfs_exfat".to_string(),
            MbrPartitionType::Linux => "mbr:linux".to_string(),
            MbrPartitionType::LinuxSwap => "mbr:linux_swap".to_string(),
            MbrPartitionType::LinuxLvm => "mbr:linux_lvm".to_string(),
            MbrPartitionType::EfiSystem => "mbr:efi_system".to_string(),
            MbrPartitionType::Extended => "mbr:extended".to_string(),
            MbrPartitionType::Empty => "mbr:empty".to_string(),
            MbrPartitionType::Custom(byte) => format!("mbr:0x{byte:02X}"),
        }
    }

    /// 人类可读标签。
    pub fn label(self) -> String {
        match self {
            MbrPartitionType::Fat32Lba => "FAT32 (LBA)".to_string(),
            MbrPartitionType::Fat32Chs => "FAT32 (CHS)".to_string(),
            MbrPartitionType::Fat16Lba => "FAT16 (LBA)".to_string(),
            MbrPartitionType::NtfsExfat => "NTFS / exFAT".to_string(),
            MbrPartitionType::Linux => "Linux".to_string(),
            MbrPartitionType::LinuxSwap => "Linux swap".to_string(),
            MbrPartitionType::LinuxLvm => "Linux LVM".to_string(),
            MbrPartitionType::EfiSystem => "EFI System".to_string(),
            MbrPartitionType::Extended => "Extended".to_string(),
            MbrPartitionType::Empty => "Empty (unused)".to_string(),
            MbrPartitionType::Custom(byte) => format!("custom (0x{byte:02X})"),
        }
    }

    /// 从线格式名称解析。
    ///
    /// 接受 `mbr:<短名>` 与 `mbr:0x<十六进制>` 两种形式。
    pub fn parse(value: &str) -> Option<Self> {
        let rest = value.strip_prefix("mbr:")?;
        let lowered = rest.to_ascii_lowercase();
        match lowered.as_str() {
            "fat32_lba" => return Some(MbrPartitionType::Fat32Lba),
            "fat32_chs" => return Some(MbrPartitionType::Fat32Chs),
            "fat16_lba" => return Some(MbrPartitionType::Fat16Lba),
            "ntfs_exfat" => return Some(MbrPartitionType::NtfsExfat),
            "linux" => return Some(MbrPartitionType::Linux),
            "linux_swap" => return Some(MbrPartitionType::LinuxSwap),
            "linux_lvm" => return Some(MbrPartitionType::LinuxLvm),
            "efi_system" => return Some(MbrPartitionType::EfiSystem),
            "extended" => return Some(MbrPartitionType::Extended),
            "empty" => return Some(MbrPartitionType::Empty),
            _ => {}
        }
        // 自定义：`0x1A` 或裸十六进制 `1a`。
        let hex = lowered.strip_prefix("0x").unwrap_or(&lowered);
        u8::from_str_radix(hex, 16)
            .ok()
            .map(MbrPartitionType::Custom)
    }

    /// UI 预设列表（不含 `Custom`，也不含 [`MbrPartitionType::Extended`]）。
    ///
    /// `Extended` 是容器而非用户可选的分区类型，故刻意不出现在这里：它的存在与
    /// 区间由逻辑分区推出（见模块文档）。它仍可被 `parse` 解析，以便旧请求不报错。
    pub fn presets() -> Vec<MbrPartitionType> {
        vec![
            MbrPartitionType::Fat32Lba,
            MbrPartitionType::Fat32Chs,
            MbrPartitionType::Fat16Lba,
            MbrPartitionType::NtfsExfat,
            MbrPartitionType::Linux,
            MbrPartitionType::LinuxSwap,
            MbrPartitionType::LinuxLvm,
            MbrPartitionType::EfiSystem,
        ]
    }
}

/// 分区的文件系统**意图**。
///
/// 三态而非 `Option<FilesystemType>`：`Option` 无法区分
/// 「未指定（继承全局默认）」与「显式不格式化」——两者都是 `None`。
/// 这个区别是实测暴露出来的：请求里写了 `filesystem: "none"` 的分区
/// 被当成"未指定"而套上了全局默认，于是用户要求的"不格式化"变成了 FAT32。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionFilesystem {
    /// 未指定：继承 [`crate::create::CreateOptions::filesystem`]。
    #[default]
    Inherit,
    /// 显式不格式化（只写分区表）。
    None,
    /// 指定文件系统。
    Some(FilesystemType),
}

impl PartitionFilesystem {
    /// 解析后的具体文件系统；`None` 表示不格式化。
    ///
    /// `Inherit` 需要调用方给出全局默认值，故不能用 `const fn`。
    pub fn resolve(self, default: FilesystemType) -> Option<FilesystemType> {
        match self {
            PartitionFilesystem::Inherit => Some(default),
            PartitionFilesystem::None => None,
            PartitionFilesystem::Some(fs) => Some(fs),
        }
    }
}

/// 分区在 MBR 布局下的归属：主分区、逻辑分区，或扩展分区容器。
///
/// **只有 MBR 用得上**（GPT 没有这个概念，所有分区一律平等），因此在 GPT/raw
/// 布局下 [`PartitionKind::Logical`] 与 [`PartitionKind::Extended`] 都会被
/// [`validate`] 明确拒绝，而不是静默当作主分区处理——那会让用户在 Host 上得到
/// 一个与预期不符的分区表。
///
/// 逻辑分区会被写进 EBR（Extended Boot Record）链，序号从 **5** 开始（Linux
/// 惯例：主分区占 1–4）。
///
/// [`PartitionKind::Extended`] 是**容器**（类型字节 `0x05`）：它占一个首扇区槽位，
/// 但**不是分区**——不占内核序号、没有数据区、不可格式化。它存在的意义有二：
///
/// 1. 有逻辑分区时，写入侧必须为它们提供一个容器（此时容器**自动**产生，用户
///    不必声明）；
/// 2. 用户想**预留**一片空间以后再放逻辑分区时，可以先建一个**空**容器。
///
/// 这是对早先「扩展分区完全不由用户表达」的放宽：容器的**位置与对齐**仍由
/// [`resolve_mbr_layout`] 决定（那部分是过度的约束），但**存在性与容量**现在由
/// 用户通过本枚举表达。详见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionKind {
    /// 主分区（占用首扇区的 4 个分区项之一）。**默认**。
    #[default]
    Primary,
    /// 逻辑分区（写入 EBR 链，序号从 5 起）。
    Logical,
    /// 扩展分区容器（类型字节 `0x05`，占槽位但**不占序号**、无数据区）。
    Extended,
}

impl PartitionKind {
    /// 线格式名称。
    pub const fn as_wire(self) -> &'static str {
        match self {
            PartitionKind::Primary => "primary",
            PartitionKind::Logical => "logical",
            PartitionKind::Extended => "extended",
        }
    }

    /// 从线格式名称解析。
    ///
    /// 接受 `primary`/`logical`/`extended` 及首字母别名。缺省由调用方决定
    /// （见 [`PartitionKind::default`]，即主分区）。
    ///
    /// **只接受英文**：命令行输出已全面英文化，输入侧再留一套中文别名会让
    /// 「能填什么」与「会看到什么」不一致；中文别名也从未进过文档。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" | "p" => Some(PartitionKind::Primary),
            "logical" | "l" => Some(PartitionKind::Logical),
            "extended" | "e" => Some(PartitionKind::Extended),
            _ => None,
        }
    }

    /// 该归属是否是一个**真正的分区**（占内核序号、有数据区）。
    ///
    /// 扩展容器不是分区：它只占首扇区槽位，`loopNpM` 里没有对应的 `M`。
    pub const fn is_partition(self) -> bool {
        !matches!(self, PartitionKind::Extended)
    }
}

/// 单个分区的规格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionSpec {
    /// 分区容量字节数。
    ///
    /// **`0` 表示「占满剩余空间」**：多分区场景下用户往往只关心前几个分区的大小，
    /// 最后一个留空即可。该语义由 [`resolve_sizes`] 统一展开，写入侧不见到 `0`。
    pub size_bytes: u64,
    /// GPT 分区类型；`None` 表示「用该布局的默认类型」。
    ///
    /// 与 [`PartitionSpec::mbr_type`] **互不相关**：哪个生效由镜像布局决定，
    /// 无关的那个被忽略（而不是报错——用户可能先用 GPT 建好再改成 MBR）。
    pub gpt_type: Option<GptPartitionType>,
    /// MBR 分区类型；`None` 表示「用该布局的默认类型」。
    pub mbr_type: Option<MbrPartitionType>,
    /// 分区名。
    ///
    /// **仅在 GPT 下写入镜像**；MBR 无此字段（见模块文档）。
    pub name: String,
    /// 该分区的文件系统意图（继承 / 不格式化 / 指定）。
    pub filesystem: PartitionFilesystem,
    /// MBR 下该分区是主分区还是逻辑分区（GPT/raw 下必须为 `Primary`）。
    pub kind: PartitionKind,
}

impl PartitionSpec {
    /// 用默认类型与自动名称构造一个「占满剩余空间」的分区。
    pub fn fill_remaining(name: impl Into<String>) -> Self {
        Self {
            size_bytes: 0,
            gpt_type: None,
            mbr_type: None,
            name: name.into(),
            filesystem: PartitionFilesystem::Inherit,
            kind: PartitionKind::Primary,
        }
    }

    /// 指定容量。
    pub const fn with_size(mut self, size_bytes: u64) -> Self {
        self.size_bytes = size_bytes;
        self
    }

    /// 指定 GPT 类型。
    pub fn with_gpt_type(mut self, gpt_type: GptPartitionType) -> Self {
        self.gpt_type = Some(gpt_type);
        self
    }

    /// 指定 MBR 类型。
    pub const fn with_mbr_type(mut self, mbr_type: MbrPartitionType) -> Self {
        self.mbr_type = Some(mbr_type);
        self
    }

    /// 指定名称。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// 指定文件系统意图。
    pub const fn with_filesystem(mut self, filesystem: PartitionFilesystem) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// 指定该分区是主分区还是逻辑分区（仅 MBR 布局有意义）。
    pub const fn with_kind(mut self, kind: PartitionKind) -> Self {
        self.kind = kind;
        self
    }

    /// 是否应写入 EBR 链的逻辑分区。
    pub const fn is_logical(&self) -> bool {
        matches!(self.kind, PartitionKind::Logical)
    }

    /// 是否是扩展分区**容器**行（占槽位、不占序号、无数据区）。
    pub const fn is_extended(&self) -> bool {
        matches!(self.kind, PartitionKind::Extended)
    }

    /// 是否是一个**真正的分区**（主分区或逻辑分区）。
    ///
    /// 扩展容器不是分区：它不占内核序号、没有可格式化的数据区、不出现在返回的
    /// 分区列表里。涉及「有几个分区」「哪些要格式化」的判定都该用它。
    pub const fn is_partition(&self) -> bool {
        self.kind.is_partition()
    }

    /// 在给定布局下实际生效的 GPT 类型（`None` → 默认）。
    pub fn effective_gpt_type(&self) -> GptPartitionType {
        self.gpt_type.clone().unwrap_or_default()
    }

    /// 在给定布局下实际生效的 MBR 类型（`None` → 默认）。
    pub fn effective_mbr_type(&self) -> MbrPartitionType {
        self.mbr_type.unwrap_or_default()
    }
}

/// 校验分区规格列表在给定布局下是否合法。
///
/// 只做**与容量无关**的结构校验（数量、名称、类型）；容量是否放得下由
/// [`resolve_sizes`] 负责，因为它需要知道镜像总容量。
pub fn validate(specs: &[PartitionSpec], layout: crate::ImageLayout) -> Result<()> {
    if specs.is_empty() {
        return Err(CoreError::InvalidArgument(
            "at least one partition is required".into(),
        ));
    }

    if !layout.has_partition_table() {
        // raw 布局没有分区表，整个镜像就是一个卷。
        if specs.len() > 1 {
            return Err(CoreError::InvalidArgument(format!(
                "layout raw does not support multiple partitions (got {})",
                specs.len()
            )));
        }
        // raw 连分区项都没有，逻辑分区与扩展容器都无从谈起。
        if specs[0].is_logical() {
            return Err(CoreError::InvalidArgument(
                "layout raw has no partition table, so it cannot hold a logical partition".into(),
            ));
        }
        if specs[0].is_extended() {
            return Err(CoreError::InvalidArgument(
                "layout raw has no partition table, so it cannot hold an extended container".into(),
            ));
        }
        return Ok(());
    }

    // 逻辑分区与扩展容器都是 MBR 独有的机制（EBR 链）。GPT 没有这个概念，
    // 静默当作主分区处理会让用户在 Host 上得到与预期不符的分区表，故明确拒绝。
    if layout != crate::ImageLayout::Mbr {
        if let Some(index) = specs.iter().position(PartitionSpec::is_logical) {
            return Err(CoreError::InvalidArgument(format!(
                "partition {} is marked logical, but layout {} has no extended-partition mechanism \
                 (logical partitions are MBR-only)",
                index + 1,
                layout.as_str()
            )));
        }
        if let Some(index) = specs.iter().position(PartitionSpec::is_extended) {
            return Err(CoreError::InvalidArgument(format!(
                "row {} is marked as an extended container, but layout {} has no extended-partition \
                 mechanism (extended partitions are MBR-only)",
                index + 1,
                layout.as_str()
            )));
        }
    }

    // 一张 MBR 表只能有**一个**扩展分区容器：首扇区里只有一个扩展分区项可写，
    // 多个容器会让后面的项无处安放（内核会忽略或读成矛盾的表）。
    let explicit_extended: Vec<usize> = specs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_extended())
        .map(|(i, _)| i)
        .collect();
    if explicit_extended.len() > 1 {
        return Err(CoreError::InvalidArgument(format!(
            "MBR allows only one extended container (got {}): rows {}. \
             Put all logical partitions in the same container",
            explicit_extended.len(),
            explicit_extended
                .iter()
                .map(|i| (i + 1).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    for (index, spec) in specs.iter().enumerate() {
        let ordinal = index + 1;

        // MBR 下不能有 `Empty`（0x00）类型的**非空**分区：那会让分区项被判为
        // 空项而"消失"，用户填的容量白填。类型模型拆开后这里能直接检查。
        if layout == crate::ImageLayout::Mbr && spec.effective_mbr_type() == MbrPartitionType::Empty
        {
            return Err(CoreError::InvalidArgument(format!(
                "partition {ordinal} has type 'Empty (unused)' and cannot be a real partition"
            )));
        }

        // 扩展容器的类型字节恒为 `0x05`，由写入侧生成，不是用户选的「分区类型」。
        // 因此容器行**忽略** `mbr_type`；而非容器行把它当作容器类型则是矛盾的
        // （用户想表达的是「这一行是容器」，那该改归属而不是改类型）。
        if layout == crate::ImageLayout::Mbr
            && !spec.is_extended()
            && spec.effective_mbr_type() == MbrPartitionType::Extended
        {
            return Err(CoreError::InvalidArgument(format!(
                "partition {ordinal} has type 'Extended' (0x05), but its kind is not an extended \
                 container. The extended type byte is generated by the system, not selectable; \
                 to turn this row into a container, change its kind to 'extended'"
            )));
        }

        // 容器没有数据区，因此不能被格式化——否则用户会以为那 64 MiB 里有个
        // 文件系统，而实际上容器内部只能是 EBR 链与逻辑分区。
        if spec.is_extended()
            && !matches!(spec.filesystem, PartitionFilesystem::None)
            && !matches!(spec.filesystem, PartitionFilesystem::Inherit)
        {
            return Err(CoreError::InvalidArgument(format!(
                "row {ordinal} is an extended container; it has no data area and cannot be formatted"
            )));
        }

        // 逻辑分区不能自己声明成扩展分区容器（会让 EBR 里的项类型为 0x05，
        // 指向另一个容器而不是数据分区）。
        if spec.is_logical() && spec.effective_mbr_type() == MbrPartitionType::Extended {
            return Err(CoreError::InvalidArgument(format!(
                "partition {ordinal} is logical, so its type cannot be 'Extended' (0x05)"
            )));
        }

        // 只有 GPT 会真正写入名字，因此只在 GPT 下校验长度——在 MBR 下因为一个
        // 不会被写入的字段而拒绝创建，对用户毫无意义。
        if layout == crate::ImageLayout::Gpt {
            let units = spec.name.encode_utf16().count();
            if units > GPT_NAME_MAX_UTF16 {
                return Err(CoreError::InvalidArgument(format!(
                    "partition name for {ordinal} is too long: {units} UTF-16 code units, limit {GPT_NAME_MAX_UTF16}"
                )));
            }
        }
    }

    // MBR 槽位计数：主分区各占一个首扇区槽位，扩展容器（无论是「有逻辑分区」
    // 而隐式产生，还是用户显式声明）**共同**占一个。这正是「3 主 + N 逻辑」的
    // 由来，也是「4 主 + 1 逻辑」被拒的原因（那需要 5 个槽位）。
    if layout == crate::ImageLayout::Mbr {
        let primaries = specs
            .iter()
            .filter(|s| s.kind == PartitionKind::Primary)
            .count();
        let logicals = specs.iter().filter(|s| s.is_logical()).count();
        // 容器只需要一个：显式声明与「有逻辑分区」是同一件事的两种来源。
        let has_extended = logicals > 0 || !explicit_extended.is_empty();
        let needed_slots = primaries + usize::from(has_extended);

        if needed_slots > MBR_MAX_PRIMARY {
            return Err(CoreError::InvalidArgument(format!(
                "the MBR first sector holds only {MBR_MAX_PRIMARY} partition entries: {primaries} \
                 primary partition(s) {plus_extended}already fill it, leaving no room for an extended \
                 container. Change one primary partition to logical, or use fewer primary partitions",
                plus_extended = if has_extended {
                    format!(
                        "plus 1 extended container {}",
                        if logicals > 0 {
                            format!("(holding {logicals} logical partition(s)) ")
                        } else {
                            String::new()
                        }
                    )
                } else {
                    String::new()
                }
            )));
        }

        if logicals > MBR_MAX_LOGICAL {
            return Err(CoreError::InvalidArgument(format!(
                "MBR allows at most {MBR_MAX_LOGICAL} logical partitions (got {logicals})"
            )));
        }
    }

    Ok(())
}

/// 该布局允许的最大**主分区**数。
///
/// MBR 下这只是主分区槽位数（4），**不是**分区总数：启用逻辑分区后总分区数可达
/// `3 主 + MBR_MAX_LOGICAL 逻辑`。分区总数的判定在 [`validate`] 里按
/// 「主分区数 + 是否启用扩展分区」计算，不能用本函数简单替代。
pub const fn max_partitions(layout: crate::ImageLayout) -> usize {
    match layout {
        crate::ImageLayout::Raw => 1,
        crate::ImageLayout::Mbr => MBR_MAX_PRIMARY,
        crate::ImageLayout::Gpt => MAX_PARTITIONS,
    }
}

/// 该布局允许的最大分区**总数**（含逻辑分区，**不含**扩展容器）。
///
/// 供 UI 限制「添加分区」按钮：MBR 最多 `3 主 + MBR_MAX_LOGICAL`，但只有在
/// 至少有一个逻辑分区时才让出那一个槽位。UI 用本函数作为宽松上界，精确的组合
/// 合法性由 [`validate`] 判定。
///
/// **扩展容器不算分区**：它不占内核序号、没有数据区，因此不计入本函数的返回值。
pub const fn max_total_partitions(layout: crate::ImageLayout) -> usize {
    match layout {
        crate::ImageLayout::Raw => 1,
        // 4 个主分区，或 3 主 + 逻辑分区（把最后一个槽位让给扩展分区容器）。
        crate::ImageLayout::Mbr => MBR_MAX_PRIMARY - 1 + MBR_MAX_LOGICAL,
        crate::ImageLayout::Gpt => MAX_PARTITIONS,
    }
}

/// 把 `Some(0)`（占满剩余空间）展开为具体容量，并校验总和不超出可用区间。
///
/// 返回与输入等长的容量列表。
///
/// ## 规则
///
/// - 恰好一个分区为 `0`：它拿到扣除其余分区后的全部剩余空间；
/// - 多个分区为 `0`：**拒绝**。让「多个待定」按某些顺序瓜分剩余空间是隐式约定，
///   用户无法从 UI 推断出结果，不如报错让人明确指定；
/// - 全部为具体值但总和超限：**拒绝**，不做静默裁剪（静默裁剪会让用户以为
///   得到的是自己填的容量）。
///
/// ## 容量下限按**分区各自的文件系统**判定
///
/// `default_filesystem` 是 [crate::create::CreateOptions::filesystem]，用于解析
/// [`PartitionFilesystem::Inherit`]。每个分区取自己的下限
/// （[`FilesystemType::minimum_bytes`]），低于下限报 [`CoreError::SizeBelowMinimum`]
/// 并带上行号与实际文件系统。
///
/// 早先的实现把 FAT32 的 64 MiB 下限**无条件**当成"镜像下限"，于是
/// 「64 MiB 镜像 + 一个占满剩余的分区」会被报成 `no_space`——而用户看到的
/// 是"存储空间不足"，无论那个分区其实要格式化成 `none`/ext4/exFAT。镜像容量
/// 本身没有下限：真正成立与否取决于各分区在其文件系统下是否够大。
///
/// ## 扩展分区容器
///
/// 容器行**参与同一套记账**：它的容量确实占用镜像空间。因此「一个分区占满剩余」
/// 与「一个容器占满剩余」是同一个名额——两者都填 `0` 会被上面的规则拒绝。
/// 这样记账只有一个来源，不必在调用方再减一次容器容量（重复扣减会让用户看到
/// 明明够用的容量被报成放不下）。
///
/// 容器的容量下限是 [`MIN_EXTENDED_BYTES`]，比分区下限宽松得多：容器里没有
/// 文件系统，只需要放得下 EBR 与至少一个扇区。
pub fn resolve_sizes(
    specs: &[PartitionSpec],
    available_bytes: u64,
    default_filesystem: FilesystemType,
) -> Result<Vec<u64>> {
    let auto_count = specs.iter().filter(|s| s.size_bytes == 0).count();
    if auto_count > 1 {
        let auto_labels: Vec<String> = specs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.size_bytes == 0)
            .map(|(i, s)| format!("row {} ({})", i + 1, describe_kind(s.kind)))
            .collect();
        return Err(CoreError::InvalidArgument(format!(
            "{auto_count} entries have no size; at most one may consume the remaining space: {}",
            auto_labels.join(", ")
        )));
    }

    let fixed_total: u64 = specs
        .iter()
        .filter(|s| s.size_bytes != 0)
        .try_fold(0u64, |acc, s| acc.checked_add(s.size_bytes))
        .ok_or_else(|| CoreError::InvalidArgument("partition sizes overflow".into()))?;

    if fixed_total > available_bytes {
        return Err(CoreError::NoSpace {
            needed: fixed_total,
            available: available_bytes,
        });
    }

    let remaining = available_bytes - fixed_total;

    // 显式给出容量的行逐行过下限：容器要放得下 EBR，分区要放得下自己的文件系统。
    for (index, spec) in specs.iter().enumerate() {
        let row = index + 1;
        if spec.size_bytes == 0 {
            continue;
        }
        if spec.is_extended() {
            if spec.size_bytes < MIN_EXTENDED_BYTES {
                return Err(CoreError::InvalidArgument(format!(
                    "the extended container in row {row} is too small: {} bytes, minimum {MIN_EXTENDED_BYTES} bytes",
                    spec.size_bytes
                )));
            }
            continue;
        }
        check_partition_floor(row, spec, default_filesystem, spec.size_bytes)?;
    }

    // 有分区要「占满剩余」时它至少要放得下自己的文件系统，否则建出来的东西
    // 用不上（FAT32）或根本建不出来。容器只需放得下 EBR。
    if auto_count == 1 {
        let (index, spec) = specs
            .iter()
            .enumerate()
            .find(|(_, s)| s.size_bytes == 0)
            .expect("auto_count == 1 保证存在");
        let row = index + 1;
        if spec.is_extended() {
            if remaining < MIN_EXTENDED_BYTES {
                return Err(CoreError::NoSpace {
                    needed: MIN_EXTENDED_BYTES,
                    available: remaining,
                });
            }
        } else {
            // 与显式容量走**同一个**判定：不格式化的分区在这里也会得到
            // 「放不下分区项」的 InvalidArgument，而不是被静默丢成零长度分区。
            check_partition_floor(row, spec, default_filesystem, remaining)?;
        }
    }

    Ok(specs
        .iter()
        .map(|s| {
            if s.size_bytes == 0 {
                remaining
            } else {
                s.size_bytes
            }
        })
        .collect())
}

/// 单个分区的容量下限校验。
///
/// 下限取该分区**实际要使用的文件系统**（`Inherit` 用 `default` 解析），
/// 因此同一个镜像里的 FAT32 与 ext4 分区各按各的门槛判。不格式化的分区没有
/// 文件系统下限，只要求放得下分区项。
fn check_partition_floor(
    row: usize,
    spec: &PartitionSpec,
    default: FilesystemType,
    size_bytes: u64,
) -> Result<()> {
    match spec.filesystem.resolve(default) {
        Some(filesystem) => {
            let minimum = filesystem.minimum_bytes();
            if size_bytes < minimum {
                return Err(CoreError::SizeBelowMinimum {
                    row,
                    filesystem: filesystem.as_str(),
                    requested: size_bytes,
                    minimum,
                });
            }
            Ok(())
        }
        None => {
            if size_bytes < crate::layout::ALIGNMENT_BYTES {
                return Err(CoreError::InvalidArgument(format!(
                    "partition {row} is {size_bytes} bytes, which is too small to hold a partition \
                     entry (minimum {} bytes)",
                    crate::layout::ALIGNMENT_BYTES
                )));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- MBR 布局求解

/// 一个已定位的分区（求解结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbrPlacement {
    /// 内核序号：主分区 1–4，逻辑分区从 5 起。
    pub index: u32,
    /// 起始 LBA（**绝对**，非相对 EBR）。
    ///
    /// 求解结果一律给绝对值；相对化只发生在 [`crate::partition`] 写 EBR 字节时。
    pub first_lba: u32,
    /// 结束 LBA（含）。
    pub last_lba: u32,
    /// 主分区还是逻辑分区。
    pub kind: PartitionKind,
    /// 分区类型字节（逻辑分区不会是 `0x05`）。
    pub type_byte: u8,
    /// 该分区在入参 `specs` 里的下标。
    pub spec_index: usize,
}

impl MbrPlacement {
    /// 扇区数。
    pub const fn sectors(&self) -> u32 {
        self.last_lba - self.first_lba + 1
    }
}

/// EBR（Extended Boot Record）在磁盘上的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EbrPlacement {
    /// EBR 自身所在 LBA。
    pub lba: u32,
    /// 本 EBR 描述的逻辑分区在 `specs` 里的下标。
    pub spec_index: usize,
    /// 下一个 EBR 的 LBA；`None` 表示这是链尾。
    pub next_lba: Option<u32>,
}

/// MBR 布局的完整求解结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbrPlan {
    /// 全部**分区**（主分区在前，逻辑分区按链序在后），`index` 已按内核惯例编号。
    ///
    /// **不含扩展容器**：容器不是分区，不占内核序号。它的槽位与区间分别在
    /// [`MbrPlan::extended_slot`] 与 [`MbrPlan::extended_range`] 里。
    pub placements: Vec<MbrPlacement>,
    /// 扩展分区在首扇区里的槽位下标；`None` 表示没有扩展分区。
    ///
    /// 有逻辑分区（隐式容器）或用户显式声明了容器（可为空）时都会是 `Some`。
    pub extended_slot: Option<usize>,
    /// 扩展分区覆盖的区间 `(first_lba, last_lba)`；`None` 表示没有扩展分区。
    ///
    /// **空容器**（没有逻辑分区）时本区间由容器行声明的容量推出，此时
    /// [`MbrPlan::ebrs`] 为空。
    pub extended_range: Option<(u32, u32)>,
    /// EBR 链，按磁盘顺序。空容器时为空。
    pub ebrs: Vec<EbrPlacement>,
    /// 显式声明容器的行在 `specs` 里的下标（用于把容量/展示对回去）。
    pub explicit_extended_spec_index: Option<usize>,
}

impl MbrPlan {
    /// 是否有逻辑分区。
    pub fn has_logical(&self) -> bool {
        !self.ebrs.is_empty()
    }

    /// 是否有扩展分区容器（有逻辑分区，或用户显式声明了空容器）。
    pub fn has_extended(&self) -> bool {
        self.extended_slot.is_some()
    }
}

/// 求解 MBR 布局：决定主分区槽位、扩展分区区间、EBR 位置与各分区绝对 LBA。
///
/// **纯函数**：只吃规格与总扇区数，因此可以在主机上穷举测试所有槽位组合
/// ——「写坏分区表」在真机上代价很高，这里必须一次算对。
///
/// ## 布局规则
///
/// 1. 主分区按 `specs` 顺序占用首扇区的槽位 `0..n_primary`；
/// 2. 若有扩展分区（有逻辑分区，或用户显式声明了容器），**紧随主分区之后**
///    再占一个槽位。因此 `n_primary + 1 <= 4`（即最多 3 主 + 扩展分区）；
/// 3. 每个分区（含逻辑分区）从下一个 1 MiB 边界开始，长度为其容量；
/// 4. 每个逻辑分区**前面**有一个 EBR 扇区，位于该逻辑分区起始 LBA 的前一个
///    扇区；首个 EBR 与逻辑分区起点对齐，后续 EBR 各自紧邻其分区；
/// 5. 扩展分区区间从**首个 EBR** 起，到**最后一个逻辑分区结束**止——必须覆盖
///    整条 EBR 链，否则其他分区工具会认为链越界；
/// 6. 分区号：主分区 1..n，逻辑分区从 5 起（Linux 惯例）。**扩展容器不占序号**。
///
/// ## 无逻辑分区时
///
/// 若也没有显式声明的容器，输出与「只写主分区」的既有行为**逐字节相同**：
/// 不产生任何 EBR、不创建扩展分区、游标推进方式与旧实现一致。这是回归底线，
/// 由测试钉住。
///
/// 若显式声明了空容器，则扩展区间为 `[对齐边界, 对齐边界 + 容器容量)`，
/// **不产生任何 EBR**。
///
/// ## 容量的两个来源
///
/// 有逻辑分区时，容器的区间**一律由 EBR 链推导**，显式容器行的容量被忽略；
/// 只有在容器为空时，其容量才由用户声明决定。这条规则必须唯一，否则「容器多大」
/// 会有两个互相矛盾的答案。
pub fn resolve_mbr_layout(
    specs: &[PartitionSpec],
    sizes: &[u64],
    total_sectors: u32,
    alignment_sectors: u64,
) -> Result<MbrPlan> {
    if specs.len() != sizes.len() {
        return Err(CoreError::InvalidArgument(
            "partition spec and size lists have different lengths".into(),
        ));
    }
    if specs.is_empty() {
        return Err(CoreError::InvalidArgument(
            "MBR requires at least one partition".into(),
        ));
    }

    let alignment = u32::try_from(alignment_sectors)
        .map_err(|_| CoreError::InvalidArgument("alignment sector count exceeds u32".into()))?;

    // 主分区先行、逻辑分区随后：这样首扇区槽位顺序稳定，且逻辑分区的 EBR 链
    // 连续排布在扩展分区区间内。容器行两类都不属于——它不产出 `MbrPlacement`。
    let primary_indices: Vec<usize> = (0..specs.len())
        .filter(|&i| specs[i].kind == PartitionKind::Primary)
        .collect();
    let logical_indices: Vec<usize> = (0..specs.len())
        .filter(|&i| specs[i].is_logical())
        .collect();
    let explicit_extended: Option<usize> = (0..specs.len()).find(|&i| specs[i].is_extended());

    let has_logical = !logical_indices.is_empty();
    // 容器的存在性有两个来源：有逻辑分区（隐式），或用户显式声明（可空）。
    let has_extended = has_logical || explicit_extended.is_some();

    let needed_slots = primary_indices.len() + usize::from(has_extended);
    if needed_slots > MBR_MAX_PRIMARY {
        return Err(CoreError::InvalidArgument(format!(
            "the MBR first sector holds only {MBR_MAX_PRIMARY} partition entries: {} primary \
             partition(s){} already fill it, leaving no room for an extended container. Change one \
             primary partition to logical, or use fewer primary partitions",
            primary_indices.len(),
            if has_extended {
                " plus 1 extended container"
            } else {
                ""
            }
        )));
    }

    let mut placements: Vec<MbrPlacement> = Vec::with_capacity(specs.len());
    let mut ebrs: Vec<EbrPlacement> = Vec::new();

    // 游标：下一个可用 LBA。与旧实现一致，从第一个对齐边界开始。
    let mut cursor = alignment;

    // ---- 主分区 ----
    for (ordinal, &spec_index) in primary_indices.iter().enumerate() {
        let sectors = sectors_for(&specs[spec_index], sizes[spec_index])?;
        let start = align_to(cursor, alignment);
        let (first, last) = place(start, sectors, total_sectors)?;

        placements.push(MbrPlacement {
            index: u32::try_from(ordinal + 1).unwrap_or(u32::MAX),
            first_lba: first,
            last_lba: last,
            kind: PartitionKind::Primary,
            type_byte: specs[spec_index].effective_mbr_type().byte(),
            spec_index,
        });
        cursor = last + 1;
    }

    // ---- 扩展分区与 EBR 链 ----
    let mut extended_slot = None;
    let mut extended_range = None;

    if has_extended {
        // 扩展分区自己占一个槽位，编号紧跟在主分区之后。
        extended_slot = Some(primary_indices.len());

        // 首个 EBR 放在扩展区间起点，需对齐；每个逻辑分区占「1 个 EBR 扇区 +
        // 自身容量」，因此逐段推进游标。
        let mut next_ebr = align_to(cursor, alignment);
        let first_ebr = next_ebr;

        for (ordinal, &spec_index) in logical_indices.iter().enumerate() {
            let sectors = sectors_for(&specs[spec_index], sizes[spec_index])?;

            // EBR 在前，分区紧随其后（EBR 占 1 个扇区，不能与分区重叠）。
            let ebr_lba = next_ebr;
            let start = ebr_lba.checked_add(1).ok_or_else(|| {
                CoreError::InvalidArgument("MBR partition layout overflow".into())
            })?;
            let (first, last) = place(start, sectors, total_sectors)?;

            if first != start {
                // `place` 只做边界检查、不会移动起点；偏离说明上面算错了。
                return Err(CoreError::VerifyFailed(
                    "MBR logical partition start disagrees with the EBR layout".into(),
                ));
            }

            // 逻辑分区序号从 5 起（主分区占 1–4，即使用不满也仍然这样编号）。
            let index = u32::try_from(MBR_MAX_PRIMARY + 1 + ordinal).unwrap_or(u32::MAX);
            placements.push(MbrPlacement {
                index,
                first_lba: first,
                last_lba: last,
                kind: PartitionKind::Logical,
                type_byte: specs[spec_index].effective_mbr_type().byte(),
                spec_index,
            });

            ebrs.push(EbrPlacement {
                lba: ebr_lba,
                spec_index,
                next_lba: None, // 链尾待定，下面回填
            });

            // 下一个 EBR 对齐到分区结束之后。
            next_ebr = align_to(last + 1, alignment);
        }

        // 回填链指针：每个 EBR 指向下一个，链尾为 `None`。
        for i in 0..ebrs.len().saturating_sub(1) {
            ebrs[i].next_lba = Some(ebrs[i + 1].lba);
        }

        if has_logical {
            // 扩展区间必须覆盖整条链：从首个 EBR 到最后一个逻辑分区结束。
            //
            // **有逻辑分区时容器行的容量被忽略**：链的实际跨度是唯一自洽的答案。
            // 同时接受两个来源（用户填的容量与链的实际跨度）只会产生矛盾输入。
            let last_lba = placements
                .iter()
                .filter(|p| p.kind == PartitionKind::Logical)
                .map(|p| p.last_lba)
                .max()
                .ok_or_else(|| {
                    CoreError::VerifyFailed("logical partition resolution produced nothing".into())
                })?;

            extended_range = Some((first_ebr, last_lba));
        } else {
            // **空容器**：没有逻辑分区可依附，区间由容器行声明的容量推出。
            //
            // 容量下限已由 [`resolve_sizes`] 保证，因此这里 `sectors_for` 不会
            // 因「不足一个扇区」失败。区间从对齐边界起、向下取容器容量。
            let spec_index = explicit_extended.ok_or_else(|| {
                CoreError::VerifyFailed("empty extended partition has no source row".into())
            })?;
            let sectors = sectors_for(&specs[spec_index], sizes[spec_index])?;
            let (first, last) = place(first_ebr, sectors, total_sectors)?;
            extended_range = Some((first, last));
        }
    }

    // 求解结果必须按内核序号排列，调用方才能直接 zip 到响应里。
    placements.sort_by_key(|p| p.index);

    Ok(MbrPlan {
        placements,
        extended_slot,
        extended_range,
        ebrs,
        explicit_extended_spec_index: explicit_extended,
    })
}

/// 求某分区需要的扇区数。
fn sectors_for(spec: &PartitionSpec, size_bytes: u64) -> Result<u32> {
    let sectors = size_bytes.div_ceil(crate::layout::SECTOR_BYTES);
    let narrowed = u32::try_from(sectors).map_err(|_| {
        CoreError::InvalidArgument(format!(
            "partition '{}' exceeds the MBR 32-bit LBA limit",
            spec.name
        ))
    })?;
    if narrowed == 0 {
        return Err(CoreError::InvalidArgument(format!(
            "partition '{}' is too small to hold a single sector",
            spec.name
        )));
    }
    Ok(narrowed)
}

/// 从 `start` 起放置 `sectors` 个扇区，校验不越界。
fn place(start: u32, sectors: u32, total_sectors: u32) -> Result<(u32, u32)> {
    let end_exclusive = start
        .checked_add(sectors)
        .ok_or_else(|| CoreError::InvalidArgument("MBR partition layout overflow".into()))?;

    if end_exclusive > total_sectors {
        return Err(CoreError::NoSpace {
            needed: u64::from(sectors) * crate::layout::SECTOR_BYTES,
            available: u64::from(total_sectors.saturating_sub(start)) * crate::layout::SECTOR_BYTES,
        });
    }
    Ok((start, end_exclusive - 1))
}

/// 向上对齐到 `align` 的整数倍（`u32` 版本）。
const fn align_to(lba: u32, align: u32) -> u32 {
    if align == 0 {
        return lba;
    }
    let rem = lba % align;
    if rem == 0 { lba } else { lba + (align - rem) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageLayout;
    use crate::layout::SECTOR_BYTES;

    fn spec(size: u64) -> PartitionSpec {
        PartitionSpec {
            size_bytes: size,
            gpt_type: Some(GptPartitionType::MicrosoftBasic),
            mbr_type: Some(MbrPartitionType::Fat32Lba),
            name: "MAIN".into(),
            filesystem: PartitionFilesystem::Some(FilesystemType::Fat32),
            kind: PartitionKind::Primary,
        }
    }

    // ------------------------------------------------ 两套类型空间

    #[test]
    fn gpt_and_mbr_types_are_independent_spaces() {
        // 拆分的理由：同一个"FAT32"在 GPT 是 BASIC Data 的 GUID，在 MBR 是 0x0C，
        // 两者没有对应关系。旧模型用单一枚举承载两套语义，导致 fat32_lba 与
        // microsoft_basic 无法区分（都映射到 BASIC）。
        let gpt = GptPartitionType::MicrosoftBasic.guid();
        assert_eq!(
            gpt.to_string().to_uppercase(),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
        assert_eq!(MbrPartitionType::Fat32Lba.byte(), 0x0C);

        // 两套空间各自独立，不存在跨空间相等性。
        assert_ne!(MbrPartitionType::Fat32Lba, MbrPartitionType::Linux);
    }

    #[test]
    fn gpt_type_wire_names_round_trip() {
        for t in GptPartitionType::presets() {
            let wire = t.as_wire();
            assert_eq!(
                GptPartitionType::parse(&wire).as_ref(),
                Some(&t),
                "wire={wire}"
            );
        }
    }

    #[test]
    fn mbr_type_wire_names_round_trip() {
        for t in MbrPartitionType::presets() {
            let wire = t.as_wire();
            assert_eq!(MbrPartitionType::parse(&wire), Some(t), "wire={wire}");
        }
    }

    #[test]
    fn custom_gpt_guid_round_trips_through_wire() {
        let guid = uuid::Uuid::parse_str("12345678-9ABC-DEF0-1234-56789ABCDEF0").unwrap();
        let t = GptPartitionType::Custom(guid);

        let wire = t.as_wire();
        assert_eq!(wire, "gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0");
        assert_eq!(GptPartitionType::parse(&wire), Some(t));

        // 大小写不敏感：用户手写小写也必须接受。
        assert_eq!(
            GptPartitionType::parse("gpt:12345678-9abc-def0-1234-56789abcdef0"),
            Some(GptPartitionType::Custom(guid))
        );
    }

    #[test]
    fn custom_mbr_byte_round_trips_through_wire() {
        let t = MbrPartitionType::Custom(0x1A);
        assert_eq!(t.as_wire(), "mbr:0x1A");
        assert_eq!(MbrPartitionType::parse("mbr:0x1a"), Some(t));
        // 裸十六进制也接受。
        assert_eq!(MbrPartitionType::parse("mbr:1a"), Some(t));
        assert_eq!(t.byte(), 0x1A);
    }

    #[test]
    fn gpt_guid_matches_crate_constants() {
        // 这些 GUID 必须与 gpt crate 的常量逐字一致，否则同一类型在写入与
        // 读取两处会被判成不同的东西。
        let cases = [
            (
                GptPartitionType::EfiSystem,
                "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            ),
            (
                GptPartitionType::MicrosoftBasic,
                "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7",
            ),
            (
                GptPartitionType::MicrosoftReserved,
                "E3C9E316-0B5C-4DB8-817D-F92DF00215AE",
            ),
            (
                GptPartitionType::WindowsRecovery,
                "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC",
            ),
            (
                GptPartitionType::LinuxFilesystem,
                "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
            ),
            (
                GptPartitionType::LinuxSwap,
                "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F",
            ),
            (
                GptPartitionType::LinuxLvm,
                "E6D6D379-F507-44C2-A23C-238F2A3DF928",
            ),
            (
                GptPartitionType::LinuxRaid,
                "A19D880F-05FC-4D3B-A006-743F0F84911E",
            ),
            (
                GptPartitionType::BiosBoot,
                "21686148-6449-6E6F-744E-656564454649",
            ),
        ];
        for (t, expected) in cases {
            assert_eq!(t.guid().to_string().to_uppercase(), expected, "{t:?}");
        }
    }

    #[test]
    fn wire_names_require_prefix() {
        // 不带前缀的名字一律拒绝——那正是过去两种布局混用的来源。
        assert_eq!(GptPartitionType::parse("microsoft_basic"), None);
        assert_eq!(MbrPartitionType::parse("fat32_lba"), None);
        assert_eq!(GptPartitionType::parse("mbr:fat32_lba"), None);
        assert_eq!(MbrPartitionType::parse("gpt:microsoft_basic"), None);
        assert_eq!(GptPartitionType::parse("gpt:nonsense"), None);
        assert_eq!(MbrPartitionType::parse("mbr:nonsense"), None);
    }

    #[test]
    fn defaults_are_the_legacy_values() {
        // 缺省必须与旧行为一致，否则老调用方会静默改变分区表。
        assert_eq!(
            GptPartitionType::default(),
            GptPartitionType::MicrosoftBasic
        );
        assert_eq!(MbrPartitionType::default(), MbrPartitionType::Fat32Lba);
    }

    #[test]
    fn effective_types_fall_back_to_defaults() {
        let s = PartitionSpec::fill_remaining("X");
        assert_eq!(s.effective_gpt_type(), GptPartitionType::MicrosoftBasic);
        assert_eq!(s.effective_mbr_type(), MbrPartitionType::Fat32Lba);

        let s = PartitionSpec::fill_remaining("X")
            .with_gpt_type(GptPartitionType::LinuxFilesystem)
            .with_mbr_type(MbrPartitionType::Linux);
        assert_eq!(s.effective_gpt_type(), GptPartitionType::LinuxFilesystem);
        assert_eq!(s.effective_mbr_type(), MbrPartitionType::Linux);
    }

    // ------------------------------------------------ 结构校验

    #[test]
    fn validate_rejects_empty() {
        let err = validate(&[], ImageLayout::Gpt).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
    }

    #[test]
    fn validate_rejects_raw_multi_partition() {
        let err = validate(&[spec(1024), spec(1024)], ImageLayout::Raw).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        assert!(validate(&[spec(1024)], ImageLayout::Raw).is_ok());
    }

    #[test]
    fn validate_enforces_mbr_four_primary_limit() {
        assert!(validate(&vec![spec(1024); 4], ImageLayout::Mbr).is_ok());

        let err = validate(&vec![spec(1024); 5], ImageLayout::Mbr).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        // 同样 5 个在 GPT 下必须通过——上限是布局相关的。
        assert!(validate(&vec![spec(1024); 5], ImageLayout::Gpt).is_ok());
    }

    #[test]
    fn validate_rejects_overlong_gpt_name_only_on_gpt() {
        let mut s = spec(1024);
        s.name = "x".repeat(GPT_NAME_MAX_UTF16 + 1);

        let err = validate(&[s.clone()], ImageLayout::Gpt).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        // MBR 不写名字，故不应因为这个字段被拒。
        assert!(validate(&[s], ImageLayout::Mbr).is_ok());
    }

    #[test]
    fn validate_counts_utf16_units_not_bytes() {
        let mut s = spec(1024);
        s.name = "汉".repeat(GPT_NAME_MAX_UTF16);
        assert!(validate(&[s], ImageLayout::Gpt).is_ok());

        let mut s = spec(1024);
        s.name = "汉".repeat(GPT_NAME_MAX_UTF16 + 1);
        assert!(validate(&[s], ImageLayout::Gpt).is_err());
    }

    #[test]
    fn validate_rejects_empty_mbr_type_on_mbr() {
        // 类型字节 0x00 会让分区项被判为空项而"消失"，用户填的容量白填。
        let mut s = spec(1024);
        s.mbr_type = Some(MbrPartitionType::Empty);

        let err = validate(&[s.clone()], ImageLayout::Mbr).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        // GPT 下同一份规格无关紧要（它不写 MBR 字节），故应通过。
        assert!(validate(&[s], ImageLayout::Gpt).is_ok());
    }

    // ------------------------------------------------ 容量展开

    #[test]
    fn resolve_sizes_expands_single_auto() {
        let sizes = resolve_sizes(
            &[spec(64 * 1024 * 1024), PartitionSpec::fill_remaining("B")],
            256 * 1024 * 1024,
            FilesystemType::Fat32,
        )
        .unwrap();
        assert_eq!(sizes[0], 64 * 1024 * 1024);
        assert_eq!(sizes[1], 192 * 1024 * 1024);
    }

    #[test]
    fn resolve_sizes_rejects_multiple_auto() {
        let err = resolve_sizes(
            &[
                PartitionSpec::fill_remaining("A"),
                PartitionSpec::fill_remaining("B"),
            ],
            256 * 1024 * 1024,
            FilesystemType::Fat32,
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
    }

    #[test]
    fn resolve_sizes_rejects_overflow_without_clamping() {
        let err = resolve_sizes(
            &[spec(512 * 1024 * 1024)],
            256 * 1024 * 1024,
            FilesystemType::Fat32,
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::NoSpace { .. }));
    }

    #[test]
    fn resolve_sizes_rejects_auto_smaller_than_its_filesystem_floor() {
        // 「占满剩余」的分区同样要过**它自己**文件系统的下限：FAT32 拿不到 33 MiB
        // 就建不出 FAT32，报 SizeBelowMinimum 并指出是哪一行。
        let err = resolve_sizes(
            &[PartitionSpec::fill_remaining("A")],
            32 * 1024 * 1024,
            FilesystemType::Fat32,
        )
        .unwrap_err();
        match err {
            CoreError::SizeBelowMinimum {
                row,
                filesystem,
                requested,
                minimum,
            } => {
                assert_eq!(row, 1);
                assert_eq!(filesystem, "fat32");
                assert_eq!(requested, 32 * 1024 * 1024);
                assert_eq!(minimum, crate::layout::MIN_FAT32_BYTES);
            }
            other => panic!("应报 SizeBelowMinimum，得到 {other:?}"),
        }
    }

    #[test]
    fn resolve_sizes_exact_fit_leaves_zero_for_nothing() {
        // 不格式化的分区没有文件系统下限，只剩"放得下分区项"这一条（1 个对齐单位）。
        let unformatted = |size| spec(size).with_filesystem(PartitionFilesystem::None);
        assert_eq!(
            resolve_sizes(
                &[unformatted(crate::layout::ALIGNMENT_BYTES)],
                crate::layout::ALIGNMENT_BYTES,
                FilesystemType::Fat32,
            )
            .unwrap(),
            vec![crate::layout::ALIGNMENT_BYTES]
        );
    }

    // ------------------------------------------------ 每分区文件系统

    #[test]
    fn filesystem_intent_is_three_state() {
        // 缺省是"继承"，不是"不格式化"——两者必须可区分。
        let s = PartitionSpec::fill_remaining("X");
        assert_eq!(s.filesystem, PartitionFilesystem::Inherit);
        assert_eq!(
            s.filesystem.resolve(FilesystemType::Fat32),
            Some(FilesystemType::Fat32)
        );

        // 显式不格式化：即使有全局默认也不格式化。
        let s = s.with_filesystem(PartitionFilesystem::None);
        assert_eq!(s.filesystem.resolve(FilesystemType::Fat32), None);

        // 显式指定：覆盖全局默认。
        let s = s.with_filesystem(PartitionFilesystem::Some(FilesystemType::Ext4));
        assert_eq!(
            s.filesystem.resolve(FilesystemType::Fat32),
            Some(FilesystemType::Ext4)
        );
    }

    #[test]
    fn builder_methods_chain() {
        let s = PartitionSpec::fill_remaining("DATA")
            .with_size(1024)
            .with_gpt_type(GptPartitionType::LinuxFilesystem)
            .with_mbr_type(MbrPartitionType::Linux)
            .with_name("ROOT")
            .with_filesystem(PartitionFilesystem::Some(FilesystemType::Ext4));
        assert_eq!(s.size_bytes, 1024);
        assert_eq!(s.name, "ROOT");
        assert_eq!(s.effective_gpt_type(), GptPartitionType::LinuxFilesystem);
        assert_eq!(s.effective_mbr_type(), MbrPartitionType::Linux);
        assert_eq!(
            s.filesystem,
            PartitionFilesystem::Some(FilesystemType::Ext4)
        );
    }

    #[test]
    fn max_partitions_follows_layout() {
        assert_eq!(max_partitions(ImageLayout::Raw), 1);
        assert_eq!(max_partitions(ImageLayout::Mbr), MBR_MAX_PRIMARY);
        assert_eq!(max_partitions(ImageLayout::Gpt), MAX_PARTITIONS);
        // 总数上界把扩展分区容器让出的那个槽位算进去。
        assert_eq!(
            max_total_partitions(ImageLayout::Mbr),
            MBR_MAX_PRIMARY - 1 + MBR_MAX_LOGICAL
        );
    }

    // ------------------------------------------------ 扩展分区与逻辑分区

    /// 1 MiB 对齐（与 `layout::ALIGNMENT_SECTORS` 一致）。
    const ALIGN: u64 = 2048;

    /// 造一个逻辑分区规格。
    fn logical(size: u64) -> PartitionSpec {
        spec(size).with_kind(PartitionKind::Logical)
    }

    /// 一个扩展分区**容器**行：归属为扩展分区，且不格式化（它没有数据区）。
    fn container(size: u64) -> PartitionSpec {
        spec(size)
            .with_kind(PartitionKind::Extended)
            .with_filesystem(PartitionFilesystem::None)
    }

    #[test]
    fn kind_wire_names_round_trip() {
        for kind in [PartitionKind::Primary, PartitionKind::Logical] {
            assert_eq!(PartitionKind::parse(kind.as_wire()), Some(kind));
        }
        assert_eq!(
            PartitionKind::parse("LOGICAL"),
            Some(PartitionKind::Logical)
        );
        assert_eq!(PartitionKind::parse("l"), Some(PartitionKind::Logical));
        assert_eq!(PartitionKind::parse("p"), Some(PartitionKind::Primary));
        assert_eq!(PartitionKind::parse("nonsense"), None);
        assert_eq!(PartitionKind::default(), PartitionKind::Primary);
    }

    #[test]
    fn extended_type_is_not_a_user_preset() {
        // `Extended` 是容器，写入侧自动创建；把它摆在预设里正是旧缺陷的来源
        // ——用户选中它得到的是一块挂不上任何东西的空壳。
        assert!(!MbrPartitionType::presets().contains(&MbrPartitionType::Extended));
        // 但仍能解析，以便存量请求不报错（线格式向后兼容）。
        assert_eq!(
            MbrPartitionType::parse("mbr:extended"),
            Some(MbrPartitionType::Extended)
        );
    }

    #[test]
    fn validate_rejects_logical_on_gpt_and_raw() {
        // GPT 没有扩展分区机制。静默当作主分区会让用户在 Host 上得到与预期
        // 不符的分区表，故必须明确拒绝。
        let err = validate(&[logical(1024)], ImageLayout::Gpt).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");

        let err = validate(&[logical(1024)], ImageLayout::Raw).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn validate_rejects_extended_type_on_a_non_container_row() {
        // 容器由**归属**表达（`PartitionKind::Extended`），不是由类型字节。
        // 一个普通主分区却填了 `0x05` 是矛盾的：类型是系统生成的，不是可选项。
        let mut s = spec(1024);
        s.mbr_type = Some(MbrPartitionType::Extended);
        let err = validate(&[s], ImageLayout::Mbr).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
        // 错误信息必须指引改用「归属」，否则用户不知道该怎么办。
        assert!(
            err.to_string().contains("change its kind to 'extended'"),
            "{err}"
        );
    }

    #[test]
    fn validate_accepts_container_row_and_ignores_its_declared_type() {
        // 归属为扩展分区时，`mbr_type` 是无关字段（容器字节恒为 `0x05`），
        // 不该因此报错——UI 在容器行上根本不渲染类型选择器。
        let mut c = container(0);
        c.mbr_type = Some(MbrPartitionType::Extended);
        assert!(validate(&[spec(1024), c], ImageLayout::Mbr).is_ok());

        // 但容器不能声明要格式化：它没有数据区。
        let mut formatted = container(0);
        formatted.filesystem = PartitionFilesystem::Some(FilesystemType::Fat32);
        let err = validate(&[spec(1024), formatted], ImageLayout::Mbr).unwrap_err();
        assert!(err.to_string().contains("has no data area"), "{err}");
    }

    #[test]
    fn validate_rejects_more_than_one_container() {
        // 首扇区里只有一个扩展分区项可写，两个容器无处安放。
        let err = validate(
            &[spec(1024), container(1024), container(1024)],
            ImageLayout::Mbr,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("allows only one extended container"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_container_on_gpt_and_raw() {
        // GPT/raw 没有扩展分区机制：静默当作主分区会让用户在 Host 上得到
        // 与预期不符的分区表。
        let c = container(1024);
        // GPT：走「非 MBR 一律拒绝」那条分支。
        let err = validate(std::slice::from_ref(&c), ImageLayout::Gpt).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
        assert!(err.to_string().contains("MBR-only"), "{err}");

        // raw：走「没有分区表」那条更早的分支（这里是**先前漏掉的一处**：
        // 只检查了 `is_logical`，容器会被静默放行）。
        let err = validate(std::slice::from_ref(&c), ImageLayout::Raw).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
        assert!(err.to_string().contains("extended container"), "{err}");
    }

    #[test]
    fn validate_counts_container_as_a_slot_but_not_as_a_partition() {
        // 「1 主 + 1 空容器」合法：2 个槽位。
        assert!(
            validate(&[spec(1024), container(1024)], ImageLayout::Mbr).is_ok(),
            "1 主 + 1 空扩展容器应当合法"
        );

        // 「4 主 + 1 容器」需要 5 个槽位，必须拒绝。
        let four = vec![
            spec(1024),
            spec(1024),
            spec(1024),
            spec(1024),
            container(1024),
        ];
        let err = validate(&four, ImageLayout::Mbr).unwrap_err();
        assert!(err.to_string().contains("4 primary partition"), "{err}");

        // 「3 主 + 1 空容器」正好用满 4 个槽位，合法。
        let three = vec![spec(1024), spec(1024), spec(1024), container(1024)];
        assert!(validate(&three, ImageLayout::Mbr).is_ok());
    }

    #[test]
    fn validate_rejects_logical_declaring_extended_type() {
        let mut s = logical(1024);
        s.mbr_type = Some(MbrPartitionType::Extended);
        let err = validate(&[s], ImageLayout::Mbr).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn validate_allows_three_primary_plus_logical() {
        // 3 主 + 1 逻辑：正好用满 4 个槽位（3 主 + 1 扩展容器）。
        let specs = vec![spec(1024), spec(1024), spec(1024), logical(1024)];
        assert!(validate(&specs, ImageLayout::Mbr).is_ok());
    }

    #[test]
    fn validate_rejects_four_primary_plus_logical() {
        // 4 主已占满槽位，扩展分区无处安放。
        let specs = vec![
            spec(1024),
            spec(1024),
            spec(1024),
            spec(1024),
            logical(1024),
        ];
        let err = validate(&specs, ImageLayout::Mbr).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
        // 必须说清"为什么"，而不是只报一个数字上限。
        assert!(err.to_string().contains("extended"), "{err}");
    }

    #[test]
    fn validate_allows_many_logical_partitions_within_slots() {
        let mut specs = vec![spec(1024)];
        specs.extend((0..10).map(|_| logical(1024)));
        assert!(validate(&specs, ImageLayout::Mbr).is_ok());
    }

    #[test]
    fn validate_rejects_too_many_logical_partitions() {
        let mut specs = vec![spec(1024)];
        specs.extend((0..=MBR_MAX_LOGICAL).map(|_| logical(1024)));
        let err = validate(&specs, ImageLayout::Mbr).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn validate_allows_four_primary_without_logical() {
        // 回归：不启用逻辑分区时，4 主分区的既有上限不变。
        assert!(validate(&vec![spec(1024); 4], ImageLayout::Mbr).is_ok());
    }

    // -------------------------------- 求解器

    /// 总扇区数：给足的镜像（128 MiB）。
    const TOTAL: u32 = 128 * 1024 * 1024 / 512;

    #[test]
    fn solver_without_logical_matches_sequential_layout() {
        // **回归底线**：没有逻辑分区时，求解结果必须与旧写入器的顺序布局一致
        // ——游标从第一个对齐边界开始，逐个分区紧接其后。
        let specs = vec![spec(64 * 1024 * 1024), spec(32 * 1024 * 1024)];
        let sizes = vec![64 * 1024 * 1024u64, 32 * 1024 * 1024];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        assert!(!plan.has_logical());
        assert_eq!(plan.extended_slot, None);
        assert_eq!(plan.extended_range, None);
        assert!(plan.ebrs.is_empty());
        assert_eq!(plan.placements.len(), 2);

        assert_eq!(plan.placements[0].index, 1);
        assert_eq!(plan.placements[0].first_lba, 2048);
        assert_eq!(plan.placements[0].kind, PartitionKind::Primary);
        // 64 MiB = 131072 扇区，故结束于 2048 + 131072 - 1。
        assert_eq!(plan.placements[0].last_lba, 2048 + 131072 - 1);

        // 第二个分区紧接着对齐后的下一个边界。
        assert_eq!(plan.placements[1].index, 2);
        assert_eq!(plan.placements[1].first_lba % 2048, 0);
        assert!(plan.placements[1].first_lba > plan.placements[0].last_lba);
    }

    #[test]
    fn solver_numbers_logical_partitions_from_five() {
        // Linux 惯例：主分区 1–4，逻辑分区从 5 起。序号必须与内核 `loopNpM`
        // 一致，否则 UI 显示的序号与设备名对不上。
        let specs = vec![
            spec(16 * 1024 * 1024),
            logical(16 * 1024 * 1024),
            logical(16 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64; 3];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        let indices: Vec<u32> = plan.placements.iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![1, 5, 6], "主分区 1，逻辑分区从 5 起");
    }

    #[test]
    fn solver_numbers_logical_from_five_even_when_primaries_are_fewer() {
        // 即使只有一个主分区，逻辑分区也从 5 起——这是内核的编号方式，
        // 不是"紧随主分区之后"。按后者编号会让 UI 序号与设备名错位。
        let specs = vec![spec(16 * 1024 * 1024), logical(16 * 1024 * 1024)];
        let sizes = vec![16 * 1024 * 1024u64; 2];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        let indices: Vec<u32> = plan.placements.iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![1, 5]);
    }

    #[test]
    fn solver_places_ebr_immediately_before_each_logical_partition() {
        // EBR 占一个扇区且不能与逻辑分区重叠：EBR 在前，分区紧随其后。
        let specs = vec![
            spec(16 * 1024 * 1024),
            logical(20 * 1024 * 1024),
            logical(20 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64, 20 * 1024 * 1024, 20 * 1024 * 1024];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        assert_eq!(plan.ebrs.len(), 2);
        for ebr in &plan.ebrs {
            let placement = plan
                .placements
                .iter()
                .find(|p| p.spec_index == ebr.spec_index)
                .unwrap();
            assert_eq!(
                placement.first_lba,
                ebr.lba + 1,
                "逻辑分区必须紧跟在它的 EBR 之后"
            );
        }
    }

    #[test]
    fn solver_links_ebr_chain_and_terminates_it() {
        let specs = vec![
            spec(16 * 1024 * 1024),
            logical(16 * 1024 * 1024),
            logical(16 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64; 3];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        assert_eq!(plan.ebrs[0].next_lba, Some(plan.ebrs[1].lba));
        assert_eq!(
            plan.ebrs[1].next_lba, None,
            "链尾必须终止，否则读取侧会绕环"
        );
    }

    #[test]
    fn solver_extended_range_covers_whole_ebr_chain() {
        // 扩展分区必须覆盖整条链：从首个 EBR 到最后一个逻辑分区结束。
        // 覆盖不全时其他分区工具会认为链越界。
        let specs = vec![
            spec(16 * 1024 * 1024),
            logical(20 * 1024 * 1024),
            logical(20 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64, 20 * 1024 * 1024, 20 * 1024 * 1024];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        let (start, end) = plan.extended_range.unwrap();
        assert_eq!(start, plan.ebrs[0].lba, "区间起点必须是首个 EBR");
        let last_logical = plan
            .placements
            .iter()
            .filter(|p| p.kind == PartitionKind::Logical)
            .map(|p| p.last_lba)
            .max()
            .unwrap();
        assert_eq!(start, plan.ebrs[0].lba);
        assert_eq!(end, last_logical, "区间终点必须是最后一个逻辑分区");
        // 每个 EBR 都必须落在区间内。
        for ebr in &plan.ebrs {
            assert!(ebr.lba >= start && ebr.lba <= end, "EBR {} 越界", ebr.lba);
        }
    }

    #[test]
    fn solver_puts_extended_container_in_slot_after_primaries() {
        let specs = vec![
            spec(16 * 1024 * 1024),
            spec(16 * 1024 * 1024),
            logical(16 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64; 3];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        // 两个主分区占槽 0、1，扩展容器占槽 2。
        assert_eq!(plan.extended_slot, Some(2));
    }

    #[test]
    fn solver_keeps_logical_partitions_non_overlapping() {
        let specs = vec![
            spec(16 * 1024 * 1024),
            logical(20 * 1024 * 1024),
            logical(20 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64, 20 * 1024 * 1024, 20 * 1024 * 1024];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        // 按磁盘位置排序后，任意两个区间不得相交。
        let mut by_pos: Vec<(u32, u32)> = plan
            .placements
            .iter()
            .map(|p| (p.first_lba, p.last_lba))
            .collect();
        by_pos.sort_unstable();
        for pair in by_pos.windows(2) {
            assert!(
                pair[1].0 > pair[0].1,
                "区间重叠：({}, {}) 与 ({}, {})",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }

        // EBR 也不能与任何分区重叠。
        for ebr in &plan.ebrs {
            for p in &plan.placements {
                assert!(
                    ebr.lba < p.first_lba || ebr.lba > p.last_lba,
                    "EBR {} 与分区 {} 重叠",
                    ebr.lba,
                    p.index
                );
            }
        }
    }

    #[test]
    fn solver_rejects_logical_when_primaries_fill_all_slots() {
        let specs: Vec<PartitionSpec> = (0..4)
            .map(|_| spec(16 * 1024 * 1024))
            .chain([logical(16 * 1024 * 1024)])
            .collect();
        let sizes = vec![16 * 1024 * 1024u64; 5];
        let err = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn solver_reports_no_space_when_chain_exceeds_image() {
        // 容量不足以容纳 EBR + 分区时必须报 NoSpace，不能静默裁剪。
        let specs = vec![spec(16 * 1024 * 1024), logical(200 * 1024 * 1024)];
        let sizes = vec![16 * 1024 * 1024u64, 200 * 1024 * 1024];
        let err = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap_err();
        assert!(matches!(err, CoreError::NoSpace { .. }), "{err:?}");
    }

    #[test]
    fn solver_rejects_mismatched_lengths() {
        let specs = vec![spec(1024)];
        let err = resolve_mbr_layout(&specs, &[1024, 1024], TOTAL, ALIGN).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn solver_rejects_zero_sector_partition() {
        // 容量为 0 的规格只可能在 `resolve_sizes` 之后出现（那一步会把 `0`
        // 展开成具体值）。若真到了这里，必须明确报错而不是写一个零长度分区项
        // ——读取侧会把零长度项判为空项而"消失"，用户填的容量白填。
        let specs = vec![spec(0)];
        let err = resolve_mbr_layout(&specs, &[0], TOTAL, ALIGN).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    // -------------------------------- 空扩展分区容器

    #[test]
    fn solver_places_empty_container_without_any_ebr() {
        // 「1 主 + 1 空容器」：容器占槽 1，区间由**用户声明的容量**推出，
        // 且**不产生任何 EBR**（里面没有逻辑分区）。
        let container_size = 32 * 1024 * 1024u64;
        let specs = vec![spec(16 * 1024 * 1024), container(container_size)];
        let sizes = vec![16 * 1024 * 1024u64, container_size];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        assert_eq!(plan.extended_slot, Some(1), "容器紧跟在主分区之后占槽");
        assert!(plan.ebrs.is_empty(), "空容器不得产生 EBR");
        assert!(!plan.has_logical(), "空容器没有逻辑分区");
        assert!(plan.has_extended(), "但扩展分区确实存在");
        assert_eq!(plan.explicit_extended_spec_index, Some(1));

        // 容器不是分区：不占内核序号、不出现在 placements 里。
        assert_eq!(plan.placements.len(), 1, "只有 1 个真正的分区");
        assert_eq!(plan.placements[0].index, 1);

        // 区间从主分区结束后的下一个对齐边界起，长度等于容器容量。
        let (first, last) = plan.extended_range.expect("空容器也要有区间");
        assert_eq!(u64::from(first) % ALIGN, 0, "容器起点必须对齐");
        assert_eq!(
            u64::from(last - first + 1) * SECTOR_BYTES,
            container_size,
            "容器区间必须精确等于声明的容量"
        );
    }

    #[test]
    fn solver_ignores_container_capacity_when_logical_partitions_exist() {
        // **容量的两个来源**：有逻辑分区时，容器区间由 EBR 链推导，用户为容器
        // 行填的容量被忽略。这条规则必须唯一，否则「容器多大」会有两个答案。
        let specs = vec![
            spec(16 * 1024 * 1024),
            container(8 * 1024 * 1024), // 故意填一个很小的值
            logical(16 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64, 8 * 1024 * 1024, 16 * 1024 * 1024];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();

        let (first, last) = plan.extended_range.unwrap();
        let span = u64::from(last - first + 1) * SECTOR_BYTES;

        assert!(
            span > 8 * 1024 * 1024,
            "容器区间必须覆盖整条 EBR 链（{span} 字节），而不是用户填的 8 MiB"
        );
        // 必须覆盖到逻辑分区结束。
        let logical = plan
            .placements
            .iter()
            .find(|p| p.kind == PartitionKind::Logical)
            .expect("应有逻辑分区");
        assert_eq!(last, logical.last_lba, "区间上界必须正好是最后一个逻辑分区");
        assert_eq!(plan.ebrs.len(), 1);
    }

    #[test]
    fn solver_container_takes_a_slot_even_when_empty() {
        // 三个主分区 + 一个空容器 = 正好 4 个槽位，合法。
        let specs = vec![
            spec(16 * 1024 * 1024),
            spec(16 * 1024 * 1024),
            spec(16 * 1024 * 1024),
            container(8 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64; 4];
        let plan = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap();
        assert_eq!(plan.extended_slot, Some(3), "空容器同样占一个槽位");

        // 四个主分区 + 一个空容器 = 5 个槽位，必须拒绝。
        let specs = vec![
            spec(16 * 1024 * 1024),
            spec(16 * 1024 * 1024),
            spec(16 * 1024 * 1024),
            spec(16 * 1024 * 1024),
            container(8 * 1024 * 1024),
        ];
        let sizes = vec![16 * 1024 * 1024u64; 5];
        let err = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn solver_reports_no_space_when_empty_container_overruns_image() {
        // 空容器也要过边界检查：声明一个超过镜像的容量必须报 NoSpace，
        // 不能静默截断（静默截断会让用户以为容器有自己填的那么大）。
        let specs = vec![spec(16 * 1024 * 1024), container(200 * 1024 * 1024)];
        let sizes = vec![16 * 1024 * 1024u64, 200 * 1024 * 1024];
        let err = resolve_mbr_layout(&specs, &sizes, TOTAL, ALIGN).unwrap_err();
        assert!(matches!(err, CoreError::NoSpace { .. }), "{err:?}");
    }

    #[test]
    fn resolve_sizes_shares_the_auto_slot_between_partition_and_container() {
        // 「占满剩余空间」只有一个名额，容器与分区共用它。两者都填 0 必须拒绝，
        // 否则"谁拿剩余"取决于实现细节，用户无法从界面推断。
        let specs = vec![spec(0), container(0)];
        let err = resolve_sizes(&specs, 128 * 1024 * 1024, FilesystemType::Fat32).unwrap_err();
        assert!(err.to_string().contains("at most one"), "{err}");

        // 单个容器填 0 时，它拿到扣除其余分区后的全部剩余。
        // 固定分区取 64 MiB：FAT32 下限之上，免得下限本身成为失败原因。
        let specs = vec![spec(64 * 1024 * 1024), container(0)];
        let sizes = resolve_sizes(&specs, 128 * 1024 * 1024, FilesystemType::Fat32).unwrap();
        assert_eq!(sizes[1], 128 * 1024 * 1024 - 64 * 1024 * 1024);
    }

    #[test]
    fn resolve_sizes_enforces_container_floor() {
        // 容器容量下限：小于下限的容器放不下任何对齐后的逻辑分区，建出来是个
        // 用不上的空壳，故明确拒绝而不是让它悄悄偏小。
        let specs = vec![container(4096)];
        let err = resolve_sizes(&specs, 128 * 1024 * 1024, FilesystemType::Fat32).unwrap_err();
        assert!(err.to_string().contains("minimum"), "{err}");

        // 刚好到下限则放行（边界不能靠"差不多"）。
        let specs = vec![container(MIN_EXTENDED_BYTES)];
        assert!(resolve_sizes(&specs, 128 * 1024 * 1024, FilesystemType::Fat32).is_ok());
    }

    #[test]
    fn container_floor_is_at_least_one_alignment_unit() {
        // 下限的取值理由：逻辑分区必须对齐，因此容器至少要能放得下一个对齐单位。
        // 若哪天对齐单位变大而这里没跟着改，容器就会建出放不下任何逻辑分区的
        // 空壳——故用测试把两者的关系钉住。
        //
        // 断言包在 `const {}` 里让它**编译期**求值：clippy 会把运行期对两个常量的
        // 比较判为 `assertions_on_constants`（两边都是常量，运行期判没有意义）。
        const { assert!(MIN_EXTENDED_BYTES >= crate::layout::ALIGNMENT_BYTES) };
    }

    // ------------------------------------------------ 下限按分区文件系统判定

    /// 构造"某个具体文件系统、给定容量"的分区。
    fn spec_with(filesystem: FilesystemType, size: u64) -> PartitionSpec {
        spec(size).with_filesystem(PartitionFilesystem::Some(filesystem))
    }

    #[test]
    fn partition_floor_depends_on_its_own_filesystem() {
        // 同一个 40 MiB 的容量：FAT32 嫌小（33 MiB 是下限，40 通过）、ext4 通过，
        // 而 40 MiB 对 exFAT 也通过。真正要证明的是**下限随分区而变化**——
        // 下面的 1 MiB 分区对 exFAT 合法、对 FAT32 非法。
        assert!(
            resolve_sizes(
                &[spec_with(FilesystemType::Fat32, 40 * 1024 * 1024)],
                256 * 1024 * 1024,
                FilesystemType::Fat32,
            )
            .is_ok()
        );

        // 1 MiB：exFAT 下限正是 1 MiB，通过；FAT32 报 SizeBelowMinimum。
        assert!(
            resolve_sizes(
                &[spec_with(FilesystemType::ExFat, 1024 * 1024)],
                256 * 1024 * 1024,
                FilesystemType::Fat32,
            )
            .is_ok()
        );

        let err = resolve_sizes(
            &[spec_with(FilesystemType::Fat32, 1024 * 1024)],
            256 * 1024 * 1024,
            FilesystemType::Fat32,
        )
        .unwrap_err();
        match err {
            CoreError::SizeBelowMinimum {
                row,
                filesystem,
                requested,
                minimum,
            } => {
                assert_eq!(row, 1);
                assert_eq!(filesystem, "fat32");
                assert_eq!(requested, 1024 * 1024);
                assert_eq!(minimum, crate::layout::MIN_FAT32_BYTES);
            }
            other => panic!("应报 SizeBelowMinimum，得到 {other:?}"),
        }
    }

    #[test]
    fn inherit_uses_the_global_default_filesystem() {
        // `Inherit` 的下限来自全局默认：同一个 1 MiB 分区，默认是 exFAT 时通过，
        // 默认是 FAT32 时被拒——这正是早先实现做不到的（它一律按 FAT32 判）。
        let inherited = PartitionSpec::fill_remaining("X").with_size(1024 * 1024);
        assert!(
            resolve_sizes(
                std::slice::from_ref(&inherited),
                256 * 1024 * 1024,
                FilesystemType::ExFat
            )
            .is_ok()
        );
        let err =
            resolve_sizes(&[inherited], 256 * 1024 * 1024, FilesystemType::Fat32).unwrap_err();
        assert!(matches!(err, CoreError::SizeBelowMinimum { .. }), "{err:?}");
    }

    #[test]
    fn unformatted_partition_only_needs_room_for_a_partition_entry() {
        // 不格式化的分区没有文件系统下限；但它仍要占一个非零的分区项。
        let bare = |size| spec(size).with_filesystem(PartitionFilesystem::None);
        assert!(
            resolve_sizes(
                &[bare(1024 * 1024)],
                256 * 1024 * 1024,
                FilesystemType::Fat32
            )
            .is_ok()
        );

        let err =
            resolve_sizes(&[bare(4096)], 256 * 1024 * 1024, FilesystemType::Fat32).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn row_number_points_at_the_offending_partition() {
        // 多分区时行号必须指向真正的违规行，而不是笼统地"容量不足"。
        let specs = vec![
            spec_with(FilesystemType::Ext4, 8 * 1024 * 1024),
            spec_with(FilesystemType::Fat32, 4 * 1024 * 1024),
        ];
        let err = resolve_sizes(&specs, 256 * 1024 * 1024, FilesystemType::Fat32).unwrap_err();
        match err {
            CoreError::SizeBelowMinimum {
                row, filesystem, ..
            } => {
                assert_eq!(row, 2, "违规的是第 2 行");
                assert_eq!(filesystem, "fat32");
            }
            other => panic!("应报 SizeBelowMinimum，得到 {other:?}"),
        }
    }

    #[test]
    fn ext4_floor_matches_measured_mke2fs_limit() {
        // 实测：1 MiB 报 `Filesystem too small for a journal`，2 MiB 可用。
        let err = resolve_sizes(
            &[spec_with(FilesystemType::Ext4, 1024 * 1024)],
            256 * 1024 * 1024,
            FilesystemType::Fat32,
        )
        .unwrap_err();
        match err {
            CoreError::SizeBelowMinimum {
                row,
                filesystem,
                minimum,
                ..
            } => {
                assert_eq!(row, 1);
                assert_eq!(filesystem, "ext4");
                assert_eq!(minimum, crate::layout::MIN_EXT4_BYTES);
            }
            other => panic!("应报 SizeBelowMinimum，得到 {other:?}"),
        }
        assert!(
            resolve_sizes(
                &[spec_with(FilesystemType::Ext4, 2 * 1024 * 1024)],
                256 * 1024 * 1024,
                FilesystemType::Fat32,
            )
            .is_ok()
        );
    }

    #[test]
    fn image_smaller_than_a_fat32_partition_reports_the_partition_not_no_space() {
        // 本次修复的核心回归：早先「64 MiB 镜像 + 占满剩余」被 FAT32 的 64 MiB
        // 无条件下限报成 no_space（前端显示"存储空间不足"）。现在它要么成功
        // （33 MiB 下限之上），要么明确指出是分区太低。
        //
        // 65011712 字节是实测里 GPT 布局下 64 MiB 镜像的真实可用区间。
        const AVAILABLE: u64 = 65_011_712;

        // 默认 FAT32、占满剩余：33 MiB 下限之上 → 放行。
        assert!(
            resolve_sizes(
                &[PartitionSpec::fill_remaining("A")],
                AVAILABLE,
                FilesystemType::Fat32
            )
            .is_ok()
        );

        // 不格式化：更不该报"空间不足"。
        assert!(
            resolve_sizes(
                &[PartitionSpec::fill_remaining("A").with_filesystem(PartitionFilesystem::None)],
                AVAILABLE,
                FilesystemType::Fat32
            )
            .is_ok()
        );

        // 真正放不下时才报 no_space（分区总和超过可用区间）。
        let err = resolve_sizes(&[spec(200 * 1024 * 1024)], AVAILABLE, FilesystemType::Fat32)
            .unwrap_err();
        assert!(matches!(err, CoreError::NoSpace { .. }), "{err:?}");
    }
}
