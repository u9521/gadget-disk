//! `gdd` —— 无状态的 mass_storage 执行进程。
//!
//! 子命令只有 `serve`（默认）：监听 UDS，按请求改 configfs。
//!
//! 它是**按需进程**：只做 mass_storage 挂载，不写任何状态文件，因此可以被
//! 随时拉起与杀死。见 [`gadgetdisk_gdd`] 的模块文档。

use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use gadgetdisk_gdd::usb_adapter::GadgetBackend;
use gadgetdisk_gdd::{DaemonConfig, DataDirs, Service, logging};

#[derive(Parser, Debug)]
#[command(
    name = "gdd",
    // 与 `gadgetdisk` 共用同一个注入版本号（`GD_VERSION`）。
    version = gadgetdisk_proto::VERSION,
    about = "GadgetDisk mass_storage executor (stateless, started on demand)"
)]
struct Cli {
    // 数据根目录。
    #[arg(
        long,
        default_value = gadgetdisk_gdd::DEFAULT_DATA_ROOT,
        help = "Data root directory."
    )]
    data_dir: PathBuf,
    // 覆盖 socket 路径。
    #[arg(long, help = "Override the socket path.")]
    socket: Option<PathBuf>,
    // 无挂载且无请求多少秒后退出（有挂载时永不退出）。
    #[arg(
        long,
        default_value_t = 60,
        help = "Exit after this many idle seconds with no mounts (never exits while mounted)."
    )]
    idle_timeout: u64,
    // 日志文件路径；缺省**不写文件**（只回显标准流）。
    //
    // 由 CLI 拉起时传入 `logs/gdd.log`。之所以做成参数而不是写死路径：
    // `gdd` 只认 `--data-dir`，而「日志放哪」是调用方的策略（测试要能指向
    // 临时文件，诊断要能换个位置）。
    #[arg(
        long,
        help = "Log file path; by default no file is written (output goes to the standard streams only).",
        long_help = "Log file path; by default no file is written (output goes to the standard streams only).\n\nThe CLI passes `logs/gdd.log` when it starts gdd. This is an argument rather than a hard-coded path because gdd only knows `--data-dir`, while where to put logs is the caller's policy (tests point it at a temp file; diagnostics may relocate it)."
    )]
    log_file: Option<PathBuf>,
    // 探测布局并打印后退出（诊断用，不起监听）。
    #[arg(
        long,
        help = "Probe the layout, print it and exit (diagnostic; does not start listening)."
    )]
    probe: bool,
}

fn main() -> StdExitCode {
    let cli = Cli::parse();
    let dirs = DataDirs::new(cli.data_dir);

    // 日志出口必须在**任何**输出之前就位，否则启动阶段的信息会漏掉。
    logging::init(cli.log_file.clone());
    logging::info(&format!(
        "startup: data_dir={} log_file={:?}",
        dirs.root().display(),
        cli.log_file
    ));

    if cli.probe {
        return probe(&dirs);
    }

    let mut config = DaemonConfig::new(dirs.clone())
        // 至少 1 秒：0 会让进程刚起来就退出，等于没有服务。
        .with_idle_timeout(std::time::Duration::from_secs(cli.idle_timeout.max(1)));
    if let Some(path) = cli.socket {
        config = config.with_socket_path(path);
    }

    // gadget 侧：接入真实 configfs。若 `/config` 不是 configfs（例如在主机或
    // 容器中运行），退回**明确失败**的替身并告警——此时挂载请求会返回带原因的
    // 错误，而不是假装成功。
    let (gadget, degraded) = GadgetBackend::detect(dirs.clone());
    match &degraded {
        None => logging::info("configfs attached"),
        Some(reason) => {
            logging::warn(&format!("gadget backend unavailable -- {reason}"));
            logging::warn(
                "mount requests will return an explicit error rather than pretending to succeed",
            );
        }
    }

    let service = Arc::new(Mutex::new(Service::new(dirs, gadget)));

    // 收到 SIGTERM/SIGINT 时优雅退出（由 shutdown 标志驱动）。
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    install_signal_flag(Arc::clone(&running));

    match gadgetdisk_gdd::run(config, service, move || {
        !running.load(std::sync::atomic::Ordering::Relaxed)
    }) {
        Ok(()) => StdExitCode::SUCCESS,
        Err(err) => {
            logging::error(&format!("{err}"));
            StdExitCode::from(3)
        }
    }
}

/// 探测布局并打印（诊断用）。
///
/// 这条路径**不写任何 configfs**，只是把探测结果摊开：真机排错时最常问的
/// 「到底选中了哪个 gadget、哪个 config、为什么」在这里一次回答。
fn probe(dirs: &DataDirs) -> StdExitCode {
    // `--probe` 的输出是**命令结果**（人要读的一小段文本），因此走 stdout；
    // 同时经 `logging::info` 记一份到日志，便于事后对照。
    match gadgetdisk_gdd::discover_layout() {
        Ok(layout) => {
            for line in [
                format!("gadget_root  : {}", layout.gadget_root.display()),
                format!("config_name  : {}", layout.config_name),
                format!("gadget_reason: {:?}", layout.gadget_reason),
                format!("config_reason: {:?}", layout.config_reason),
            ] {
                println!("{line}");
                logging::info(&line);
            }
            match gadgetdisk_gdd::usb_adapter::UsbGadget::new(dirs.clone()) {
                Ok(gadget) => {
                    use gadgetdisk_gdd::GadgetView as _;
                    let line = format!("udc          : {:?}", gadget.udc());
                    println!("{line}");
                    logging::info(&line);
                    for lun in gadget.luns() {
                        let line = format!(
                            "lun.{}        : {} ({:?}, attached={}, effective={})",
                            lun.index, lun.image_path, lun.mode, lun.attached, lun.effective
                        );
                        println!("{line}");
                        logging::info(&line);
                    }
                }
                Err(err) => {
                    let line = format!("cannot attach configfs: {err}");
                    eprintln!("{line}");
                    logging::error(&line);
                }
            }
            StdExitCode::SUCCESS
        }
        Err(err) => {
            let line = format!("probe failed: {err}");
            eprintln!("{line}");
            logging::error(&line);
            StdExitCode::from(1)
        }
    }
}

/// 安装仅置位的信号处理器，使 accept 循环能在下一轮观察到退出请求。
///
/// 处理器内只做一次原子写入（异步信号安全），不做任何 IO。
fn install_signal_flag(running: Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::OnceLock;

    static FLAG: OnceLock<Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();
    let _ = FLAG.set(running);

    // SAFETY: 处理器只做原子写入，是异步信号安全的。
    unsafe extern "C" fn handler(_signal: libc::c_int) {
        if let Some(flag) = FLAG.get() {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // SAFETY: signal(2) 注册处理器。
    unsafe {
        let handler = handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

#[cfg(test)]
mod tests {
    /// `gdd --help` 必须全英文，且每个选项都要有说明。
    ///
    /// 这条测试是**设备实测漏项后补的**：此前只有 `gadgetdisk` 有同类检查，
    /// 于是 `gdd` 的五个选项继续以中文 `///` 呈现——本机测试全绿，只有把
    /// `--help` 打到设备上才看得见。两类二进制都要守。
    #[test]
    fn help_is_english_and_non_empty() {
        use clap::CommandFactory;

        let text = super::Cli::command().render_long_help().to_string();

        assert!(
            !text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "`gdd --help` 里有中文，用户可见的文案必须全英文：\n{text}"
        );

        // 每个选项名之后的第一个非空行必须是说明文字。
        let mut missing = Vec::new();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if !trimmed.starts_with("--") || trimmed.contains("  ") {
                continue;
            }
            let has_text = lines[i + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .is_some_and(|l| !l.starts_with('-') || l.len() > trimmed.len());
            if !has_text {
                missing.push(trimmed.to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "`gdd --help` 里有选项缺少说明（`///` 可能没转成 help 属性）：{missing:?}"
        );
    }
}
