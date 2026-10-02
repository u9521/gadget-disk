//! GadgetDisk 本地 loop 挂载：loop ioctl、`mount(2)`、能力探测与释放顺序。
//!
//! ## 职责
//!
//! 提供在设备本地通过 loop 机制将磁盘镜像挂载至目标文件系统的核心编排。
//! 直接利用 Linux 原生内核驱动（如 vfat）读写镜像，避免在用户态引入重复的文件系统解析。
//!
//! ## 可测试性
//!
//! 全部内核访问经两个 trait 收敛：
//!
//! | trait | 抽象的内核接口 |
//! |---|---|
//! | [`loopdev::LoopControl`] | `LOOP_CTL_GET_FREE` / `LOOP_SET_FD` / `LOOP_SET_STATUS64` / `LOOP_CLR_FD` |
//! | [`mount::Mounter`] | `mount(2)` / `umount2(2)` / `sync(2)` |
//!
//! 于是「调用顺序」与「回退序」这两个最高风险的逻辑可以在主机上用
//! [`loopdev::MemLoop`] + [`mount::MemMounter`] 断言，不需要真实 `/dev/loop*`
//! （见 [docs/testing.md](../../../docs/testing.md)）。
//!
//! ## 两条硬约束
//!
//! 1. **释放顺序**：`sync` → `umount` → `LOOP_CLR_FD` → 校验。
//!    顺序错了会造成静默的数据损坏。
//! 2. **互斥**：同一镜像绝不可同时作为 gadget LUN 与 loop 附件。
//!    该约束由 gdd 的状态机保证（本 crate 不感知 gadget）。
//!
//! 规格见 [docs/ondevice-loop-mount.md](../../../docs/ondevice-loop-mount.md)。

pub mod attach;
pub mod blockdev;
pub mod caps;
pub mod error;
pub mod loopdev;
pub mod mount;

pub use attach::{AttachRequest, DetachSelector, LoopMounter, Mounted};
pub use blockdev::{BLKGETSIZE64, size_bytes};
pub use caps::{CapabilitySource, RealSource, parse_filesystems, parse_max_part, probe};
pub use error::{LoopError, LoopResult};
pub use loopdev::{
    LOOP_CLR_FD, LOOP_CONTROL, LOOP_CTL_GET_FREE, LOOP_SET_CAPACITY, LOOP_SET_FD,
    LOOP_SET_STATUS64, LoopControl, LoopStatus, MemLoop, RealLoopControl, loop_path,
    parse_loop_index,
};
pub use mount::{MemMounter, Mounter, RealMounter, mount_table};

/// 仅供测试使用的临时目录助手。
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 创建一个全新的临时目录。
    pub fn temp_dir(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "gadgetdisk-loop-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }
}
