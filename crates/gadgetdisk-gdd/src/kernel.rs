//! `gdd` 与内核操作之间的边界（seam trait）。
//!
//! ## 为什么抽象在这里
//!
//! `gadgetdisk-usb` 已把**内核访问**抽象为 [`ConfigFs`](gadgetdisk_usb::ConfigFs)，
//! 使「操作顺序」这类最高风险的逻辑能在主机上断言。
//!
//! 本模块抽象的是**另一个层次**：`gdd` 作为执行者，只关心
//! 「把哪些镜像挂成哪个 LUN」这类语义操作，而不关心 configfs 路径细节。
//! 具体实现见 [`crate::usb_adapter`]。
//!
//! ## 两个 trait：可写与只读，按**谁持有**分开
//!
//! | trait | 谁能用 | 能做什么 |
//! |---|---|---|
//! | [`GadgetView`] | `gdd` 与 CLI | **只读** configfs 真值（UDC、LUN、某镜像是否在导出） |
//! | [`MassStorageOps`] | **仅 `gdd`** | 挂载/卸载/拆除/重绑 |
//!
//! 这个切分沿用 [`LoopOps`] 只读的既有先例：CLI 需要判断「这个镜像是否正被导出」
//! 才能在删除/导入/本地挂载前拦住它，但它不该有改 gadget 的能力——否则就绕过了
//! `gdd` 这道唯一写入者。类型层面挡住比约定挡住更可靠。
//!
//! 这样 `gadgetdisk-usb` 与 `gadgetdisk-loop` **不需要依赖 `gdd`**，
//! 避免循环依赖；同时编排逻辑可以用内存替身完整测试。

use std::path::Path;

use gadgetdisk_proto::{Attachment, Capabilities, ErrorCode, LunInfo, MountDevice};

/// 内核操作失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", .code.as_str())]
pub struct KernelError {
    /// 稳定错误码（直接回给客户端）。
    pub code: ErrorCode,
    /// 人类可读说明。
    pub message: String,
}

impl KernelError {
    /// 以错误码与说明构造。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 该能力在当前内核上不可用。
    pub fn unsupported(what: &str) -> Self {
        Self::new(
            ErrorCode::Internal,
            format!("{what}: kernel support is not wired up yet"),
        )
    }
}

/// 本 crate 的 kernel 结果类型。
pub type KernelResult<T> = std::result::Result<T, KernelError>;

/// gadget 的**只读**视图。
///
/// 让调用方判断「有没有在导出、导出的是谁」，而不给它改的能力。
pub trait GadgetView {
    /// 当前 UDC 名；无可用控制器时为 `None`。
    fn udc(&self) -> Option<String>;

    /// 当前各 LUN 状态（读 configfs 真值）。
    fn luns(&self) -> Vec<LunInfo>;

    /// 该镜像当前是否作为某个 LUN 的后端。
    fn is_mounted(&self, image: &Path) -> bool;

    /// 当前占用的全部 LUN 镜像路径。
    fn mounted_images(&self) -> Vec<std::path::PathBuf>;
}

/// mass_storage 的**可写**操作 —— **仅 `gdd` 持有**。
pub trait MassStorageOps: GadgetView {
    /// 挂载/更新一批 LUN。返回读回的内核真值。
    ///
    /// 已存在的 LUN 就地更新（强制弹出该 LUN，**不动 UDC**）；新增 LUN 会走
    /// 一次「断 UDC → 建目录与链接 → 绑回」的紧凑段（内核 `fsg_lun_make`
    /// 在 gadget 已绑定时返回 `EBUSY`）。
    ///
    /// `force_rebind` 让本次即使没有结构性改动也走那个紧凑段——这是身份改动
    /// （`idVendor`/字符串）生效的唯一途径，因为它们只在下次 bind 时被主机看到。
    fn mount(&mut self, devices: &[MountDevice], force_rebind: bool) -> KernelResult<Vec<LunInfo>>;

    /// 弹出**一个** LUN 的介质，保留 LUN 目录、链接与其余 LUN。
    fn unmount_lun(&mut self, lun: u8) -> KernelResult<Vec<LunInfo>>;

    /// 弹出全部 LUN 的介质，仍保留 function 与链接。
    fn eject_all(&mut self) -> KernelResult<Vec<LunInfo>>;

    /// 删除一个**空闲槽位**（弹出介质 → `rmdir lun.N` → 重建链接并重绑 UDC）。
    ///
    /// `lun.0` 不可删（内核 `EPERM`，只能弹出）。
    fn delete_slot(&mut self, lun: u8) -> KernelResult<Vec<LunInfo>>;

    /// 拆除我们自己的全部痕迹（断 UDC → 清 file → 删链接 → 删 function）。
    ///
    /// 返回未能完成的清理项（空表示全部成功）。
    fn teardown(&mut self) -> Vec<String>;

    /// 重新绑定 UDC，让身份等改动生效。
    fn rebind(&mut self) -> KernelResult<String>;

    /// 设备是否已被主机弹出（我们的痕迹还在、但**全部** LUN 的后端都已解绑）。
    ///
    /// **单个** LUN 为空不算弹出——那是正常的按 LUN 卸载。
    fn is_ejected(&self) -> bool;

    /// 弹出后的收尾：删我们的链接与 function，**不解绑 UDC**。
    ///
    /// 不解绑的理由：一旦解绑，Android 的 `init` 会立刻按 `sys.usb.config`
    /// 重装它自己的配置，把状态搅乱。
    ///
    /// 返回未能完成的清理项（空表示全部成功）。失败只报告不抛出——configfs
    /// 在仍绑定时可能拒绝删链接/删目录，留下残留好过让清理整体失败。
    fn cleanup_after_eject(&mut self) -> Vec<String>;
}

/// loop（设备本地挂载）操作 —— **只读**。
///
/// `serve` 需要知道「哪些镜像正被 loop 占用」才能拒绝把同一镜像同时挂成
/// gadget LUN（数据安全底线），也需要报告 loop 能力。但它**不执行**挂载：
/// 那由 CLI 的 [`gadgetdisk_cli`] 一侧负责。
///
/// 只读接口使 `gdd` 无法越过边界去改 loop，从而在类型层面守住这条线。
pub trait LoopOps {
    /// 能力探测结果。
    fn capabilities(&self) -> Capabilities;

    /// 当前全部 loop 附件（读 `/sys/block/loopN/loop/backing_file`）。
    fn attachments(&self) -> Vec<Attachment>;
}

/// 尚未接入内核时的替身：所有会改状态的操作都返回明确错误。
///
/// 它**不会**伪装成功——误报成功会掩盖真实问题。
#[derive(Debug, Default)]
pub struct NullBackend {
    udc: Option<String>,
}

impl NullBackend {
    /// 构造替身；`udc` 可用于模拟「有控制器」以测试状态展示路径。
    pub fn new(udc: Option<String>) -> Self {
        Self { udc }
    }
}

/// 可注入 loop 附件状态的替身，用于测试「跨进程互斥」。
///
/// 生产环境里 loop 占用来自内核真值（`/sys/block/loopN/loop/backing_file`），
/// 由 CLI 建立的绑定是**另一个进程**写入的，`gdd` 的内存状态看不到。
/// 本替身让主机测试能模拟这种「外部已占用」的情形。
#[derive(Debug, Default, Clone)]
pub struct FakeLoopOps {
    attachments: Vec<Attachment>,
}

impl FakeLoopOps {
    /// 空附件。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个「已被 loop 挂载」的镜像。
    pub fn with_attachment(mut self, image: impl Into<String>) -> Self {
        let image = image.into();
        self.attachments.push(Attachment {
            loop_dev: format!("/dev/block/loop{}", self.attachments.len()),
            image,
            loop_part_devs: Vec::new(),
            mountpoint: "/data/adb/gadget-disk/mnt/x".into(),
            read_only: false,
        });
        self
    }
}

impl LoopOps for FakeLoopOps {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            loop_control: true,
            max_part: 7,
            filesystems: vec!["vfat".into()],
            mass_storage_supported: false,
            selinux_enforcing: false,
        }
    }

    fn attachments(&self) -> Vec<Attachment> {
        self.attachments.clone()
    }
}

impl GadgetView for NullBackend {
    fn udc(&self) -> Option<String> {
        self.udc.clone()
    }

    fn luns(&self) -> Vec<LunInfo> {
        Vec::new()
    }

    fn is_mounted(&self, _image: &Path) -> bool {
        false
    }

    fn mounted_images(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

impl MassStorageOps for NullBackend {
    fn mount(
        &mut self,
        _devices: &[MountDevice],
        _force_rebind: bool,
    ) -> KernelResult<Vec<LunInfo>> {
        Err(KernelError::unsupported("mount gadget"))
    }

    fn unmount_lun(&mut self, _lun: u8) -> KernelResult<Vec<LunInfo>> {
        Ok(Vec::new())
    }

    fn eject_all(&mut self) -> KernelResult<Vec<LunInfo>> {
        Ok(Vec::new())
    }

    fn delete_slot(&mut self, _lun: u8) -> KernelResult<Vec<LunInfo>> {
        Err(KernelError::unsupported("delete slot"))
    }

    fn teardown(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn rebind(&mut self) -> KernelResult<String> {
        Err(KernelError::unsupported("rebind UDC"))
    }

    fn is_ejected(&self) -> bool {
        // 替身没有任何挂载，无从谈「弹出」。
        false
    }

    fn cleanup_after_eject(&mut self) -> Vec<String> {
        Vec::new()
    }
}

impl LoopOps for NullBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            loop_control: false,
            max_part: 0,
            filesystems: Vec::new(),
            mass_storage_supported: false,
            selinux_enforcing: false,
        }
    }

    fn attachments(&self) -> Vec<Attachment> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_backend_never_pretends_success() {
        let mut backend = NullBackend::new(Some("dummy_udc.0".into()));

        // 读操作可以给出空结果。
        assert_eq!(backend.udc().as_deref(), Some("dummy_udc.0"));
        assert!(backend.luns().is_empty());
        assert!(backend.attachments().is_empty());

        // 改状态的操作必须明确失败，而不是假装成功。
        let err = backend
            .mount(
                &[MountDevice {
                    lun: None,
                    image_path: "/x.img".into(),
                    mode: gadgetdisk_proto::Mode::Rw,
                    inquiry_string: None,
                }],
                false,
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("not wired up"));
    }

    #[test]
    fn null_backend_unmount_of_nothing_is_ok() {
        // 弹出空集合是幂等的：没有占用就无需报错。
        let mut backend = NullBackend::default();
        assert!(backend.eject_all().unwrap().is_empty());
        assert!(backend.unmount_lun(0).unwrap().is_empty());
        assert!(backend.teardown().is_empty());
        // 删除槽位是**改状态**的操作，替身必须明确失败而不是假装成功。
        assert!(backend.delete_slot(1).is_err());
    }

    #[test]
    fn null_backend_reports_no_capabilities() {
        let backend = NullBackend::default();
        let caps = backend.capabilities();
        assert!(!caps.loop_control);
        assert!(!caps.mass_storage_supported);
        assert_eq!(caps.max_part, 0);
        assert!(caps.filesystems.is_empty());
    }

    #[test]
    fn null_backend_is_not_ejected() {
        // 没有挂载就谈不上弹出——不得让上层误判为「需要清理」。
        let mut backend = NullBackend::default();
        assert!(!backend.is_ejected());
        assert!(backend.cleanup_after_eject().is_empty());
    }
}
