//! configfs 路径常量与 UDC 探测。
//!
//! 规格见 [docs/android-integration.md](../../../../docs/android-integration.md) 的
//! 「configfs 路径常量」。

use std::path::{Path, PathBuf};

use crate::configfs::{ConfigFs, FsError, FsResult};

/// gadget 根目录的**兜底**默认值。
///
/// **不是**运行时唯一取值：真实设备上可能同时存在多个 gadget（实测红魔9 Pro
/// 上除 `g1` 外还有 vendor 私有的 `g2`）。挂载前必须经 [`discover`] 探测，
/// 本常量只在探测需要兜底时使用。
///
/// AOSP 依据：[system/core/rootdir/init.usb.configfs.rc][rc] 全文件**只**引用
/// `usb_gadget/g1` 与 `configs/b.1`（逐行核对，140 行）。
///
/// [rc]: https://android.googlesource.com/platform/system/core/+/refs/heads/main/rootdir/init.usb.configfs.rc
pub const DEFAULT_GADGET_DIR: &str = "g1";

/// gadget 根目录（兜底值，拼接 `usb_gadget/` 前缀）。
pub const GADGET_ROOT: &str = "/config/usb_gadget/g1";

/// 配置目录名的**兜底**默认值。
///
/// AOSP 的 `init.usb.configfs.rc` 里 adb/mtp/ptp/accessory/rndis 全部共用
/// `configs/b.1`，因此它是最可靠的兜底，但同样以 [`discover`] 的探测结果为准。
pub const DEFAULT_CONFIG_NAME: &str = "b.1";

/// 配置名（兜底值，保留旧名以兼容既有引用）。
pub const CONFIG_NAME: &str = DEFAULT_CONFIG_NAME;

/// mass_storage function 目录名。
///
/// 带模块后缀，便于在 `functions/` 里一眼认出属于谁，也避免与 Android 自带的
/// `mass_storage.0`（若存在）撞名。
pub const FUNCTION_NAME: &str = "mass_storage.gadget-disk";

/// 配置中的符号链接名。
///
/// **必须与 Android 的 `fN` 命名错开**：AOSP 的 `sys.usb.config=none` 动作只删
/// `configs/b.1/f1..f3`（见上引 rc），异名链接因此能在 Android 的 teardown 中
/// 幸存——这正是「清 UDC 后立刻建链接」这一缓解办法成立的前提（已由 AOSP
/// 源码确证，不再是推断）。
pub const LINK_NAME: &str = FUNCTION_NAME;

/// 已探测到的 configfs 布局。
///
/// 由 [`discover`] 产生，把「用哪个 gadget、哪个 config」从编译期常量变成
/// 运行期事实。所有依赖 config 名的路径都必须经它计算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// gadget 根的**绝对**路径，用于构造 `ConfigFs`。
    pub gadget_root: PathBuf,
    /// 配置目录名（如 `b.1`）。
    pub config_name: String,
    /// 为什么选这个 gadget（日志与诊断页用）。
    pub gadget_reason: GadgetChoice,
    /// 为什么选这个 config（日志与诊断页用）。
    pub config_reason: ConfigChoice,
}

/// gadget 的选择理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GadgetChoice {
    /// 该 gadget 当前已绑定到目标 UDC（最可信）。
    BoundToUdc,
    /// 未找到绑定的 gadget，退回到 AOSP 默认的 `g1`。
    DefaultG1,
    /// 只有一个 gadget，直接用。
    SoleGadget,
}

/// config 目录的选择理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigChoice {
    /// 该配置目录里有符号链接 → Android 正在用它。
    HasLinks,
    /// 退回到 AOSP 默认的 `b.1`。
    DefaultB1,
    /// 只有一个配置目录，直接用。
    SoleConfig,
    /// 一个配置目录都没有，将由挂载流程创建 `b.1`。
    ToCreate,
}

impl Layout {
    /// AOSP 兜底布局（`g1` + `b.1`）。
    ///
    /// 只用于既有测试与「探测尚未接入」的调用点；生产路径必须传入
    /// [`discover`](crate::discover::discover) 的结果。
    pub fn aosp_default() -> Self {
        Self {
            gadget_root: PathBuf::from(GADGET_ROOT),
            config_name: DEFAULT_CONFIG_NAME.to_string(),
            gadget_reason: GadgetChoice::DefaultG1,
            config_reason: ConfigChoice::DefaultB1,
        }
    }

    /// 配置目录相对路径。
    pub fn config_path(&self) -> String {
        format!("configs/{}", self.config_name)
    }

    /// 配置中的符号链接相对路径。
    pub fn link_path(&self) -> String {
        format!("{}/{}", self.config_path(), LINK_NAME)
    }

    /// 供日志/诊断展示的一句话描述。
    pub fn describe(&self) -> String {
        format!(
            "{} (gadget={:?}, config={}={:?})",
            self.gadget_root.display(),
            self.gadget_reason,
            self.config_name,
            self.config_reason
        )
    }
}

/// function 相对 gadget 根的路径。
pub fn function_path() -> String {
    format!("functions/{FUNCTION_NAME}")
}

/// 第 `index` 个 LUN 的相对路径。
pub fn lun_path(index: u8) -> String {
    format!("{}/lun.{index}", function_path())
}

/// 第 `index` 个 LUN 的某个属性。
pub fn lun_attr(index: u8, attr: &str) -> String {
    format!("{}/{attr}", lun_path(index))
}

/// `UDC` 属性相对路径。
pub const UDC_ATTR: &str = "UDC";

/// LUN 目录数上限。
///
/// 内核的 `FSG_MAX_LUNS` 是 `US_BULK_MAX_LUN_LIMIT + 1`（见
/// `drivers/usb/gadget/function/storage_common.h`），且 `fsg_lun_make` 对
/// `num >= FSG_MAX_LUNS` 返回 `ERANGE`。这里取 **8** 作为我们自设的上限：
/// 它是 MSG 传统上的 LUN 上限，也远多于实际会用到的数量。真正的内核上限若更低，
/// 创建时内核会以自己的错误码拒绝，我们如实上报。
pub const MAX_LUNS: u8 = 8;

/// `forced_eject` 属性名（write-only）。
///
/// 写入任意非零字节即让内核**强制**解绑该 LUN 的后端文件——它会先清掉
/// `prevent_medium_removal` 再调 `fsg_store_file(..., "")`，因此即使主机
/// 之前锁了介质（SCSI `PREVENT_ALLOW_MEDIUM_REMOVAL`）也能弹出。
/// 见 `drivers/usb/gadget/function/storage_common.c`。
pub const LUN_ATTR_FORCED_EJECT: &str = "forced_eject";

/// `inquiry_string` 属性名。
///
/// 该字符串回给主机的 SCSI INQUIRY（VID/PRODUCT/REV 之外的厂商与产品字段）。
/// 内核用 `snprintf(..., "%-28s", buf)` 写入定长缓冲，故长度上限为
/// [`INQUIRY_STRING_MAX`]。
pub const LUN_ATTR_INQUIRY_STRING: &str = "inquiry_string";

/// `inquiry_string` 的长度上限。
///
/// 依据（源码）：`storage_common.c` 的 `fsg_store_inquiry_string` 用
/// `snprintf(curlun->inquiry_string, sizeof(curlun->inquiry_string), "%-28s", buf)`，
/// 而 `INQUIRY_STRING_LEN` 是 `8 + 16 + 4 + 1`。超长会被**静默截断**，
/// 因此我们在写入前自行拒绝，而不是让用户以为设置生效了。
pub const INQUIRY_STRING_MAX: usize = 28;

/// gadget 字符串描述符的长度上限，单位是 **UTF-8 字节**。
///
/// 依据（源码）：`drivers/usb/gadget/configfs.c` 的 `usb_string_copy` 在
/// `strlen(s) > USB_MAX_STRING_LEN` 时返回 `-EOVERFLOW`。`strlen` 数的是
/// **字节**，因此中文一个字（3 字节）算 3 而不是 1。
///
/// 名字里带 `BYTES` 是刻意的：M10 曾按**字符数**校验并附带「只允许可打印
/// ASCII」，两者都是错的——AVD 实测 42 个汉字（126 字节）写入成功、43 个
/// （129 字节）返回 `rc=1`，而 ASCII 限制则让中文产品名根本无法设置。
///
/// 上界为何也足够：`composite.c` 的 `get_string` 把 `strlen` 截到 126 后交给
/// `utf8s_to_utf16s`，UTF-16 单元数不会超过字节数，故描述符长度
/// `(n + 1) * 2 <= 254` 不会溢出 `bLength`（u8）。
pub const STRING_MAX_BYTES: usize = 126;

/// 需要备份/还原的 gadget 顶层属性（字符串型）。
///
/// `idVendor`、`idProduct`、`bcdUSB`、`bDeviceClass`、`bDeviceSubClass`、
/// `bDeviceProtocol`、`bcdDevice` 以及三个字符串描述符。
pub const BACKUP_ATTRS: &[&str] = &[
    "idVendor",
    "idProduct",
    "bcdUSB",
    "bDeviceClass",
    "bDeviceSubClass",
    "bDeviceProtocol",
    "bcdDevice",
];

/// 字符串描述符相对路径（`strings/<lang>/<name>`）。
pub const STRING_LANG: &str = "0x409";

/// 需要备份/还原的字符串描述符。
pub const STRING_ATTRS: &[&str] = &["manufacturer", "product", "serialnumber"];

/// LUN 的 `inquiry_string` 属性相对路径。
pub fn lun_inquiry_path(index: u8) -> String {
    lun_attr(index, LUN_ATTR_INQUIRY_STRING)
}

/// LUN 的 `forced_eject` 属性相对路径。
pub fn lun_forced_eject_path(index: u8) -> String {
    lun_attr(index, LUN_ATTR_FORCED_EJECT)
}

/// 字符串描述符的相对路径。
pub fn string_attr(name: &str) -> String {
    format!("strings/{STRING_LANG}/{name}")
}

/// UDC 名探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Udc {
    /// 找到了可用的 UDC。
    Available(String),
    /// 没有可用 UDC（`sys.usb.controller` 为空或不存在）。
    None,
}

impl Udc {
    /// UDC 名；无可用时为 `None`。
    pub fn name(&self) -> Option<&str> {
        match self {
            Udc::Available(name) => Some(name),
            Udc::None => None,
        }
    }

    /// 是否可用。
    pub fn is_available(&self) -> bool {
        matches!(self, Udc::Available(_))
    }
}

/// 从系统属性读取 UDC 名。
///
/// 规格：UDC 控制器来源为系统属性 `sys.usb.controller`。
/// **不依赖 `getprop` 可执行文件存在**（模块包自包含约束），
/// 直接经 `__system_property_get` 读取。
pub fn read_udc_name() -> Udc {
    match system_property("sys.usb.controller") {
        Some(name) if !name.is_empty() => Udc::Available(name),
        _ => Udc::None,
    }
}

/// 读取 Android 系统属性。
///
/// 返回 `None` 表示属性不存在（或读取失败）。空字符串与不存在在此处区分：
/// 前者返回 `Some("")`。
#[cfg(target_os = "android")]
pub fn system_property(key: &str) -> Option<String> {
    /// Android bionic 的属性读取上限（`PROP_VALUE_MAX`）。
    const PROP_VALUE_MAX: usize = 92;

    let Ok(c_key) = std::ffi::CString::new(key) else {
        return None;
    };

    let mut buffer = [0u8; PROP_VALUE_MAX];
    // SAFETY: __system_property_get 写入我们提供的缓冲区，长度由常量保证。
    let len = unsafe {
        libc::__system_property_get(c_key.as_ptr(), buffer.as_mut_ptr().cast::<libc::c_char>())
    };
    if len <= 0 {
        // 属性不存在。
        return None;
    }

    let len = (len as usize).min(PROP_VALUE_MAX - 1);
    Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

/// 非 Android 平台没有系统属性。
///
/// 返回 `None` 使上层走 `no_udc` 分支，而不是假装有控制器。
#[cfg(not(target_os = "android"))]
pub fn system_property(_key: &str) -> Option<String> {
    None
}

/// UDC 状态目录的父路径（sysfs，**不在** configfs 里）。
pub const UDC_CLASS_DIR: &str = "/sys/class/udc";

/// UDC 的 `state` 文件路径。
pub fn udc_state_path(udc: &str) -> PathBuf {
    Path::new(UDC_CLASS_DIR).join(udc).join("state")
}

/// 读取 UDC 的 `state`（真实 sysfs 路径）。
///
/// 返回 `Ok(state)`；读不到（内核无该文件、或名字不再存在）返回 `Err`，
/// 由调用方决定如何降级。**不要**用 `Option` 吞掉原因：`effective` 的语义
/// 正是「能否确认已生效」，读不到就该如实说不能。
pub fn read_udc_state(udc: &str) -> FsResult<String> {
    read_udc_state_in(Path::new(UDC_CLASS_DIR), udc)
}

/// 在指定的 UDC class 目录下读取 `state`。
///
/// 抽出基目录是为了让主机测试能指向临时目录——`/sys` 在开发机上是只读的，
/// 否则「`effective` 是否正确反映 `state`」这条最该被钉住的语义就无法测试。
pub fn read_udc_state_in(class_dir: &Path, udc: &str) -> FsResult<String> {
    let path = class_dir.join(udc).join("state");
    std::fs::read_to_string(&path)
        .map(|value| value.trim().to_string())
        .map_err(|source| FsError::Io { path, source })
}

/// 该 `state` 是否表示「主机已接受配置」。
///
/// 内核 UDC 的 `state` 取值形如 `not attached` / `addressed` / `configured` /
/// `suspended`。只有 `configured` 说明主机真的 SET_CONFIGURATION 成功——
/// 真机实测中「Windows 报代码 10 / 指定不存在的设备」时它恰好停在 `addressed`，
/// 而旧实现只看 `lun.0/file` 非空就报 `effective: true`，属于谎报成功。
pub fn state_means_configured(state: &str) -> bool {
    state.trim().eq_ignore_ascii_case("configured")
}

/// 从 `ConfigFs` 读取当前 UDC 绑定值（空串表示未绑定）。
pub fn read_bound_udc(fs: &impl ConfigFs) -> FsResult<String> {
    fs.read(UDC_ATTR)
}

/// 校验路径真的是 configfs。
///
/// 这是**写前必做**的检查：若 `/config` 未挂载为 configfs，
/// 写入会破坏普通文件系统的内容。
pub fn verify_configfs(path: &Path) -> FsResult<()> {
    // `rustix::fs::statfs` 返回 Result，不需要裸 libc 调用或手工零初始化。
    let stat = rustix::fs::statfs(path).map_err(|err| FsError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from(err),
    })?;

    // `f_type` 是**平台相关**的 `FsWord`：Linux（`linux_raw` 后端）是 `c_long`，
    // Android（`libc` 后端）是 `u64`，部分平台是 `u32`。因此既不能写
    // `i64::from`（Android 上 `u64 -> i64` 无此 impl），也不能写
    // `try_into`（宿主上 `i64 -> i64` 会被 clippy 判为 useless_conversion），
    // 更不能写 `as`（触发本仓库已设为 deny 的 `cast_sign_loss`）。
    // 统一经 `i128` 中转：`i64`/`u64`/`u32` 三种形态都能无损转入，且三目标均零告警。
    let magic = i128::from(stat.f_type);
    if magic != i128::from(crate::configfs::CONFIGFS_MAGIC) {
        return Err(FsError::NotConfigFs {
            path: path.to_path_buf(),
            // 仅用于诊断输出；`f_type` 的实际取值远在 i64 范围内。
            magic: i64::try_from(magic).unwrap_or(i64::MIN),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configfs::MemConfigFs;

    #[test]
    fn lun_attr_helpers_cover_inquiry_and_forced_eject() {
        assert_eq!(
            lun_inquiry_path(1),
            "functions/mass_storage.gadget-disk/lun.1/inquiry_string"
        );
        assert_eq!(
            lun_forced_eject_path(2),
            "functions/mass_storage.gadget-disk/lun.2/forced_eject"
        );
    }

    #[test]
    fn caps_match_kernel_source_limits() {
        // `fsg_store_inquiry_string` 用 "%-28s" 定宽；超长会被内核静默截断。
        assert_eq!(INQUIRY_STRING_MAX, 28);
        // `usb_string_copy` 在 > USB_MAX_STRING_LEN(126) 时返回 EOVERFLOW。
        // 注意单位是**字节**（内核用 strlen），不是字符数。
        assert_eq!(STRING_MAX_BYTES, 126);
        assert_eq!(MAX_LUNS, 8);
    }

    #[test]
    fn paths_match_documented_constants() {
        assert_eq!(function_path(), "functions/mass_storage.gadget-disk");
        assert_eq!(lun_path(0), "functions/mass_storage.gadget-disk/lun.0");
        assert_eq!(
            lun_attr(0, "cdrom"),
            "functions/mass_storage.gadget-disk/lun.0/cdrom"
        );
        assert_eq!(string_attr("product"), "strings/0x409/product");
        // 链接名必须与 function 名一致，且**不得**是 Android 的 `fN`。
        assert_eq!(LINK_NAME, FUNCTION_NAME);
        assert!(!LINK_NAME.starts_with('f') || LINK_NAME.len() > 3);
    }

    /// 链接名绝不能落进 AOSP 的 `fN` 命名空间。
    ///
    /// 回归：AOSP 的 `sys.usb.config=none` 动作只删 `configs/b.1/f1..f3`
    /// （见 `init.usb.configfs.rc`）。我们的链接一旦叫 `f1`，Android 的
    /// teardown 就会连它一起删掉，「清 UDC 后立刻建链接」的缓解办法随即失效。
    #[test]
    fn link_name_never_collides_with_aosp_fn_links() {
        for n in 1..=9 {
            assert_ne!(LINK_NAME, format!("f{n}"), "链接名不得与 AOSP 的 fN 撞名");
        }
    }

    #[test]
    fn layout_derives_paths_from_discovered_config_name() {
        let layout = Layout {
            gadget_root: PathBuf::from("/config/usb_gadget/g2"),
            config_name: "b.2".into(),
            gadget_reason: GadgetChoice::BoundToUdc,
            config_reason: ConfigChoice::HasLinks,
        };
        assert_eq!(layout.config_path(), "configs/b.2");
        assert_eq!(layout.link_path(), "configs/b.2/mass_storage.gadget-disk");
        assert!(layout.describe().contains("g2"));
        assert!(layout.describe().contains("b.2"));
    }

    #[test]
    fn udc_helpers_distinguish_available_from_none() {
        let available = Udc::Available("dummy_udc.0".into());
        assert!(available.is_available());
        assert_eq!(available.name(), Some("dummy_udc.0"));

        let none = Udc::None;
        assert!(!none.is_available());
        assert_eq!(none.name(), None);
    }

    #[test]
    fn read_bound_udc_returns_empty_when_unbound() {
        let mut fs = MemConfigFs::new();
        fs.add_file(UDC_ATTR);
        // 未绑定时内核返回空串（或含换行）。
        assert_eq!(read_bound_udc(&fs).unwrap(), "");

        // 绑定后读回名字。
        fs.preset("__bind", "");
        fs.write(UDC_ATTR, "dummy_udc.0").ok();
        // MemConfigFs 中 UDC 是普通属性，写入即生效。
        assert_eq!(read_bound_udc(&fs).unwrap(), "dummy_udc.0");
    }

    #[test]
    fn backup_attr_lists_are_complete() {
        // 文档要求备份这 7 个 gadget 属性与 3 个字符串描述符。
        assert_eq!(BACKUP_ATTRS.len(), 7);
        assert!(BACKUP_ATTRS.contains(&"idVendor"));
        assert!(BACKUP_ATTRS.contains(&"bcdDevice"));
        assert_eq!(STRING_ATTRS.len(), 3);
        assert!(STRING_ATTRS.contains(&"serialnumber"));
    }

    #[test]
    fn verify_configfs_rejects_non_configfs_path() {
        // /tmp 是普通文件系统，必须被拒绝（否则会写坏它）。
        let err = verify_configfs(Path::new("/tmp")).unwrap_err();
        match err {
            FsError::NotConfigFs { magic, .. } => {
                assert_ne!(magic, crate::configfs::CONFIGFS_MAGIC);
            }
            other => panic!("期望 NotConfigFs，得到 {other:?}"),
        }
    }

    #[test]
    fn verify_configfs_reports_missing_path() {
        let err = verify_configfs(Path::new("/nonexistent-xyz")).unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
    }

    #[test]
    fn non_android_has_no_system_property() {
        // 主机上没有 Android 属性系统；必须返回 None 而不是编造值。
        #[cfg(not(target_os = "android"))]
        assert_eq!(system_property("sys.usb.controller"), None);
    }
}
