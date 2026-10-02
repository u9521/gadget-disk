//! 全局锁。
//!
//! 规格见 [docs/architecture.md](../../../../docs/architecture.md) 的「并发与一致性」：
//! gdd 内持有**单一全局锁**串行化所有 configfs 与 loop 操作；持锁期间
//! 第二个请求返回 `busy` 而非竞态写入。
//!
//! 这里实现为 **try-lock**：拿不到锁立即返回 `busy`，而不是排队等待。
//! 理由：调用方是 WebUI 的一次性 CLI，等待只会让 UI 卡住而无进度反馈；
//! 明确的 `busy` 让 UI 能提示「另一操作正在进行」。

use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

/// 全局操作锁。
///
/// 用 [`Arc`] 包裹以便在多个线程（每个连接一个）之间共享。
#[derive(Debug, Clone, Default)]
pub struct GlobalLock {
    inner: Arc<Mutex<()>>,
}

impl GlobalLock {
    /// 新建未持有的锁。
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试获取锁；已被持有时返回 `None`（调用方应回 `busy`）。
    pub fn try_acquire(&self) -> Option<Guard<'_>> {
        match self.inner.try_lock() {
            Ok(guard) => Some(Guard { _guard: guard }),
            Err(TryLockError::Poisoned(poisoned)) => {
                // 前一个持有者 panic 了。锁本身仍然有效，继续使用，
                // 因为拒绝服务比继续服务更糟（gdd 是单进程常驻）。
                Some(Guard {
                    _guard: poisoned.into_inner(),
                })
            }
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// 是否当前被持有（仅用于诊断与测试）。
    pub fn is_held(&self) -> bool {
        match self.inner.try_lock() {
            Ok(guard) => {
                drop(guard);
                false
            }
            Err(TryLockError::Poisoned(_)) => true,
            Err(TryLockError::WouldBlock) => true,
        }
    }
}

/// 全局操作互斥锁的 RAII 作用域凭证。持有期间所有并发的状态变更请求均返回 Busy，直至其析构。
#[derive(Debug)]
pub struct Guard<'a> {
    _guard: MutexGuard<'a, ()>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_succeeds_when_free() {
        let lock = GlobalLock::new();
        assert!(!lock.is_held());
        let guard = lock.try_acquire().expect("空闲时应能获取");
        assert!(lock.is_held());
        drop(guard);
        assert!(!lock.is_held());
    }

    #[test]
    fn second_acquire_reports_busy() {
        let lock = GlobalLock::new();
        let _first = lock.try_acquire().unwrap();

        // 第二个请求必须拿到 None（→ 上层回 busy），而不是阻塞。
        assert!(lock.try_acquire().is_none());
    }

    #[test]
    fn lock_is_reusable_after_release() {
        let lock = GlobalLock::new();
        {
            let _guard = lock.try_acquire().unwrap();
        }
        assert!(lock.try_acquire().is_some());
    }

    #[test]
    fn cloned_handles_share_one_lock() {
        let lock = GlobalLock::new();
        let other = lock.clone();

        let guard = lock.try_acquire().unwrap();
        // 通过另一个句柄也必须看到「已被持有」。
        assert!(other.try_acquire().is_none());
        drop(guard);
        assert!(other.try_acquire().is_some());
    }

    #[test]
    fn poisoned_lock_still_acquirable() {
        let lock = GlobalLock::new();
        let other = lock.clone();

        // 让一个线程在持锁时 panic，制造 poisoning。
        let handle = std::thread::spawn(move || {
            let _guard = other.try_acquire().unwrap();
            panic!("故意 panic 以制造 poisoning");
        });
        assert!(handle.join().is_err());

        // gdd 必须继续服务，而不是永久 busy。
        assert!(
            lock.try_acquire().is_some(),
            "poisoned 锁应仍可获取，否则 gdd 会永久拒绝服务"
        );
    }
}
