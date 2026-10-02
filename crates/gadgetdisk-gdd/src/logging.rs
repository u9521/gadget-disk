//! 日志：ISO8601 时间戳 + 等级 + 消息，写入文件并回显标准流。
//!
//! ## 为什么自己实现而不引第三方 crate
//!
//! 只需要「取当前 UTC 时间并格式化」这一件事。`chrono`/`time` 会带来可观的依赖
//! 树，而本仓库刻意保持依赖精简（见 `Cargo.toml` 的 workspace 依赖表）。UTC 的
//! 年月日换算是一次简单的民用历算法，不值得为它引一个 crate。
//!
//! ## 谁用
//!
//! `gdd` 与 CLI **共用**这一份实现（CLI 依赖 `gadgetdisk-gdd`），因此两边格式
//! 一致。但它们写**各自的文件**：`logs/gdd.log` 与 `logs/cli.log`。共用文件会让
//! 「谁写的这一行」需要靠猜，而排查一次挂载恰恰最需要区分这一点。
//!
//! ## 回显
//!
//! `Info` → stdout，`Warn`/`Error` → stderr。这样：
//!
//! - `scripts/deploy.py` 仍能从 stdout 读到冒烟测试的判定标记；
//! - 警告与错误落在 stderr，与「命令的正常输出」分离。
//!
//! ## 失败必须降级，不得影响功能
//!
//! 日志文件打不开（目录只读、磁盘满）时**只**回显标准流，并在 stderr 说明一次。
//! 日志是诊断手段，不是功能前提——让它把挂载搞失败是本末倒置。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 正常进展。
    Info,
    /// 可疑但不影响继续（例如上下文被改写、非镜像目录）。
    Warn,
    /// 失败。
    Error,
}

impl Level {
    /// 等级在行内的文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    /// 该等级应回显到哪条流（`true` = stderr）。
    const fn to_stderr(self) -> bool {
        matches!(self, Level::Warn | Level::Error)
    }
}

/// 单个日志文件的大小上限；超过即轮转一代（`<name>.1`）。
///
/// 与 `gadgetdisk_cli::cli_paths::LOG_MAX_BYTES` 同值。这里重复声明是为了让
/// `gdd` 不依赖 CLI 侧模块（依赖方向是 CLI → gdd，不能反过来）。
pub const LOG_MAX_BYTES: u64 = 256 * 1024;

/// 进程内唯一的日志出口。
struct Sink {
    /// 目标文件；`None` 表示只回显标准流。
    file: Option<File>,
    /// 已写出过一次「打不开日志文件」的告警，避免每次调用都刷屏。
    warned_open_failure: bool,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// 初始化日志出口。**幂等**：重复调用只更新目标文件。
///
/// `path` 为 `None` 时只回显标准流（不写文件）。父目录不存在会被创建。
///
/// 之所以允许重复调用：CLI 的 `prepare()` 之后初始化一次，而 `serve` 可能在
/// 同一进程里再次初始化；`gdd` 的 `--probe` 路径也会初始化。幂等让调用方不必
/// 关心顺序。
pub fn init(path: Option<PathBuf>) {
    let sink = SINK.get_or_init(|| {
        Mutex::new(Sink {
            file: None,
            warned_open_failure: false,
        })
    });

    let opened = path.as_deref().and_then(open_for_append);
    let Ok(mut guard) = sink.lock() else {
        return;
    };
    guard.file = opened;
    // 目标变了，允许对新目标再报一次打不开。
    guard.warned_open_failure = false;
}

/// 打开（或创建）日志文件用于追加，必要时先轮转。
///
/// 返回 `None` 表示打不开——调用方只回显标准流。
fn open_for_append(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    rotate_if_needed(path);

    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// 超过上限时把 `<name>` 改名为 `<name>.1`（覆盖旧的 `.1`）。
///
/// **保留一代而不是清空**：日志的价值在于事后排查，清空会把刚发生的事一起丢掉。
fn rotate_if_needed(path: &Path) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > LOG_MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("log");
    let rotated = path.with_file_name(format!("{name}.1"));
    // Windows 上 rename 不覆盖已存在目标，但本模块只跑在 Android/Linux；
    // 为稳妥仍先删一次，失败不影响后续。
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, &rotated);
}

/// 写一行日志。`message` 不要带换行（内部会按行拆分处理）。
pub fn log(level: Level, message: &str) {
    let line = format!("{} [{}] {message}", timestamp_utc(), level.as_str());

    if let Some(sink) = SINK.get()
        && let Ok(mut guard) = sink.lock()
    {
        match guard.file.as_mut() {
            Some(file) => {
                let _ = writeln!(file, "{line}");
                // 立刻 flush：`gdd` 会被 kill -9（用户卸载、`deploy.py` 重启
                // 前的清理），缓冲区里的最后几行恰恰是最有价值的。
                let _ = file.flush();
            }
            None => {
                if !guard.warned_open_failure {
                    guard.warned_open_failure = true;
                    eprintln!("failed to open the log file; output will only go to stderr");
                }
            }
        }
    }

    if level.to_stderr() {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// 正常进展。
pub fn info(message: &str) {
    log(Level::Info, message);
}

/// 可疑但继续。
pub fn warn(message: &str) {
    log(Level::Warn, message);
}

/// 失败。
pub fn error(message: &str) {
    log(Level::Error, message);
}

/// 当前 UTC 时间的 ISO8601 形式（秒精度，`Z` 结尾）。
///
/// 形如 `2026-10-04T06:12:33Z`。取不到系统时间时退化为 `1970-01-01T00:00:00Z`
/// 而不是 panic——日志时间戳不值得让进程崩掉。
pub fn timestamp_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

/// 把 Unix 秒格式化为 ISO8601（UTC）。
///
/// 用**民用历**（proleptic Gregorian）算法把「自 1970-01-01 起的天数」换算为
/// 年月日。这是 Howard Hinnant 的 `civil_from_days`，纯整数运算、无循环、
/// 无查表，也不依赖时区数据。
pub fn format_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 「自 1970-01-01 起的天数」→ `(年, 月, 日)`。
///
/// 见 Howard Hinnant, *chrono-Compatible Low-Level Date Algorithms*。
/// 把纪元平移到 0000-03-01，使闰日落在年末，从而用整数除法直接算出年月。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    // 平移：把 1970-01-01 变成「0000-03-01 起的天数」。
    let z = days_since_epoch + 719_468;

    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    // `doe` 是纪元内日序，数学上落在 [0, 146096]；`era` 的取整方向保证了
    // `z - era * 146_097` 不可能为负（这正是上面 `z - 146_096` 偏移的用途）。
    let doe = u64::try_from(z - era * 146_097).expect("doe 由构造保证落在 [0, 146096]");
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = i64::try_from(yoe).expect("yoe 落在 [0, 399]") + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]，3 月为 0
    // `d` 与 `m` 分别由上面的构造保证落在 [1, 31] 与 [1, 12]。
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("日期落在 [1, 31]");
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("月份落在 [1, 12]");

    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_utc_matches_known_instants() {
        // 纪元。
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        // 闰日（2024-02-29）——民用历算法最容易错的地方。
        assert_eq!(format_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        // 非闰年的 3 月 1 日。
        assert_eq!(format_utc(1_709_251_200), "2024-03-01T00:00:00Z");
        // 一个带时分秒的时刻。
        assert_eq!(format_utc(1_760_000_000), "2025-10-09T08:53:20Z");
        // 世纪闰年规则：1900 不是闰年、2000 是。用 2000-02-29 验证。
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn timestamp_utc_is_well_formed() {
        let text = timestamp_utc();
        // 形如 2026-10-04T06:12:33Z：长度固定，且以 Z 结尾。
        assert_eq!(text.len(), 20, "得到 {text}");
        assert!(text.ends_with('Z'));
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[10..11], "T");
    }

    #[test]
    fn levels_render_uppercase() {
        assert_eq!(Level::Info.as_str(), "INFO");
        assert_eq!(Level::Warn.as_str(), "WARN");
        assert_eq!(Level::Error.as_str(), "ERROR");
        // 警告与错误走 stderr，信息走 stdout。
        assert!(!Level::Info.to_stderr());
        assert!(Level::Warn.to_stderr());
        assert!(Level::Error.to_stderr());
    }

    #[test]
    fn writes_timestamped_lines_to_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "gd-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("nested/gdd.log");

        init(Some(path.clone()));
        info("第一条");
        warn("第二条");

        let text = std::fs::read_to_string(&path).expect("日志文件应存在");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "得到 {text:?}");
        assert!(lines[0].contains("[INFO]"), "得到 {}", lines[0]);
        assert!(lines[0].ends_with("第一条"));
        assert!(lines[1].contains("[WARN]"));
        assert!(lines[1].ends_with("第二条"));
        // 每行都以 ISO8601 时间戳开头。
        for line in &lines {
            assert!(line.starts_with("20"), "时间戳缺失：{line}");
            assert_eq!(&line[10..11], "T", "时间戳格式不对：{line}");
        }

        // 收尾：避免影响后续测试的文件句柄。
        init(None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotates_when_the_file_exceeds_the_limit() {
        let dir = std::env::temp_dir().join(format!("gd-logrot-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cli.log");

        // 造一个超限文件。
        let oversize = usize::try_from(LOG_MAX_BYTES + 1).expect("256 KiB 远小于 usize 上限");
        std::fs::write(&path, vec![b'x'; oversize]).unwrap();

        init(Some(path.clone()));
        info("轮转后的一行");
        init(None);

        // 新文件只有刚写的一行；旧内容进了 `.1`。
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("轮转后的一行"), "得到 {text}");
        assert!(
            u64::try_from(text.len()).expect("文件长度不会超过 u64") < LOG_MAX_BYTES,
            "轮转后不该仍然超限：{}",
            text.len()
        );
        let rotated = dir.join("cli.log.1");
        assert!(rotated.is_file(), "应保留一代 .1");
        assert!(std::fs::metadata(&rotated).unwrap().len() > LOG_MAX_BYTES);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unusable_path_degrades_without_panicking() {
        // 目标是一个**目录**，必然打不开；只回显标准流，不得 panic。
        let dir = std::env::temp_dir().join(format!("gd-logbad-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        init(Some(dir.clone()));
        info("这行只会回显");
        warn("这行也是");
        init(None);

        // 目录仍在，且没有把目录当成文件写坏。
        assert!(dir.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn init_is_idempotent_and_can_be_cleared() {
        let dir = std::env::temp_dir().join(format!("gd-logidem-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("a.log");

        init(Some(path.clone()));
        init(Some(path.clone()));
        info("两次 init 之后");
        init(None);
        // 清掉文件出口后，后续日志不再写文件。
        info("不应出现在文件里");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("两次 init 之后"));
        assert!(!text.contains("不应出现在文件里"), "得到 {text}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
