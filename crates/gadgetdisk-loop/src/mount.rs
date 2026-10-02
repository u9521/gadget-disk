//! `mount(2)` / `umount(2)` 的抽象。
//!
//! ## 为什么不用 `mount` 命令
//!
//! Android 的 `mount` 由 toybox 提供，行为随版本漂移（选项解析差异、
//! 某些版本不带 `-t` 的自动探测）。`mount(2)` 是稳定 ABI，
//! 且能让错误码精确对应到 errno。
//!
//! ## 挂载命名空间
//!
//! 规格（[docs/ondevice-loop-mount.md](../../../../docs/ondevice-loop-mount.md)）
//! 要求默认在 init 的全局 mount namespace 中执行（由 root 管理器经 `su -M` 在外层保障），
//! 以最大化第三方可见性。本模块专注于在当前执行命名空间内调用 `mount(2)` 与 `umount2(2)`。

use std::path::{Path, PathBuf};

use crate::error::{LoopError, LoopResult};

/// 挂载操作。
pub trait Mounter {
    /// 挂载 `source` 到 `target`，文件系统类型 `fstype`。
    ///
    /// `read_only` 对应 `MS_RDONLY`。
    fn mount(
        &mut self,
        source: &Path,
        target: &Path,
        fstype: &str,
        read_only: bool,
    ) -> LoopResult<()>;

    /// 卸载 `target`。目标本就未挂载时返回 `false`（幂等）。
    fn umount(&mut self, target: &Path) -> LoopResult<bool>;

    /// 刷出页缓存。
    ///
    /// 释放顺序的第一步。内核的 `sync()` 是全局的；本模块只暴露它，
    /// 由 [`crate::attach`] 决定何时调用。
    fn sync_all(&mut self) -> LoopResult<()>;

    /// `target` 当前是否已挂载。
    fn is_mounted(&self, target: &Path) -> bool;
}

/// 直接调用 `mount(2)` / `umount2(2)` 的实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealMounter;

impl RealMounter {
    pub fn new() -> Self {
        Self
    }
}

impl Mounter for RealMounter {
    fn mount(
        &mut self,
        source: &Path,
        target: &Path,
        fstype: &str,
        read_only: bool,
    ) -> LoopResult<()> {
        let mut flags = rustix::mount::MountFlags::empty();
        if read_only {
            flags |= rustix::mount::MountFlags::RDONLY;
        }
        // 不使用 MS_NOSUID/MS_NODEV 等加固标志：目标是给用户编辑的
        // FAT32 卷，vfat 驱动本身不支持这些语义，传了会被内核忽略或拒绝，
        // 反而制造「参数非法」的假象。
        rustix::mount::mount(source, target, fstype, flags, None::<&std::ffi::CStr>).map_err(
            |err| {
                let raw = std::io::Error::from(err);
                let (err_code, hint) = match raw.raw_os_error() {
                    Some(libc::ENODEV) => (
                        gadgetdisk_proto::ErrorCode::FilesystemUnsupported,
                        "the kernel does not support that filesystem",
                    ),
                    Some(libc::EBUSY) => {
                        (gadgetdisk_proto::ErrorCode::Busy, "the mount point is busy")
                    }
                    Some(libc::EPERM) | Some(libc::EACCES) => (
                        gadgetdisk_proto::ErrorCode::PermissionDenied,
                        "permission denied or rejected by SELinux",
                    ),
                    Some(libc::ENOENT) => (
                        gadgetdisk_proto::ErrorCode::ImageNotFound,
                        "the mount point or device does not exist",
                    ),
                    Some(libc::EINVAL) => (
                        gadgetdisk_proto::ErrorCode::InvalidArgument,
                        "invalid mount arguments (the image may be unformatted)",
                    ),
                    _ => (
                        gadgetdisk_proto::ErrorCode::Internal,
                        "unknown kernel error",
                    ),
                };
                LoopError::new(
                    err_code,
                    format!(
                        "failed to mount {} at {}: {hint} ({raw})",
                        source.display(),
                        target.display()
                    ),
                )
            },
        )
    }

    fn umount(&mut self, target: &Path) -> LoopResult<bool> {
        match rustix::mount::unmount(target, rustix::mount::UnmountFlags::empty()) {
            Ok(()) => Ok(true),
            Err(err) => {
                let raw = std::io::Error::from(err);
                // EINVAL/ENOENT 表示本来就没挂载：幂等成功。
                if matches!(raw.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOENT)) {
                    return Ok(false);
                }
                if raw.raw_os_error() == Some(libc::EBUSY) {
                    return Err(LoopError::busy(format!(
                        "failed to unmount {}: the mount point is still in use by live processes ({raw})",
                        target.display()
                    )));
                }
                Err(LoopError::io("unmount", target, raw))
            }
        }
    }

    fn sync_all(&mut self) -> LoopResult<()> {
        // SAFETY: sync(2) 无参数、无内存副作用。
        unsafe { libc::sync() };
        Ok(())
    }

    fn is_mounted(&self, target: &Path) -> bool {
        mount_table().into_iter().any(|entry| entry == target)
    }
}

/// 解析 `/proc/self/mounts` 的挂载点列表。
///
/// 解析 `self` 而非 `mounts`：命名空间切换后两者可能不同，
/// 而我们关心的是**当前进程看到的**挂载视图。
pub fn mount_table() -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        // 内核把空格转义为 `\040`；本项目路径不含空格，但仍做基本反转义。
        .map(|raw| PathBuf::from(raw.replace("\\040", " ")))
        .collect()
}

/// 内存替身：记录调用顺序，并可模拟内核差异。
#[derive(Debug, Default, Clone)]
pub struct MemMounter {
    mounted: Vec<PathBuf>,
    /// 按顺序记录的调用。
    pub trace: Vec<String>,
    /// `mount` 是否失败（模拟缺 vfat 的内核）。
    pub fail_mount: bool,
    /// `umount` 是否因占用而失败。
    pub fail_umount_busy: bool,
}

impl MemMounter {
    /// 构造空替身。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置一个已有挂载（模拟残留）。
    pub fn with_mounted(mut self, path: &str) -> Self {
        self.mounted.push(PathBuf::from(path));
        self
    }

    /// 当前挂载点快照。
    pub fn mounted(&self) -> Vec<PathBuf> {
        self.mounted.clone()
    }
}

impl Mounter for MemMounter {
    fn mount(
        &mut self,
        source: &Path,
        target: &Path,
        fstype: &str,
        read_only: bool,
    ) -> LoopResult<()> {
        self.trace.push(format!(
            "mount:{}:{}:{fstype}:{read_only}",
            source.display(),
            target.display()
        ));
        if self.fail_mount {
            return Err(LoopError::filesystem(format!(
                "failed to mount {}: the kernel does not support {fstype}",
                source.display()
            )));
        }
        self.mounted.push(target.to_path_buf());
        Ok(())
    }

    fn umount(&mut self, target: &Path) -> LoopResult<bool> {
        self.trace.push(format!("umount:{}", target.display()));
        if self.fail_umount_busy {
            return Err(LoopError::busy(format!(
                "failed to unmount {}: still in use by some process",
                target.display()
            )));
        }
        let before = self.mounted.len();
        self.mounted.retain(|p| p != target);
        Ok(self.mounted.len() != before)
    }

    fn sync_all(&mut self) -> LoopResult<()> {
        self.trace.push("sync".into());
        Ok(())
    }

    fn is_mounted(&self, target: &Path) -> bool {
        self.mounted.iter().any(|p| p == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_mounter_umount_is_idempotent() {
        let mut m = MemMounter::new();
        assert!(!m.umount(Path::new("/mnt/x")).unwrap());
    }

    #[test]
    fn mem_mounter_tracks_mount_and_umount() {
        let mut m = MemMounter::new();
        let target = Path::new("/data/adb/gadget-disk/mnt/a.img");
        assert!(!m.is_mounted(target));
        m.mount(Path::new("/dev/block/loop0"), target, "vfat", false)
            .unwrap();
        assert!(m.is_mounted(target));
        assert!(m.umount(target).unwrap());
        assert!(!m.is_mounted(target));
        assert_eq!(
            m.trace,
            vec![
                "mount:/dev/block/loop0:/data/adb/gadget-disk/mnt/a.img:vfat:false".to_string(),
                "umount:/data/adb/gadget-disk/mnt/a.img".to_string(),
            ]
        );
    }

    #[test]
    fn mem_mounter_reports_busy_umount_as_error() {
        let mut m = MemMounter::new().with_mounted("/mnt/x");
        m.fail_umount_busy = true;
        let err = m.umount(Path::new("/mnt/x")).unwrap_err();
        assert_eq!(err.code, gadgetdisk_proto::ErrorCode::Busy);
        // 失败后仍视为挂载中：调用方据此知道状态没有真的改变。
        assert!(m.is_mounted(Path::new("/mnt/x")));
    }
}
