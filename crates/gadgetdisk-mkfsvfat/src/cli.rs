//! 命令行解析，参数与 `mkfs.fat(8)` 对齐。
//!
//! ## 为什么手写而不是用 clap 的 derive
//!
//! `mkfs.fat` 的位置参数是 `DEVICE [BLOCK-COUNT]`，且 `BLOCK-COUNT` 语义特殊
//! （**单位是 KiB，不是扇区数**）。用 clap 的 derive 表达"可选位置参数 + 一群
//! 短选项"会得到与 dosfstools 不同的错误信息与用法文本，反而不利于兼容。
//! 这里手写解析，把用法文本与 dosfstools 保持一致。
//!
//! ## 与 dosfstools 的差异
//!
//! - **只支持 `-F 32`**：传 12/16 明确报错。理由见 crate 文档（静默降级是实测过的坑）。
//! - **多一个 `-V`/`--version`**：dosfstools 只有 `--help`；本工具补上版本开关，
//!   好让打包/验收能核对「包里的二进制确实是这一版」。
//! - 未实现 `-c`（坏块检查）、`-l`（坏块列表）、`-m`（引导消息）、`-A`/`--variant`
//!   （Atari 变体）、`-g`（几何参数）、`-M`（介质类型）、`-r`（根目录项数）、
//!   `-f`（FAT 份数）、`-D`（BIOS 驱动器号）、`-h`（隐藏扇区）。
//!   这些在 GadgetDisk 的使用场景里没有意义；传了会**明确报错**而不是静默忽略。

use std::fmt;

/// 解析后的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// 目标设备或映像文件。
    pub device: String,
    /// `BLOCK-COUNT`：**单位为 KiB**（与 dosfstools 一致）。
    pub block_count: Option<u64>,
    /// `-F`：FAT 位数。本工具只接受 32。
    pub fat_bits: u32,
    /// `-n`：卷标。
    pub label: Option<String>,
    /// `-i`：卷 ID（32 位十六进制）。
    pub volume_id: Option<u32>,
    /// `-S`：逻辑扇区大小。
    pub sector_size: u32,
    /// `-s`：每簇扇区数。
    pub sectors_per_cluster: Option<u32>,
    /// `-R`：保留扇区数。
    pub reserved_sectors: Option<u16>,
    /// `--offset`：在映像文件的指定**扇区**处写入。
    pub offset_sectors: u64,
    /// `-v`：详细输出。
    pub verbose: bool,
    /// `--invariant`：使用固定卷 ID，便于测试复现。
    pub invariant: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            device: String::new(),
            block_count: None,
            fat_bits: 32,
            label: None,
            volume_id: None,
            sector_size: crate::DEFAULT_SECTOR_SIZE,
            sectors_per_cluster: None,
            reserved_sectors: None,
            offset_sectors: 0,
            verbose: false,
            invariant: false,
        }
    }
}

impl Cli {
    /// `--offset` 换算出的字节偏移。
    pub fn offset_bytes(&self) -> u64 {
        self.offset_sectors
            .saturating_mul(u64::from(self.sector_size))
    }

    /// 用法文本（与 dosfstools 的 `mkfs.fat` 结构一致）。
    pub fn help_text() -> String {
        format!(
            "Usage: mkfs.vfat [OPTIONS] DEVICE [BLOCK-COUNT]\n\
             \n\
             Create a FAT32 filesystem on DEVICE (a block device or an image file).\n\
             BLOCK-COUNT is the number of 1024-byte blocks (KiB); if omitted, the\n\
             whole device (or the remaining space after --offset) is used.\n\
             \n\
             Options:\n\
             \x20 -F FAT-SIZE        FAT bits. Only 32 is supported by this build.\n\
             \x20 -n VOLUME-NAME    Volume label (at most 11 characters).\n\
             \x20 -i VOLUME-ID      Volume ID, 32-bit hexadecimal (e.g. 2e24ec82).\n\
             \x20 -S SECTOR-SIZE    Logical sector size in bytes (default {sector}).\n\
             \x20 -s SECTORS/CLUSTER  Sectors per cluster (power of two).\n\
             \x20 -R RESERVED       Number of reserved sectors.\n\
             \x20 --offset SECTOR    Write the filesystem at SECTOR of the device.\n\
             \x20 -v                Verbose output.\n\
             \x20 --invariant       Use a fixed volume ID (for reproducible tests).\n\
             \x20 -h, --help        Display this help and exit.\n\
             \x20 -V, --version     Display the version and exit.\n\
             \n\
             Notes:\n\
             \x20 This tool creates FAT32 only. It will not silently fall back to\n\
             \x20 FAT12/FAT16 (that behaviour of other tools has caused real bugs).\n\
             \x20 When writing into a partition, callers normally attach the\n\
             \x20 partition through a loop device instead of using --offset.\n",
            sector = crate::DEFAULT_SECTOR_SIZE
        )
    }
}

/// 解析失败的类型。
///
/// `-h/--help` 与 `-V/--version` 是**成功的早退**（输出后以 0 退出），却与真正的
/// 用法错误共用「没有 `Cli` 可返回」这一条通道。用一个显式的种类区分，好过让
/// `main` 去猜文本前缀——后者在文案改动时会静默失效（把帮助当错误，或反之）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliOutcome {
    /// 真正的用法错误：退出码非 0，并打印用法。
    Error,
    /// 打印帮助并以 0 退出。
    Help,
    /// 打印版本并以 0 退出。
    Version,
}

/// 解析错误（或帮助/版本请求）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// 要展示的文本。
    pub message: String,
    /// 本次早退的种类。
    pub outcome: CliOutcome,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CliError {}

fn err(message: impl Into<String>) -> CliError {
    CliError {
        message: message.into(),
        outcome: CliOutcome::Error,
    }
}

/// 构造「打印帮助」的早退。
fn help() -> CliError {
    CliError {
        message: Cli::help_text(),
        outcome: CliOutcome::Help,
    }
}

/// 构造「打印版本」的早退。
fn version() -> CliError {
    CliError {
        message: format!("mkfs.vfat {}", crate::VERSION),
        outcome: CliOutcome::Version,
    }
}

/// 解析命令行（不含 `argv[0]`）。
pub fn parse_args(argv: &[String]) -> Result<Cli, CliError> {
    let mut cli = Cli::default();
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0;

    // 选项与位置参数可以交错（dosfstools 亦然）。
    while i < argv.len() {
        let arg = argv[i].as_str();
        i += 1;

        // `--` 之后一律按位置参数处理。
        if arg == "--" {
            positionals.extend(argv[i..].iter().cloned());
            break;
        }

        // 长选项：`--name` 与 `--name=value` 两种写法。
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let mut take_value = |what: &str| -> Result<String, CliError> {
                if let Some(v) = inline.clone() {
                    return Ok(v);
                }
                if i < argv.len() {
                    let v = argv[i].clone();
                    i += 1;
                    Ok(v)
                } else {
                    Err(err(format!("option --{what} requires a value")))
                }
            };
            match name {
                "help" => return Err(help()),
                "version" => return Err(version()),
                "offset" => {
                    let v = take_value("offset")?;
                    cli.offset_sectors = v
                        .parse()
                        .map_err(|_| err(format!("--offset is not an integer: {v}")))?;
                }
                "invariant" => cli.invariant = true,
                other => return Err(err(format!("unknown option: --{other}"))),
            }
            continue;
        }

        // 短选项：支持 `-n LABEL` 与 `-nLABEL` 两种写法。
        if arg.len() > 1 && arg.starts_with('-') {
            let flag = arg.as_bytes()[1] as char;
            let inline = if arg.len() > 2 { Some(&arg[2..]) } else { None };
            let mut take_value = |what: char| -> Result<String, CliError> {
                if let Some(v) = inline {
                    return Ok(v.to_string());
                }
                if i < argv.len() {
                    let v = argv[i].clone();
                    i += 1;
                    Ok(v)
                } else {
                    Err(err(format!("option -{what} requires a value")))
                }
            };
            match flag {
                'h' => return Err(help()),
                // `-V` 在 dosfstools 里**没有**定义（其 man page 只有 `--help`），
                // 这里补一个与其它 GNU 工具一致的版本开关，好让构建脚本能校验
                // 「包里的二进制确实是这一版」。属已知差异，记在 help 与文档里。
                'V' => return Err(version()),
                'v' => cli.verbose = true,
                'F' => {
                    let v = take_value('F')?;
                    cli.fat_bits = v
                        .parse()
                        .map_err(|_| err(format!("-F is not an integer: {v}")))?;
                    if cli.fat_bits != 32 {
                        // 明确拒绝而不是静默降级：静默产生 FAT16 是实测过的坑。
                        return Err(err(format!(
                            "this tool supports FAT32 only (-F 32), got -F {}; \
                             FAT12 and FAT16 widths are not supported",
                            cli.fat_bits
                        )));
                    }
                }
                'n' => cli.label = Some(take_value('n')?),
                'i' => {
                    let v = take_value('i')?;
                    let hex = v.trim_start_matches("0x").trim_start_matches("0X");
                    cli.volume_id = Some(
                        u32::from_str_radix(hex, 16)
                            .map_err(|_| err(format!("-i is not 32-bit hexadecimal: {v}")))?,
                    );
                }
                'S' => {
                    let v = take_value('S')?;
                    let size: u32 = v
                        .parse()
                        .map_err(|_| err(format!("-S is not an integer: {v}")))?;
                    if !size.is_power_of_two() || size < 512 {
                        return Err(err(format!(
                            "-S must be a power of two >= 512 (512/1024/2048/4096/...), got {size}"
                        )));
                    }
                    cli.sector_size = size;
                }
                's' => {
                    let v = take_value('s')?;
                    let spc: u32 = v
                        .parse()
                        .map_err(|_| err(format!("-s is not an integer: {v}")))?;
                    if !spc.is_power_of_two() || spc > 128 {
                        return Err(err(format!(
                            "-s must be a power of two in 1..=128, got {spc}"
                        )));
                    }
                    cli.sectors_per_cluster = Some(spc);
                }
                'R' => {
                    let v = take_value('R')?;
                    cli.reserved_sectors = Some(
                        v.parse()
                            .map_err(|_| err(format!("-R is not an integer: {v}")))?,
                    );
                }
                other => return Err(err(format!("unknown option: -{other}"))),
            }
            continue;
        }

        positionals.push(arg.to_string());
    }

    // 位置参数：DEVICE [BLOCK-COUNT]
    match positionals.len() {
        0 => return Err(err("missing the DEVICE argument")),
        1 => cli.device = positionals[0].clone(),
        2 => {
            cli.device = positionals[0].clone();
            let raw = &positionals[1];
            cli.block_count = Some(
                raw.parse()
                    .map_err(|_| err(format!("BLOCK-COUNT is not an integer: {raw}")))?,
            );
        }
        n => {
            return Err(err(format!(
                "too many positional arguments (got {n}): expected DEVICE [BLOCK-COUNT]"
            )));
        }
    }

    if let Some(label) = &cli.label
        && label.chars().count() > crate::LABEL_LEN
    {
        return Err(err(format!(
            "volume label must be at most {} characters (got {}): '{label}'",
            crate::LABEL_LEN,
            label.chars().count()
        )));
    }

    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_device_only() {
        let cli = parse_args(&args(&["/dev/block/loop0"])).unwrap();
        assert_eq!(cli.device, "/dev/block/loop0");
        assert_eq!(cli.block_count, None);
        assert_eq!(cli.fat_bits, 32);
        assert_eq!(cli.offset_sectors, 0);
    }

    #[test]
    fn block_count_is_parsed() {
        let cli = parse_args(&args(&["/dev/x", "65536"])).unwrap();
        assert_eq!(cli.block_count, Some(65536));
    }

    #[test]
    fn block_count_rejects_non_numeric() {
        assert!(parse_args(&args(&["/dev/x", "64M"])).is_err());
    }

    #[test]
    fn fat32_is_accepted_and_others_rejected_loudly() {
        assert!(parse_args(&args(&["-F", "32", "/dev/x"])).is_ok());
        // 静默降级 FAT16 是实测过的坑，故必须报错。
        let err = parse_args(&args(&["-F", "16", "/dev/x"])).unwrap_err();
        assert!(
            err.message.contains("supports FAT32 only"),
            "得到 {}",
            err.message
        );
        assert!(parse_args(&args(&["-F", "12", "/dev/x"])).is_err());
    }

    #[test]
    fn label_is_parsed_and_length_checked() {
        let cli = parse_args(&args(&["-n", "MYDISK", "/dev/x"])).unwrap();
        assert_eq!(cli.label.as_deref(), Some("MYDISK"));

        // 11 个字符合法。
        assert!(parse_args(&args(&["-n", "ABCDEFGHIJK", "/dev/x"])).is_ok());
        // 12 个即超限（dosfstools 的卷标上限）。
        let err = parse_args(&args(&["-n", "ABCDEFGHIJKL", "/dev/x"])).unwrap_err();
        assert!(err.message.contains("at most"), "得到 {}", err.message);
    }

    #[test]
    fn inline_values_are_accepted() {
        // dosfstools 允许 `-nLABEL` 这种紧贴写法。
        let cli = parse_args(&args(&["-nMYDISK", "-F32", "/dev/x"])).unwrap();
        assert_eq!(cli.label.as_deref(), Some("MYDISK"));
        assert_eq!(cli.fat_bits, 32);
    }

    #[test]
    fn volume_id_accepts_hex_with_and_without_prefix() {
        let cli = parse_args(&args(&["-i", "2e24ec82", "/dev/x"])).unwrap();
        assert_eq!(cli.volume_id, Some(0x2e24_ec82));

        let cli = parse_args(&args(&["-i", "0x2E24EC82", "/dev/x"])).unwrap();
        assert_eq!(cli.volume_id, Some(0x2e24_ec82));

        assert!(parse_args(&args(&["-i", "zzz", "/dev/x"])).is_err());
    }

    #[test]
    fn offset_is_in_sectors_and_converts_to_bytes() {
        let cli = parse_args(&args(&["--offset", "2048", "/dev/x"])).unwrap();
        assert_eq!(cli.offset_sectors, 2048);
        // 默认 512 字节扇区 → 1 MiB。
        assert_eq!(cli.offset_bytes(), 1048576);

        // 扇区大小改变时字节偏移随之改变。
        let cli = parse_args(&args(&["-S", "4096", "--offset", "256", "/dev/x"])).unwrap();
        assert_eq!(cli.offset_bytes(), 1048576);
    }

    #[test]
    fn offset_accepts_equals_form() {
        let cli = parse_args(&args(&["--offset=1024", "/dev/x"])).unwrap();
        assert_eq!(cli.offset_sectors, 1024);
    }

    #[test]
    fn sector_size_must_be_power_of_two_at_least_512() {
        assert!(parse_args(&args(&["-S", "512", "/dev/x"])).is_ok());
        assert!(parse_args(&args(&["-S", "4096", "/dev/x"])).is_ok());

        assert!(parse_args(&args(&["-S", "256", "/dev/x"])).is_err());
        assert!(parse_args(&args(&["-S", "1000", "/dev/x"])).is_err());
    }

    #[test]
    fn sectors_per_cluster_is_bounded() {
        assert!(parse_args(&args(&["-s", "8", "/dev/x"])).is_ok());
        assert!(parse_args(&args(&["-s", "128", "/dev/x"])).is_ok());
        assert!(parse_args(&args(&["-s", "256", "/dev/x"])).is_err());
        assert!(parse_args(&args(&["-s", "3", "/dev/x"])).is_err());
    }

    #[test]
    fn reserved_sectors_parsed() {
        let cli = parse_args(&args(&["-R", "32", "/dev/x"])).unwrap();
        assert_eq!(cli.reserved_sectors, Some(32));
    }

    #[test]
    fn flags_are_recognised() {
        let cli = parse_args(&args(&["-v", "--invariant", "/dev/x"])).unwrap();
        assert!(cli.verbose);
        assert!(cli.invariant);
    }

    #[test]
    fn help_is_reported_as_an_early_exit_carrying_usage() {
        // 用 Err 承载 help 文本：main 据 outcome 打印到 stdout 并以 0 退出。
        let err = parse_args(&args(&["--help"])).unwrap_err();
        assert_eq!(err.outcome, CliOutcome::Help);
        assert!(
            err.message.contains("Usage: mkfs.vfat"),
            "得到 {}",
            err.message
        );
        assert!(err.message.contains("BLOCK-COUNT"));

        let err = parse_args(&args(&["-h"])).unwrap_err();
        assert_eq!(err.outcome, CliOutcome::Help);
        assert!(err.message.contains("Usage: mkfs.vfat"));
    }

    #[test]
    fn version_is_reported_as_an_early_exit_with_the_injected_version() {
        // `-V`/`--version` 让打包与验收能核对包里的二进制确实是这一版。
        for flag in ["-V", "--version"] {
            let err = parse_args(&args(&[flag])).unwrap_err();
            assert_eq!(err.outcome, CliOutcome::Version, "{flag}");
            assert!(
                err.message.contains(crate::VERSION),
                "{flag} 应回显注入的版本 {}，得到 {}",
                crate::VERSION,
                err.message
            );
            assert!(err.message.starts_with("mkfs.vfat "), "{flag}");
        }
    }

    #[test]
    fn usage_errors_are_not_mistaken_for_early_exits() {
        // 「以 Usage 开头就是帮助」这条文本前缀判据已删除（帮助与错误的文案都可能
        // 带有它），outcome 才是唯一权威。
        let err = parse_args(&args(&["--nonsense"])).unwrap_err();
        assert_eq!(err.outcome, CliOutcome::Error);

        let err = parse_args(&args(&[])).unwrap_err();
        assert_eq!(err.outcome, CliOutcome::Error);
        assert!(err.message.contains("DEVICE"), "得到 {}", err.message);
    }

    #[test]
    fn missing_values_are_reported() {
        assert!(parse_args(&args(&["-n"])).is_err());
        assert!(parse_args(&args(&["-F"])).is_err());
        assert!(parse_args(&args(&["--offset"])).is_err());
    }

    #[test]
    fn unknown_options_are_rejected_not_ignored() {
        // 静默忽略会让用户以为参数生效了。
        assert!(parse_args(&args(&["--nonsense", "/dev/x"])).is_err());
        assert!(parse_args(&args(&["-Z", "/dev/x"])).is_err());
    }

    #[test]
    fn too_many_positionals_rejected() {
        assert!(parse_args(&args(&["/dev/x", "1024", "extra"])).is_err());
    }

    #[test]
    fn double_dash_stops_option_parsing() {
        let cli = parse_args(&args(&["--", "-weird-name"])).unwrap();
        assert_eq!(cli.device, "-weird-name");
    }

    #[test]
    fn options_may_follow_positionals() {
        let cli = parse_args(&args(&["/dev/x", "1024", "-n", "LBL"])).unwrap();
        assert_eq!(cli.device, "/dev/x");
        assert_eq!(cli.block_count, Some(1024));
        assert_eq!(cli.label.as_deref(), Some("LBL"));
    }
}
