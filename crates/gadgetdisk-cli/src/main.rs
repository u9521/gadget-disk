//! `gadgetdisk` CLI。
//!
//! ## 它与 `gdd` 的分工
//!
//! `gadgetdisk` 是**一次性**命令，也是 WebUI 的 REST 后端（`serve`）。它持有
//! 全部决策与状态：
//!
//! | 归属 | 内容 |
//! |---|---|
//! | **CLI** | 镜像增删查、导入、loop 挂载、能力探测、身份配置（`idVendor`/字符串）、`run/state.json`（导出意图）、开机对账（`boot`） |
//! | **`gdd`** | 仅 mass_storage：绑 LUN、按 LUN 弹出、拆除、重绑 UDC |
//!
//! 因此只有 `mount`/`unmount` 会经 UDS 找 `gdd`；其余子命令就地执行，**不需要
//! 任何后台进程**。
//!
//! ## 子命令
//!
//! **全部都是一次性命令**，没有分组层级：
//!
//! | 子命令 | 是否经 `gdd` |
//! |---|---|
//! | `mount` / `unmount` / `delete-slot` / `rebind` | 是（只有它们要改 configfs） |
//! | `status` / `list` / `capabilities` / `create` / `delete` | 否，就地执行 |
//! | `attach-loop` / `detach-loop` / `list-loop` | 否，就地执行 |
//! | `serve` | 否，但它自己长驻（空闲超时退出），并负责拉起 `gdd` |
//! | `boot` / `uninstall` / `config` | 否（`boot` 供 `service.sh`，`uninstall` 供 `uninstall.sh`） |
//! | `df` | 否，只读查询可用空间 |
//!
//! 经 `gdd` 的命令用 `--data-dir` 推导 socket 路径（可用 `--socket` 覆盖）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode as StdExitCode;

use clap::{Parser, Subcommand};
use gadgetdisk_cli::client::{self, Command, CommandDevice};
use gadgetdisk_cli::gadget_adapter::{self, ExportIntent, IdentityEditor, IntentLun};
use gadgetdisk_cli::loop_adapter::LoopMounts;
use gadgetdisk_cli::output;
use gadgetdisk_gdd::{DataDirs, GadgetView};
use gadgetdisk_proto::{ErrorCode, Mode};

#[derive(Parser, Debug)]
#[command(
    name = "gadgetdisk",
    // 版本号由构建脚本经 `GD_VERSION` 注入（`uv run gd-build --version`）；
    // 常量放在 `gadgetdisk-proto`，与 `gdd` 共用同一个值。
    version = gadgetdisk_proto::VERSION,
    about = "GadgetDisk: emulate an Android device as a USB mass storage device"
)]
// 帮助文本（about/help/long_about）一律**英文**：它是 CLI 面向使用者的输出。
// 中文说明保留为 `//` 注释——本仓库的注释规范仍是中文，且 clap derive 会把
// `///` 直接渲染成 help，因此凡是要给用户看的文案都必须写成显式属性。
struct Cli {
    // 数据根目录。
    //
    // 声明为**全局**参数，因此写在子命令前后都可以：
    // `gadgetdisk --data-dir X status` 与 `gadgetdisk status --data-dir X` 等价。
    // 这不是便利性糖：WebUI 的回退通道把参数拼成一行字符串，若 `--data-dir`
    // 只能出现在某个固定位置，前端就得为每个子命令记住不同的拼接规则。
    #[arg(
        long,
        global = true,
        default_value = gadgetdisk_gdd::DEFAULT_DATA_ROOT,
        help = "Data root directory.",
        long_help = "Data root directory.\n\nDeclared global, so it may appear before or after the subcommand: `gadgetdisk --data-dir X status` and `gadgetdisk status --data-dir X` are equivalent. This is not sugar: the WebUI fallback channel joins arguments into a single string, so a fixed position would force the frontend to remember a different rule per subcommand."
    )]
    data_dir: PathBuf,

    // 覆盖 `gdd` 的 socket 路径（缺省为 `<数据目录>/run/gdd.sock`）。
    #[arg(
        long,
        global = true,
        help = "Override the `gdd` socket path.",
        long_help = "Override the `gdd` socket path (default: `<data-dir>/run/gdd.sock`)."
    )]
    socket: Option<PathBuf>,

    // 模块根目录；用于定位自带的 `bin/mkfs.vfat`。
    //
    // 与 `--data-dir` 一样声明为**全局**参数：WebUI 的回退通道把参数拼成一行
    // 字符串，前端不必为每个子命令记住不同的拼接位置。
    //
    // 缺省时按数据目录推出（`<data-dir>` 的父目录 + `modules/gadget-disk`），
    // 使 `gadgetdisk create` 在典型安装下无需显式传入也能找到自带工具。
    #[arg(
        long,
        global = true,
        help = "Module root directory; used to locate the bundled `bin/mkfs.vfat`.",
        long_help = "Module root directory; used to locate the bundled `bin/mkfs.vfat`.\n\nLike `--data-dir` this is global, so the WebUI fallback channel does not need a per-subcommand rule. When omitted it is derived from the data directory (`<data-dir>`'s parent + `modules/gadget-disk`), so `gadgetdisk create` finds the bundled tool without an explicit flag on a typical install."
    )]
    module_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: TopLevel,
}

// `gadgetdisk` 的全部子命令。
//
// 架构采用扁平子命令设计：仅直接操作 configfs 的命令（`mount` / `unmount` /
// `delete-slot` / `rebind`）通过 socket 委托 `gdd` 执行，其余子命令（状态查询、
// 镜像创建、本地 loop 挂载等）均由独立进程就地执行。
#[derive(Subcommand, Debug)]
#[command(
    long_about = "All `gadgetdisk` subcommands.\n\nFlat subcommand design: only the commands that touch configfs directly (`mount` / `unmount` / `delete-slot` / `rebind`) are delegated to `gdd` over a socket; every other subcommand (status queries, image creation, local loop mounts, ...) runs in place in its own process."
)]
enum TopLevel {
    // ------------------------------------------------ 只读状态与列举
    /// Query mount status.
    Status,
    /// List images.
    List,
    /// Probe capabilities (loop, filesystems, gadget support).
    Capabilities,
    /// List local loop attachments.
    #[command(name = "list-loop")]
    ListLoop,

    // ------------------------------------------------ 导出为 USB 设备（经 gdd）
    /// Export images as USB mass storage devices (supports multiple LUNs).
    ///
    /// `--mode` / `--lun` / `--inquiry` are **repeatable** and **aligned by index**
    /// with the positional image arguments:
    ///
    /// ```text
    /// gadgetdisk mount a.img b.iso --mode rw --mode cdrom --inquiry DISK
    /// ```
    ///
    /// So `a.img` takes the 1st `--mode` (`rw`) and the 1st `--inquiry` (`DISK`),
    /// and `b.iso` takes the 2nd (`cdrom`); the 2nd `--inquiry` is absent, so none
    /// is set.
    ///
    /// A per-image option syntax (e.g. `--device a.img:rw:DISK`) would complicate
    /// both the parser and the documentation, whereas positional alignment matches
    /// familiar CLI conventions such as `-v`/`-I`.
    Mount {
        /// Image path(s).
        #[arg(required = true)]
        images: Vec<PathBuf>,
        /// Device mode: rw | ro | cdrom (repeatable, aligned by index).
        #[arg(long = "mode")]
        modes: Vec<String>,
        /// LUN index (repeatable, aligned by index); `gdd` picks a free LUN by default.
        #[arg(long = "lun")]
        luns: Vec<u8>,
        /// SCSI INQUIRY string (repeatable, aligned by index).
        #[arg(long = "inquiry")]
        inquiries: Vec<String>,
        /// Force a UDC rebind.
        ///
        /// Identity changes do not trigger a rebind implicitly (they take effect on
        /// the next USB connection); this flag re-enumerates immediately without
        /// replugging.
        #[arg(long, default_value_t = false)]
        rebind: bool,
    },
    /// Unmount USB exports (ejects all by default).
    Unmount {
        /// Target LUN; ejects all by default.
        #[arg(long)]
        lun: Option<u8>,
    },
    /// Delete the given idle slot.
    ///
    /// Unlike `unmount --lun N`: ejecting only clears the medium (slot and
    /// parameters are kept), whereas deleting makes that index cease to exist.
    /// `lun.0` is created by the kernel alongside the function and **cannot** be
    /// deleted.
    #[command(name = "delete-slot")]
    DeleteSlot {
        /// Slot index.
        #[arg(long)]
        lun: u8,
    },
    /// Rebind the UDC so a new USB identity takes effect immediately.
    ///
    /// Equivalent to `POST /api/v1/rebind` and to `mount --rebind`, but it needs
    /// **no** image: it rebinds the UDC once to make the host re-enumerate.
    ///
    /// This is a manual force-apply channel, not the normal path: saving an
    /// identity does **not** call it (an automatic rebind would make the USB link
    /// flap, and after a disconnect the system `init` resets VID/PID from
    /// `sys.usb.config` anyway).
    Rebind,

    // ------------------------------------------------ 镜像管理（就地执行）
    /// Create an image.
    Create {
        /// Target path.
        path: PathBuf,
        /// Size in bytes.
        #[arg(long)]
        size: u64,
        /// Layout: raw | gpt | mbr.
        #[arg(long, default_value = "gpt")]
        layout: String,
        /// Filesystem: fat32 | exfat | ext4.
        ///
        /// FAT32 is formatted via the bundled `bin/mkfs.vfat` tool;
        /// exFAT/ext4 use system `mkfs` utilities if available (see the
        /// `mkfs` array in `capabilities`).
        #[arg(long, default_value = "fat32")]
        filesystem: String,
        /// Volume label.
        #[arg(long, default_value = "GADGETDISK")]
        label: String,
        /// Partition spec, repeatable; format `SIZE[/GPT-TYPE[/MBR-TYPE[/NAME[/FS[/KIND]]]]]`.
        ///
        /// `SIZE` is a byte count; `0` means "consume the remaining space" (**at most
        /// one** partition may do this). Types are namespaced by the `gpt:` / `mbr:`
        /// prefixes (two unrelated type spaces; which applies is decided by
        /// `--layout`). An empty field means the default. `FS` of `none` means "do not
        /// format".
        ///
        /// Fields are separated by `/` rather than `:` because wire-format type names
        /// already carry a `gpt:` / `mbr:` prefix, and one colon cannot serve as both
        /// separator and prefix.
        ///
        /// `KIND` is `primary` (the default) or `logical`: **only meaningful for MBR**.
        /// Logical partitions are written into the EBR chain with indices starting at
        /// 5, and the extended container is created automatically. This option may be
        /// repeated; when omitted it behaves as before, i.e. a single partition filling
        /// the remaining space.
        #[arg(
            long = "partition",
            value_name = "SIZE[/GPT-TYPE[/MBR-TYPE[/NAME[/FS[/KIND]]]]]"
        )]
        partitions: Vec<String>,
    },
    /// Delete the given disk image.
    Delete {
        /// Image path.
        path: PathBuf,
    },
    // ------------------------------------------------ 本地 loop 挂载
    /// Mount an image into the device's local filesystem (for on-device viewing and editing).
    #[command(name = "attach-loop")]
    AttachLoop {
        /// Image path.
        image: PathBuf,
        /// Mount mode: rw | ro.
        #[arg(long, default_value = "rw")]
        mode: String,
        /// Target partition index.
        #[arg(long)]
        partition: Option<u32>,
        /// Mount read-only.
        #[arg(long, default_value_t = false)]
        read_only: bool,
    },
    /// Release a local loop mount.
    #[command(name = "detach-loop")]
    DetachLoop {
        /// Image path.
        #[arg(long)]
        image: Option<PathBuf>,
        /// Loop device.
        #[arg(long)]
        loop_dev: Option<String>,
    },

    // ------------------------------------------------ 服务与生命周期
    /// Run the WebUI's REST backend (started on demand, exits when idle).
    Serve {
        /// Module root directory (`api.json` is written into its `webroot/`).
        #[arg(long, default_value = gadgetdisk_gdd::DEFAULT_MODULE_ROOT)]
        module_dir: PathBuf,
        /// Listen port; `0` lets the kernel assign an ephemeral port.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Exit after this many idle seconds.
        #[arg(long, default_value_t = 60)]
        idle_timeout: u64,
    },
    /// Boot-time reconcile: restore USB export state from `run/state.json`.
    ///
    /// The **only** entry point of `service.sh`. All decisions live here (the Rust
    /// side); the script only passes arguments.
    ///
    /// Three cases (in priority order):
    ///
    /// 1. an export intent exists -> drop entries whose image is gone and re-export
    ///    the rest;
    /// 2. no intent, but our leftover functions/links remain in the kernel -> clean
    ///    them up;
    /// 3. neither -> nothing to do.
    ///
    /// On failure `run/state.json` is **kept** and the reason is written to
    /// `logs/service.log`, so the intent "the USB drive comes back after a reboot" is
    /// never erased by a single failure.
    Boot,
    /// Module uninstall cleanup: tear down USB exports, restore the system default
    /// identity, and clear runtime state.
    ///
    /// Called by `uninstall.sh`. It does **not** delete the data directory itself
    /// (images may be tens of GiB; the script decides).
    Uninstall,
    /// Read and write persistent config (`config/gadget.json`).
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    // ------------------------------------------------ 路径工具（只读）
    /// Show filesystem free space (used by the WebUI for size pre-checks).
    Df {
        /// Target path; defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// `config security` 的实现。
///
/// 与 REST 的 `GET|POST /api/v1/config/security` 是**同一套逻辑的双通道接入**：
/// 校验均使用 [`gadgetdisk_cli::selinux::validate_context_format`]，落盘均调用
/// [`GadgetConfig::store_image_context`]（原子读—改—写，完整保留 USB 身份）。统一实现
/// 可彻底避免双通道行为漂移，杜绝保存安全标签时意外覆盖 USB 设备标识配置。
fn run_config_security(dirs: &DataDirs, command: SecurityCommand) -> StdExitCode {
    use gadgetdisk_cli::cli_paths::GadgetConfig;

    let config = GadgetConfig::load(dirs);

    match command {
        SecurityCommand::Get => emit(output::success_value(&serde_json::json!({
            "image_context": config.resolved_image_context(),
            "configured": config.image_context,
            "default": gadgetdisk_cli::selinux::DEFAULT_IMAGE_CONTEXT,
            "path": gadgetdisk_cli::cli_paths::gadget_config(dirs).to_string_lossy(),
        }))),
        SecurityCommand::Set { image_context } => {
            // 校验规则与 WebUI/REST 完全一致（含空白、控制字符与长度），
            // 且**先校验再落盘**：非法值一旦持久化，之后每次挂载都会再失败一次。
            let value = match gadgetdisk_cli::selinux::validate_context_format(&image_context) {
                Ok(value) => value,
                Err(message) => return emit(output::usage(message)),
            };
            if let Err(err) = GadgetConfig::store_image_context(dirs, Some(&value)) {
                return emit(output::failure(
                    ErrorCode::Internal,
                    format!("failed to write config: {err}"),
                ));
            }
            emit(output::success_value(&serde_json::json!({
                "image_context": value,
                "path": gadgetdisk_cli::cli_paths::gadget_config(dirs).to_string_lossy(),
                // 已挂载的 loop/LUN 不会因为改标签而重新校验权限——内核在打开
                // 后备文件时按当时的标签 pin 住它。
                "note": "will be applied to image files under images/ on the next mount",
            })))
        }
        SecurityCommand::Clear => {
            if let Err(err) = GadgetConfig::store_image_context(dirs, None) {
                return emit(output::failure(
                    ErrorCode::Internal,
                    format!("failed to write config: {err}"),
                ));
            }
            emit(output::success_value(&serde_json::json!({
                "image_context": gadgetdisk_cli::selinux::DEFAULT_IMAGE_CONTEXT,
                "note": "reset to the built-in default context",
            })))
        }
    }
}

/// 解析 USB 的 VID/PID：接受 `0x18d1` 与 `6353` 两种写法。
///
/// 为什么不用 clap 的 `u16` 解析器：它只认十进制，`--vid 0x18d1` 会报
/// `invalid digit found in string`。而 `0x` 前缀正是用户写 VID 时最自然的形式
/// （`lsusb` 也这么显示），所以这里自己解析并把范围错误变成明确的用法提示。
fn parse_usb_id(text: &str) -> Option<u16> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).ok();
    }
    // 无前缀时按**十六进制**解释：USB 的 VID/PID 惯例都是十六进制，
    // 且 `0x` 可选时用户不会预期 `18d1` 被当成十进制。
    u16::from_str_radix(text, 16).ok()
}

/// `config` 的子命令。
#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print the current persistent config and the values in effect in the kernel.
    Get,
    /// Set the USB device identity (only the given fields are changed).
    Set {
        /// `idVendor`, in **hexadecimal** (optional `0x` prefix): `18d1` or `0x18d1`.
        ///
        /// Declared as `String` rather than `u16` because clap's integer parser only
        /// accepts decimal and would reject `--vid 0x18d1` with `invalid digit found
        /// in string`, whereas the `0x` prefix is the most natural way to write a VID
        /// (`lsusb` prints `18d1:4ee7` too).
        ///
        /// **Without a prefix the value is still hexadecimal**, not decimal: VID/PID
        /// are conventionally hexadecimal, and this is what makes `18d1` acceptable
        /// (hex digits with letters can never be decimal). So `--vid 18d1` is
        /// equivalent to `--vid 0x18d1`, and **not** to `--vid 6353`.
        #[arg(long)]
        vid: Option<String>,
        /// `idProduct`, in **hexadecimal** (optional `0x` prefix): `4ee7` or `0x4ee7`.
        #[arg(long)]
        pid: Option<String>,
        /// Manufacturer string.
        #[arg(long)]
        manufacturer: Option<String>,
        /// Product string.
        #[arg(long)]
        product: Option<String>,
        /// Serial number.
        #[arg(long)]
        serial: Option<String>,
    },
    /// SELinux security context for image files.
    ///
    /// The kernel thread (`file-storage`) reads the backing image in its own domain;
    /// if the image's label does not allow it to read, the host side will "see the
    /// device but read no contents". Before mounting, the CLI relabels the files
    /// **inside the image directory** to the value set here.
    Security {
        #[command(subcommand)]
        command: SecurityCommand,
    },
}

/// `config security` 的子命令。
#[derive(Subcommand, Debug)]
enum SecurityCommand {
    /// Print the currently effective target context (and where the default comes from).
    Get,
    /// Set the target context.
    Set {
        /// Of the form `u:object_r:system_file:s0`.
        #[arg(long)]
        image_context: String,
    },
    /// Clear the setting and fall back to the built-in default.
    Clear,
}

fn main() -> StdExitCode {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.clone();
    let socket = cli.socket.clone();
    let module_dir = cli
        .module_dir
        .clone()
        .or_else(|| default_module_dir(&data_dir));

    match cli.command {
        // ---- 只读状态与列举（就地执行，不拉起 gdd） ----
        TopLevel::Status => run_local(&data_dir, module_dir.as_deref(), LocalOp::Status),
        TopLevel::List => run_local(&data_dir, module_dir.as_deref(), LocalOp::List),
        TopLevel::Capabilities => {
            run_local(&data_dir, module_dir.as_deref(), LocalOp::Capabilities)
        }
        TopLevel::ListLoop => with_dirs(&data_dir, run_list_loop),

        // ---- 导出为 USB 设备（经 gdd） ----
        mount @ TopLevel::Mount { .. } => run_mount(mount, &data_dir, socket.as_deref()),
        unmount @ TopLevel::Unmount { .. } => {
            run_over_socket(unmount, &data_dir, socket.as_deref())
        }
        slot @ TopLevel::DeleteSlot { .. } => run_over_socket(slot, &data_dir, socket.as_deref()),
        rebind @ TopLevel::Rebind => run_over_socket(rebind, &data_dir, socket.as_deref()),

        // ---- 镜像管理（就地执行） ----
        // `--filesystem` / `--label` / `--partition` 现在都会**真正生效**：
        // 它们被解析为 REST create 契约里的字段（见 `LocalOp::Create`）。
        // 历史上的"静默忽略"缺陷已修复（docs/roadmap.md 已知缺陷 #1）。
        TopLevel::Create {
            path,
            size,
            layout,
            filesystem,
            label,
            partitions,
        } => run_local(
            &data_dir,
            module_dir.as_deref(),
            LocalOp::Create {
                path,
                size,
                layout,
                filesystem,
                label,
                partitions,
            },
        ),
        TopLevel::Delete { path } => {
            run_local(&data_dir, module_dir.as_deref(), LocalOp::Delete { path })
        }
        // ---- 本地 loop 挂载（就地执行） ----
        TopLevel::AttachLoop {
            image,
            mode,
            partition,
            read_only,
        } => with_dirs(&data_dir, |dirs| {
            run_attach_loop(dirs, image, &mode, partition, read_only)
        }),
        TopLevel::DetachLoop { image, loop_dev } => {
            with_dirs(&data_dir, |dirs| run_detach_loop(dirs, image, loop_dev))
        }

        // ---- 服务与生命周期 ----
        TopLevel::Serve {
            module_dir,
            port,
            idle_timeout,
        } => run_serve(data_dir, module_dir, port, idle_timeout),
        TopLevel::Boot => run_boot(&data_dir),
        TopLevel::Uninstall => run_uninstall(&data_dir),
        TopLevel::Config { command } => run_config(data_dir, command),

        // ---- 路径工具（只读） ----
        TopLevel::Df { path } => run_df(&path),
    }
}

/// 解析 `--partition SIZE[/GPT-TYPE[/MBR-TYPE[/NAME[/FS]]]]` 参数列表。
///
/// 返回 `Ok(None)` 表示用户没给任何分区（交由后端按单分区占满处理）。
///
/// ## 为什么用 `/` 而不是 `:` 分隔
///
/// 类型线格式自带 `gpt:` / `mbr:` 前缀（见 [`gadgetdisk_core::GptPartitionType`]），
/// 若字段分隔符也用 `:`，`SIZE:gpt:linux_filesystem` 就会被切成
/// `["SIZE", "gpt", "linux_filesystem"]`——前缀的冒号与分隔符**无法区分**。
/// 用 `/` 后两种冒号各司其职，且 `/` 不出现在任何类型名或 GUID 里。
///
/// ## 示例
///
/// ```text
/// --partition 64M                                        # 占位，类型/名称/文件系统全默认
/// --partition 1G/gpt:linux_filesystem/mbr:linux/ROOT/ext4
/// --partition 0/gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0//DATA/none
/// ```
///
/// 空字段表示"用默认值"；`FS` 为 `none` 表示该分区**不格式化**。
/// `SIZE` 接受纯字节数或带单位后缀（`64M`、`1G`）。
fn parse_partition_args(raw: &[String]) -> Result<Option<Vec<serde_json::Value>>, String> {
    if raw.is_empty() {
        return Ok(None);
    }

    let mut out = Vec::with_capacity(raw.len());
    for (i, item) in raw.iter().enumerate() {
        let ordinal = i + 1;
        // `splitn(6)`：名称里可能含 `/`，故限制切分次数。第 6 段是分区归属
        // （`primary`/`logical`），放在**末尾**以免破坏既有的 5 段语法。
        let mut parts = item.splitn(6, '/');
        let size_text = parts.next().unwrap_or("").trim();
        let gpt_text = parts.next().map(str::trim);
        let mbr_text = parts.next().map(str::trim);
        let name_text = parts.next().map(str::trim);
        let fs_text = parts.next().map(str::trim);
        let kind_text = parts.next().map(str::trim);

        let size_bytes = parse_size_text(size_text).ok_or_else(|| {
            format!("cannot parse the size of --partition #{ordinal}: {size_text} (examples: 64M, 1G, 0)")
        })?;

        let mut object = serde_json::json!({ "size_bytes": size_bytes });

        if let Some(t) = gpt_text.filter(|t| !t.is_empty()) {
            if gadgetdisk_core::GptPartitionType::parse(t).is_none() {
                return Err(format!(
                    "cannot parse the GPT type of --partition #{ordinal}: {t} \
                     (examples: gpt:microsoft_basic, gpt:linux_filesystem, \
                     gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0)"
                ));
            }
            object["gpt_type"] = serde_json::Value::String(t.to_string());
        }

        if let Some(t) = mbr_text.filter(|t| !t.is_empty()) {
            if gadgetdisk_core::MbrPartitionType::parse(t).is_none() {
                return Err(format!(
                    "cannot parse the MBR type of --partition #{ordinal}: {t} \
                     (examples: mbr:fat32_lba, mbr:linux, mbr:0x1A)"
                ));
            }
            object["mbr_type"] = serde_json::Value::String(t.to_string());
        }

        if let Some(n) = name_text.filter(|n| !n.is_empty()) {
            object["name"] = serde_json::Value::String(n.to_string());
        }

        if let Some(f) = fs_text.filter(|f| !f.is_empty()) {
            // `none` 表示该分区不格式化。
            if f != "none" && gadgetdisk_core::FilesystemType::parse(f).is_none() {
                return Err(format!(
                    "cannot parse the filesystem of --partition #{ordinal}: {f} \
                     (one of fat32|exfat|ext4|none)"
                ));
            }
            object["filesystem"] = serde_json::Value::String(f.to_string());
        }

        if let Some(k) = kind_text.filter(|k| !k.is_empty()) {
            if gadgetdisk_core::PartitionKind::parse(k).is_none() {
                return Err(format!(
                    "cannot parse the kind of --partition #{ordinal}: {k} \
                     (one of primary|logical|extended; the latter two are MBR-only)"
                ));
            }
            object["kind"] = serde_json::Value::String(k.to_string());
        }

        out.push(object);
    }

    Ok(Some(out))
}

/// 解析容量文本（纯数字或 `64M`/`1G` 之类）。
fn parse_size_text(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (digits, multiplier) = match text.chars().last()? {
        'k' | 'K' => (&text[..text.len() - 1], 1024u64),
        'm' | 'M' => (&text[..text.len() - 1], 1024 * 1024),
        'g' | 'G' => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        't' | 'T' => (&text[..text.len() - 1], 1024u64.pow(4)),
        c if c.is_ascii_digit() => (text, 1),
        _ => return None,
    };
    let value: u64 = digits.trim().parse().ok()?;
    value.checked_mul(multiplier)
}

/// 需要数据目录的子命令的统一入口：准备目录后交给 `body`。
///
/// 所有子命令都经过这里，因此「目录一定存在且权限正确」对用户是「随便跑哪个
/// 命令都会发生」，不依赖开机的某个特定时机。
fn with_dirs<F>(data_dir: &Path, body: F) -> StdExitCode
where
    F: FnOnce(DataDirs) -> StdExitCode,
{
    match prepare(data_dir.to_path_buf()) {
        Ok(dirs) => body(dirs),
        Err(code) => code,
    }
}

/// 把一条请求发给 `gdd`（必要时先拉起它），返回应答或已渲染的错误。
fn send_to_gdd(
    dirs: &DataDirs,
    socket_path: &Path,
    request: &gadgetdisk_proto::Message,
) -> Result<gadgetdisk_proto::Message, StdExitCode> {
    match gadgetdisk_cli::serve::gdd_request(dirs, socket_path, request) {
        Ok(response) => Ok(response),
        Err((code, message)) => Err(emit(output::failure(code, message))),
    }
}

/// 需要与 `gdd` 通信的子命令的统一入口。
fn with_gdd_dirs<F>(data_dir: &Path, socket: Option<&Path>, body: F) -> StdExitCode
where
    F: FnOnce(DataDirs, PathBuf) -> StdExitCode,
{
    match prepare(data_dir.to_path_buf()) {
        Ok(dirs) => {
            let socket_path = socket
                .map(Path::to_path_buf)
                .unwrap_or_else(|| dirs.socket_path());
            body(dirs, socket_path)
        }
        Err(code) => code,
    }
}

/// 统一的目录准备：建目录。
///
/// 所有子命令都要经过这里，因此「目录一定存在且权限正确」对用户是
/// 「随便跑哪个命令都会发生」，不需要依赖开机的某个特定时机。
fn prepare(data_dir: PathBuf) -> Result<DataDirs, StdExitCode> {
    let dirs = DataDirs::new(data_dir);
    if let Err(err) = dirs.create_all() {
        return Err(emit(output::failure(
            ErrorCode::Internal,
            format!("failed to prepare the data directory: {err}"),
        )));
    }

    // 日志出口在目录就绪后立刻装好，使**所有**子命令（含 `serve`）都写
    // `logs/cli.log`。放在这里而不是各子命令里，是为了避免「某个子命令忘了
    // 初始化」这种静默缺失。
    gadgetdisk_gdd::logging::init(Some(gadgetdisk_cli::cli_paths::cli_log(&dirs)));
    Ok(dirs)
}

/// 输出一个 [`output::JsonOutput`] 并返回其退出码。
fn emit(result: output::JsonOutput) -> StdExitCode {
    if result.to_stderr {
        eprint!("{}", result.line());
    } else {
        print!("{}", result.line());
    }
    let _ = std::io::stdout().flush();
    StdExitCode::from(result.exit_code.as_u8())
}

/// 运行 REST 后端（`serve`）。
///
/// 空闲自动退出，因此它不是一个「常驻服务」；WebUI 需要时再拉起它。
fn run_serve(data_dir: PathBuf, module_dir: PathBuf, port: u16, idle_timeout: u64) -> StdExitCode {
    let dirs = match prepare(data_dir) {
        Ok(dirs) => dirs,
        Err(code) => return code,
    };

    let mut config = gadgetdisk_cli::serve::ServeConfig::new(dirs, module_dir);
    config.port = port;
    config.idle_timeout = std::time::Duration::from_secs(idle_timeout.max(1));

    match gadgetdisk_cli::serve::run(config) {
        Ok(()) => StdExitCode::SUCCESS,
        Err(err) => emit(output::failure(
            ErrorCode::Internal,
            format!("failed to start serve: {err}"),
        )),
    }
}

/// 开机对账：把 configfs 收敛到 `run/state.json` 的导出意图。
///
/// **总是以成功退出**：失败不应让 `service.sh` 认为进程崩溃（那会引入退避
/// 重启甚至 bootloop 保护那一整套复杂度）。真正的结果体现在
/// `run/state.json` 是否被兑现、以及 `logs/service.log` 的记录上。
fn run_boot(data_dir: &Path) -> StdExitCode {
    let dirs = match prepare(data_dir.to_path_buf()) {
        Ok(dirs) => dirs,
        Err(code) => return code,
    };

    let outcome = gadgetdisk_cli::serve::boot_reconcile(&dirs);
    let line = outcome.describe();
    // 同时写日志文件与 stderr：前者供 WebUI 展示，后者供手动排查。
    let _ = gadgetdisk_cli::serve::append_service_log(&dirs, &line);
    println!("gadgetdisk boot: {line}");
    StdExitCode::SUCCESS
}

/// 模块卸载前的收尾。
///
/// 与 `boot` 的区别：这里**明确要求**把导出拆干净并还原 Android 身份，而不是
/// 按意图恢复；而且失败要如实回报（卸载脚本据此决定是否继续删数据目录）。
fn run_uninstall(data_dir: &Path) -> StdExitCode {
    let dirs = match prepare(data_dir.to_path_buf()) {
        Ok(dirs) => dirs,
        Err(code) => return code,
    };

    let mut failures = Vec::new();

    // 1. 拆除导出（经 UDS；gdd 不在跑时会先被拉起，因为确实有东西要拆）。
    if gadget_adapter::any_exported() {
        match gadgetdisk_cli::serve::ensure_gdd(&dirs, &dirs.socket_path()) {
            Ok(()) => {
                if let Err((code, message)) = gadgetdisk_cli::serve::gdd_request(
                    &dirs,
                    &dirs.socket_path(),
                    &gadgetdisk_proto::Message::UnmountRequest(gadgetdisk_proto::UnmountRequest {
                        lun: None,
                    }),
                ) {
                    failures.push(format!("failed to tear down exports ({code:?}): {message}"));
                }
            }
            Err((_code, message)) => failures.push(format!("cannot start gdd: {message}")),
        }
    }

    // 2. 还原 Android 身份。
    if let Some(mut editor) = IdentityEditor::open() {
        failures.extend(editor.restore_backup(&dirs));
    }

    // 3. 清理运行期状态。**不删数据目录**（镜像可能有几十 GiB，交给脚本决定）。
    let _ = gadgetdisk_cli::gadget_adapter::clear_intent(&dirs);
    let _ = gadgetdisk_usb::jsonfile::remove_if_exists(
        &gadgetdisk_cli::cli_paths::identity_backup(&dirs),
    );

    if failures.is_empty() {
        println!("gadgetdisk uninstall: cleanup finished");
        StdExitCode::SUCCESS
    } else {
        emit(output::failure(
            ErrorCode::Internal,
            format!(
                "cleanup had {} failure(s): {}",
                failures.len(),
                failures.join("; ")
            ),
        ))
    }
}

/// 读写持久配置（`config/gadget.json`）。
fn run_config(data_dir: PathBuf, command: ConfigCommand) -> StdExitCode {
    let dirs = match prepare(data_dir) {
        Ok(dirs) => dirs,
        Err(code) => return code,
    };

    match command {
        ConfigCommand::Security { command } => run_config_security(&dirs, command),
        ConfigCommand::Get => {
            let stored = gadgetdisk_cli::serve::load_identity(&dirs);
            // 同时回内核里的**当前生效值**：用户最常问的是「我设的生效了吗」，
            // 只回文件内容无法回答这个问题。
            let effective = IdentityEditor::open().map(|editor| editor.read());
            // 镜像目标上下文与身份同在 `config/gadget.json`，因此一并回显。
            //
            // **不可或缺**：WebUI 设置页在 REST 服务不可用时将回退至 `config get`
            // 读取标签。若缺失这三个字段，界面在回退通道下将显示为空白，造成与
            // `config security` 子命令查询结果不一致。
            let security = gadgetdisk_cli::cli_paths::GadgetConfig::load(&dirs);
            emit(output::success_value(&serde_json::json!({
                "config": stored,
                "effective": effective,
                "image_context": security.resolved_image_context(),
                "image_context_configured": security.image_context,
                "default_image_context": gadgetdisk_cli::selinux::DEFAULT_IMAGE_CONTEXT,
                "path": gadgetdisk_cli::cli_paths::gadget_config(&dirs).to_string_lossy(),
            })))
        }
        ConfigCommand::Set {
            vid,
            pid,
            manufacturer,
            product,
            serial,
        } => {
            let mut identity = gadgetdisk_cli::serve::load_identity(&dirs);
            if let Some(text) = vid {
                match parse_usb_id(&text) {
                    Some(value) => identity.id_vendor = Some(value),
                    None => {
                        return emit(output::usage(format!(
                            "VID must be a hexadecimal integer in 0x0000..0xffff (e.g. 18d1 or 0x18d1): {text}"
                        )));
                    }
                }
            }
            if let Some(text) = pid {
                match parse_usb_id(&text) {
                    Some(value) => identity.id_product = Some(value),
                    None => {
                        return emit(output::usage(format!(
                            "PID must be a hexadecimal integer in 0x0000..0xffff (e.g. 4ee7 or 0x4ee7): {text}"
                        )));
                    }
                }
            }
            if let Some(value) = manufacturer {
                identity.manufacturer = Some(value);
            }
            if let Some(value) = product {
                identity.product = Some(value);
            }
            if let Some(value) = serial {
                identity.serial = Some(value);
            }

            if let Err((code, message)) =
                gadgetdisk_cli::serve::save_and_apply_identity(&dirs, &identity)
            {
                return emit(output::failure(code, message));
            }
            emit(output::success_value(&serde_json::json!({
                "config": identity,
                "path": gadgetdisk_cli::cli_paths::gadget_config(&dirs).to_string_lossy(),
                // 身份不断开 USB，因此**不会**立刻对主机生效。把这一点作为应答的
                // 一部分返回，让 WebUI 不必自己猜、也不必编造「已生效」。
                "applies_on": "next-connect",
                "note": "config saved. The host will enumerate the new device identity after \
            you replug the USB cable. Note that system init may reset the kernel's temporary values after a \
            disconnect, so the saved config is authoritative.",
            })))
        }
    }
}

/// 就地执行、不经 `gdd` 的操作。
///
/// ## 为什么用枚举而不是直接收 `TopLevel`
///
/// `TopLevel` 里有十几个变体，其中多数需要 `gdd` 或文件系统路径；把它们都塞进
/// 一个函数会让「哪些操作是就地执行的」这一分类消失。这层窄枚举把这个分类写成
/// 类型约束：要新增一个就地操作，必须显式加一个变体。
enum LocalOp {
    Status,
    List,
    Capabilities,
    Create {
        path: PathBuf,
        size: u64,
        layout: String,
        filesystem: String,
        label: String,
        /// `SIZE[/GPT-TYPE[/MBR-TYPE[/NAME[/FS[/KIND]]]]]` 形式的原始分区规格，解析在 `run_local` 里做。
        partitions: Vec<String>,
    },
    Delete {
        path: PathBuf,
    },
}

/// 就地执行一个不涉及 gadget 导出的操作。
///
/// 复用 [`LiveBackend`](gadgetdisk_cli::serve::LiveBackend)——`serve`（REST）已经
/// 用它实现了同一批操作，因此这里**无需第二份实现**，两条通道的行为天然一致。
/// 返回的 `serde_json::Value` 直接作为 CLI 的输出，与经 socket 转发时的格式相同。
/// 由数据目录推出模块根目录的缺省位置。
///
/// 安装布局是 `<模块根>/` 与 `/data/adb/gadget-disk/` 平级：
/// `/data/adb/modules/gadget-disk/` 与 `/data/adb/gadget-disk/`。
/// 数据目录形如 `/data/adb/gadget-disk`，故模块根为
/// `../modules/gadget-disk`。推不出来时返回 `None`（只探测系统工具）。
fn default_module_dir(data_dir: &Path) -> Option<PathBuf> {
    let parent = data_dir.parent()?;
    let candidate = parent.join("modules").join("gadget-disk");
    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

fn run_local(data_dir: &Path, module_dir: Option<&Path>, op: LocalOp) -> StdExitCode {
    with_dirs(data_dir, |dirs| {
        // 一次性进程：导入必须跑完再返回（响应之后本进程就没了，
        // 后台线程会被一起带走，`job_id` 也无从轮询）。
        let mut backend = gadgetdisk_cli::serve::LiveBackend::new(dirs, PathBuf::new());
        // 带上模块目录，使自带的 `bin/mkfs.vfat` 能被探测到。
        if let Some(dir) = module_dir {
            backend = backend.with_module_dir(dir);
        }
        use gadgetdisk_cli::rest::Backend as _;

        let result = match op {
            LocalOp::Status => backend.status(),
            LocalOp::List => backend.images(),
            LocalOp::Capabilities => backend.capabilities(),
            LocalOp::Create {
                path,
                size,
                layout,
                filesystem,
                label,
                partitions,
            } => {
                // 分区规格在这里解析为结构化 JSON——**复用 REST 契约的字段名**，
                // 因此 CLI 与 REST 两条通道的语义不可能漂移（同一份 Backend::create）。
                let parsed = match parse_partition_args(&partitions) {
                    Ok(value) => value,
                    Err(message) => return emit(output::usage(&message)),
                };
                let mut body = serde_json::json!({
                    "path": path.to_string_lossy(),
                    "size_bytes": size,
                    "layout": layout,
                    "filesystem": filesystem,
                    "volume_label": label,
                });
                if let Some(list) = parsed {
                    body["partitions"] = serde_json::Value::Array(list);
                }
                backend.create(body.to_string().as_bytes())
            }
            LocalOp::Delete { path } => {
                let body = serde_json::json!({ "path": path.to_string_lossy() }).to_string();
                backend.delete(body.as_bytes())
            }
        };

        match result {
            Ok(value) => emit(output::success_value(&value)),
            Err((code, message)) => emit(output::failure(code, message)),
        }
    })
}

/// `mount`：先应用身份，再经 UDS 让 `gdd` 挂载，最后按**内核真值**写导出意图。
///
/// 写意图这一步是本命令最容易被忽略但却关键的职责：`run/state.json` 是
/// 「重启后该恢复成什么样」的唯一依据，而 `gdd` 明确不写它。因此它必须由 CLI 在
/// 挂载成功后，用 `gdd` 返回的内核真值来写——而不是写「我请求了什么」。
/// 请求与事实可能不同（某个 LUN 建失败、内核不支持 `inquiry_string` 等）。
fn run_mount(command: TopLevel, data_dir: &Path, socket: Option<&Path>) -> StdExitCode {
    let TopLevel::Mount {
        images,
        modes,
        luns,
        inquiries,
        rebind,
    } = command
    else {
        unreachable!("run_mount 只接受 TopLevel::Mount")
    };

    // 重复选项**按下标与位置参数对齐**：第 i 个镜像取第 i 项，缺省项用各自的
    // 默认值（rw / 自动分配 / 不设置）。数量多于镜像的额外选项视为用法错误而
    // 不是静默忽略：用户写了 `--mode rw --mode ro` 却只给一个镜像，多半是漏了
    // 路径。
    if modes.len() > images.len() {
        return emit(output::usage(
            "more --mode values than images (options align with images by index)",
        ));
    }
    if luns.len() > images.len() {
        return emit(output::usage(
            "more --lun values than images (options align with images by index)",
        ));
    }
    if inquiries.len() > images.len() {
        return emit(output::usage(
            "more --inquiry values than images (options align with images by index)",
        ));
    }

    let devices = match images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let mode = match modes.get(index) {
                Some(value) => client::parse_mode(value)?,
                None => Mode::Rw,
            };
            Ok(CommandDevice {
                image: image.clone(),
                mode,
                lun: luns.get(index).copied(),
                inquiry_string: inquiries.get(index).cloned(),
            })
        })
        .collect::<Result<Vec<_>, client::ClientError>>()
    {
        Ok(devices) => devices,
        Err(err) => return emit(output::usage(err.to_string())),
    };

    let parsed = Command::Mount { devices, rebind };
    let request = match client::build_request(&parsed) {
        Ok(request) => request,
        Err(err) => return emit(output::usage(err.to_string())),
    };

    with_gdd_dirs(data_dir, socket, |dirs, socket_path| {
        // 0. 镜像文件的 SELinux 上下文：内核线程要能读写它，否则 PC 侧能认出设备
        //    但读不出内容（只允许读时写入还会被**静默丢弃**）。只改**我们镜像
        //    目录里**的文件；目录外只警告。必须在 `gdd` 挂载**之前**做完。
        //
        //    目标值只解析一次：同一批导出必须用同一个上下文，否则配置在操作途中
        //    被 WebUI 改掉会让同一次挂载里的两个 LUN 被打上不同标签。
        let target = gadgetdisk_cli::image_context::target_for(&dirs);
        for image in &images {
            for warning in gadgetdisk_cli::image_context::check_in(&dirs.images(), image, &target) {
                gadgetdisk_gdd::logging::warn(&warning);
                // `logging::warn` 已写 stderr；这里再打到 stdout，使 WebUI 的回退
                // 通道（读 stdout）也能拿到。CLI 契约允许前置警告行——WebUI 的
                // `tryExtractJson` 会跳过非 JSON 前缀（已由单测覆盖）。
                println!("gadgetdisk: warning: {warning}");
            }
        }

        // 1. 身份：写 configfs（若配置存在）。
        //
        // **不改 UDC**。身份只在主机**重新枚举**时被读到，而重新枚举由用户拔插
        // 数据线触发；自动重绑会让 USB 链路抖动，且真机上断开后 init 还会按
        // `sys.usb.config` 重置 VID/PID，收益被抹掉。`--rebind` 是显式逃生口。
        let identity = gadgetdisk_cli::serve::load_identity(&dirs);
        if !identity.is_empty() {
            match IdentityEditor::open() {
                Some(mut editor) => {
                    // 改身份前先备份 Android 的原始值（只备份一次）。
                    if let Err(err) = editor.capture_backup_if_absent(&dirs) {
                        eprintln!(
                            "gadgetdisk: failed to back up the identity (continuing with the mount): {err}"
                        );
                    }
                    if let Err(err) = editor.apply(&identity) {
                        return emit(output::failure(
                            ErrorCode::InvalidArgument,
                            format!("cannot apply the USB device identity: {err}"),
                        ));
                    }
                }
                None => {
                    eprintln!("gadgetdisk: configfs unavailable; skipping identity application");
                }
            }
        }

        // 2. 经 UDS 交给 gdd。
        let response = match send_to_gdd(&dirs, &socket_path, &request) {
            Ok(response) => response,
            Err(code) => return code,
        };

        // 3. 用**内核真值**更新导出意图。
        if let gadgetdisk_proto::Message::MountResponse(ref resp) = response {
            let intent = ExportIntent {
                version: 1,
                luns: resp
                    .devices
                    .iter()
                    .filter(|lun| lun.attached && !lun.image_path.is_empty())
                    .map(|lun| IntentLun {
                        index: lun.index,
                        image_path: lun.image_path.clone(),
                        mode: lun.mode.as_str().to_string(),
                        inquiry_string: lun.inquiry_string.clone(),
                    })
                    .collect(),
            };
            if let Err(err) = gadget_adapter::save_intent(&dirs, &intent) {
                // 意图写不进去意味着「重启后不会自动恢复」——必须让用户知道，
                // 但挂载本身已经成功，所以用 stderr 提示而不是把命令判失败。
                eprintln!(
                    "gadgetdisk: warning: failed to write the export intent; it will not be restored automatically after a reboot: {err}"
                );
            }
        }

        match output::success(&response) {
            Ok(out) => emit(out),
            Err(err) => emit(output::failure(
                ErrorCode::Internal,
                format!("failed to encode the response: {err}"),
            )),
        }
    })
}

/// 把 `unmount` / `delete-slot` / `rebind` 交给 `gdd`，并按**内核真值**更新意图。
///
/// 与 `mount` 对称：`gdd` 负责改 configfs，CLI 负责记「重启后该恢复成什么样」。
/// 全部弹出后意图为空 → 文件被删除（`save_intent` 的语义），因此下次开机的
/// `boot` 会判定「无事可做」。
fn run_over_socket(command: TopLevel, data_dir: &Path, socket: Option<&Path>) -> StdExitCode {
    let parsed = match command {
        TopLevel::Unmount { lun } => Command::Unmount { lun },
        TopLevel::DeleteSlot { lun } => Command::DeleteSlot { lun },
        TopLevel::Rebind => Command::Rebind,
        other => {
            return emit(output::usage(format!(
                "internal error: {other:?} is not a subcommand that goes through gdd"
            )));
        }
    };

    let request = match client::build_request(&parsed) {
        Ok(request) => request,
        Err(err) => return emit(output::usage(err.to_string())),
    };

    with_gdd_dirs(data_dir, socket, |dirs, socket_path| {
        let response = match send_to_gdd(&dirs, &socket_path, &request) {
            Ok(response) => response,
            Err(code) => return code,
        };

        // 意图按内核真值重写：按 LUN 卸载只是少一项，全部弹出则清空文件。
        if let gadgetdisk_proto::Message::DeleteSlotResponse(ref resp) = response {
            let intent = gadget_adapter::intent_from_luns(&resp.devices);
            if let Err(err) = gadget_adapter::save_intent(&dirs, &intent) {
                eprintln!("gadgetdisk: warning: failed to update the export intent: {err}");
            }
        }

        if let gadgetdisk_proto::Message::UnmountResponse(ref resp) = response {
            let intent = gadget_adapter::ExportIntent {
                version: 1,
                luns: resp
                    .devices
                    .iter()
                    .filter(|lun| lun.attached && !lun.image_path.is_empty())
                    .map(|lun| gadget_adapter::IntentLun {
                        index: lun.index,
                        image_path: lun.image_path.clone(),
                        mode: lun.mode.as_str().to_string(),
                        inquiry_string: lun.inquiry_string.clone(),
                    })
                    .collect(),
            };
            if let Err(err) = gadget_adapter::save_intent(&dirs, &intent) {
                eprintln!("gadgetdisk: warning: failed to update the export intent: {err}");
            }

            // 全部弹出时没有意图可留，顺带把身份还回去——这是「不再当 U 盘」的
            // 完整语义，否则手机会带着我们设的 VID/产品名继续跑 MTP。
            if intent.is_empty()
                && let Some(mut editor) = IdentityEditor::open()
            {
                let failures = editor.restore_backup(&dirs);
                for failure in failures {
                    eprintln!("gadgetdisk: identity restore incomplete: {failure}");
                }
            }
        }

        match output::success(&response) {
            Ok(out) => emit(out),
            Err(err) => emit(output::failure(
                ErrorCode::Internal,
                format!("failed to encode the response: {err}"),
            )),
        }
    })
}

/// 把镜像挂到设备本地（一次性操作）。
///
/// 数据安全底线：镜像正作为 USB 设备导出时**不得**本地挂载——两边同时写入会
/// 损坏文件系统。判据取自 **configfs 真值**，因此即使 `gdd` 不在运行也能正确
/// 拒绝（`gdd` 只在导出期间存在，不能依赖它来做这个判断）。
///
/// ## 为什么此处亦须修正 SELinux 安全标签（真机实测）
///
/// 本地 loop 挂载的底层镜像同样由**内核工作线程**读写，SELinux 依据该内核线程所属安全域
/// 进行权限判定。因此“挂载前确保安全标签允许内核线程读写”这一约束对 loop 挂载与本模块
/// gadget 导出同等适用。修正必须在 `LOOP_SET_FD`（`LoopMounts::attach` 内的 `try_attach`）
/// **之前**完成：内核在打开后备镜像文件的瞬间即按**当时**的标签锁定访问权限。
fn run_attach_loop(
    dirs: DataDirs,
    image: PathBuf,
    mode: &str,
    partition: Option<u32>,
    read_only: bool,
) -> StdExitCode {
    if let Err(resp) = check_gadget_not_holding(&dirs, &image) {
        return emit(resp);
    }

    // `--mode ro` 与 `--read-only` 等价；`rw` 之外的值视为用法错误。
    let read_only = match mode {
        "rw" => read_only,
        "ro" => true,
        other => {
            return emit(output::usage(format!(
                "unknown mode: {other} (available: rw, ro)"
            )));
        }
    };

    // 修正失败/目录外**不阻断**挂载（挂载本身仍有意义），但必须回报给用户。
    // 只读挂载同样应用目标标签：不区分 ro/rw，才能让「先只读挂载、随后改读写」
    // 不会突然失败。
    let warnings = gadgetdisk_cli::image_context::check(&dirs, &image);
    for warning in &warnings {
        gadgetdisk_gdd::logging::warn(warning);
        // 与 `run_mount` 同样再打一份到 stdout：WebUI 的 CLI 回退通道读 stdout。
        println!("gadgetdisk: warning: {warning}");
    }

    let mut mounts = LoopMounts::new(dirs);
    match mounts.attach(&image, read_only, partition) {
        Ok(attachment) => emit(output::success_value(&serde_json::json!({
            "loop_dev": attachment.loop_dev,
            "loop_part_devs": attachment.loop_part_devs,
            "mountpoint": attachment.mountpoint,
            // 英文诊断文本（面向 CLI/API 使用者），UI 只作排查线索展示，
            // 与协议里 `message` 的地位一致：**无稳定性承诺**。
            "warnings": warnings,
        }))),
        Err(err) => emit(output::failure(err.code, err.message)),
    }
}

/// 释放本地 loop 挂载（一次性操作）。
fn run_detach_loop(
    dirs: DataDirs,
    image: Option<PathBuf>,
    loop_dev: Option<String>,
) -> StdExitCode {
    if image.is_none() && loop_dev.is_none() {
        return emit(output::usage("either --image or --loop-dev is required"));
    }

    let mut mounts = LoopMounts::new(dirs);
    match mounts.detach(image.as_deref(), loop_dev.as_deref()) {
        Ok(released) => emit(output::success_value(&serde_json::json!({
            "released": !released.is_empty(),
        }))),
        Err(err) => emit(output::failure(err.code, err.message)),
    }
}

/// 列出本地 loop 附件（读内核真值）。
fn run_list_loop(dirs: DataDirs) -> StdExitCode {
    let mounts = LoopMounts::new(dirs);
    let attachments = mounts.attachments();
    emit(output::success_value(&serde_json::json!({
        "attachments": attachments,
    })))
}

/// 该镜像当前是否正作为 USB 设备导出？
///
/// 只读 configfs 真值来判定占用——因此**不依赖 `gdd` 是否在跑**。
/// 这条检查必须在本地挂载之前做：同一镜像同时被两边写入会损坏文件系统。
fn check_gadget_not_holding(_dirs: &DataDirs, image: &Path) -> Result<(), output::JsonOutput> {
    let exported = gadget_adapter::GadgetReader::open()
        .map(|reader| reader.is_mounted(image))
        .unwrap_or(false);
    if exported {
        return Err(output::failure(
            ErrorCode::ImageInUse,
            format!(
                "image {} is currently exported as a USB device; unmount the USB export before \
                 mounting it locally, otherwise writes from both sides will corrupt the filesystem",
                image.display()
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- 只读工具
//
// 工具（df）的实现位于 `gadgetdisk_core::fsinfo`，与 REST 的
// `/api/v1/tool/df` **共用同一份逻辑与同一组字段名**。曾经 CLI 与 REST 各写
// 一遍时，两者会随时间漂移（字段改名只改一处），因此刻意只保留一处归属。

/// 把 `CoreError` 归类后映射为协议错误码。
///
/// 必须**逐项**映射：早期实现把所有非 `NotFound` 的错误都当作
/// `permission_denied`，于是 `ELOOP`（符号链接成环）、`ENAMETOOLONG`
/// 等会误导用户去查权限，而真正原因完全不同。
fn fs_error_code(err: &gadgetdisk_core::CoreError) -> ErrorCode {
    use gadgetdisk_core::fsinfo::FsError;
    match gadgetdisk_core::fsinfo::classify(err) {
        FsError::NotFound => ErrorCode::ImageNotFound,
        FsError::PermissionDenied => ErrorCode::PermissionDenied,
        FsError::Invalid => ErrorCode::InvalidArgument,
        FsError::Other => ErrorCode::Internal,
    }
}

/// 输出文件系统可用空间，供 WebUI 做容量预检。
fn run_df(path: &Path) -> StdExitCode {
    // 目标可能尚不存在（WebUI 要在创建 `images/` 之前预检空间），
    // `available_bytes` 会向上找到**最近的已存在祖先**再测量，
    // 并把实际测量的对象回显为 `path`，避免误读成目标目录的空间。
    match gadgetdisk_core::fsinfo::available_bytes(path) {
        Ok((target, available, total)) => emit(output::success_value(
            // 与 REST 的 `/api/v1/tool/df` 共用同一份字段定义（见 `serve::df_payload`）：
            // 两条通道的 `df` 形状若不同，WebUI 只有在真的走回退通道时才会暴露问题。
            &gadgetdisk_cli::serve::df_payload(&target, available, total),
        )),
        Err(err) => emit(output::failure(fs_error_code(&err), err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_partition_args, parse_size_text, parse_usb_id};

    // ------------------------------------------------ 帮助文本语言

    /// 所有子命令的帮助文本必须是**英文**，且每一项都要有说明。
    ///
    /// 为什么需要这条测试：clap 的 derive 把 `///` doc comment 直接渲染成帮助，
    /// 而本仓库的注释规范是中文。因此凡是要给用户看的文案都必须写成显式的
    /// `help = ...` 属性——**漏掉一处不会编译失败**，只会让那一项悄悄变空或
    /// 漏出中文。这类缺陷只有渲染出来才看得见。
    #[test]
    fn every_help_screen_is_english_and_non_empty() {
        use clap::CommandFactory;

        /// 渲染某个子命令路径（空路径 = 顶层）的帮助文本。
        fn render(path: &[&str]) -> String {
            let mut cmd = super::Cli::command();
            for name in path {
                cmd = cmd
                    .find_subcommand_mut(name)
                    .unwrap_or_else(|| panic!("子命令 {name} 不存在"))
                    .clone();
            }
            cmd.render_long_help().to_string()
        }

        // 子命令清单从 clap 自身取，新增子命令会被自动覆盖（不会漏测）。
        let names: Vec<String> = {
            let cmd = super::Cli::command();
            cmd.get_subcommands()
                .map(|s| s.get_name().to_string())
                // `help` 由 clap 内置生成，其文案是 clap 自己的。
                .filter(|n| n != "help")
                .collect()
        };
        assert!(names.len() >= 15, "子命令数量异常：{}", names.len());

        let mut paths: Vec<Vec<String>> = vec![Vec::new()];
        paths.extend(names.iter().map(|n| vec![n.clone()]));
        // `config` 还有一层子命令。
        for sub in ["get", "set", "security"] {
            paths.push(vec!["config".into(), sub.into()]);
        }

        for path in paths {
            let refs: Vec<&str> = path.iter().map(String::as_str).collect();
            let text = render(&refs);
            let label = if path.is_empty() {
                "gadgetdisk".to_string()
            } else {
                format!("gadgetdisk {}", path.join(" "))
            };

            assert!(
                !text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "`{label} --help` 里有中文，用户可见的文案必须全英文：\n{text}"
            );

            // 每个选项都必须有非空说明：只检查紧跟在选项名之后确实有文字。
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
                "`{label} --help` 里有选项缺少说明（`///` 可能没转成 help 属性）：{missing:?}"
            );
        }
    }

    // ------------------------------------------------ 分区参数解析

    #[test]
    fn no_partition_args_yields_none() {
        // 不给 --partition 时必须回 None，让后端走"单分区占满"的既有语义。
        assert_eq!(parse_partition_args(&[]).unwrap(), None);
    }

    #[test]
    fn partition_size_accepts_units_and_bare_bytes() {
        assert_eq!(parse_size_text("1024"), Some(1024));
        assert_eq!(parse_size_text("64M"), Some(64 * 1024 * 1024));
        assert_eq!(parse_size_text("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_text("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size_text("512K"), Some(512 * 1024));
        assert_eq!(parse_size_text(" 4G "), Some(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn partition_size_rejects_garbage() {
        assert_eq!(parse_size_text("abc"), None);
        assert_eq!(parse_size_text(""), None);
        assert_eq!(parse_size_text("M"), None);
        assert_eq!(parse_size_text("-1"), None);
    }

    #[test]
    fn partition_size_detects_overflow() {
        // 乘出 u64 溢出时必须回 None，而不是 panic 或回绕。
        assert_eq!(parse_size_text("99999999999999999999T"), None);
    }

    #[test]
    fn partition_arg_parses_size_only() {
        let parsed = parse_partition_args(&["64M".to_string()]).unwrap().unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["size_bytes"], 64 * 1024 * 1024);
        // 未给出的字段不应出现（由后端按布局与文件系统推断）。
        assert!(parsed[0].get("gpt_type").is_none());
        assert!(parsed[0].get("mbr_type").is_none());
        assert!(parsed[0].get("name").is_none());
        assert!(parsed[0].get("filesystem").is_none());
    }

    #[test]
    fn partition_arg_parses_all_five_segments() {
        // SIZE/GPT/MBR/NAME/FS —— 类型分域，两套类型各自独立。
        let parsed =
            parse_partition_args(&["1G/gpt:linux_filesystem/mbr:linux/ROOT/ext4".to_string()])
                .unwrap()
                .unwrap();
        assert_eq!(parsed[0]["size_bytes"], 1024 * 1024 * 1024);
        assert_eq!(parsed[0]["gpt_type"], "gpt:linux_filesystem");
        assert_eq!(parsed[0]["mbr_type"], "mbr:linux");
        assert_eq!(parsed[0]["name"], "ROOT");
        assert_eq!(parsed[0]["filesystem"], "ext4");
    }

    #[test]
    fn partition_arg_parses_kind_as_sixth_segment() {
        // `KIND` 放在**末尾**，以免破坏既有的 5 段语法。
        let parsed = parse_partition_args(&[
            "64M/gpt:linux_filesystem/mbr:linux/L1/fat32/logical".to_string()
        ])
        .unwrap()
        .unwrap();
        assert_eq!(parsed[0]["size_bytes"], 64 * 1024 * 1024);
        assert_eq!(parsed[0]["mbr_type"], "mbr:linux");
        assert_eq!(parsed[0]["name"], "L1");
        assert_eq!(parsed[0]["filesystem"], "fat32");
        assert_eq!(parsed[0]["kind"], "logical");
    }

    #[test]
    fn partition_arg_omits_kind_when_absent() {
        // 缺省不发 `kind` 字段：老调用方的请求形状必须逐字节不变。
        let parsed = parse_partition_args(&["64M".to_string()]).unwrap().unwrap();
        assert!(
            parsed[0].get("kind").is_none(),
            "未指定归属时不应下发 kind，让后端用自己的默认值"
        );
        // 显式写 `primary` 则如实下发。
        let parsed = parse_partition_args(&["64M/////primary".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(parsed[0]["kind"], "primary");
    }

    #[test]
    fn partition_arg_accepts_kind_aliases() {
        for (text, expected) in [("logical", "logical"), ("l", "l"), ("primary", "primary")] {
            let parsed = parse_partition_args(&[format!("64M/////{text}")])
                .unwrap()
                .unwrap();
            assert_eq!(parsed[0]["kind"], expected, "输入 {text}");
        }
    }

    #[test]
    fn partition_arg_rejects_unknown_kind() {
        let err = parse_partition_args(&[
            "64M/gpt:microsoft_basic/mbr:fat32_lba/A/fat32/weird".to_string()
        ])
        .unwrap_err();
        assert!(err.contains("kind"), "错误信息应说明是归属字段：{err}");
        assert!(err.contains("#1"), "错误信息应带序号：{err}");
    }

    #[test]
    fn partition_arg_accepts_custom_guid_and_byte() {
        let parsed = parse_partition_args(&[
            "64M/gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0/mbr:0x1A/DATA/fat32".to_string(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed[0]["gpt_type"],
            "gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0"
        );
        assert_eq!(parsed[0]["mbr_type"], "mbr:0x1A");
        assert_eq!(parsed[0]["name"], "DATA");
    }

    #[test]
    fn partition_arg_supports_none_filesystem() {
        // `none` = 该分区不格式化。
        let parsed = parse_partition_args(&["64M///SCRATCH/none".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(parsed[0]["filesystem"], "none");
        assert_eq!(parsed[0]["name"], "SCRATCH");
    }

    #[test]
    fn partition_arg_allows_zero_for_fill_remaining() {
        let parsed = parse_partition_args(&["0".to_string()]).unwrap().unwrap();
        assert_eq!(parsed[0]["size_bytes"], 0);
    }

    #[test]
    fn partition_arg_name_may_not_contain_slash() {
        // `splitn(5)` 只切前四处；名称里若出现 `/`，它与其后内容一并落到第 5 段
        // （文件系统段），于是因文件系统非法而被**明确拒绝**——而不是静默
        // 截断名称。这是可接受的：`/` 是保留的分隔符。
        let err = parse_partition_args(&["64M/gpt:microsoft_basic/mbr:fat32_lba/A/B".to_string()])
            .unwrap_err();
        assert!(err.contains("cannot parse the filesystem"), "得到 {err}");
    }

    #[test]
    fn partition_arg_name_with_colon_is_fine() {
        // 冒号在名称里是安全的：分隔符是 `/`，而 `gpt:`/`mbr:` 前缀只在各自
        // 的字段内解析，两者不会互相干扰。
        let parsed =
            parse_partition_args(&["64M/gpt:microsoft_basic/mbr:fat32_lba/A:B".to_string()])
                .unwrap()
                .unwrap();
        assert_eq!(parsed[0]["name"], "A:B");
    }

    #[test]
    fn partition_arg_rejects_unknown_gpt_type() {
        let err = parse_partition_args(&["64M/gpt:nonsense".to_string()]).unwrap_err();
        assert!(err.contains("cannot parse the GPT type"), "得到 {err}");
    }

    #[test]
    fn partition_arg_rejects_unknown_mbr_type() {
        let err = parse_partition_args(&["64M/gpt:microsoft_basic/mbr:nonsense".to_string()])
            .unwrap_err();
        assert!(err.contains("cannot parse the MBR type"), "得到 {err}");
    }

    #[test]
    fn partition_arg_rejects_unprefixed_type() {
        // 不带前缀的名字一律拒绝——那正是过去两种布局混用的来源。
        let err = parse_partition_args(&["64M/linux".to_string()]).unwrap_err();
        assert!(err.contains("cannot parse the GPT type"), "得到 {err}");
    }

    #[test]
    fn partition_arg_rejects_unknown_filesystem() {
        let err = parse_partition_args(&["64M///X/nonsense".to_string()]).unwrap_err();
        assert!(err.contains("cannot parse the filesystem"), "得到 {err}");
    }

    #[test]
    fn partition_arg_rejects_bad_size_with_ordinal() {
        let err = parse_partition_args(&["64M".to_string(), "oops".to_string()]).unwrap_err();
        assert!(err.contains("#2"), "错误信息应指明是第几个：{err}");
    }

    #[test]
    fn partition_args_preserve_order() {
        let parsed =
            parse_partition_args(&["64M".to_string(), "128M".to_string(), "0".to_string()])
                .unwrap()
                .unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["size_bytes"], 64 * 1024 * 1024);
        assert_eq!(parsed[1]["size_bytes"], 128 * 1024 * 1024);
        assert_eq!(parsed[2]["size_bytes"], 0);
    }

    /// VID/PID 必须同时接受 `0x` 前缀与裸十六进制。
    ///
    /// 回归（AVD 实测）：最初把 `--vid` 声明为 `Option<u16>`，clap 只认十进制，
    /// `--vid 0x18d1` 直接报 `invalid digit found in string`——而 `0x` 前缀正是
    /// 用户写 VID 时最自然的形式（`lsusb` 也这么显示）。帮助文本还写着「可用
    /// `0x` 前缀」，即文档与实现对不上。
    #[test]
    fn usb_ids_accept_hex_with_and_without_prefix() {
        assert_eq!(parse_usb_id("0x18d1"), Some(0x18d1));
        assert_eq!(parse_usb_id("0X18D1"), Some(0x18d1));
        assert_eq!(parse_usb_id("18d1"), Some(0x18d1));
        // 裸串按十六进制解释：`4ee7` 是 20199，不是「4ee7 十进制」（那不存在）。
        assert_eq!(parse_usb_id("4ee7"), Some(0x4ee7));
        assert_eq!(parse_usb_id(" 0x0000 "), Some(0));
        assert_eq!(parse_usb_id("0xffff"), Some(0xffff));
    }

    #[test]
    fn usb_ids_reject_out_of_range_and_garbage() {
        // 超出 u16 必须被拒，而不是回绕。
        assert_eq!(parse_usb_id("0x10000"), None);
        // 裸串按十六进制解释，因此 "65536" 是 0x65536，超范围。
        assert_eq!(parse_usb_id("65536"), None);
        // 明确钉住「裸串不是十进制」：6353 十进制 = 0x18d1，但这里解析为 0x6353。
        // 这条断言的存在意义是防止有人日后「顺手」把裸串改成十进制解析——
        // 那会与 `0x` 前缀形式产生两种不一致的语义。
        assert_eq!(parse_usb_id("6353"), Some(0x6353));
        assert_ne!(parse_usb_id("6353"), Some(0x18d1));
        // 非法字符。
        assert_eq!(parse_usb_id("zzzz"), None);
        assert_eq!(parse_usb_id(""), None);
    }

    // ------------------------------------------------ loop 挂载前的上下文修正

    /// `attach-loop` **必须**在挂载 loop 之前修正镜像 SELinux 上下文。
    ///
    /// ## 为什么是源码断言
    ///
    /// `LoopMounts` 是具体类型（内部 `RealLoopControl` + `RealMounter`），主机上
    /// 没有 `/dev/loop-control`，因此「标签先于 `LOOP_SET_FD`」在主机上**不可观测**。
    /// 而顺序恰恰是唯一重要的事：内核在打开后备文件的那一刻按当时的标签 pin 住它，
    /// 之后修正对这次挂载无效。这与 `saving_identity_never_rebinds`、
    /// `gadgetdisk-gdd/tests/scope.rs` 用源码扫描钉住边界是同一个手法。
    #[test]
    fn attach_loop_fixes_the_context_before_touching_the_kernel() {
        let source = include_str!("main.rs");

        let start = source
            .find("fn run_attach_loop(")
            .expect("应能找到 run_attach_loop");
        let rest = &source[start..];
        let end = rest
            .find("\n/// 释放本地 loop 挂载")
            .expect("该函数后面应是 run_detach_loop");
        let body = &rest[..end];

        let check = body
            .find("image_context::check(")
            .expect("attach-loop 必须检查镜像上下文（真机实测 loop 同样受限制）");
        let attach = body
            .find("mounts.attach(")
            .expect("attach-loop 必须调用 mounts.attach");
        assert!(
            check < attach,
            "上下文修正必须早于 mounts.attach（其内部 LOOP_SET_FD 会 pin 住当时的标签）"
        );
        // 正例：警告要真的回到调用方，否则「改了标签」用户看不到（目录外文件只警告）。
        assert!(
            body.contains("\"warnings\": warnings"),
            "上下文警告必须随应答回报给调用方"
        );
    }
}
