//! CLI 的可测试核心：子命令解析结果 → 请求消息、应答 → JSON 输出、退出码。
//!
//! 二进制 `gadgetdisk` 只负责参数解析与 IO，把全部决策放在此处以便主机测试。
//!
//! 内核侧的接线分两处，按**归属**切分：
//!
//! - `gadget_adapter`：CLI 侧的**只读**导出视图 + **身份**读写（`idVendor`/
//!   `strings`）。它只实现 `GadgetView`，**不**实现 `MassStorageOps`——因此
//!   CLI 在类型层面不可能绕过 `gdd` 去改 LUN。
//! - `loop_adapter`：loop 挂载与只读视图。
//!
//! `gdd` 的编排逻辑只认 `MassStorageOps`/`LoopOps`，因此两个底层 crate 都不
//! 依赖它（见
//! [daemon 编排 Note](../../../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)）。
//!
//! `http` 与 `rest` 是 WebUI 的 REST 通道（`gadgetdisk serve`）：前者是手写的
//! 最小 HTTP/1.1 传输层，后者把请求路由到「CLI 直做」或「转发 gdd」。
//! 设计理由见
//! [按需进程模型 Note](../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

pub mod cli_paths;
pub mod client;
pub mod gadget_adapter;
pub mod http;
pub mod image_context;
pub mod job;
pub mod loop_adapter;
pub mod mkfs;
pub mod output;
pub mod rest;
pub mod selinux;
pub mod serve;
pub mod upload;

#[cfg(test)]
pub(crate) mod testutil;

pub use client::{ClientError, Connection, build_request};
pub use gadget_adapter::{ExportIntent, IntentLun};
pub use loop_adapter::LoopMounts;
pub use mkfs::{MkfsFormatter, MkfsProbe};
pub use output::{ExitCode, JsonOutput};
pub use rest::ApiInfo;
