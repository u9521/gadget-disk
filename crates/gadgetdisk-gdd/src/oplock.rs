//! 跨进程操作锁。
//!
//! 规格见 [按需进程模型 Note](../../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。
//!
//! ## 为什么需要它
//!
//! M8 把 loop 挂载移出了 gdd：`gadgetdisk serve`（或一次性 `attach-loop`）
//! 与 gdd 现在是**两个进程**，却都要改 gadget/loop 状态。原先 gdd 内的
//! [`crate::lock::GlobalLock`] 只在单进程内有效，跨进程需要一个内核层面的锁。
//!
//! 选 `flock(2)` 而不是锁文件（`mkdir`/`O_EXCL`）的理由：
//!
//! - **进程死亡自动释放**：锁归 fd 所有，进程被 `kill -9` 时内核回收，
//!   不会留下陈旧锁；锁文件方案必须自己判断 pid 是否还活着，而 pid 会被
//!   复用（M3 已实测过这个坑）。
//! - **无需清理逻辑**：没有「上次崩溃留下的锁目录」这种状态。
//!
//! 语义与 `GlobalLock` 一致：**try-lock**，拿不到立即返回 `None`
//! （调用方回 `busy`），而不是排队等待——等待只会让 UI 卡住而无进度反馈。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// 跨进程操作锁。
///
/// 每次 [`try_acquire`](Self::try_acquire) 都会打开锁文件并尝试加锁；
/// 返回的 [`OpGuard`] 持有该 fd，drop 时解锁（并关闭 fd）。
#[derive(Debug, Clone)]
pub struct OpLock {
    path: PathBuf,
}

impl OpLock {
    /// 以锁文件路径构造。文件不存在时在首次加锁时创建。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 锁文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 尝试获取独占锁；已被其他进程持有时返回 `None`。
    ///
    /// 用 `LOCK_EX | LOCK_NB`：非阻塞，拿不到立即返回，符合 try-lock 语义。
    pub fn try_acquire(&self) -> Option<OpGuard> {
        // 父目录必须存在；调用方（DataDirs::create_all）已保证。
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .ok()?;

        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Some(OpGuard { _file: file }),
            // EWOULDBLOCK（已被持有）或其他错误都视为「拿不到」。
            Err(_) => None,
        }
    }
}

/// 持锁凭据：只要它存活，跨进程锁就被持有。
///
/// 不实现 `Clone`：重复克隆会让同一进程以为自己持有多把锁，
/// 而 `flock` 对同一 fd 是幂等的、对不同 fd 则会互相阻塞——容易写出死锁。
#[derive(Debug)]
pub struct OpGuard {
    _file: File,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn lock_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = testutil::temp_dir(tag);
        let path = dir.join("ops.lock");
        (dir, path)
    }

    #[test]
    fn acquires_when_uncontended() {
        let (dir, path) = lock_path("oplock-free");
        let lock = OpLock::new(&path);
        let guard = lock.try_acquire().expect("空闲时应能取得锁");
        drop(guard);
        testutil::cleanup(&dir);
    }

    #[test]
    fn second_acquire_on_a_different_fd_is_refused() {
        // 这是跨进程语义的主机模拟：`flock` 按**打开文件描述**判定，
        // 两次独立 open 得到两个 fd，第二次必须失败——正如两个进程。
        let (dir, path) = lock_path("oplock-busy");
        let lock = OpLock::new(&path);

        let held = lock.try_acquire().expect("首次应成功");
        assert!(
            lock.try_acquire().is_none(),
            "已持有时第二次必须返回 None（对应 busy）"
        );

        drop(held);
        // 释放后必须能重新取得，否则锁会永久卡住。
        let again = lock.try_acquire().expect("释放后应能重新取得");
        drop(again);

        testutil::cleanup(&dir);
    }

    #[test]
    fn creates_the_lock_file_on_demand() {
        let (dir, path) = lock_path("oplock-create");
        assert!(!path.exists(), "前置条件：锁文件尚不存在");

        let lock = OpLock::new(&path);
        let guard = lock.try_acquire().expect("应自动创建锁文件并加锁");
        assert!(path.exists(), "加锁后锁文件应存在");

        drop(guard);
        testutil::cleanup(&dir);
    }

    #[test]
    fn missing_parent_directory_is_not_a_panic() {
        // 拿不到锁只是 `None`，不得 panic——gdd 启动期目录可能尚未就绪。
        let lock = OpLock::new("/gd-definitely-missing-dir/ops.lock");
        assert!(lock.try_acquire().is_none());
    }

    #[test]
    fn exposes_its_path_for_diagnostics() {
        let lock = OpLock::new("/tmp/x.lock");
        assert_eq!(lock.path(), Path::new("/tmp/x.lock"));
    }
}
