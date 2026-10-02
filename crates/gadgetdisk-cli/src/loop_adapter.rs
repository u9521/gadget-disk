//! loop 侧的内核接线点：把 `gadgetdisk-loop` 接到 gdd 的**只读** `LoopOps`，
//! 并对外暴露 CLI 直做的一次性挂载/卸载。
//!
//! 与 [`crate::gadget_adapter`] 对称：这里是 loop 侧的唯一接线点，gdd 因此
//! 不依赖 `gadgetdisk-loop`（见
//! [gdd 编排 Note](../../../../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)）。
//!
//! ## 为什么 gdd 只拿到只读接口
//!
//! loop 挂载由 CLI（`serve` 与一次性 `attach-loop`）执行，gdd 只在
//! 「有镜像被导出为 USB 设备」期间存在。gdd 仍需知道**哪些镜像正被 loop
//! 占用**才能守住「同一镜像不可同时为 gadget LUN 与 loop 附件」这条底线，
//! 但它不该有改 loop 的能力——因此 trait 只剩 `capabilities` / `attachments`。
//! 挂载/卸载作为 [`LoopMounts`] 的普通方法供 CLI 直接调用。
//!
//! ## 启动清理的位置
//!
//! loop 残留（进程被杀留下的 `loopN` 与 `/data/adb/gadget-disk/mnt/*` 挂载）
//! 必须在**使用前**清掉，否则用户会看到「镜像明明没在用却报 image_in_use」。
//! 清理在 [`LoopMounts::new`] 里同步执行，且只处理后备文件位于本模块
//! `images/` 下的设备——绝不碰其他应用的 loop。

use std::path::{Path, PathBuf};

use gadgetdisk_gdd::kernel::{KernelError, KernelResult, LoopOps};
use gadgetdisk_gdd::paths::DataDirs;

use crate::cli_paths::Offsets;
use gadgetdisk_loop::attach::{AttachRequest, DetachSelector, LoopMounter};
use gadgetdisk_loop::caps::{RealSource, probe};
use gadgetdisk_loop::error::LoopError;
use gadgetdisk_loop::loopdev::{LoopControl, RealLoopControl};
use gadgetdisk_loop::mount::RealMounter;
use gadgetdisk_proto::{Attachment, Capabilities, ErrorCode};

/// 已记录的活跃 loop 附件：`(镜像, 挂载点, loop 设备)`。
///
/// ## 为什么必须落盘
///
/// loop 挂载由 CLI 执行，每次操作都是**新进程**。判断「某个 loop 是本模块的
/// 活跃附件」还是「上次崩溃的残留」不能靠进程内状态，也不能只靠「后备文件在 images/ 下」
/// ——后者会让启动清理误拆正在使用的挂载。因此将活跃附件记录持久化于 `run/loop-attachments.json`。
///
/// 记录本身**不是真相来源**：真正绑定与否以内核为准（`attachments()`）。它只回答
/// 「这个绑定是不是我们有意保持的」。因此读到损坏文件时按「没有记录」处理，
/// 最坏结果是多清理一次残留，而不是拒绝服务。
#[derive(Debug, Default, Clone)]
struct Registry {
    entries: Vec<(PathBuf, PathBuf, String)>,
}

impl Registry {
    /// 从磁盘读取；不存在或损坏时返回空。
    fn from_disk(dirs: &DataDirs) -> Self {
        let Ok(text) = std::fs::read_to_string(crate::cli_paths::loop_registry(dirs)) else {
            return Self::default();
        };
        let Ok(parsed) = serde_json::from_str::<Vec<(String, String, String)>>(&text) else {
            return Self::default();
        };
        Self {
            entries: parsed
                .into_iter()
                .map(|(image, mountpoint, dev)| {
                    (PathBuf::from(image), PathBuf::from(mountpoint), dev)
                })
                .collect(),
        }
    }

    /// 写回磁盘（尽力而为：失败只影响下次启动清理的精度，不影响本次操作）。
    fn save(&self, dirs: &DataDirs) {
        let payload: Vec<(String, String, String)> = self
            .entries
            .iter()
            .map(|(image, mountpoint, dev)| {
                (
                    image.display().to_string(),
                    mountpoint.display().to_string(),
                    dev.clone(),
                )
            })
            .collect();
        if let Ok(text) = serde_json::to_string(&payload) {
            std::fs::write(crate::cli_paths::loop_registry(dirs), text).ok();
        }
    }

    fn insert(&mut self, image: &Path, mountpoint: &Path, loop_dev: &str) {
        // 按**镜像**与**挂载点**双向去重。
        //
        // 原实现写的是 `m != image`（拿挂载点与新镜像比较），因此同一镜像
        // 再次登记时旧记录不会被清掉，会留下两条指向不同挂载点的记录。
        self.entries
            .retain(|(i, m, _)| i != image && m != mountpoint);
        self.entries.push((
            image.to_path_buf(),
            mountpoint.to_path_buf(),
            loop_dev.to_string(),
        ));
    }

    fn remove(&mut self, image: &Path) {
        self.entries.retain(|(i, _, _)| i != image);
    }

    fn remove_by_dev(&mut self, loop_dev: &str) {
        self.entries.retain(|(_, _, d)| d != loop_dev);
    }
}

/// 真实的 loop 挂载后端。
pub struct LoopMounts {
    dirs: DataDirs,
    inner: LoopMounter<RealLoopControl, RealMounter>,
    registry: Registry,
}

impl LoopMounts {
    /// 构造后端，**不做**任何清理。
    ///
    /// 只读路径（如 `list-loop`）必须用这个：列出附件不该有副作用。早期实现
    /// 在构造时无条件跑「启动清理」，于是 `list-loop` 会先把自己的活跃挂载
    /// 拆掉再返回空列表（实测）。
    pub fn new(dirs: DataDirs) -> Self {
        let caps = probe(&RealSource);
        let mut inner = LoopMounter::new(RealLoopControl::new(), RealMounter::new(), caps);
        // 用内核真值重建「已挂载」记录：新进程的内存记录是空的，而 detach 按
        // 这些记录找目标——不重建就会永远报「没有匹配的 loop 附件」。
        // 只读操作，不改变内核状态。
        if let Err(err) = inner.sync_from_kernel(&dirs.images(), &dirs.mnt()) {
            eprintln!(
                "gadgetdisk loop: failed to read kernel loop state: {}",
                err.message
            );
        }
        Self {
            registry: Registry::from_disk(&dirs),
            dirs,
            inner,
        }
    }

    /// 构造后端并**清理残留**（供 `serve`/`gdd` 启动等明确需要卫生工作的场合）。
    ///
    /// 清理失败不阻止启动：它只是尽力而为的卫生工作，
    /// 失败原因写入 stderr，功能仍可用（用户可手动重试 detach）。
    pub fn new_with_cleanup(dirs: DataDirs) -> Self {
        let mut this = Self::new(dirs);
        if let Err(err) = this.cleanup() {
            eprintln!(
                "gadgetdisk loop: failed to clean up leftover loop devices at startup (non-fatal): {}",
                err.message
            );
        }
        this
    }

    /// 数据目录。
    pub fn dirs(&self) -> &DataDirs {
        &self.dirs
    }

    /// 清掉属于本模块的**残留** loop 与挂载。
    ///
    /// 「残留」= 归属本模块且**未被记录为活跃附件**。已记录的活跃附件会传给
    /// `cleanup_stale` 作为 `keep`，因此这个调用不会拆掉别的进程正在用的挂载。
    pub fn cleanup(&mut self) -> Result<Vec<String>, LoopError> {
        self.registry = Registry::from_disk(&self.dirs);
        let keep: Vec<u32> = self
            .registry
            .entries
            .iter()
            .filter_map(|(_, _, dev)| gadgetdisk_loop::parse_loop_index(dev))
            .collect();
        self.inner
            .cleanup_stale(&self.dirs.images(), &self.dirs.mnt(), &keep)
    }

    /// 能力快照（loop 侧探测结果）。
    pub fn capabilities(&self) -> Capabilities {
        self.inner.capabilities().clone()
    }

    /// 当前全部 loop 附件。
    ///
    /// **取自内核真值**（`LOOP_GET_STATUS64` 的 `lo_file_name` / sysfs），
    /// 而不是 `LoopMounter` 的内存记录。两个理由：
    ///
    /// 1. gdd 需要用它判定互斥，而 loop 绑定可能是**另一个进程**建立的，
    ///    内存记录看不到；
    /// 2. 进程重启后内存记录为空，而内核里可能仍挂着上一次运行建立的 loop。
    ///
    /// 只报告后备文件位于本模块 `images/` 下的 loop——其他应用的 loop
    /// 与本模块无关，绝不能出现在这里（更不会被误释放）。
    pub fn attachments(&self) -> Vec<Attachment> {
        let images = self.dirs.images();
        let Ok(bound) = self.inner.loop_control().list_bound() else {
            return Vec::new();
        };

        bound
            .into_iter()
            .filter_map(|(index, status)| {
                let backing = status.backing_file?;
                if !backing.starts_with(&images) {
                    return None;
                }
                let name = backing.file_name()?.to_string_lossy().into_owned();
                Some(Attachment {
                    image: backing.display().to_string(),
                    loop_dev: gadgetdisk_loop::loopdev::loop_path(index)
                        .display()
                        .to_string(),
                    // 分区子设备需另行探测；此处留空表示「未知」，
                    // 而不是编造一个不存在的路径。
                    loop_part_devs: Vec::new(),
                    mountpoint: self.dirs.mnt().join(&name).display().to_string(),
                    read_only: status.read_only,
                })
            })
            .collect()
    }

    /// 把镜像挂到本地（**一次性操作，由 CLI 直接执行**）。
    ///
    /// ## 分区偏移的来源（顺序即优先级）
    ///
    /// 1. 调用方显式指定的分区序号 → 该分区的 `start_lba`；
    /// 2. 未指定 → **第一个**分区（分区表里的首个非空项）；
    /// 3. 无分区表（`raw`）或分区表读不出来 → 持久化缓存；
    /// 4. 缓存也没有 → `0`（整盘）。
    ///
    /// **为什么以分区表为准，而不是缓存**（已实测）：缓存只在 `create` 时写入。
    /// 用户**导入**的镜像（或从别处拷来的）没有缓存，旧实现于是回退到偏移 0 →
    /// 挂的是整盘而不是分区 → 内核以 `filesystem_unsupported`（EINVAL）失败，
    /// 也就是说**导入的镜像根本挂不上**。读分区表同时解决了这一点，并让
    /// 「选择挂哪个分区」成为可能（此前 `partition_index` 只在 partscan 路径
    /// 生效，而该路径在 Android 上不可用，等于被静默忽略）。
    pub fn attach(
        &mut self,
        image: &Path,
        read_only: bool,
        partition_index: Option<u32>,
    ) -> KernelResult<Attachment> {
        let Some(name) = image.file_name().and_then(|n| n.to_str()) else {
            return Err(KernelError::new(
                ErrorCode::InvalidArgument,
                "the image path has no file name",
            ));
        };
        let Some(mountpoint) = self.dirs.mountpoint(name) else {
            return Err(KernelError::new(
                ErrorCode::InvalidArgument,
                "cannot derive a mount point",
            ));
        };

        let (offset, resolved_index) = self.resolve_offset(image, name, partition_index)?;
        let mut req = AttachRequest::new(image, &mountpoint, read_only, offset);
        req.partition_index = resolved_index;

        // **挂载前与内核真值对账**。
        //
        // `serve` 为长期存活的按需服务进程，仅在初始化启动时执行过一次内核状态对账；
        // 若 loop 挂载随后由其他进程（如一次性 CLI 执行 `detach-loop`）释放，
        // 其内部内存记录将残留失效条目，导致后续 `attach` 挂载点占用检查误报：
        //
        // ```text
        // {"error":"busy","message":"the mount point is already in use: …"}
        // ```
        //
        // 而此时内核中 `list-loop` 与 `/proc/self/mounts` 均为空（真机实测缺陷）。
        // 对账同时将其他进程新建的挂载补充进记录，使 `detach` 能够正常寻址与清理。
        //
        // 失败不阻断挂载流程：读不到内核状态时，后续底层 ioctl 调用会给出权威错误。
        if let Err(err) = self.inner.reconcile(&self.dirs.images(), &self.dirs.mnt()) {
            eprintln!(
                "gadgetdisk loop: failed to reconcile the mount records (continuing): {}",
                err.message
            );
        }

        let mounted = self.inner.attach(&req).map_err(map_loop_error)?;
        let attachment = mounted.to_attachment(image);
        // 落盘登记「这是有意保持的活跃附件」，供其他进程的启动清理区分残留。
        self.registry
            .insert(image, &mountpoint, &attachment.loop_dev);
        self.registry.save(&self.dirs);
        Ok(attachment)
    }

    /// 读镜像的分区表（供 UI 选择分区）。
    pub fn partitions(&self, image: &Path) -> KernelResult<gadgetdisk_core::PartitionScan> {
        gadgetdisk_core::read_partitions(image).map_err(|err| {
            let code = match &err {
                gadgetdisk_core::CoreError::NotFound(_) => ErrorCode::ImageNotFound,
                gadgetdisk_core::CoreError::UnsupportedLayout(_) => ErrorCode::UnsupportedLayout,
                gadgetdisk_core::CoreError::Io(io)
                    if io.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    ErrorCode::PermissionDenied
                }
                _ => ErrorCode::Internal,
            };
            KernelError::new(code, err.to_string())
        })
    }

    /// 决定本次挂载用的分区偏移与分区序号。
    ///
    /// 优先级见 [`Self::attach`] 的文档。**分区序号不存在时明确报错**，而不是
    /// 静默回退到整盘——后者会让用户以为挂上了分区 2，实际挂的是别的区间。
    fn resolve_offset(
        &self,
        image: &Path,
        name: &str,
        requested: Option<u32>,
    ) -> KernelResult<(u64, u32)> {
        // 分区表读不出来不是致命错误：损坏的镜像仍应按缓存/整盘尝试，
        // 由 mount(2) 给出最终判断。
        let scan = gadgetdisk_core::read_partitions(image).ok();

        if let Some(scan) = scan {
            if scan.partitions.is_empty() {
                // 无分区表（raw 或无法识别）：整盘。
                return Ok((0, 1));
            }

            let index = requested.unwrap_or_else(|| scan.default_index().unwrap_or(1));
            let Some(entry) = scan.partitions.iter().find(|p| p.index == index) else {
                let available: Vec<String> = scan
                    .partitions
                    .iter()
                    .map(|p| p.index.to_string())
                    .collect();
                return Err(KernelError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "the image has no partition {index} (available partitions: {})",
                        available.join(", ")
                    ),
                ));
            };
            return Ok((entry.offset_bytes(), index));
        }

        // 回退：旧行为（缓存 → 0）。仅用于分区表不可读的情形。
        Ok((Offsets::get(&self.dirs, name), requested.unwrap_or(1)))
    }

    /// 释放指定镜像或 loop 设备的附件（**一次性操作，由 CLI 直接执行**）。
    ///
    /// **释放前同样与内核真值对账**：`detach` 依据 `self.mounted` 中的记录查找目标，
    /// 而长期存活的服务进程（`serve`）可能缺失其他进程新建的挂载——未对账将误报
    /// “未找到匹配的 loop 附件”，而内核中该设备依然处于挂载状态（与 `attach` 侧同一根因）。
    pub fn detach(
        &mut self,
        image: Option<&Path>,
        loop_dev: Option<&str>,
    ) -> KernelResult<Vec<String>> {
        if let Err(err) = self.inner.reconcile(&self.dirs.images(), &self.dirs.mnt()) {
            eprintln!(
                "gadgetdisk loop: failed to reconcile the mount records (continuing): {}",
                err.message
            );
        }

        let selector = match (image, loop_dev) {
            (Some(path), _) => {
                let mountpoint = self
                    .registry
                    .entries
                    .iter()
                    .find(|(i, _, _)| i == path)
                    .map(|(_, m, _)| m.clone())
                    .or_else(|| path.file_name().map(|name| self.dirs.mnt().join(name)))
                    .ok_or_else(|| {
                        KernelError::new(
                            ErrorCode::InvalidArgument,
                            format!("cannot derive a mount point from {}", path.display()),
                        )
                    })?;
                DetachSelector::Mountpoint(mountpoint)
            }
            (None, Some(dev)) => {
                let index = gadgetdisk_loop::parse_loop_index(dev).ok_or_else(|| {
                    KernelError::new(
                        ErrorCode::InvalidArgument,
                        format!("cannot parse the loop device name: {dev}"),
                    )
                })?;
                DetachSelector::Index(index)
            }
            (None, None) => DetachSelector::All,
        };

        let released = self.inner.detach(&selector).map_err(map_loop_error)?;

        // 同步登记表并落盘：`All` 清空，否则按寻址方式清理对应条目。
        match (image, loop_dev) {
            (Some(path), _) => self.registry.remove(path),
            (None, Some(dev)) => self.registry.remove_by_dev(dev),
            (None, None) => self.registry = Registry::default(),
        }
        self.registry.save(&self.dirs);
        Ok(released)
    }
}

/// 把 `LoopError` 映射为协议错误。
pub fn map_loop_error(err: LoopError) -> KernelError {
    KernelError::new(err.code, err.message)
}

impl LoopOps for LoopMounts {
    fn capabilities(&self) -> Capabilities {
        self.capabilities()
    }

    fn attachments(&self) -> Vec<Attachment> {
        self.attachments()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gadgetdisk_gdd::kernel::LoopOps;

    fn dirs(tag: &str) -> (DataDirs, PathBuf) {
        let root = crate::testutil::temp_dir(tag);
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        (dirs, root)
    }

    #[test]
    fn registry_tracks_and_forgets_entries() {
        let mut registry = Registry::default();
        let image = Path::new("/data/adb/gadget-disk/images/a.img");
        let mnt = Path::new("/data/adb/gadget-disk/mnt/a.img");

        assert!(registry.entries.is_empty());
        registry.insert(image, mnt, "/dev/block/loop52");
        assert_eq!(registry.entries.len(), 1);

        registry.remove(image);
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn registry_insert_replaces_same_image_or_mountpoint() {
        let mut registry = Registry::default();
        let image = Path::new("/x/a.img");
        let old = Path::new("/mnt/old");
        let new = Path::new("/mnt/new");

        registry.insert(image, old, "/dev/block/loop1");
        registry.insert(image, new, "/dev/block/loop2");
        assert_eq!(registry.entries.len(), 1, "同一镜像不应留下两条记录");
        assert_eq!(registry.entries[0].2, "/dev/block/loop2");
    }

    #[test]
    fn registry_can_remove_by_loop_device() {
        // `detach-loop --loop-dev` 按设备名寻址，登记表必须能按设备名删。
        let mut registry = Registry::default();
        registry.insert(
            Path::new("/x/a.img"),
            Path::new("/mnt/a"),
            "/dev/block/loop7",
        );
        registry.remove_by_dev("/dev/block/loop7");
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn registry_round_trips_through_disk() {
        // 这是「区分残留与在用」的关键：新进程必须能读到别的进程留下的记录。
        let (dirs, root) = dirs("loop-registry-disk");
        let mut registry = Registry::default();
        registry.insert(
            Path::new("/x/a.img"),
            Path::new("/mnt/a"),
            "/dev/block/loop9",
        );
        registry.save(&dirs);

        let reloaded = Registry::from_disk(&dirs);
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].2, "/dev/block/loop9");

        crate::testutil::cleanup(&root);
    }

    #[test]
    fn registry_from_disk_tolerates_missing_or_corrupt_file() {
        // 读不到就当作「没有记录」：最坏是多清理一次残留，而不是拒绝服务。
        let (dirs, root) = dirs("loop-registry-corrupt");
        assert!(Registry::from_disk(&dirs).entries.is_empty(), "文件不存在");

        std::fs::write(crate::cli_paths::loop_registry(&dirs), b"{not json").unwrap();
        assert!(Registry::from_disk(&dirs).entries.is_empty(), "内容损坏");

        crate::testutil::cleanup(&root);
    }

    #[test]
    fn listing_is_not_destructive() {
        // 回归：`LoopMounts::new` 曾无条件跑启动清理，于是 `list-loop`
        // 会先拆掉活跃挂载再返回空列表。`new()` 现在必须不做清理。
        let (dirs, root) = dirs("loop-listing-safe");
        let mounts = LoopMounts::new(dirs);
        // 只读路径不产生任何副作用：附件列表来自内核，此时为空。
        assert!(mounts.attachments().is_empty());
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn capabilities_are_exposed_through_the_read_only_trait() {
        // gdd 只拿得到 `LoopOps`（只读），互斥判定依赖它的 `attachments()`。
        // 这里断言 **trait 方法确实委派到固有实现**，而不是返回一个默认值。
        let (dirs, root) = dirs("loop-caps");
        let mounts = LoopMounts::new(dirs);

        assert_eq!(
            LoopOps::capabilities(&mounts),
            mounts.capabilities(),
            "trait 实现必须委派到固有方法"
        );
        assert_eq!(
            LoopOps::attachments(&mounts),
            mounts.attachments(),
            "trait 实现必须委派到固有方法"
        );

        crate::testutil::cleanup(&root);
    }

    #[test]
    fn offset_cache_falls_back_to_zero() {
        let (dirs, root) = dirs("loop-offset");
        // 缓存不存在 → 0（等价「无分区表」），不得 panic。
        assert_eq!(Offsets::get(&dirs, "nope.img"), 0);

        // 损坏内容同样是 0，而不是解析失败向上冒。
        std::fs::write(crate::cli_paths::offsets_json(&dirs), b"not-a-number").unwrap();
        assert_eq!(Offsets::get(&dirs, "bad.img"), 0);

        Offsets::set(&dirs, "ok.img", 1_048_576).unwrap();
        assert_eq!(Offsets::get(&dirs, "ok.img"), 1_048_576);

        crate::testutil::cleanup(&root);
    }

    #[test]
    fn attach_rejects_an_image_without_a_file_name() {
        // `/` 没有文件名：必须在触碰内核前就拒绝。
        let (dirs, root) = dirs("loop-attach-noname");
        let mut mounts = LoopMounts::new(dirs);
        let err = mounts.attach(Path::new("/"), false, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn detach_without_a_selector_means_all_and_reports_nothing_to_do() {
        // `None`/`None` 表示「全部释放」。本机（容器）没有属于本模块的
        // loop 附件，因此必须**明确报错**而不是静默成功——静默成功会让
        // 用户以为已释放，而实际什么都没做。
        let (dirs, root) = dirs("loop-detach-all");
        let mut mounts = LoopMounts::new(dirs);
        let err = mounts.detach(None, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::ImageNotFound, "应报告「没有可释放的」");
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn detach_with_a_bad_loop_device_name_is_rejected() {
        let (dirs, root) = dirs("loop-detach-bad");
        let mut mounts = LoopMounts::new(dirs);
        let err = mounts.detach(None, Some("not-a-loop")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn map_loop_error_preserves_code_and_message() {
        let err = LoopError::capability("内核不支持");
        let mapped = map_loop_error(err);
        assert_eq!(mapped.code, ErrorCode::LoopUnsupported);
        assert_eq!(mapped.message, "内核不支持");
    }

    /// **`attach` 必须在调用内核之前与内核真值对账**。
    ///
    /// ## 为什么是源码断言
    ///
    /// `LoopMounts` 内部是具体类型 `LoopMounter<RealLoopControl, RealMounter>`，
    /// 主机上没有 `/dev/loop-control`，因此「陈旧记录会不会误报 busy」在主机上
    /// **不可端到端观测**（该行为本身已由 `gadgetdisk-loop` 的
    /// `stale_records_do_not_make_attach_report_busy` 用替身钉住）。这里钉的是
    /// **接线**：对账确实发生在 `attach` 之前，而不是被漏掉或挪到后面。
    ///
    /// 真机实测的缺陷形态：`serve` 长期存活，loop 被一次性 CLI 拆掉后它仍持有
    /// 旧记录，`POST /api/v1/loop/attach` 回
    /// `{"error":"busy","message":"the mount point is already in use: …"}`，
    /// 而内核里 `list-loop` 与 `/proc/self/mounts` 都是空的。
    #[test]
    fn attach_reconciles_with_the_kernel_before_touching_it() {
        let source = include_str!("loop_adapter.rs");

        let start = source
            .find("    pub fn attach(")
            .expect("应能找到 LoopMounts::attach");
        let rest = &source[start..];
        let end = rest
            .find("\n    /// 读镜像的分区表")
            .expect("attach 后面应是 partitions");
        let body = &rest[..end];

        let reconcile = body
            .find("self.inner.reconcile(")
            .expect("attach 必须与内核真值对账（否则陈旧记录会误报 busy）");
        let attach = body
            .find("self.inner.attach(")
            .expect("attach 必须调用 inner.attach");
        assert!(
            reconcile < attach,
            "对账必须早于 inner.attach（占用判定在它内部）"
        );
        // 对账用的是**真实目录**，而不是猜出来的父目录。
        assert!(
            body.contains("self.dirs.images()") && body.contains("self.dirs.mnt()"),
            "对账必须传入 images/ 与 mnt/ 目录，归属判据才与别处一致"
        );
        // 对账失败不得阻断挂载：读不到内核状态时，后面的 ioctl 才是权威错误来源。
        assert!(
            body.contains("continuing"),
            "对账失败应只告警并继续，不阻断挂载"
        );
    }

    /// 造一个真实的镜像文件（用 core 的创建器）。
    fn make_image(dirs: &DataDirs, name: &str, layout: gadgetdisk_core::ImageLayout) -> PathBuf {
        let path = dirs.images().join(name);
        gadgetdisk_core::create_image(
            gadgetdisk_core::create::CreateOptions::new(&path)
                // 比下限大 4 MiB：有分区表的布局要在盘首（GPT 还含盘尾）
                // 留结构，恰好等于下限的镜像放不下一个下限大小的分区。
                .with_size(gadgetdisk_core::MIN_FAT32_BYTES + 4 * 1024 * 1024)
                .with_layout(layout),
            &crate::testutil::TestMkfsFormatter,
        )
        .unwrap();
        path
    }

    #[test]
    fn offset_comes_from_partition_table_not_the_cache() {
        // **回归**：缓存只在 create 时写入。用户**导入**的镜像没有缓存，旧实现
        // 于是回退到偏移 0 → 挂整盘而不是分区 → mount 以 filesystem_unsupported
        // 失败，也就是「导入的镜像根本挂不上」。
        let (dirs, root) = dirs("loop-offset-from-table");
        let image = make_image(&dirs, "imported.img", gadgetdisk_core::ImageLayout::Gpt);

        // 明确删掉缓存，模拟导入来的镜像。
        Offsets::remove(&dirs, "imported.img").ok();
        assert_eq!(Offsets::get(&dirs, "imported.img"), 0);

        let mounts = LoopMounts::new(dirs);
        let (offset, index) = mounts
            .resolve_offset(&image, "imported.img", None)
            .expect("分区表可读时必须能解析偏移");

        assert_eq!(index, 1);
        assert_eq!(
            offset, 1_048_576,
            "必须用分区表的 1 MiB 偏移，而不是缓存缺失导致的 0"
        );
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn explicit_partition_index_selects_that_partition() {
        let (dirs, root) = dirs("loop-offset-explicit");
        let image = make_image(&dirs, "one.img", gadgetdisk_core::ImageLayout::Gpt);
        let mounts = LoopMounts::new(dirs);

        let (offset, index) = mounts.resolve_offset(&image, "one.img", Some(1)).unwrap();
        assert_eq!((offset, index), (1_048_576, 1));
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn missing_partition_index_is_rejected_with_the_available_list() {
        // 旧实现里 `--partition 2` 在单分区镜像上**静默成功**（lo_offset 路径
        // 忽略该参数），用户以为挂了分区 2、实际挂的是别的区间。必须明确报错。
        let (dirs, root) = dirs("loop-offset-bad-index");
        let image = make_image(&dirs, "one.img", gadgetdisk_core::ImageLayout::Gpt);
        let mounts = LoopMounts::new(dirs);

        let err = mounts
            .resolve_offset(&image, "one.img", Some(2))
            .expect_err("不存在的分区必须报错");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.contains("no partition 2"), "{}", err.message);
        assert!(
            err.message.contains("available partitions: 1"),
            "{}",
            err.message
        );
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn raw_image_uses_whole_disk_offset() {
        let (dirs, root) = dirs("loop-offset-raw");
        let image = make_image(&dirs, "flat.img", gadgetdisk_core::ImageLayout::Raw);
        let mounts = LoopMounts::new(dirs);

        let (offset, index) = mounts.resolve_offset(&image, "flat.img", None).unwrap();
        assert_eq!(offset, 0, "raw 布局没有分区表，偏移必须是 0");
        assert_eq!(index, 1);
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn partitions_reports_the_created_layout() {
        let (dirs, root) = dirs("loop-partitions-scan");
        let image = make_image(&dirs, "scan.img", gadgetdisk_core::ImageLayout::Mbr);
        let mounts = LoopMounts::new(dirs);

        let scan = mounts.partitions(&image).unwrap();
        assert_eq!(scan.layout, gadgetdisk_core::ImageLayout::Mbr);
        assert_eq!(scan.partitions.len(), 1);
        assert_eq!(scan.partitions[0].offset_bytes(), 1_048_576);
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn partitions_on_a_missing_image_is_image_not_found() {
        let (dirs, root) = dirs("loop-partitions-missing");
        let mounts = LoopMounts::new(dirs);
        let err = mounts
            .partitions(Path::new("/nonexistent/nope.img"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ImageNotFound);
        crate::testutil::cleanup(&root);
    }

    #[test]
    fn logical_partition_resolves_to_its_absolute_offset() {
        // **闭环**：建一个含逻辑分区的 MBR 镜像，再经挂载适配器解析偏移。
        // 逻辑分区的偏移必须等于创建时报告的值——这是 loop 挂载真正吃到的
        // 数字（`LOOP_SET_STATUS64` 的 `lo_offset` 只认绝对字节偏移）。
        //
        // 本测试只关心**偏移与序号**，故用一个不落盘的格式化器：真正的 FAT32
        // 格式化由 `gadgetdisk-core` 的测试覆盖，这里依赖它只会让测试变慢且在
        // 缺少 mkfsvfat 二进制时无法运行。
        struct NoopFormatter;

        impl gadgetdisk_core::fs::Formatter for NoopFormatter {
            fn format(
                &self,
                _image_path: &Path,
                plan: &gadgetdisk_core::fs::FormatPlan,
            ) -> gadgetdisk_core::Result<gadgetdisk_core::fs::FormattedVolume> {
                Ok(gadgetdisk_core::fs::FormattedVolume {
                    filesystem: plan.filesystem,
                    label: plan.label.clone(),
                    tool: None,
                })
            }
        }

        let (dirs, root) = dirs("loop-logical-offset");
        let image = dirs.image_path("logical.img").unwrap();
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();

        let specs = vec![
            gadgetdisk_core::PartitionSpec::fill_remaining("P1")
                .with_size(64 * 1024 * 1024)
                .with_filesystem(gadgetdisk_core::partspec::PartitionFilesystem::None),
            gadgetdisk_core::PartitionSpec::fill_remaining("L1")
                .with_size(64 * 1024 * 1024)
                .with_kind(gadgetdisk_core::PartitionKind::Logical)
                .with_filesystem(gadgetdisk_core::partspec::PartitionFilesystem::None),
        ];
        let created = gadgetdisk_core::create_image(
            gadgetdisk_core::create::CreateOptions::new(&image)
                .with_size(512 * 1024 * 1024)
                .with_layout(gadgetdisk_core::ImageLayout::Mbr)
                .with_partitions(specs),
            &NoopFormatter,
        )
        .unwrap();

        // 序号 1（主）与 5（逻辑）。
        assert_eq!(
            created
                .partitions
                .iter()
                .map(|p| p.index)
                .collect::<Vec<_>>(),
            vec![1, 5]
        );

        let mounts = LoopMounts::new(dirs);
        let scan = mounts.partitions(&image).unwrap();
        assert_eq!(scan.partitions.len(), 2);
        assert_eq!(
            scan.partitions.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 5],
            "逻辑分区在扫描结果里必须是序号 5"
        );

        // 读回的偏移必须与创建时报告的一致。
        for (read, created) in scan.partitions.iter().zip(&created.partitions) {
            assert_eq!(
                read.offset_bytes(),
                created.offset_bytes,
                "分区 {} 的偏移读写不一致",
                read.index
            );
        }
        // 逻辑分区的偏移严格大于主分区，且不是 0。
        assert!(scan.partitions[1].offset_bytes() > scan.partitions[0].offset_bytes());
        assert_ne!(scan.partitions[1].offset_bytes(), 0);

        crate::testutil::cleanup(&root);
    }
}
