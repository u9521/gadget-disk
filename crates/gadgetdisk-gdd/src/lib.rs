//! GadgetDisk `gdd`：**无状态**的 mass_storage 执行进程。
//!
//! ## 职责边界（只有一件事）
//!
//! `gdd` 只做 mass_storage 挂载：把镜像绑成 LUN、按 LUN 弹出介质、拆除导出、
//! 重绑 UDC、在设备被弹出后清理自己的痕迹。它**不**做镜像增删、导入、loop
//! 挂载（那些由 CLI 就地执行），也**不**碰 gadget 身份
//! （`idVendor`/`idProduct`/`strings/*`/`os_desc` —— 那归 CLI）。
//!
//! 这条边界不是约定，而是可检查的事实：
//!
//! - 它只依赖 [`kernel::MassStorageOps`]（可写）与 [`kernel::GadgetView`]（只读），
//!   身份相关的类型根本不在它的依赖里；
//! - `crates/gadgetdisk-gdd/src/**` 有源码扫描测试，出现身份属性或
//!   `state.json` 字面量即失败。
//!
//! ## 无状态
//!
//! `gdd` 不读写任何状态文件。「上次导出到哪」记在 CLI 拥有的
//! `run/state.json` 里；`gdd` 每次操作都从 configfs 现读内核真值。因此它是
//! 可以被随时拉起/杀死的短命进程，重启后行为与之前完全一致。
//!
//! ## 进程模型
//!
//! 它**不是常驻服务**：只在「有镜像被导出为 USB 设备」期间存在，无挂载且空闲
//! 一段时间后自行退出（见
//! [按需进程模型 Note](../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)）。
//!
//! ## 安全底线（不变量）
//!
//! 1. socket 目录 `0700 root:root`，且以 `SO_PEERCRED` 校验对端 uid 为 0；
//! 2. 不监听 TCP（HTTP 由 CLI 的 `serve` 提供，仅绑 `127.0.0.1`）；
//!    不使用抽象套接字；
//! 3. 同一镜像**绝不可**同时作为两个 LUN 的后端。
//!
//! 规格见 [docs/protocol.md](../../../docs/protocol.md)、
//! [docs/architecture.md](../../../docs/architecture.md)。

pub mod kernel;
pub mod lock;
pub mod logging;
pub mod oplock;
pub mod paths;
pub mod server;
pub mod service;
pub mod socket;
pub mod usb_adapter;

pub use kernel::{
    FakeLoopOps, GadgetView, KernelError, KernelResult, LoopOps, MassStorageOps, NullBackend,
};
pub use lock::GlobalLock;
pub use logging::{info, warn};
pub use oplock::OpLock;
pub use paths::{DEFAULT_DATA_ROOT, DEFAULT_MODULE_ROOT, DataDirs};
pub use server::{DaemonConfig, DaemonError, bind, run, serve_connection};
pub use service::{Service, error};
pub use socket::{Listener, Peer, PeerCredentials, SocketError};
pub use usb_adapter::{GadgetBackend, UsbGadget, discover_layout, map_gadget_error};

/// 仅供测试使用的临时目录助手。
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
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
            "gadgetdisk-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 递归清理。
    pub fn cleanup(path: &Path) {
        std::fs::remove_dir_all(path).ok();
    }
}
