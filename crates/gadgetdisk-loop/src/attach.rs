//! 挂载与释放的编排：**本 crate 最需要测试的部分**。
//!
//! 规格见 [docs/ondevice-loop-mount.md](../../../../docs/ondevice-loop-mount.md)。
//! 这里集中了两条硬约束：
//!
//! 1. **调用顺序**：`LOOP_SET_FD` → `LOOP_SET_STATUS64` → `mount`；
//!    释放时必须 `sync` → `umount` → `LOOP_CLR_FD` → 校验。
//!    顺序错了好一点是报错，坏一点是**静默的数据损坏**
//!    （例如先清 fd 再 umount，内核会把已挂载的文件系统抽掉底层设备）。
//! 2. **单一挂载路径**：`lo_offset` 分区偏移。绝不静默失败，
//!    也绝不回退到自实现编辑器（非目标）。
//!
//! ## 为什么只有 `lo_offset`（2026-10-06 移除 partscan）
//!
//! 曾有第二条路径：`LO_FLAGS_PARTSCAN` 让**内核**解析分区表并派生
//! `loopNpM` 分区子设备。它在 Android 上**恒不可用**——没有 udev/devtmpfs，
//! 内核即使扫描出分区也不会在 `/dev` 下建节点（实测 `max_part = 7`，
//! 但 `loopNpM` 不存在，`mount` 报 `ENOENT`）。后果是每次挂载先白走一轮
//! 注定失败的 partscan（多分配一个 loop 设备再清理），才回退到偏移路径。
//! 更糟的是能力探测只能看到 `max_part > 0`，于是向用户宣称「支持」而实际
//! 必然回退——正是「未验证即标注」要防的乐观报告。
//!
//! 因此路径与报告字段一并删除，`lo_offset` 成为唯一路径。
//! **`LO_FLAGS_PARTSCAN` 位本身仍然保留**：偏移路径必须显式清掉它，
//! 否则内核会在偏移处再解析一次分区表并派生出错误的子设备。
//!
//! 顺序在主机上用 [`crate::loopdev::MemLoop`] +
//! [`crate::mount::MemMounter`] 断言，不需要真实 `/dev`。

use std::path::{Path, PathBuf};

use gadgetdisk_proto::{Attachment, Capabilities};

use crate::error::{LoopError, LoopResult};
use crate::loopdev::{LoopControl, loop_path};
use crate::mount::Mounter;

/// 一次挂载请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRequest {
    /// 镜像路径。
    pub image: PathBuf,
    /// 挂载点。
    pub mountpoint: PathBuf,
    /// 是否只读。
    pub read_only: bool,
    /// 分区起始偏移（字节）。`0` 表示无分区表（`raw` 布局）。
    pub partition_offset_bytes: u64,
    /// 要挂载的分区序号（`gpt`/`mbr` 下通常为 1）。
    pub partition_index: u32,
}

impl AttachRequest {
    /// 构造（分区序号默认 1）。
    pub fn new(
        image: impl Into<PathBuf>,
        mountpoint: impl Into<PathBuf>,
        read_only: bool,
        partition_offset_bytes: u64,
    ) -> Self {
        Self {
            image: image.into(),
            mountpoint: mountpoint.into(),
            read_only,
            partition_offset_bytes,
            partition_index: 1,
        }
    }

    /// 是否是带分区表的镜像。
    pub fn is_partitioned(&self) -> bool {
        self.partition_offset_bytes > 0
    }
}

/// 已挂载的 record：释放时需要它才能按正确顺序拆掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    /// loop 序号。
    pub index: u32,
    /// 挂载点。
    pub mountpoint: PathBuf,
    /// 实际挂载的设备。
    pub device: PathBuf,
    /// 是否只读。
    pub read_only: bool,
}

impl Mounted {
    /// 转成协议层的 [`Attachment`]。
    pub fn to_attachment(&self, image: &Path) -> Attachment {
        Attachment {
            image: image.display().to_string(),
            loop_dev: loop_path(self.index).display().to_string(),
            // 分区子设备只由 partscan 路径产生，该路径已移除；保留空值。
            loop_part_devs: Vec::new(),
            mountpoint: self.mountpoint.display().to_string(),
            read_only: self.read_only,
        }
    }
}

/// 把 loop 设备与 `mount(2)` 组合起来的编排器。
#[derive(Debug, Clone)]
pub struct LoopMounter<L, M> {
    loop_ctl: L,
    mounter: M,
    caps: Capabilities,
    /// 已挂载记录，键为 loop 序号。
    mounted: Vec<Mounted>,
}

impl<L: LoopControl, M: Mounter> LoopMounter<L, M> {
    /// 以已探测的能力构造。
    pub fn new(loop_ctl: L, mounter: M, caps: Capabilities) -> Self {
        Self {
            loop_ctl,
            mounter,
            caps,
            mounted: Vec::new(),
        }
    }

    /// 能力快照。
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// loop 控制器的只读引用（测试断言 trace 用）。
    pub fn loop_control(&self) -> &L {
        &self.loop_ctl
    }

    /// 挂载器的只读引用。
    pub fn mounter(&self) -> &M {
        &self.mounter
    }

    /// 当前由本编排器登记的挂载。
    pub fn mounted(&self) -> &[Mounted] {
        &self.mounted
    }

    /// 用**内核真值**重建「已挂载」记录。
    ///
    /// ## 为什么必需
    ///
    /// `self.mounted` 是进程内状态，而 M8 起每次 CLI 调用都是新进程，
    /// `detach` 也按镜像/挂载点在这些记录里找目标。不重建的话，新进程眼里
    /// 「什么都没有挂载」，于是 `detach-loop --image X` 永远报
    /// 「没有匹配的 loop 附件」——实测如此，且用户无法卸载自己的挂载。
    ///
    /// 只采纳后备文件位于 `images_dir` 之下的 loop：绝不把别人的 loop
    /// 拉进记录，否则 `detach All` 会去动不属于本模块的设备。
    ///
    /// 分区子设备需另行探测，这里留空（记成「未知」而不是编造一个路径）。
    ///
    /// ## 与 [`Self::reconcile`] 的分工
    ///
    /// 本方法**无条件以本次读到的内核状态为准**，用于进程启动等「内存记录本就
    /// 不可信」的场合（包括需要清空的场景）。长期存活的进程（`serve`）要用
    /// [`Self::reconcile`]：它额外保留「内核已被别人拆掉、但挂载表里仍在」的
    /// 记录，见那里的说明。
    pub fn sync_from_kernel(&mut self, images_dir: &Path, mnt_dir: &Path) -> LoopResult<usize> {
        let adopted = Self::scan_kernel(self.loop_ctl.list_bound()?, images_dir, mnt_dir);
        self.mounted = adopted;
        Ok(self.mounted.len())
    }

    /// 把 `list_bound` 的输出转成「属于本模块」的记录。
    ///
    /// 纯函数，供 [`Self::sync_from_kernel`] 与 [`Self::reconcile`] 共用，
    /// 保证两条路径的归属判据（`images/` 之下）永远一致。
    fn scan_kernel(
        bound: Vec<(u32, crate::loopdev::LoopStatus)>,
        images_dir: &Path,
        mnt_dir: &Path,
    ) -> Vec<Mounted> {
        bound
            .into_iter()
            .filter_map(|(index, status)| {
                let backing = status.backing_file?;
                if !backing.starts_with(images_dir) {
                    return None;
                }
                let name = backing.file_name()?;
                Some(Mounted {
                    index,
                    mountpoint: mnt_dir.join(name),
                    device: loop_path(index),
                    read_only: status.read_only,
                })
            })
            .collect()
    }

    /// **与内核真值对账**：修正内存记录里与内核/挂载表不一致的条目。
    ///
    /// ## 解决什么（真机实测缺陷）
    ///
    /// `serve` 为长期常驻服务进程，仅在**启动时**调用 [`Self::sync_from_kernel`] 一次。
    /// 若 loop 挂载随后由其他进程（如一次性 CLI 执行 `detach-loop`）释放，`serve`
    /// 内部将残留陈旧内存记录，导致后续 `POST /api/v1/loop/attach` 在
    /// [`Self::attach`] 的挂载点占用检查处误报：
    ///
    /// ```text
    /// {"error":"busy","message":"the mount point is already in use: …/mnt/disk.img"}
    /// ```
    ///
    /// 而此时内核中 `list-loop` 与 `/proc/self/mounts` 均为空——用户侧观察到
    /// “提示挂载点已被占用，但内核无任何活跃挂载”，且只能重启 `serve` 才能恢复。
    ///
    /// ## 双重判定规则
    ///
    /// 逐条校验 `mounted` 中的记录是否仍然成立：
    ///
    /// - **内核仍绑定该 loop 序号** → 记录有效（无论挂载表状态如何）；
    /// - **内核已解绑，但挂载表中仍有该挂载点** → 依然**保留**记录。
    ///   此逻辑为 [`Self::detach`] 能够正常寻址与清理的必要前提：`detach` 依据挂载点定位目标
    ///   并依序执行 `sync` → `umount` → `clear_fd`，而“loop 已被外部解绑、挂载点却还在”
    ///   正是其必须处理的边缘状态（例如外部直接调用 `losetup -d` 后）。若在此处剔除该记录，
    ///   用户将彻底无法通过本模块卸载该残留挂载点，从而引发更严重的资源泄露。
    /// - 两者均不成立 → 确认为陈旧失效记录，予以丢弃。
    ///
    /// 采纳方向同样重要：内核中**新出现**的、属于本模块的绑定须补充进记录，
    /// 否则长期存活的 `serve` 无法感知其他进程建立的挂载（`list-loop` 查询的是
    /// `attachments()` 的内核真值，暂时不受影响，但后续 `detach` 会找不到目标）。
    ///
    /// 返回被丢弃的陈旧记录数（诊断用）。
    pub fn reconcile(&mut self, images_dir: &Path, mnt_dir: &Path) -> LoopResult<usize> {
        let from_kernel = Self::scan_kernel(self.loop_ctl.list_bound()?, images_dir, mnt_dir);

        // 先丢弃：内核里已不存在该序号的记录，且挂载表里也没有对应挂载点。
        //
        // 「内核里不存在」用序号判定而非后备文件路径：序号是内核对同一个 loop
        // 设备的稳定标识，而后备文件路径可能因镜像被替换而变化。
        let before = self.mounted.len();
        self.mounted.retain(|m| {
            if from_kernel.iter().any(|k| k.index == m.index) {
                return true;
            }
            // 内核已解绑，但挂载点还在 → 留着让 `detach` 能拆掉它。
            self.mounter.is_mounted(&m.mountpoint)
        });
        let dropped = before - self.mounted.len();

        // 再补：内核里属于本模块、而记录里没有的绑定。
        for entry in from_kernel {
            if !self.mounted.iter().any(|m| m.index == entry.index) {
                self.mounted.push(entry);
            }
        }

        Ok(dropped)
    }

    /// 挂载一个镜像。
    ///
    /// 成功返回 [`Mounted`]；失败时**保证不留下半成品**
    /// （已设置的 loop 设备会被清掉，已挂的挂载点会先卸载）。
    pub fn attach(&mut self, req: &AttachRequest) -> LoopResult<Mounted> {
        // 能力前置检查：把「这台设备不支持」与「这次操作失败」分开报，
        // 因为前者重试无用，必须引导用户改走 USB 编辑路径。
        if !self.caps.loop_control {
            return Err(LoopError::capability(
                "/dev/loop-control is missing or cannot be opened: this kernel has no loop devices",
            ));
        }
        if self.caps.filesystems.is_empty() {
            return Err(LoopError::filesystem(
                "the kernel offers no block-device filesystem (no vfat/exfat in /proc/filesystems); local mounts are unavailable",
            ));
        }
        if !req.image.is_file() {
            return Err(LoopError::not_found(format!(
                "the image does not exist or is not a regular file: {}",
                req.image.display()
            )));
        }

        // 占用判定按内存记录；**长期常驻的服务进程必须先 [`Self::reconcile`]**，
        // 否则其他进程释放的挂载将在此处被误报为 `busy`。调用方（CLI 的
        // `LoopMounts`）持有 `images/` 与 `mnt/` 权威目录，故对账流程由其发起——
        // 本类型刻意不持有上述两路径，以避免与 `DataDirs` 产生状态漂移。
        if self.mounted.iter().any(|m| m.mountpoint == req.mountpoint) {
            return Err(LoopError::busy(format!(
                "the mount point is already in use: {}",
                req.mountpoint.display()
            )));
        }

        std::fs::create_dir_all(&req.mountpoint)
            .map_err(|err| LoopError::io("create mount point", &req.mountpoint, err))?;

        self.try_attach(req)
    }

    /// 执行一次完整挂载。
    ///
    /// 只有 `lo_offset` 一条路径：`raw` 镜像传偏移 `0`，带分区表的镜像传
    /// 解析出的分区偏移。**不再尝试 partscan**——理由见模块文档。
    fn try_attach(&mut self, req: &AttachRequest) -> LoopResult<Mounted> {
        // 必须**按挂载意图**选择打开方式（已实测）：
        // 内核在 `LOOP_SET_FD` 里检查后备文件是否可写，若以只读打开，
        // 它会自动置上 `LO_FLAGS_READ_ONLY`；随后以读写挂载该块设备
        // 会以 `EACCES` 失败——错误信息完全指不到真正的原因
        // （看起来像 SELinux，实际是打开模式）。
        let file = if req.read_only {
            std::fs::File::open(&req.image)
        } else {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&req.image)
        }
        .map_err(|err| LoopError::io("open image", &req.image, err))?;

        // 1. 取空闲 loop 设备。失败即能力问题，无需清理。
        let index = self.loop_ctl.get_free().map_err(|err| {
            LoopError::capability(format!(
                "/dev/loop-control allocation failed: {}",
                err.message
            ))
        })?;

        // 2. 绑定 fd。失败必须清掉 fd，否则会留下一个「绑定了镜像但没挂载」
        //    的 loop 设备，且因为 autoclear 未生效，它会一直占着镜像文件。
        let setup = (|| -> LoopResult<()> {
            self.loop_ctl.set_fd(index, &file)?;
            // 必须显式传 partscan=false：否则内核会在偏移处
            // 再解析一次分区表，派生出错误的子设备。
            self.loop_ctl
                .set_status64(index, req.partition_offset_bytes, req.read_only, false)?;
            Ok(())
        })();

        if let Err(err) = setup {
            // 清理：先 umount（可能已挂），再 clear_fd。
            self.mounter.umount(&req.mountpoint).ok();
            self.loop_ctl.clear_fd(index).ok();
            return Err(err);
        }

        let device = loop_path(index);

        // 4. 挂载。文件系统按内核能力挑：vfat 优先（FAT32 镜像）。
        let fstype = pick_filesystem(&self.caps);
        if let Err(err) = self
            .mounter
            .mount(&device, &req.mountpoint, fstype, req.read_only)
        {
            // 挂载失败也要拆干净：否则 loop 设备会一直被占用。
            self.mounter.umount(&req.mountpoint).ok();
            self.loop_ctl.clear_fd(index).ok();
            return Err(err);
        }

        // 5. 校验挂载确实生效。内核偶尔会「成功返回但没挂上」
        //    （例如挂载点被 bind mount 覆盖），不校验就会报告假成功。
        if !self.mounter.is_mounted(&req.mountpoint) {
            self.mounter.umount(&req.mountpoint).ok();
            self.loop_ctl.clear_fd(index).ok();
            return Err(LoopError::protocol(format!(
                "after mounting {}, the mount point did not appear in /proc/self/mounts",
                req.mountpoint.display()
            )));
        }

        // 6. 挂载成功后**才**开启 autoclear：此刻设备由挂载持有，
        //    置位不会再触发「无人持有即解绑」。失败不致命——
        //    启动清理仍是第一道保险。
        if let Err(err) = self.loop_ctl.set_autoclear(index) {
            eprintln!(
                "gadgetdisk loop: failed to enable autoclear on loop{index} (non-fatal): {}",
                err.message
            );
        }

        let mounted = Mounted {
            index,
            mountpoint: req.mountpoint.clone(),
            device: device.clone(),
            read_only: req.read_only,
        };
        self.mounted.push(mounted.clone());
        Ok(mounted)
    }

    /// 释放一个挂载，严格按 `sync` → `umount` → `clear_fd` → 校验 执行。
    ///
    /// `selector` 可以是 loop 序号、镜像路径或挂载点。
    pub fn detach(&mut self, selector: &DetachSelector) -> LoopResult<Vec<String>> {
        let targets: Vec<Mounted> = self
            .mounted
            .iter()
            .filter(|m| selector.matches(m))
            .cloned()
            .collect();

        if targets.is_empty() {
            return Err(LoopError::not_found(format!(
                "no matching loop attachment: {}",
                selector.describe()
            )));
        }

        let mut released = Vec::new();
        for target in &targets {
            self.release_one(target)?;
            released.push(loop_path(target.index).display().to_string());
        }
        self.mounted.retain(|m| !targets.contains(m));
        Ok(released)
    }

    /// 释放单条记录。顺序不可调整（见模块文档）。
    fn release_one(&mut self, target: &Mounted) -> LoopResult<()> {
        // 1. sync：必须在 umount 之前。若反过来，页缓存会在设备
        //    已经摘掉之后才回写，导致数据丢失。
        self.mounter.sync_all()?;

        // 2. umount。若因占用失败，必须立刻中止——继续 clear_fd
        //    会把底层设备从已挂载的文件系统下抽走。
        match self.mounter.umount(&target.mountpoint) {
            Ok(_) => {}
            Err(err) => {
                return Err(LoopError::busy(format!(
                    "{}: release aborted during umount (clear_fd did not run; data is safe)",
                    err.message
                )));
            }
        }

        // 3. clear_fd。
        self.loop_ctl.clear_fd(target.index)?;

        // 4. 校验：挂载点必须已消失、loop 设备必须已解除绑定。
        if self.mounter.is_mounted(&target.mountpoint) {
            return Err(LoopError::protocol(format!(
                "after release, {} is still in the mount table",
                target.mountpoint.display()
            )));
        }
        let status = self.loop_ctl.status(target.index)?;
        if status.is_bound() {
            return Err(LoopError::protocol(format!(
                "after release, loop{} is still bound to {}",
                target.index,
                status
                    .backing_file
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )));
        }
        Ok(())
    }

    /// 启动清理：拆掉属于**本模块**的**残留** loop 与挂载。
    ///
    /// 判定依据有两条，缺一不可：
    ///
    /// 1. 后备文件位于 `images_dir` 之下 —— 归属判据，**绝不**触碰非本模块的
    ///    loop 设备（规格明确要求）；
    /// 2. 该 loop **序号不在 `keep` 里** —— 活跃判据。
    ///
    /// 第二条是必需的，不是保守：M8 起 loop 挂载归 CLI，每次一次性调用都会新建
    /// `LoopMounter` 并（曾）执行一次「启动清理」。没有一个进程内的东西能区分
    /// 「上次崩溃的残留」与「刚刚由另一个进程建立的活跃挂载」——只看归属会把
    /// **正在用的**挂载拆掉（实测：`list-loop` 因为自身的启动清理把刚 attach 的
    /// 设备解绑，于是永远回报空列表，`detach-loop` 也再也找不到它）。
    /// 调用方因此必须把「已记录的活跃附件」传进来。
    pub fn cleanup_stale(
        &mut self,
        images_dir: &Path,
        mnt_dir: &Path,
        keep: &[u32],
    ) -> LoopResult<Vec<String>> {
        let bound = self.loop_ctl.list_bound()?;
        let mut cleaned = Vec::new();

        for (index, status) in bound {
            let Some(backing) = status.backing_file.as_deref() else {
                continue;
            };
            if !backing.starts_with(images_dir) {
                // 别人的 loop 设备：绝不触碰。
                continue;
            }
            if keep.contains(&index) {
                // 已记录的活跃附件：它是**在用**的，不是残留。
                continue;
            }

            // 卸掉 mnt/ 下所有指向该设备的挂载。挂载点名字取自
            // 镜像文件名；即使镜像已被删除也能推出候选路径。
            if let Some(name) = backing.file_name() {
                let mountpoint = mnt_dir.join(name);
                self.mounter.sync_all()?;
                self.mounter.umount(&mountpoint).ok();
            }

            if self.loop_ctl.clear_fd(index)? {
                cleaned.push(loop_path(index).display().to_string());
            }
        }

        // 清掉 mnt/ 下任何仍然存在的挂载（镜像文件已被删除、
        // 因此无法从 backing file 推出名字的残留）。
        if let Ok(entries) = std::fs::read_dir(mnt_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if self.mounter.is_mounted(&path) {
                    self.mounter.sync_all()?;
                    self.mounter.umount(&path).ok();
                    cleaned.push(format!("umount:{}", path.display()));
                }
            }
        }

        self.mounted.clear();
        Ok(cleaned)
    }
}

/// 给 `mount(2)` 用的文件系统类型。
///
/// 镜像由 `gadgetdisk-core` 创建，目前只有 FAT32，
/// 因此优先 `vfat`；内核只提供 `exfat` 时用它兜底
/// （将来支持 exFAT 镜像时无需改这里）。
fn pick_filesystem(caps: &Capabilities) -> &'static str {
    if caps.filesystems.iter().any(|f| f == "vfat") {
        "vfat"
    } else {
        "exfat"
    }
}

/// 释放时的选择器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachSelector {
    /// 按 loop 序号。
    Index(u32),
    /// 按挂载点。
    Mountpoint(PathBuf),
    /// 全部。
    All,
}

impl DetachSelector {
    fn matches(&self, m: &Mounted) -> bool {
        match self {
            DetachSelector::Index(i) => m.index == *i,
            DetachSelector::Mountpoint(p) => m.mountpoint == *p,
            DetachSelector::All => true,
        }
    }

    fn describe(&self) -> String {
        match self {
            DetachSelector::Index(i) => format!("loop{i}"),
            DetachSelector::Mountpoint(p) => p.display().to_string(),
            DetachSelector::All => "all".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopdev::MemLoop;
    use crate::mount::MemMounter;
    use gadgetdisk_proto::ErrorCode;

    /// 构造一套可用的能力。
    ///
    /// `max_part` 固定为 7（AVD 实测值）：**它不再影响挂载路径**，
    /// 仅用于 loop 设备次设备号的推导。显式给出实测值可防止
    /// 「哪天有人又拿它去分支」而不被测试发现。
    fn caps() -> Capabilities {
        Capabilities {
            loop_control: true,
            max_part: 7,
            filesystems: vec!["vfat".into()],
            mass_storage_supported: false,
            selinux_enforcing: false,
        }
    }

    /// 建一个测试用镜像文件与挂载点。
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = crate::testutil::temp_dir(tag);
        let image = root.join("a.img");
        std::fs::write(&image, vec![0u8; 4096]).unwrap();
        let mnt = root.join("mnt");
        let mnt = mnt.join("a.img");
        (root, image, mnt)
    }

    type TestMounter = LoopMounter<MemLoop, MemMounter>;

    fn mounter() -> TestMounter {
        LoopMounter::new(MemLoop::new(), MemMounter::new(), caps())
    }

    #[test]
    fn offset_strategy_sets_fd_then_status_then_mount() {
        let (_root, image, mnt) = fixture("loop-order");
        let mut lm = mounter();

        let req = AttachRequest::new(&image, &mnt, false, 1_048_576);
        lm.attach(&req).unwrap();

        assert_eq!(
            lm.loop_control().trace,
            vec![
                "get_free".to_string(),
                "set_fd:0:4096".to_string(),
                // 偏移必须真的传进内核，且 partscan 必须为 false。
                "set_status64:0:1048576:false:false".to_string(),
                // autoclear 只在挂载成功之后才置位。
                "set_autoclear:0".to_string(),
            ]
        );
        assert_eq!(
            lm.mounter().trace,
            vec![
                [
                    "mount:/dev/block/loop0:",
                    &mnt.display().to_string(),
                    ":vfat:false",
                ]
                .concat()
            ]
        );
        assert!(lm.mounter().is_mounted(&mnt));
    }

    #[test]
    fn partitioned_image_allocates_loop_device_once() {
        // **这是移除 partscan 的可测量改进**。
        //
        // 此前：`max_part > 0` 时会先尝试 partscan → 分配 loop0 → 发现
        // `loop0p1` 不存在 → 清理 → 回退分配 loop1 → 才挂上。每次挂载
        // 白付一轮注定失败的 ioctl 与一次设备分配/清理。
        //
        // 现在：无论 `max_part` 多大，有分区表也只分配**一次**。
        let (_root, image, mnt) = fixture("loop-single-alloc");
        let mut lm = mounter(); // caps().max_part == 7

        let req = AttachRequest::new(&image, &mnt, false, 1_048_576);
        assert!(req.is_partitioned());
        let mounted = lm.attach(&req).unwrap();

        assert_eq!(mounted.index, 0, "有分区表时仍只用第一个 loop 设备");
        assert_eq!(
            lm.loop_control()
                .trace
                .iter()
                .filter(|l| *l == "get_free")
                .count(),
            1,
            "只应分配一次 loop 设备，不得先分配再清理"
        );
        assert!(
            !lm.loop_control()
                .trace
                .iter()
                .any(|l| l.starts_with("clear_fd")),
            "成功挂载路径不应出现清理"
        );
        // 必须挂整盘 loop 设备 + 偏移，而不是分区子设备。
        assert!(lm.mounter().trace[0].starts_with("mount:/dev/block/loop0:"));
        assert!(
            !lm.mounter().trace.iter().any(|l| l.contains("loop0p")),
            "不得再引用分区子设备"
        );
    }

    #[test]
    fn max_part_never_changes_the_mount_path() {
        // 回归守卫：`max_part` 曾决定是否尝试 partscan。移除该路径后
        // 它**必须**与挂载行为无关——否则说明有人重新引入了分支。
        let (_root, image, mnt) = fixture("loop-maxpart-irrelevant");

        for max_part in [0u32, 7, 64] {
            let mut c = caps();
            c.max_part = max_part;
            let mut lm = LoopMounter::new(MemLoop::new(), MemMounter::new(), c);

            let req = AttachRequest::new(&image, &mnt, false, 1_048_576);
            let mounted = lm.attach(&req).unwrap();
            assert_eq!(mounted.index, 0, "max_part={max_part} 时仍应走偏移路径");
            assert_eq!(
                lm.loop_control().trace,
                vec![
                    "get_free".to_string(),
                    "set_fd:0:4096".to_string(),
                    "set_status64:0:1048576:false:false".to_string(),
                    "set_autoclear:0".to_string(),
                ],
                "max_part={max_part} 不应改变调用序列"
            );
            lm.detach(&DetachSelector::All).unwrap();
        }
    }

    #[test]
    fn attach_failure_clears_the_loop_device() {
        let (_root, image, mnt) = fixture("loop-attach-fail");
        let mut m = MemMounter::new();
        m.fail_mount = true;
        let mut lm = LoopMounter::new(MemLoop::new(), m, caps());

        let req = AttachRequest::new(&image, &mnt, false, 0);
        let err = lm.attach(&req).unwrap_err();
        assert_eq!(err.code, ErrorCode::FilesystemUnsupported);

        // 挂载失败必须拆掉 loop 设备，否则镜像被永久占用。
        assert!(lm.loop_control().trace.contains(&"clear_fd:0".to_string()));
        assert!(lm.loop_control().bound().is_empty());
        // 也不能留下半挂载。
        assert!(!lm.mounter().is_mounted(&mnt));
    }

    #[test]
    fn status_failure_clears_the_loop_device() {
        let (_root, image, mnt) = fixture("loop-status-fail");
        let lo = MemLoop::new().failing_on(0, "set_status64");
        let mut lm = LoopMounter::new(lo, MemMounter::new(), caps());

        let req = AttachRequest::new(&image, &mnt, false, 0);
        assert!(lm.attach(&req).is_err());
        // set_fd 已成功，set_status64 失败：必须回到未绑定状态。
        assert!(lm.loop_control().bound().is_empty());
    }

    #[test]
    fn attach_rejects_missing_image_before_touching_the_kernel() {
        let (_root, _image, mnt) = fixture("loop-missing");
        let mut lm = mounter();

        let req = AttachRequest::new("/nonexistent-xyz.img", &mnt, false, 0);
        let err = lm.attach(&req).unwrap_err();
        assert_eq!(err.code, ErrorCode::ImageNotFound);
        // 关键：绝不能先分配 loop 设备再发现镜像不存在。
        assert!(lm.loop_control().trace.is_empty());
    }

    #[test]
    fn attach_rejects_when_loop_control_is_unavailable() {
        let (_root, image, mnt) = fixture("loop-nocontrol");
        let mut c = caps();
        c.loop_control = false;
        let mut lm = LoopMounter::new(MemLoop::new(), MemMounter::new(), c);

        let req = AttachRequest::new(&image, &mnt, false, 0);
        let err = lm.attach(&req).unwrap_err();
        assert_eq!(err.code, ErrorCode::LoopUnsupported);
        // 能力缺失时必须区分于操作失败：消息要能引导走 USB 路径。
        assert!(err.message.contains("loop-control"));
    }

    #[test]
    fn attach_rejects_when_no_filesystem_is_available() {
        let (_root, image, mnt) = fixture("loop-nofs");
        let mut c = caps();
        c.filesystems.clear();
        let mut lm = LoopMounter::new(MemLoop::new(), MemMounter::new(), c);

        let req = AttachRequest::new(&image, &mnt, false, 0);
        let err = lm.attach(&req).unwrap_err();
        assert_eq!(err.code, ErrorCode::FilesystemUnsupported);
    }

    #[test]
    fn attach_rejects_a_mountpoint_already_in_use() {
        let (_root, image, mnt) = fixture("loop-busy");
        let mut lm = mounter();
        let req = AttachRequest::new(&image, &mnt, false, 0);
        lm.attach(&req).unwrap();

        let err = lm.attach(&req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
    }

    #[test]
    fn detach_runs_sync_then_umount_then_clear_fd() {
        let (_root, image, mnt) = fixture("loop-detach-order");
        let mut lm = mounter();
        let req = AttachRequest::new(&image, &mnt, false, 0);
        let mounted = lm.attach(&req).unwrap();
        let before = lm.mounter().trace.len();

        let released = lm.detach(&DetachSelector::Index(mounted.index)).unwrap();
        assert_eq!(released, vec!["/dev/block/loop0".to_string()]);

        // 释放三步的顺序是硬约束：sync 必须在 umount 之前
        // （否则页缓存在设备摘掉后才回写），umount 必须在 clear_fd 之前
        // （否则内核会把底层设备从已挂载的文件系统下抽走）。
        let trace = &lm.mounter().trace[before..];
        assert_eq!(trace[0], "sync");
        assert!(trace[1].starts_with("umount:"));
        let lo_trace = lm.loop_control().trace.clone();
        let sync_pos = lm.mounter().trace.len();
        assert!(sync_pos > before);
        assert_eq!(
            lo_trace.last().map(String::as_str),
            Some("clear_fd:0"),
            "clear_fd 必须是最后一步"
        );
        assert!(lm.loop_control().bound().is_empty());
    }

    #[test]
    fn detach_stops_before_clear_fd_when_umount_is_busy() {
        let (_root, image, mnt) = fixture("loop-detach-busy");
        let mut lm = mounter();
        let req = AttachRequest::new(&image, &mnt, false, 0);
        lm.attach(&req).unwrap();

        // 模拟挂载点被占用。
        lm.mounter.fail_umount_busy = true;
        let err = lm.detach(&DetachSelector::All).unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
        // 最关键的一条：umount 失败后**绝不能**执行 clear_fd，
        // 否则底层设备被抽走，已挂载的文件系统立刻不一致。
        assert!(
            !lm.loop_control()
                .trace
                .iter()
                .any(|t| t.starts_with("clear_fd")),
            "umount 失败后不得 clear_fd"
        );
        // 记录仍然保留，用户修好占用后可以重试。
        assert_eq!(lm.mounted().len(), 1);
    }

    #[test]
    fn detach_without_match_reports_not_found() {
        let (_root, _image, _mnt) = fixture("loop-detach-none");
        let mut lm = mounter();
        let err = lm.detach(&DetachSelector::Index(9)).unwrap_err();
        assert_eq!(err.code, ErrorCode::ImageNotFound);
    }

    #[test]
    fn sync_from_kernel_makes_detach_work_in_a_fresh_process() {
        // 回归：`mounted` 是进程内状态，而每次 CLI 调用都是新进程。
        // 不重建的话，新进程眼里「什么都没挂载」，detach 永远报「没有匹配的
        // loop 附件」——用户无法卸载自己的挂载（实测）。
        let root = crate::testutil::temp_dir("loop-sync-kernel");
        let images = root.join("images");
        let mnt = root.join("mnt");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&mnt).unwrap();

        let ours = images.join("ours.img");
        let lo = MemLoop::new()
            .with_bound(0, &ours.display().to_string(), 0)
            .with_bound(1, "/data/local/tmp/not-ours.img", 0);
        let mut lm = LoopMounter::new(lo, MemMounter::new(), caps());

        // 模拟「另一个进程刚建立的附件」：本进程记录为空。
        assert!(lm.mounted().is_empty());
        let adopted = lm.sync_from_kernel(&images, &mnt).unwrap();

        assert_eq!(adopted, 1, "只应采纳本模块的 loop");
        assert_eq!(lm.mounted()[0].index, 0);
        assert_eq!(lm.mounted()[0].mountpoint, mnt.join("ours.img"));

        // 现在按镜像卸载必须能找到目标。
        let released = lm.detach(&DetachSelector::Mountpoint(mnt.join("ours.img")));
        assert!(released.is_ok(), "重建后必须能卸载：{released:?}");
    }

    #[test]
    fn cleanup_only_touches_loop_devices_backed_by_our_images() {
        let root = crate::testutil::temp_dir("loop-cleanup");
        let images = root.join("images");
        let mnt = root.join("mnt");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&mnt).unwrap();

        // loop0 是我们的；loop1 是别人的（不在 images/ 下）。
        let lo = MemLoop::new()
            .with_bound(0, &images.join("ours.img").display().to_string(), 0)
            .with_bound(1, "/data/local/tmp/someone-else.img", 0);
        let mounter = MemMounter::new().with_mounted(&mnt.join("ours.img").display().to_string());
        let mut lm = LoopMounter::new(lo, mounter, caps());

        // 无活跃附件（keep 为空）→ 两个都属于「残留」范畴，但只有我们的被清理。
        let cleaned = lm.cleanup_stale(&images, &mnt, &[]).unwrap();
        assert_eq!(cleaned, vec!["/dev/block/loop0".to_string()]);

        // 别人的设备必须完好无损。
        let bound = lm.loop_control().bound();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].0, 1);
    }

    #[test]
    fn cleanup_never_touches_a_loop_recorded_as_live() {
        // 这是回归测试：M8 起 loop 挂载归一次性 CLI，每次调用都会新建
        // LoopMounter。若「启动清理」不区分残留与在用，它会把别的进程正在用的
        // 挂载拆掉——实测表现为 `list-loop` 永远返回空列表。
        let root = crate::testutil::temp_dir("loop-cleanup-live");
        let images = root.join("images");
        let mnt = root.join("mnt");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&mnt).unwrap();

        let lo = MemLoop::new()
            .with_bound(0, &images.join("live.img").display().to_string(), 0)
            .with_bound(1, &images.join("stale.img").display().to_string(), 0);
        let mounter = MemMounter::new()
            .with_mounted(&mnt.join("live.img").display().to_string())
            .with_mounted(&mnt.join("stale.img").display().to_string());
        let mut lm = LoopMounter::new(lo, mounter, caps());

        // loop0 被登记为活跃附件 → 必须原封不动。
        let cleaned = lm.cleanup_stale(&images, &mnt, &[0]).unwrap();
        assert_eq!(cleaned, vec!["/dev/block/loop1".to_string()]);

        let bound = lm.loop_control().bound();
        assert_eq!(bound.len(), 1, "活跃附件必须仍在");
        assert_eq!(bound[0].0, 0);
        assert!(
            lm.mounter().is_mounted(&mnt.join("live.img")),
            "活跃附件的挂载点不得被卸载"
        );
    }

    #[test]
    fn cleanup_umounts_leftover_mountpoints_even_without_a_backing_file() {
        let root = crate::testutil::temp_dir("loop-cleanup-orphan");
        let images = root.join("images");
        let mnt = root.join("mnt");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&mnt).unwrap();
        let orphan = mnt.join("deleted.img");
        // 真实残留挂载点必然是个目录（mount(2) 不会凭空创建它）。
        std::fs::create_dir_all(&orphan).unwrap();

        let mounter = MemMounter::new().with_mounted(&orphan.display().to_string());
        let mut lm = LoopMounter::new(MemLoop::new(), mounter, caps());

        let cleaned = lm.cleanup_stale(&images, &mnt, &[]).unwrap();
        assert_eq!(cleaned, vec![format!("umount:{}", orphan.display())]);
        assert!(!lm.mounter().is_mounted(&orphan));
    }

    // ------------------------------------------------ 与内核真值对账（长期存活进程）

    /// **回归（真机实测）**：`serve` 持有别的进程早已拆掉的记录时，
    /// `attach` 会误报 `busy`，而内核里没有任何挂载。
    #[test]
    fn stale_records_do_not_make_attach_report_busy() {
        let (root, image, mnt) = fixture("loop-stale-busy");
        let images = root.join("images");
        std::fs::create_dir_all(&images).unwrap();

        // 构造「serve 启动时同步过一次」的局面：内存记录里有这条挂载。
        let mut lm = mounter();
        lm.sync_from_kernel(&images, &root.join("mnt")).unwrap();
        // 但那个时刻内核里其实有绑定 → 先手工塞进一条记录，模拟启动时的快照。
        lm.mounted.push(Mounted {
            index: 7,
            mountpoint: mnt.clone(),
            device: loop_path(7),
            read_only: false,
        });

        // 随后**另一个进程**把它拆了：MemLoop 里没有 loop7，MemMounter 里也没有挂载点。
        // 于是「陈旧记录」成立，`reconcile` 必须丢掉它。
        let dropped = lm.reconcile(&images, &root.join("mnt")).unwrap();
        assert_eq!(dropped, 1, "内核与挂载表都没有 → 陈旧记录必须被丢弃");
        assert!(lm.mounted().is_empty());

        // 关键断言：此时挂载同一个镜像**不得**再报 busy。
        let req = AttachRequest::new(&image, &mnt, false, 0);
        let mounted = lm.attach(&req).expect("陈旧记录不该让挂载失败");
        assert_eq!(mounted.mountpoint, mnt);
    }

    /// 反向保护：**内核已解绑但挂载点仍在**的记录必须保留。
    ///
    /// 这是 `detach` 的硬前提（它按挂载点找目标并执行 sync → umount → clear_fd）。
    /// 若对账把这条也删掉，用户就再也无法卸载这个残留挂载点——修好一个错误、
    /// 引入一个更糟的。
    #[test]
    fn reconcile_keeps_a_live_mountpoint_even_after_the_loop_is_gone() {
        let (root, _image, mnt) = fixture("loop-reconcile-orphan");
        let images = root.join("images");
        std::fs::create_dir_all(&images).unwrap();

        // 内核里**没有** loop 绑定（MemLoop 为空），但挂载表里挂着这个点。
        let mounter = MemMounter::new().with_mounted(&mnt.display().to_string());
        let mut lm = LoopMounter::new(MemLoop::new(), mounter, caps());
        lm.mounted.push(Mounted {
            index: 3,
            mountpoint: mnt.clone(),
            device: loop_path(3),
            read_only: false,
        });

        let dropped = lm.reconcile(&images, &root.join("mnt")).unwrap();
        assert_eq!(dropped, 0, "挂载点还在 → 绝不是陈旧记录");
        assert_eq!(lm.mounted().len(), 1);

        // 而且必须真的能卸掉它（这正是保留它的理由）。
        let released = lm.detach(&DetachSelector::Mountpoint(mnt.clone()));
        assert!(released.is_ok(), "残留挂载点必须可被卸载：{released:?}");
        assert!(!lm.mounter().is_mounted(&mnt));
    }

    /// 采纳方向：别的进程**新建**的挂载必须被长期存活进程看见。
    #[test]
    fn reconcile_adopts_bindings_created_by_another_process() {
        let root = crate::testutil::temp_dir("loop-reconcile-adopt");
        let images = root.join("images");
        let mnt_dir = root.join("mnt");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&mnt_dir).unwrap();

        let ours = images.join("ours.img");
        let lo = MemLoop::new()
            .with_bound(5, &ours.display().to_string(), 0)
            // 别人的 loop：绝不能进记录，否则 `detach All` 会去动它。
            .with_bound(6, "/data/local/tmp/not-ours.img", 0);
        let mut lm = LoopMounter::new(lo, MemMounter::new(), caps());
        assert!(lm.mounted().is_empty(), "本进程记录初始为空");

        let dropped = lm.reconcile(&images, &mnt_dir).unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(lm.mounted().len(), 1, "只采纳本模块的绑定");
        assert_eq!(lm.mounted()[0].index, 5);
        assert_eq!(lm.mounted()[0].mountpoint, mnt_dir.join("ours.img"));
    }

    /// `sync_from_kernel` 仍然**无条件以内核为准**（用于进程启动）。
    ///
    /// 它与 `reconcile` 的分工必须有行为差异，否则两者可以互相替换，
    /// 「启动时清空重来」的语义就丢了。
    #[test]
    fn sync_from_kernel_still_resets_to_the_kernel_truth() {
        let (root, _image, mnt) = fixture("loop-sync-resets");
        let images = root.join("images");
        std::fs::create_dir_all(&images).unwrap();

        // 挂载表里挂着、内核里没有 → `reconcile` 会保留，而 `sync_from_kernel` 必须清掉。
        let mounter = MemMounter::new().with_mounted(&mnt.display().to_string());
        let mut lm = LoopMounter::new(MemLoop::new(), mounter, caps());
        lm.mounted.push(Mounted {
            index: 3,
            mountpoint: mnt.clone(),
            device: loop_path(3),
            read_only: false,
        });

        let adopted = lm.sync_from_kernel(&images, &root.join("mnt")).unwrap();
        assert_eq!(adopted, 0);
        assert!(
            lm.mounted().is_empty(),
            "sync 是「以内核为准」，不保留仅存在于挂载表的记录"
        );
    }

    #[test]
    fn pick_filesystem_prefers_vfat_and_falls_back_to_exfat() {
        assert_eq!(pick_filesystem(&caps()), "vfat");
        let mut c = caps();
        c.filesystems = vec!["exfat".into()];
        assert_eq!(pick_filesystem(&c), "exfat");
    }

    #[test]
    fn attach_rejects_when_kernel_reports_no_mount_after_success() {
        let (_root, image, mnt) = fixture("loop-fake-success");
        // 模拟内核「成功返回但没真的挂上」。
        let mounter = LyingMounter;
        let mut lm = LoopMounter::new(MemLoop::new(), mounter, caps());

        let req = AttachRequest::new(&image, &mnt, false, 0);
        let err = lm.attach(&req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("mounts"));
        // 假成功也必须清理干净。
        assert!(lm.loop_control().bound().is_empty());
    }

    /// 一个 `mount` 声称成功但 `is_mounted` 恒为 false 的替身。
    #[derive(Debug)]
    struct LyingMounter;

    impl Mounter for LyingMounter {
        fn mount(
            &mut self,
            _source: &Path,
            _target: &Path,
            _fstype: &str,
            _read_only: bool,
        ) -> LoopResult<()> {
            Ok(())
        }

        fn umount(&mut self, _target: &Path) -> LoopResult<bool> {
            Ok(false)
        }

        fn sync_all(&mut self) -> LoopResult<()> {
            Ok(())
        }

        fn is_mounted(&self, _target: &Path) -> bool {
            false
        }
    }
}
