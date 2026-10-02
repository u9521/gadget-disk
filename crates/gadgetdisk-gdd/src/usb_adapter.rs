//! 把 `gadgetdisk-usb` 的真实 configfs 实现接到 [`MassStorageOps`]。
//!
//! 这是**唯一的接线点**，因此 `gdd` 的编排逻辑本身不依赖 `gadgetdisk-usb`，
//! 两个 crate 之间也不存在环（见
//! [gdd 编排 Note](../../../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)）。
//!
//! ## 本模块只接 mass_storage
//!
//! 身份（`idVendor`/`idProduct`/`strings/*`/`os_desc`）由 CLI 直接写 configfs，
//! 不经过这里，也不经过 `gdd`。见 [`gadgetdisk_usb::identity`]。

use std::path::{Path, PathBuf};

use gadgetdisk_proto::{ErrorCode, LunInfo, MountDevice};
use gadgetdisk_usb::configfs::{FsError, RealConfigFs};
use gadgetdisk_usb::error::GadgetError;
use gadgetdisk_usb::mass_storage::{LunRequest, MassStorage};
use gadgetdisk_usb::paths as usb_paths;

use crate::kernel::{GadgetView, KernelError, KernelResult, MassStorageOps, NullBackend};
use crate::paths::DataDirs;

/// 探测当前的 gadget / config 布局。
///
/// 以 `sys.usb.controller` 为线索：**已绑定到它的 gadget 最可信**；选不出来时
/// 退回 AOSP 默认（`g1` + `b.1`），仍然选不出就报错——**绝不盲写**。
///
/// 探测而非硬编码 `g1` 的理由见 [`gadgetdisk_usb::discover`]：真机上存在
/// vendor 私有的第二个 gadget（实测红魔 9 Pro 上有 `g2`）。
pub fn discover_layout() -> Result<gadgetdisk_usb::Layout, GadgetError> {
    let tree = gadgetdisk_usb::RealGadgetTree::default();
    let udc = usb_paths::read_udc_name();
    gadgetdisk_usb::discover(&tree, udc.name())
        .map_err(|err| GadgetError::ConfigfsUnavailable(err.to_string()))
}

/// 由 `gadgetdisk-usb` 支撑的真实 mass_storage 操作。
pub struct UsbGadget {
    /// mass_storage 编排器。
    storage: MassStorage<RealConfigFs>,
    /// 数据目录（诊断用）。
    dirs: DataDirs,
    /// 缓存的镜像容量（`LunInfo` 需要）。
    ///
    /// 只优化同一进程内的重复查询；**不是**真相来源——缓存没有时现场 `stat`。
    sizes: Vec<(String, u64)>,
}

impl UsbGadget {
    /// 以数据目录构造。
    ///
    /// 会先**探测** gadget/config 布局，再校验它真的是 configfs；否则返回错误，
    /// 避免误写普通文件系统造成数据损坏。
    pub fn new(dirs: DataDirs) -> Result<Self, GadgetError> {
        let layout = discover_layout()?;
        gadgetdisk_usb::ensure_configfs(&layout.gadget_root)?;

        let fs = RealConfigFs::new(&layout.gadget_root);
        Ok(Self {
            storage: MassStorage::new(fs).with_layout(layout),
            dirs,
            sizes: Vec::new(),
        })
    }

    /// 访问内部编排器（诊断用）。
    pub fn storage(&self) -> &MassStorage<RealConfigFs> {
        &self.storage
    }

    /// 数据目录。
    pub fn dirs(&self) -> &DataDirs {
        &self.dirs
    }

    /// LUN 的镜像容量。
    ///
    /// 优先用挂载时记下的缓存；缓存没有（**一次性 CLI 调用**就是这样，每次都是
    /// 新进程）就现场 `stat` 后端文件——LUN 的容量就是文件大小，因此这不是猜测，
    /// 而是同一个事实的另一个来源。
    ///
    /// 回归：改为「status 由 CLI 就地执行」后，一次性进程里缓存恒为空，
    /// 容量会一直显示 0（AVD 实测确认）。
    fn lun_size(&self, image_path: &str) -> u64 {
        if let Some((_, size)) = self.sizes.iter().find(|(path, _)| path == image_path) {
            return *size;
        }
        if image_path.is_empty() {
            return 0;
        }
        std::fs::metadata(image_path).map(|m| m.len()).unwrap_or(0)
    }

    /// 把协议层设备描述转成编排层的 LUN 请求。
    fn to_request(device: &MountDevice, index: u8) -> LunRequest {
        LunRequest {
            index,
            image_path: device.image_path.clone(),
            mode: device.mode,
            inquiry_string: device.inquiry_string.clone(),
        }
    }

    /// 把编排层读回的 LUN 状态转成协议层。
    fn to_info(&self, lun: gadgetdisk_usb::LunState) -> LunInfo {
        LunInfo {
            index: lun.index,
            size_bytes: self.lun_size(&lun.image_path),
            image_path: lun.image_path,
            mode: lun.mode,
            inquiry_string: lun.inquiry_string,
            attached: lun.attached,
            effective: lun.effective,
            deletable: lun.deletable,
        }
    }
}

/// 把 `GadgetError` 映射为协议错误码。
pub fn map_gadget_error(err: &GadgetError) -> KernelError {
    let (code, message) = match err {
        GadgetError::NoUdc => (
            ErrorCode::NoUdc,
            "no USB controller available (sys.usb.controller is empty); normal when USB is not connected".to_string(),
        ),
        GadgetError::MassStorageUnsupported => (
            ErrorCode::MassStorageUnsupported,
            "the kernel does not support the mass_storage gadget function".to_string(),
        ),
        // 参数问题：LUN 序号越界、INQUIRY 超长等。属于调用方错误，不是设备问题。
        GadgetError::InvalidArgument(msg) => (ErrorCode::InvalidArgument, msg.clone()),
        GadgetError::ImageUnavailable(msg) => (ErrorCode::InvalidArgument, msg.clone()),
        GadgetError::Backup(msg) => (ErrorCode::Internal, format!("gadget backup/restore failed: {msg}")),
        GadgetError::Config(msg) => (ErrorCode::Internal, format!("config did not take effect: {msg}")),
        // 探测不出 gadget 布局：属于设备/内核环境问题，不是用户参数问题。
        GadgetError::ConfigfsUnavailable(msg) => (
            ErrorCode::ConfigfsUnavailable,
            format!("cannot determine a usable USB gadget: {msg}"),
        ),
        // 配置建立但未生效：明确报错，**不得**让上层误以为挂载成功。
        GadgetError::NotActive(msg) => (
            ErrorCode::NotActive,
            format!("the USB config was built but did not take effect (host did not accept it): {msg}"),
        ),
        GadgetError::Fs(fs_err) => {
            let code = match fs_err {
                FsError::NotConfigFs { .. } => ErrorCode::PermissionDenied,
                FsError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    ErrorCode::PermissionDenied
                }
                _ => ErrorCode::Internal,
            };
            (code, fs_err.to_string())
        }
    };
    KernelError::new(code, message)
}

impl GadgetView for UsbGadget {
    fn udc(&self) -> Option<String> {
        self.storage.detect_udc().ok()
    }

    fn luns(&self) -> Vec<LunInfo> {
        self.storage
            .luns()
            .into_iter()
            .map(|lun| self.to_info(lun))
            .collect()
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

impl MassStorageOps for UsbGadget {
    fn mount(&mut self, devices: &[MountDevice], force_rebind: bool) -> KernelResult<Vec<LunInfo>> {
        // LUN 序号由 `gdd` 的 service 层分配好（未指定时取第一个空闲的），
        // 因此到达这里时必定是 `Some`。缺失则视为内部错误而不是猜测。
        let mut requests = Vec::with_capacity(devices.len());
        for device in devices {
            let Some(index) = device.lun else {
                return Err(KernelError::new(
                    ErrorCode::Internal,
                    "internal error: the LUN index was not assigned before dispatch",
                ));
            };
            let size = std::fs::metadata(&device.image_path)
                .map(|m| m.len())
                .unwrap_or(0);
            self.sizes.push((device.image_path.clone(), size));
            requests.push(Self::to_request(device, index));
        }

        self.storage
            .mount_with_options(&requests, force_rebind)
            .map_err(|err| map_gadget_error(&err))?;

        Ok(self.luns())
    }

    fn unmount_lun(&mut self, lun: u8) -> KernelResult<Vec<LunInfo>> {
        self.storage
            .unmount_lun(lun)
            .map_err(|err| map_gadget_error(&err))?;
        // 该 LUN 已解绑，容量缓存对应项清掉避免报旧值。
        self.sizes.retain(|(path, _)| {
            self.storage
                .luns()
                .iter()
                .any(|l| l.attached && &l.image_path == path)
        });
        Ok(self.luns())
    }

    fn eject_all(&mut self) -> KernelResult<Vec<LunInfo>> {
        self.storage
            .eject_all()
            .map_err(|err| map_gadget_error(&err))?;
        self.sizes.clear();
        Ok(self.luns())
    }

    fn delete_slot(&mut self, lun: u8) -> KernelResult<Vec<LunInfo>> {
        self.storage
            .delete_slot(lun)
            .map_err(|err| map_gadget_error(&err))?;
        // 被删槽位的容量缓存清掉，避免下一次 status 报旧值。
        self.sizes.retain(|(path, _)| {
            self.storage
                .luns()
                .iter()
                .any(|l| l.attached && &l.image_path == path)
        });
        Ok(self.luns())
    }

    fn teardown(&mut self) -> Vec<String> {
        let failures = self.storage.teardown();
        self.sizes.clear();
        failures
    }

    fn rebind(&mut self) -> KernelResult<String> {
        self.storage.rebind().map_err(|err| map_gadget_error(&err))
    }

    fn is_ejected(&self) -> bool {
        // 判据完全在 `MassStorage` 里：我们的 function 与链接存在，且**全部**
        // LUN 的后端都已解绑（单个 LUN 为空属正常的按 LUN 卸载，不算弹出）。
        //
        // 回归（AVD 实测）：早期实现先要求「`luns()` 里有 attached 的项」再看
        // `file` 是否为空——但弹出**就是** `file` 变空，那一刻 `attached` 已经是
        // false，条件永远不成立，清理从不触发。
        self.storage.is_ejected()
    }

    fn cleanup_after_eject(&mut self) -> Vec<String> {
        let failures = self.storage.cleanup_after_eject();
        self.sizes.clear();
        failures
    }
}

// ---------------------------------------------------------------- 后端选择

/// gadget 侧后端：真实 configfs 或明确失败的替身。
///
/// 之所以用枚举而不是 `Box<dyn MassStorageOps>`：`Service` 与 `gdd::run` 都是
/// 泛型的，需要**单一具体类型**。枚举同时保留了「未接线时明确失败」的语义。
pub enum GadgetBackend {
    /// 真实的 configfs 实现（gadget 根由探测决定）。
    Real(Box<UsbGadget>),
    /// 内核能力不可用（例如 `/config` 不是 configfs）：改状态操作明确失败。
    Unavailable(NullBackend),
}

impl GadgetBackend {
    /// 尽最大努力接入真实实现；失败时返回原因与替身。
    ///
    /// 返回 `(backend, Option<reason>)`：`reason` 非空表示退化到替身，
    /// 调用方应记录它，以便用户知道为何挂载不可用。
    pub fn detect(dirs: DataDirs) -> (Self, Option<String>) {
        match UsbGadget::new(dirs) {
            Ok(gadget) => (GadgetBackend::Real(Box::new(gadget)), None),
            Err(err) => {
                let reason = map_gadget_error(&err).message;
                (
                    GadgetBackend::Unavailable(NullBackend::default()),
                    Some(reason),
                )
            }
        }
    }

    /// 是否为真实后端。
    pub fn is_real(&self) -> bool {
        matches!(self, GadgetBackend::Real(_))
    }
}

impl GadgetView for GadgetBackend {
    fn udc(&self) -> Option<String> {
        match self {
            GadgetBackend::Real(g) => g.udc(),
            GadgetBackend::Unavailable(g) => g.udc(),
        }
    }

    fn luns(&self) -> Vec<LunInfo> {
        match self {
            GadgetBackend::Real(g) => g.luns(),
            GadgetBackend::Unavailable(g) => g.luns(),
        }
    }

    fn is_mounted(&self, image: &Path) -> bool {
        match self {
            GadgetBackend::Real(g) => g.is_mounted(image),
            GadgetBackend::Unavailable(g) => g.is_mounted(image),
        }
    }

    fn mounted_images(&self) -> Vec<PathBuf> {
        match self {
            GadgetBackend::Real(g) => g.mounted_images(),
            GadgetBackend::Unavailable(g) => g.mounted_images(),
        }
    }
}

impl MassStorageOps for GadgetBackend {
    fn mount(&mut self, devices: &[MountDevice], force_rebind: bool) -> KernelResult<Vec<LunInfo>> {
        match self {
            GadgetBackend::Real(g) => g.mount(devices, force_rebind),
            GadgetBackend::Unavailable(_) => Err(KernelError::new(
                ErrorCode::PermissionDenied,
                "configfs unavailable (/config is not mounted as configfs); cannot mount a USB device",
            )),
        }
    }

    fn unmount_lun(&mut self, lun: u8) -> KernelResult<Vec<LunInfo>> {
        match self {
            GadgetBackend::Real(g) => g.unmount_lun(lun),
            GadgetBackend::Unavailable(g) => g.unmount_lun(lun),
        }
    }

    fn eject_all(&mut self) -> KernelResult<Vec<LunInfo>> {
        match self {
            GadgetBackend::Real(g) => g.eject_all(),
            GadgetBackend::Unavailable(g) => g.eject_all(),
        }
    }

    fn delete_slot(&mut self, lun: u8) -> KernelResult<Vec<LunInfo>> {
        match self {
            GadgetBackend::Real(g) => g.delete_slot(lun),
            GadgetBackend::Unavailable(_) => Err(KernelError::new(
                ErrorCode::PermissionDenied,
                "configfs unavailable (/config is not mounted as configfs); cannot delete a slot",
            )),
        }
    }

    fn teardown(&mut self) -> Vec<String> {
        match self {
            GadgetBackend::Real(g) => g.teardown(),
            GadgetBackend::Unavailable(g) => g.teardown(),
        }
    }

    fn rebind(&mut self) -> KernelResult<String> {
        match self {
            GadgetBackend::Real(g) => g.rebind(),
            GadgetBackend::Unavailable(_) => Err(KernelError::new(
                ErrorCode::PermissionDenied,
                "configfs unavailable (/config is not mounted as configfs); cannot rebind the UDC",
            )),
        }
    }

    fn is_ejected(&self) -> bool {
        match self {
            GadgetBackend::Real(g) => g.is_ejected(),
            GadgetBackend::Unavailable(g) => g.is_ejected(),
        }
    }

    fn cleanup_after_eject(&mut self) -> Vec<String> {
        match self {
            GadgetBackend::Real(g) => g.cleanup_after_eject(),
            GadgetBackend::Unavailable(g) => g.cleanup_after_eject(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_no_udc_explains_it_is_normal() {
        let err = map_gadget_error(&GadgetError::NoUdc);
        assert_eq!(err.code, ErrorCode::NoUdc);
        assert!(
            err.message.contains("normal when USB is not connected"),
            "得到 {}",
            err.message
        );
    }

    #[test]
    fn map_mass_storage_unsupported() {
        let err = map_gadget_error(&GadgetError::MassStorageUnsupported);
        assert_eq!(err.code, ErrorCode::MassStorageUnsupported);
    }

    #[test]
    fn map_invalid_argument_stays_invalid_argument() {
        // 参数问题（LUN 越界、INQUIRY 超长）必须如实报 invalid_argument，
        // 而不是被笼统归为 internal —— 那会让用户去查设备而不是查自己的输入。
        let err = map_gadget_error(&GadgetError::InvalidArgument("LUN 9 out of range".into()));
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.contains("out of range"));
    }

    #[test]
    fn map_config_failure_is_internal_but_explains_itself() {
        let err = map_gadget_error(&GadgetError::Config(
            "some attribute read back inconsistently".into(),
        ));
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(
            err.message.contains("did not take effect"),
            "得到 {}",
            err.message
        );
    }

    #[test]
    fn map_not_configfs_is_permission_denied() {
        let fs_err = FsError::NotConfigFs {
            path: PathBuf::from("/tmp"),
            magic: 0x0102_1994,
        };
        let err = map_gadget_error(&GadgetError::Fs(fs_err));
        assert_eq!(err.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn map_not_active_is_its_own_code() {
        // 「配置建立但未生效」必须有自己的错误码，否则 WebUI 无从区分
        // 「挂载成功」与「看起来成功但其实主机没接受」。
        let err = map_gadget_error(&GadgetError::NotActive("UDC 未绑定".into()));
        assert_eq!(err.code, ErrorCode::NotActive);
    }

    #[test]
    fn unavailable_backend_fails_loudly_for_writes() {
        let mut backend = GadgetBackend::Unavailable(NullBackend::default());
        assert!(!backend.is_real());
        let err = backend
            .mount(
                &[MountDevice {
                    lun: Some(0),
                    image_path: "/x.img".into(),
                    mode: gadgetdisk_proto::Mode::Rw,
                    inquiry_string: None,
                }],
                false,
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert!(err.message.contains("configfs"));
    }
}
