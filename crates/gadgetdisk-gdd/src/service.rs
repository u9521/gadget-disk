//! 请求编排：把协议消息变成对 [`MassStorageOps`] 的调用。
//!
//! ## `gdd` 是无状态的
//!
//! 本模块只持有两样东西：数据目录（用于校验镜像路径）与一把操作锁。它
//! **不读写任何状态文件**——「上次导出到哪」记在 CLI 拥有的 `run/state.json`
//! 里，`gdd` 每次操作都从 configfs 现读内核真值。
//!
//! 这带来两个直接好处：
//!
//! 1. **重启即正确**：`gdd` 是被按需拉起的短命进程，任何进程内缓存都会在
//!    下一次调用时消失；从内核现读则永远是对的。
//! 2. **只有一处写状态**：CLI 是 `run/state.json` 的唯一写入者，不存在
//!    「两个进程各写一半」的一致性问题。
//!
//! ## `gdd` 只做 mass_storage
//!
//! 它不碰 `idVendor`/`idProduct`/`strings/*`/`os_desc`（那些归 CLI 的
//! `gadgetdisk_usb::identity`），也不做镜像增删、导入、loop 挂载。这条边界由
//! `crates/gadgetdisk-gdd` 的源码扫描测试守住。

use std::path::{Path, PathBuf};

use gadgetdisk_proto::{
    ErrorCode, ErrorResponse, Message, MountDevice, RebindResponse, UnmountResponse,
};
use gadgetdisk_usb::MAX_LUNS;

use crate::kernel::{KernelError, MassStorageOps};
use crate::lock::GlobalLock;
use crate::logging;
use crate::paths::DataDirs;

/// `gdd` 的共享状态。
///
/// 所有字段由 [`Service::handle`] 串行访问；跨连接共享靠
/// `Arc<Mutex<Service>>`（由 [`crate::server`] 负责）。
pub struct Service<G: MassStorageOps> {
    /// 数据目录（用于校验镜像路径）。
    pub dirs: DataDirs,
    /// 全局操作锁。
    pub lock: GlobalLock,
    /// mass_storage 操作。
    pub gadget: G,
}

impl<G: MassStorageOps> Service<G> {
    /// 组装一个 service。
    pub fn new(dirs: DataDirs, gadget: G) -> Self {
        Self {
            dirs,
            lock: GlobalLock::new(),
            gadget,
        }
    }

    /// 处理一条请求消息，返回应答消息。
    ///
    /// 只读请求（`Status`）不加全局锁，避免 UI 轮询被长操作挡住；
    /// 改状态的请求必须先取得全局锁，否则回 `busy`。
    pub fn handle(&mut self, request: Message) -> Message {
        match request {
            // ---- 只读请求：不加锁 ----
            Message::StatusRequest => Message::StatusResponse(gadgetdisk_proto::StatusResponse {
                udc: self.gadget.udc(),
                devices: self.gadget.luns(),
            }),

            // ---- 改状态请求：必须先取全局锁 ----
            other => {
                let request_id = other.id();

                // 跨连接并发快速失败检测：若已有并发操作持有操作锁，则立刻返回 Busy，
                // 避免调用方在 WebUI 出现不可预期的长阻塞。单进程内的分派安全由外层持有的 `&mut self` 保障。
                if self.lock.try_acquire().is_none() {
                    return error(
                        ErrorCode::Busy,
                        "another operation is in progress; try again later",
                    );
                }

                match other {
                    Message::MountRequest(req) => self.mount(&req.devices, req.rebind),
                    Message::UnmountRequest(req) => self.unmount(req.lun),
                    Message::RebindRequest(_) => self.rebind(),
                    Message::DeleteSlotRequest(req) => self.delete_slot(req.lun),

                    // 其余变体都是 gdd → client 方向的消息，客户端发来即非法。
                    _ => error(
                        ErrorCode::InvalidArgument,
                        format!("unsupported message id: {request_id:#04X}"),
                    ),
                }
            }
        }
    }

    /// 挂载/更新一批 LUN。
    fn mount(&mut self, devices: &[MountDevice], force_rebind: bool) -> Message {
        if devices.is_empty() {
            return error(ErrorCode::InvalidArgument, "devices must not be empty");
        }

        let mut normalized: Vec<MountDevice> = Vec::with_capacity(devices.len());
        for device in devices {
            // 互斥判据：同一镜像**绝不可**同时出现在两个 LUN 上。
            //
            // 与 loop 附件的互斥由 **CLI** 检查（它是 loop 的唯一创建者）；这里
            // 只管 gadget 内部的自洽性。双重写入同一镜像会损坏文件系统，
            // 因此这条检查留在唯一能写 LUN 的地方是必要的。
            if normalized
                .iter()
                .any(|other| other.image_path == device.image_path)
            {
                return error(
                    ErrorCode::ImageInUse,
                    format!(
                        "image {} is assigned to multiple LUNs in this request; \
                         a single image cannot back multiple LUNs simultaneously",
                        device.image_path
                    ),
                );
            }
            if let Err(err) = self.validate_image(Path::new(&device.image_path)) {
                return error(err.code, err.message);
            }
            normalized.push(device.clone());
        }

        // 分配 LUN 序号：未指定时优先**复用该镜像当前所在的 LUN**，否则取第一个
        // 空闲的。
        //
        // 复用这一步是必要的，不只是优化：WebUI 的「挂载 / 应用」会把它当前显示的
        // 整张设备表发过来，其中已经挂着的镜像也在里面。若一律分配新 LUN，同一个
        // 镜像就会被放到两个 LUN 上——而那是明确禁止的（两端写入损坏文件系统），
        // 会以 `invalid_argument` 被拒。复用让「再点一次应用」变成幂等操作，
        // 符合用户对「应用」这个词的预期。
        let current: Vec<(u8, String)> = self
            .gadget
            .luns()
            .into_iter()
            .filter(|l| l.attached && !l.image_path.is_empty())
            .map(|l| (l.index, l.image_path))
            .collect();
        let mut occupied: Vec<u8> = self.gadget.luns().iter().map(|l| l.index).collect();

        let mut taken: Vec<u8> = Vec::new();
        for device in &mut normalized {
            if device.lun.is_some() {
                if let Some(index) = device.lun {
                    taken.push(index);
                }
                continue;
            }
            // 该镜像已经挂在某个 LUN 上 → 复用它。
            if let Some((index, _)) = current
                .iter()
                .find(|(index, path)| path == &device.image_path && !taken.contains(index))
            {
                taken.push(*index);
                device.lun = Some(*index);
                continue;
            }
            let mut candidate = 0u8;
            loop {
                if candidate >= MAX_LUNS {
                    return error(
                        ErrorCode::InvalidArgument,
                        format!(
                            "no free LUN slots available (limit {MAX_LUNS}); unmount or delete an existing slot first"
                        ),
                    );
                }
                if !occupied.contains(&candidate) && !taken.contains(&candidate) {
                    break;
                }
                candidate += 1;
            }
            taken.push(candidate);
            // 该序号下面的 LUN 已被占用但我们要改它 → 后续 `mount` 会就地更新它。
            occupied.push(candidate);
            device.lun = Some(candidate);
        }

        match self.gadget.mount(&normalized, force_rebind) {
            Ok(luns) => Message::MountResponse(gadgetdisk_proto::MountResponse { devices: luns }),
            Err(err) => error(err.code, err.message),
        }
    }

    /// 卸载：`Some(n)` 只弹出该 LUN 的介质（保留 LUN 目录），`None` 全部拆除。
    fn unmount(&mut self, lun: Option<u8>) -> Message {
        let before: Vec<u8> = self.gadget.luns().iter().map(|l| l.index).collect();

        match lun {
            // 按 LUN 弹出：只影响该 LUN 的介质。
            Some(index) => match self.gadget.unmount_lun(index) {
                Ok(devices) => Message::UnmountResponse(UnmountResponse {
                    released: vec![index],
                    devices,
                }),
                Err(err) => error(err.code, err.message),
            },
            // 全部弹出并拆除。
            None => {
                let failures = self.gadget.teardown();
                for failure in &failures {
                    logging::warn(&format!("teardown incomplete: {failure}"));
                }
                Message::UnmountResponse(UnmountResponse {
                    released: before,
                    devices: self.gadget.luns(),
                })
            }
        }
    }

    /// 删除一个空闲槽位。
    ///
    /// 与「按 LUN 弹出」的区别：弹出只清 `file`（槽位保留、参数保留），删除则
    /// 让该序号回到「不存在」。因此删除之后重新挂载同号会走一次 LUN 创建。
    fn delete_slot(&mut self, lun: u8) -> Message {
        match self.gadget.delete_slot(lun) {
            Ok(devices) => {
                Message::DeleteSlotResponse(gadgetdisk_proto::DeleteSlotResponse { devices })
            }
            Err(err) => error(err.code, err.message),
        }
    }

    /// 重新绑定 UDC，让身份等改动生效。
    fn rebind(&mut self) -> Message {
        match self.gadget.rebind() {
            Ok(udc) => Message::RebindResponse(RebindResponse { udc }),
            Err(err) => error(err.code, err.message),
        }
    }

    /// 校验镜像存在、是常规文件。
    fn validate_image(&self, path: &Path) -> Result<(), KernelError> {
        let metadata = std::fs::metadata(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                KernelError::new(
                    ErrorCode::ImageNotFound,
                    format!("image not found: {}", path.display()),
                )
            } else {
                KernelError::new(ErrorCode::PermissionDenied, err.to_string())
            }
        })?;

        if !metadata.is_file() {
            return Err(KernelError::new(
                ErrorCode::NotRegularFile,
                format!("path is not a regular file: {}", path.display()),
            ));
        }
        Ok(())
    }
}

/// 构造错误应答。
pub fn error(code: ErrorCode, message: impl Into<String>) -> Message {
    Message::Error(ErrorResponse {
        code,
        message: message.into(),
    })
}

/// 解析镜像路径的文件名（供日志使用）。
pub fn image_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_string)
}

/// 把路径规范化为绝对路径（不要求它存在）。
pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::NullBackend;
    use crate::testutil;
    use gadgetdisk_proto::Mode;

    type TestService = Service<NullBackend>;

    fn service(tag: &str) -> (TestService, PathBuf) {
        let root = testutil::temp_dir(tag);
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        (
            Service::new(dirs, NullBackend::new(Some("dummy_udc.0".into()))),
            root,
        )
    }

    fn expect_error(message: Message) -> ErrorResponse {
        match message {
            Message::Error(err) => err,
            other => panic!("期望 Error，得到 {other:?}"),
        }
    }

    fn device(path: &str) -> MountDevice {
        MountDevice {
            lun: None,
            image_path: path.to_string(),
            mode: Mode::Rw,
            inquiry_string: None,
        }
    }

    #[test]
    fn status_reports_udc_without_lock() {
        let (mut svc, root) = service("svc-status");
        match svc.handle(Message::StatusRequest) {
            Message::StatusResponse(status) => {
                assert_eq!(status.udc.as_deref(), Some("dummy_udc.0"));
                assert!(status.devices.is_empty());
            }
            other => panic!("期望 StatusResponse，得到 {other:?}"),
        }
        testutil::cleanup(&root);
    }

    #[test]
    fn busy_is_returned_when_lock_is_held() {
        let (mut svc, root) = service("svc-busy");
        // 从另一个句柄持锁，模拟另一个操作进行中（同句柄借用会冲突）。
        let other = svc.lock.clone();
        let _guard = other.try_acquire().unwrap();

        let response = svc.handle(Message::UnmountRequest(gadgetdisk_proto::UnmountRequest {
            lun: None,
        }));
        assert_eq!(expect_error(response).code, ErrorCode::Busy);

        testutil::cleanup(&root);
    }

    #[test]
    fn status_is_served_even_when_lock_is_held() {
        // 只读请求不应被长操作挡住，否则 UI 无法显示进度。
        let (mut svc, root) = service("svc-status-unlocked");
        let other = svc.lock.clone();
        let _guard = other.try_acquire().unwrap();
        assert!(matches!(
            svc.handle(Message::StatusRequest),
            Message::StatusResponse(_)
        ));
        testutil::cleanup(&root);
    }

    #[test]
    fn mount_missing_image_reports_not_found() {
        let (mut svc, root) = service("svc-mount-missing");
        let missing = svc.dirs.images().join("nope.img");
        let response = svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
            devices: vec![device(&missing.to_string_lossy())],
            rebind: false,
        }));
        assert_eq!(expect_error(response).code, ErrorCode::ImageNotFound);
        testutil::cleanup(&root);
    }

    #[test]
    fn mount_empty_devices_is_rejected() {
        let (mut svc, root) = service("svc-mount-empty");
        let response = svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
            devices: Vec::new(),
            rebind: false,
        }));
        assert_eq!(expect_error(response).code, ErrorCode::InvalidArgument);
        testutil::cleanup(&root);
    }

    #[test]
    fn mount_directory_is_rejected_as_not_regular_file() {
        let (mut svc, root) = service("svc-mount-dir");
        let dir = svc.dirs.images().to_string_lossy().into_owned();
        let response = svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
            devices: vec![device(&dir)],
            rebind: false,
        }));
        assert_eq!(expect_error(response).code, ErrorCode::NotRegularFile);
        testutil::cleanup(&root);
    }

    /// 同一镜像落到两个 LUN 会被两端同时写入 → 损坏文件系统。
    #[test]
    fn same_image_on_two_luns_is_rejected() {
        let (mut svc, root) = service("svc-dup-image");
        let target = svc.dirs.images().join("a.img");
        std::fs::write(&target, b"data").unwrap();
        let path = target.to_string_lossy().into_owned();

        let response = svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
            devices: vec![device(&path), device(&path)],
            rebind: false,
        }));
        let err = expect_error(response);
        assert_eq!(err.code, ErrorCode::ImageInUse);
        assert!(
            err.message.contains("multiple LUNs"),
            "得到 {}",
            err.message
        );

        testutil::cleanup(&root);
    }

    /// `MountRequest.rebind` 必须一路传到后端。
    ///
    /// 回归：它曾被反序列化后**静默丢弃**，于是「改身份 → 挂载」在本次没有结构性
    /// 改动时不会重绑，主机永远看不到新描述符——而 CLI/REST 都已经按「它会生效」
    /// 的假设在发这个字段。
    #[test]
    fn mount_request_rebind_reaches_the_backend() {
        use gadgetdisk_proto::LunInfo;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Recorder {
            seen: Arc<Mutex<Vec<bool>>>,
        }
        impl crate::kernel::GadgetView for Recorder {
            fn udc(&self) -> Option<String> {
                Some("dummy_udc.0".into())
            }
            fn luns(&self) -> Vec<LunInfo> {
                Vec::new()
            }
            fn is_mounted(&self, _image: &std::path::Path) -> bool {
                false
            }
            fn mounted_images(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
        }
        impl crate::kernel::MassStorageOps for Recorder {
            fn mount(
                &mut self,
                _devices: &[MountDevice],
                force_rebind: bool,
            ) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                self.seen.lock().unwrap().push(force_rebind);
                Ok(Vec::new())
            }
            fn unmount_lun(&mut self, _lun: u8) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                Ok(Vec::new())
            }
            fn eject_all(&mut self) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                Ok(Vec::new())
            }
            fn delete_slot(&mut self, _lun: u8) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                Ok(Vec::new())
            }
            fn teardown(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn rebind(&mut self) -> crate::kernel::KernelResult<String> {
                Ok("dummy_udc.0".into())
            }
            fn is_ejected(&self) -> bool {
                false
            }
            fn cleanup_after_eject(&mut self) -> Vec<String> {
                Vec::new()
            }
        }

        let root = testutil::temp_dir("svc-rebind-flag");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let target = dirs.images().join("a.img");
        std::fs::write(&target, b"data").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut svc = Service::new(
            dirs,
            Recorder {
                seen: Arc::clone(&seen),
            },
        );

        for flag in [false, true] {
            svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
                devices: vec![device(&target.to_string_lossy())],
                rebind: flag,
            }));
        }

        assert_eq!(
            *seen.lock().unwrap(),
            vec![false, true],
            "rebind 必须原样传给后端"
        );
        testutil::cleanup(&root);
    }

    /// `NullBackend` 的挂载永远失败 → 错误码必须是 `internal` 而不是假装成功。
    #[test]
    fn backend_failure_is_reported_not_hidden() {
        let (mut svc, root) = service("svc-backend-fail");
        let target = svc.dirs.images().join("a.img");
        std::fs::write(&target, b"data").unwrap();

        let response = svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
            devices: vec![device(&target.to_string_lossy())],
            rebind: false,
        }));
        assert_eq!(expect_error(response).code, ErrorCode::Internal);
        testutil::cleanup(&root);
    }

    /// 未指定 LUN 时应**复用该镜像当前所在的 LUN**（而不是分配新的）。
    ///
    /// 回归（AVD 实测）：WebUI 的「挂载 / 应用」会发来整张设备表，其中包含已经
    /// 挂着的镜像。若一律分配新 LUN，同一镜像会落到两个 LUN 上，被数据安全底线
    /// 拒绝（`invalid_argument`），于是「再点一次应用」报错。
    #[test]
    fn unspecified_lun_reuses_the_images_current_lun() {
        use gadgetdisk_proto::LunInfo;

        #[derive(Default)]
        struct FixedBackend {
            mounted: Vec<(u8, String)>,
            seen: Vec<Vec<Option<u8>>>,
        }
        impl crate::kernel::GadgetView for FixedBackend {
            fn udc(&self) -> Option<String> {
                Some("dummy_udc.0".into())
            }
            fn luns(&self) -> Vec<LunInfo> {
                self.mounted
                    .iter()
                    .map(|(index, path)| LunInfo {
                        index: *index,
                        image_path: path.clone(),
                        size_bytes: 0,
                        mode: gadgetdisk_proto::Mode::Rw,
                        inquiry_string: None,
                        attached: true,
                        effective: true,
                        deletable: *index != 0,
                    })
                    .collect()
            }
            fn is_mounted(&self, _image: &std::path::Path) -> bool {
                false
            }
            fn mounted_images(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
        }
        impl crate::kernel::MassStorageOps for FixedBackend {
            fn mount(
                &mut self,
                devices: &[MountDevice],
                _force_rebind: bool,
            ) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                self.seen.push(devices.iter().map(|d| d.lun).collect());
                Ok(Vec::new())
            }
            fn unmount_lun(&mut self, _lun: u8) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                Ok(Vec::new())
            }
            fn eject_all(&mut self) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                Ok(Vec::new())
            }
            fn delete_slot(&mut self, _lun: u8) -> crate::kernel::KernelResult<Vec<LunInfo>> {
                Ok(Vec::new())
            }
            fn teardown(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn rebind(&mut self) -> crate::kernel::KernelResult<String> {
                Ok("dummy_udc.0".into())
            }
            fn is_ejected(&self) -> bool {
                false
            }
            fn cleanup_after_eject(&mut self) -> Vec<String> {
                Vec::new()
            }
        }

        let root = testutil::temp_dir("svc-lun-reuse");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let a = dirs.images().join("a.img");
        let b = dirs.images().join("b.img");
        std::fs::write(&a, b"data").unwrap();
        std::fs::write(&b, b"data").unwrap();

        let mut svc = Service::new(
            dirs,
            FixedBackend {
                // a.img 已经挂在 lun.1 上（不是 0，以便区分「复用」与「取最小空闲」）。
                mounted: vec![(1, a.to_string_lossy().into_owned())],
                ..Default::default()
            },
        );

        // 发来整张表：a.img（未指定 LUN）+ b.img（未指定 LUN）。
        svc.handle(Message::MountRequest(gadgetdisk_proto::MountRequest {
            devices: vec![device(&a.to_string_lossy()), device(&b.to_string_lossy())],
            rebind: false,
        }));

        let seen = {
            let guard = &svc.gadget;
            guard.seen.clone()
        };
        assert_eq!(
            seen,
            vec![vec![Some(1), Some(0)]],
            "a.img 应复用 lun.1；b.img 取最小空闲 lun.0"
        );
        testutil::cleanup(&root);
    }

    /// 删除槽位必须分派到后端，并把结果/错误原样带回。
    #[test]
    fn delete_slot_is_dispatched_to_the_backend() {
        let (mut svc, root) = service("svc-delete-slot");
        // `NullBackend` 的 delete_slot 明确失败 → 错误码必须是 internal。
        let response = svc.handle(Message::DeleteSlotRequest(
            gadgetdisk_proto::DeleteSlotRequest { lun: 1 },
        ));
        assert_eq!(expect_error(response).code, ErrorCode::Internal);
        testutil::cleanup(&root);
    }

    #[test]
    fn rebind_reports_backend_failure() {
        let (mut svc, root) = service("svc-rebind-fail");
        let response = svc.handle(Message::RebindRequest(
            gadgetdisk_proto::RebindRequest::default(),
        ));
        assert_eq!(expect_error(response).code, ErrorCode::Internal);
        testutil::cleanup(&root);
    }

    #[test]
    fn unmount_all_reports_emptied_devices() {
        let (mut svc, root) = service("svc-unmount");
        match svc.handle(Message::UnmountRequest(gadgetdisk_proto::UnmountRequest {
            lun: None,
        })) {
            Message::UnmountResponse(resp) => assert!(resp.devices.is_empty()),
            other => panic!("期望 UnmountResponse，得到 {other:?}"),
        }
        testutil::cleanup(&root);
    }

    #[test]
    fn unmount_single_lun_reports_that_lun() {
        let (mut svc, root) = service("svc-unmount-one");
        match svc.handle(Message::UnmountRequest(gadgetdisk_proto::UnmountRequest {
            lun: Some(1),
        })) {
            Message::UnmountResponse(resp) => assert_eq!(resp.released, vec![1]),
            other => panic!("期望 UnmountResponse，得到 {other:?}"),
        }
        testutil::cleanup(&root);
    }

    /// client 方向的消息发到 `gdd` 不得被当成有效请求。
    #[test]
    fn response_only_messages_are_rejected() {
        let (mut svc, root) = service("svc-unsupported");
        let response = svc.handle(Message::StatusResponse(gadgetdisk_proto::StatusResponse {
            udc: None,
            devices: Vec::new(),
        }));
        assert_eq!(expect_error(response).code, ErrorCode::InvalidArgument);
        testutil::cleanup(&root);
    }

    #[test]
    fn image_name_and_absolute_helpers() {
        assert_eq!(
            image_name(Path::new("/data/adb/gadget-disk/images/a.img")).as_deref(),
            Some("a.img")
        );
        assert!(absolute(Path::new("/x")).is_absolute());
        assert!(absolute(Path::new("rel")).is_absolute());
    }
}
