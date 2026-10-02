//! GadgetDisk 通信协议：版本握手、消息定义与帧编解码。
//!
//! 传输为 `AF_UNIX` **路径** socket；访问控制靠目录 `0700` + `SO_PEERCRED`，
//! 本 crate 只管字节层，不处理 socket。
//!
//! 规格来源：[docs/protocol.md](../../../docs/protocol.md)。
//!
//! ## 协议面只覆盖 `gdd` 的职责
//!
//! 本协议是 **CLI ↔ `gdd`** 的私有通道，而 `gdd` 只做 mass_storage 挂载。
//! 因此这里**没有**镜像增删/导入/loop/能力探测的消息——那些操作由 CLI 就地
//! 执行，不经过任何 IPC。见
//! [按需进程模型 Note](../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。
//!
//! 注意 [`Capabilities`] / [`Attachment`] / [`ImageInfo`] 等**结构**仍然保留：
//! 它们是 CLI 的 `serve` 对 WebUI 的 REST 应答体（`rest.rs` 直接序列化它们），
//! 只是不再作为 socket 消息传输。删除它们会把 REST 的契约一起删掉。

pub mod codec;
pub mod message;

pub use codec::{
    FrameError, MAX_FRAME_BYTES, PROTOCOL_VERSION, read_frame, read_handshake, read_json,
    write_frame, write_handshake, write_json,
};
pub use message::{
    Attachment, Capabilities, DeleteSlotRequest, DeleteSlotResponse, ErrorCode, ErrorResponse,
    ImageInfo, ImageLayout, ImageState, JobState, JobStatus, LunInfo, Message, Mode, MountDevice,
    MountRequest, MountResponse, RebindRequest, RebindResponse, StatusResponse, UnmountRequest,
    UnmountResponse,
};

/// 本模块的版本号（由构建脚本注入，缺省取 Cargo 包版本）。
///
/// ## 为什么放在这里
///
/// `gadgetdisk` 与 `gdd` 都要报同一个版本，而两者都已经依赖本 crate：把常量放在
/// 唯一的公共依赖里，比在每个 `main.rs` 里各写一遍表达式更难写错。
///
/// ## 为什么用 `option_env!` 而不是 `env!`
///
/// 构建脚本以环境变量 `GD_VERSION`（`uv run gd-build --version <v>`）注入发布版本；
/// 未注入时（`cargo run`、`cargo test`、IDE）回落到 `Cargo.toml` 的包版本，保持
/// 开发期行为不变。
///
/// `option_env!` 会被 cargo 记录为**环境依赖**：改 `GD_VERSION` 会触发重编，
/// 因此不存在"改了版本号却拿到旧二进制"的情况（实测确认：默认 0.1.0 →
/// `GD_VERSION=2.3.4` 出 2.3.4 → 不设又回 0.1.0）。
///
/// `match option_env!(..) { Some(v) => v, None => env!(..) }` 在 const 上下文里
/// 合法（在 1.85.1 与 stable 上均实测通过；本仓库 MSRV 见根 `Cargo.toml`）。
pub const VERSION: &str = match option_env!("GD_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
