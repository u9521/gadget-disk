//! `mkfs.vfat`：FAT 文件系统格式化工具。
//!
//! ## 为什么是独立二进制
//!
//! 设备上**不存在** `mkfs.vfat`（AVD 实测：无 dosfstools、toybox 亦无 `mkfs`），
//! 因此 GadgetDisk 必须自带一个。做成独立二进制而不是库函数，有两个理由：
//!
//! 1. **与其他 `mkfs` 形态一致**：`mkfs.exfat`、`mkfs.ext4` 都是外部进程，
//!    FAT32 走同一个调用路径后，格式化流程不再有"内置特例"；
//! 2. **可被用户直接调用**：与 `mkfs.fat` 命令行兼容，排查问题时能手工验证。
//!
//! ## 命令行对齐 dosfstools
//!
//! 参数尽量与 `mkfs.fat(8)` 一致，使熟悉 dosfstools 的用户无需重新学习。
//! 已知差异记录在 [`Cli::help_text`] 与 [docs/disk-image-format.md]。
//!
//! ## 强制项（来自实测，见 docs/disk-image-format.md）
//!
//! 1. **必须显式指定 FAT32**。`fatfs` 默认按容量自动选择，实测会把
//!    约 32.5 MiB（65525 簇）以下的区间静默格式化为 FAT16。本工具只做 FAT32：
//!    `-F 16` 会**明确报错**而不是静默降级。
//! 2. **用 `StreamSlice` 限定区间**，禁止对整盘格式化——那会覆盖分区表。

pub mod cli;
pub mod format;

pub use cli::{Cli, CliError, CliOutcome, parse_args};
pub use format::{Fat32Volume, FormatError, format_fat32, normalize_label};

/// 本二进制的版本号。
///
/// 由构建脚本通过环境变量 `GD_VERSION` 注入（`uv run gd-build --version`），
/// 未注入时回落到 Cargo 的包版本。`option_env!` 会被 cargo 记录为**环境依赖**，
/// 因此改值必然触发重编，不会留下旧版本号的产物。
///
/// **与 `gadgetdisk_proto::VERSION` 是同一份表达式**，而不是本 crate 另立一套：
/// 三个二进制必须报同一个版本。之所以不直接复用 `gadgetdisk-proto` 的常量，是因为
/// 那会把 `serde`/`thiserror` 拖进一个只做 FAT 格式化的工具；两者的一致性由
/// 构建脚本与验收步骤核对（同一次 `gd-build` 后三个二进制的自报版本必须相同）。
pub const VERSION: &str = match option_env!("GD_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// FAT 目录项里卷标字段的固定宽度（字节）。
pub const LABEL_LEN: usize = 11;

/// 逻辑扇区大小（`mkfs.fat` 的 `-S` 默认值）。
pub const DEFAULT_SECTOR_SIZE: u32 = 512;

/// 默认卷标（与既有行为一致）。
pub const DEFAULT_LABEL: &str = "GADGETDISK";

/// FAT32 规范要求的簇数下限。
///
/// 低于该值时 `fatfs` 即使显式指定 `FatType::Fat32` 也会静默回退 FAT16，
/// 故格式化后必须校验。
pub const MIN_FAT32_CLUSTERS: u64 = 65525;
