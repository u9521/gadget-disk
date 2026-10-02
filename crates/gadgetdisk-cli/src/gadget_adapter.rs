//! CLI 侧的 gadget 接线：**只读**的导出视图 + **身份**读写。
//!
//! ## 为什么 CLI 也需要接触 configfs
//!
//! 三件事必须在 CLI 侧做，且都**不能**经过 `gdd`：
//!
//! 1. **判断某个镜像是否正被导出**（删除/导入/本地挂载前必须拦住它）。
//!    这是只读的，任何进程都能读 configfs 真值；经过 `gdd` 反而多一层失败点。
//! 2. **写身份**（`idVendor`/`idProduct`/`strings`/`os_desc`）。`gdd` 的职责被
//!    严格限定为 mass_storage（见 [`gadgetdisk_gdd`] 的边界说明），身份归 CLI。
//! 3. **备份/还原 Android 的原始身份**。身份是 CLI 改的，还原自然也归 CLI。
//!
//! ## 为什么用只读 trait
//!
//! 本模块只实现 [`GadgetView`]（`udc`/`luns`/`is_mounted`/`mounted_images`），
//! **不**实现 `MassStorageOps`。于是 CLI 在类型层面就不可能绕过 `gdd` 去改
//! LUN——「谁是 configfs 的唯一写入者」这条边界由编译器守住，而不是靠约定。
//!
//! 唯一的例外是 [`apply_identity`] 与 [`restore_identity`]：它们写的是身份
//! 属性（`idVendor` 等），与 LUN 无关，且 `gdd` 明确不碰这些。

use std::path::{Path, PathBuf};

use gadgetdisk_gdd::DataDirs;
use gadgetdisk_gdd::kernel::{GadgetView, KernelError};
use gadgetdisk_gdd::usb_adapter::discover_layout;
use gadgetdisk_proto::{ErrorCode, LunInfo};
use gadgetdisk_usb::configfs::RealConfigFs;
use gadgetdisk_usb::error::{GadgetError, GadgetResult};
use gadgetdisk_usb::identity::{Identity, IdentityBackup};
use gadgetdisk_usb::mass_storage::{LunState, MassStorage};

/// CLI 侧的只读 gadget 视图。
pub struct GadgetReader {
    storage: MassStorage<RealConfigFs>,
}

/// CLI 侧的 configfs 句柄（身份读写用）。
pub struct IdentityEditor {
    fs: RealConfigFs,
    config_dir: String,
}

/// 打开 configfs（探测布局 + 校验真的是 configfs）。
///
/// 返回 `None` 而不是错误：调用方（`serve`/`status`）在无 configfs 的环境里
/// 应当能降级为「未挂载」而不是整体失败——主机上跑测试就是这种情况。
fn open() -> Option<(RealConfigFs, gadgetdisk_usb::Layout)> {
    let layout = discover_layout().ok()?;
    gadgetdisk_usb::ensure_configfs(&layout.gadget_root).ok()?;
    let fs = RealConfigFs::new(&layout.gadget_root);
    Some((fs, layout))
}

impl GadgetReader {
    /// 尝试打开只读视图；无可用 configfs 时返回 `None`。
    pub fn open() -> Option<Self> {
        let (fs, layout) = open()?;
        Some(Self {
            storage: MassStorage::new(fs).with_layout(layout),
        })
    }

    /// 内部编排器（诊断用）。
    pub fn storage(&self) -> &MassStorage<RealConfigFs> {
        &self.storage
    }
}

/// 把编排层读回的 LUN 转成协议层（容量现场 `stat`——CLI 是短命进程，
/// 没有跨请求缓存可依赖）。
fn to_info(lun: LunState) -> LunInfo {
    let size = if lun.image_path.is_empty() {
        0
    } else {
        std::fs::metadata(&lun.image_path)
            .map(|m| m.len())
            .unwrap_or(0)
    };
    LunInfo {
        index: lun.index,
        size_bytes: size,
        image_path: lun.image_path,
        mode: lun.mode,
        inquiry_string: lun.inquiry_string,
        attached: lun.attached,
        effective: lun.effective,
        deletable: lun.deletable,
    }
}

impl GadgetView for GadgetReader {
    fn udc(&self) -> Option<String> {
        self.storage.detect_udc().ok()
    }

    fn luns(&self) -> Vec<LunInfo> {
        self.storage.luns().into_iter().map(to_info).collect()
    }

    fn is_mounted(&self, image: &Path) -> bool {
        let target = image.to_string_lossy();
        self.storage
            .luns()
            .iter()
            .any(|lun| lun.attached && lun.image_path == target)
    }

    fn mounted_images(&self) -> Vec<PathBuf> {
        self.storage
            .luns()
            .into_iter()
            .filter(|lun| lun.attached && !lun.image_path.is_empty())
            .map(|lun| PathBuf::from(lun.image_path))
            .collect()
    }
}

/// 读取当前导出状态，读不到时返回空（**不**伪造）。
///
/// 无 configfs（主机、容器、`/config` 未挂载）时返回 `(None, [])`：这如实表示
/// 「不知道有没有导出」，调用方据此走降级路径，而不是假装「没有导出」。
pub fn read_export_state() -> (Option<String>, Vec<LunInfo>) {
    match GadgetReader::open() {
        Some(reader) => (reader.udc(), reader.luns()),
        None => (None, Vec::new()),
    }
}

/// 当前是否有镜像正被导出。
///
/// 这是判断「有没有导出」的**正确**方式：看有没有 LUN 绑着后端文件（内核真值）。
///
/// 千万不要用 `udc().is_some()` 代替——`detect_udc()` 在没有绑定时会退回到
/// 系统属性 `sys.usb.controller`，于是「设备上有可用的控制器」会被误读成
/// 「正在导出」。AVD 实测过这个 bug（见 `MassStorage::rebind` 的注释）。
///
/// > M11 起，判断「是否已绑定」的需求只出现在手动重绑路径里，由 `gdd` 自己用
/// > `MassStorage::bound_udc()` 完成。CLI 侧不再需要这个判据——身份保存与挂载
/// > 都已和 UDC 解耦，见 [`crate::serve::save_and_apply_identity`]。
///
/// 用于「删除/导入/本地挂载前必须拦住」的检查。
pub fn any_exported() -> bool {
    match GadgetReader::open() {
        Some(reader) => !reader.mounted_images().is_empty(),
        // 读不到 configfs 时保守返回 `false`：`gdd` 才是唯一写入者，它若不在
        // 导出状态，configfs 也不该有我们的 LUN。真正的保护在 `gdd` 侧的
        // 「同一镜像不得出现在两个 LUN」以及 CLI 侧的路径校验。
        None => false,
    }
}

impl IdentityEditor {
    /// 尝试打开身份编辑器；无可用 configfs 时返回 `None`。
    pub fn open() -> Option<Self> {
        let (fs, layout) = open()?;
        Some(Self {
            fs,
            config_dir: layout.config_path(),
        })
    }

    /// 读取当前（内核里的）身份。
    pub fn read(&self) -> Identity {
        Identity::read_current(&self.fs)
    }

    /// 应用身份并逐项读回核实。
    pub fn apply(&mut self, identity: &Identity) -> GadgetResult<Vec<String>> {
        identity.apply(&mut self.fs)
    }

    /// 备份当前身份（**仅在还没有备份时**）。
    ///
    /// ## 为什么「已有备份时保留」
    ///
    /// 已存在的那份记录的是 **Android 的原始身份**。中途再捕获会读到**我们自己
    /// 写进去的值**，把它当成「原始值」——还原后手机就会带着我们的 VID/产品名
    /// 继续跑 MTP。因此首次捕获之后就不再覆盖。
    ///
    /// ## 为什么允许「我们已经在 configfs 里」时捕获
    ///
    /// 曾经这里还要求「我们的 function 与链接都不存在」才捕获，理由是怕把
    /// 我们造成的状态记成原始状态。但那会让**最常见**的路径漏掉备份：
    /// 用户先挂载（此时身份还是 Android 的）、再改身份——改身份这一刻我们的
    /// function 已经在 configfs 里了，于是捕获被跳过，身份再也还原不回去。
    /// AVD 实测过这个组合：卸载后 `idVendor` 仍是我们设的值。
    ///
    /// 真正需要排除的只有**链接**那一项（见 `IdentityBackup::capture`：它跳过
    /// 我们自己的链接名与任何指向我们 function 的链接）。身份属性本身不因我们的
    /// 存在而失真——只要捕获发生在 `apply` **之前**，读到的就还是 Android 的值，
    /// 而所有调用点都保证了这个顺序。
    pub fn capture_backup_if_absent(&self, dirs: &DataDirs) -> std::io::Result<bool> {
        if crate::cli_paths::identity_backup(dirs).exists() {
            return Ok(false);
        }
        let backup = IdentityBackup::capture(&self.fs, &self.config_dir);
        if backup.is_empty() {
            return Ok(false);
        }
        backup.store(&dirs.run())?;
        Ok(true)
    }

    /// 还原备份并删除备份文件。
    ///
    /// 返回未能完成的项（空表示全部成功）。
    pub fn restore_backup(&mut self, dirs: &DataDirs) -> Vec<String> {
        let Some(backup) = IdentityBackup::load(&dirs.run()) else {
            return Vec::new();
        };
        let failures = backup.restore(&mut self.fs, &self.config_dir);
        // 只有全部成功才删备份：失败时保留它，下次还能再试。
        if failures.is_empty() {
            let _ = gadgetdisk_usb::jsonfile::remove_if_exists(&crate::cli_paths::identity_backup(
                dirs,
            ));
        }
        failures
    }
}

/// 把 `GadgetError` 映射为协议错误码（CLI 侧用）。
pub fn map_gadget_error(err: &GadgetError) -> KernelError {
    let (code, message) = match err {
        GadgetError::NoUdc => (
            ErrorCode::NoUdc,
            "no USB controller available (sys.usb.controller is empty)".to_string(),
        ),
        GadgetError::InvalidArgument(msg) => (ErrorCode::InvalidArgument, msg.clone()),
        GadgetError::ConfigfsUnavailable(msg) => (
            ErrorCode::ConfigfsUnavailable,
            format!("cannot determine a usable USB gadget: {msg}"),
        ),
        GadgetError::Config(msg) => (
            ErrorCode::Internal,
            format!("config did not take effect: {msg}"),
        ),
        other => (ErrorCode::Internal, other.to_string()),
    };
    KernelError::new(code, message)
}

/// 读取 `run/state.json` 里的导出意图。
pub fn load_intent(dirs: &DataDirs) -> Option<ExportIntent> {
    let text = std::fs::read_to_string(crate::cli_paths::state_json(dirs)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写入导出意图（**只写非空**；空意图意味着删除文件）。
pub fn save_intent(dirs: &DataDirs, intent: &ExportIntent) -> std::io::Result<()> {
    if intent.luns.is_empty() {
        return gadgetdisk_usb::jsonfile::remove_if_exists(&crate::cli_paths::state_json(dirs));
    }
    let text = serde_json::to_string_pretty(intent)?;
    gadgetdisk_usb::jsonfile::write_json_atomic(
        &crate::cli_paths::state_json(dirs),
        text.as_bytes(),
    )
}

/// 删除导出意图。
pub fn clear_intent(dirs: &DataDirs) -> std::io::Result<()> {
    gadgetdisk_usb::jsonfile::remove_if_exists(&crate::cli_paths::state_json(dirs))
}

/// 从内核真值构造导出意图（**唯一**的构造方式）。
///
/// 刻意只接受 `Vec<LunInfo>`：意图必须反映「我们刚刚造成了什么」，而不是
/// 「我们打算做什么」。两者不一致时，内核真值才是可信的那份——这也是为什么
/// `gdd` 的挂载/卸载应答里都带回操作后的 `devices`。
pub fn intent_from_luns(luns: &[LunInfo]) -> ExportIntent {
    ExportIntent {
        version: 1,
        luns: luns
            .iter()
            .filter(|lun| lun.attached && !lun.image_path.is_empty())
            .map(|lun| IntentLun {
                index: lun.index,
                image_path: lun.image_path.clone(),
                mode: lun.mode.as_str().to_string(),
                inquiry_string: lun.inquiry_string.clone(),
            })
            .collect(),
    }
}

/// `run/state.json` 的内容：**导出意图**（重启后要恢复成什么样）。
///
/// ## 这是「意图」而不是「事实」
///
/// 事实永远是 configfs 里的内核真值。本文件回答的是另一个问题：
/// **重启之后我们应该把什么重新挂出去**。因此它只在 CLI 成功完成一次挂载后
/// 写入，并且只记重启恢复所需的字段（不含容量、不含 UDC）。
///
/// 它**不**记录「当前挂载了什么」——那由 `status` 现读内核。两者可能短暂不
/// 一致（例如设备刚被拔出、gdd 已清理但 CLI 还没被调用），此时 CLI 的
/// `status` 会通过 `pending_intent` 如实指出这一点，而不是静默改写意图。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExportIntent {
    /// schema 版本。
    #[serde(default = "default_version")]
    pub version: u32,
    /// 要恢复的 LUN 列表。
    #[serde(default)]
    pub luns: Vec<IntentLun>,
}

fn default_version() -> u32 {
    1
}

/// 意图里的单个 LUN。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntentLun {
    /// LUN 序号。
    pub index: u8,
    /// 镜像路径。
    pub image_path: String,
    /// 设备模式（`rw`/`ro`/`cdrom`）。
    pub mode: String,
    /// INQUIRY 字符串。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inquiry_string: Option<String>,
}

impl ExportIntent {
    /// 是否为空（没有要恢复的东西）。
    pub fn is_empty(&self) -> bool {
        self.luns.is_empty()
    }
}

/// 模式字符串 → 协议枚举。
pub fn parse_mode(value: &str) -> gadgetdisk_proto::Mode {
    match value.to_ascii_lowercase().as_str() {
        "ro" => gadgetdisk_proto::Mode::Ro,
        "cdrom" => gadgetdisk_proto::Mode::Cdrom,
        _ => gadgetdisk_proto::Mode::Rw,
    }
}
