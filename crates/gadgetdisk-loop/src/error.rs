//! 本 crate 的错误类型。
//!
//! 错误码的选择是**对外契约**的一部分：gdd 会把它直接回给客户端
//! （见 [docs/protocol.md](../../../../docs/protocol.md) 的错误码表）。
//! 因此这里的分类必须让用户能分辨「这台设备根本不支持」与
//! 「这次操作失败了」——前者要引导走 USB 编辑路径，后者要能重试。

use std::path::Path;

use gadgetdisk_proto::ErrorCode;

/// 本 crate 的结果类型。
pub type LoopResult<T> = std::result::Result<T, LoopError>;

/// loop 挂载失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LoopError {
    /// 回给客户端的稳定错误码。
    pub code: ErrorCode,
    /// 人类可读说明（含内核 errno 的语义化解释）。
    pub message: String,
}

impl LoopError {
    /// 以错误码与说明构造。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 能力缺失：**重试无用**，必须引导用户改走别的路径。
    pub fn capability(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::LoopUnsupported, message)
    }

    /// 文件系统不受支持。
    pub fn filesystem(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::FilesystemUnsupported, message)
    }

    /// 镜像不存在。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ImageNotFound, message)
    }

    /// 设备正被占用（例如挂载点仍在使用中）。
    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Busy, message)
    }

    /// 参数非法。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    /// 其它内部错误。
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// 把「打开/读取某个路径」的 IO 失败转成带路径上下文的错误。
    pub fn io(what: &str, path: &Path, err: std::io::Error) -> Self {
        let code = match err.raw_os_error() {
            Some(libc::ENOENT) => ErrorCode::ImageNotFound,
            Some(libc::EACCES) | Some(libc::EPERM) => ErrorCode::PermissionDenied,
            Some(libc::EBUSY) => ErrorCode::Busy,
            _ => ErrorCode::Internal,
        };
        Self::new(code, format!("{what} {} failed: {err}", path.display()))
    }

    /// 把 errno 翻译成人能读懂的说明。
    ///
    /// 原始 ioctl 错误（如 `EINVAL`）对用户毫无意义，且会掩盖真实原因。
    /// `op` 是 ioctl 名，`index` 是 loop 序号。
    pub fn ioctl(op: &str, index: u32, err: std::io::Error) -> Self {
        let hint = match err.raw_os_error() {
            Some(libc::ENXIO) => "the loop device is unbound or does not exist",
            Some(libc::EINVAL) => "the kernel rejects the arguments (bad offset or flags)",
            Some(libc::ENOTTY) | Some(libc::ENOSYS) => {
                "this kernel does not support that loop ioctl"
            }
            Some(libc::EBUSY) => "the device is busy",
            Some(libc::EPERM) | Some(libc::EACCES) => "permission denied",
            Some(libc::ENODEV) => "the device disappeared",
            _ => "unknown kernel error",
        };
        Self::new(
            ErrorCode::Internal,
            format!("{op} failed on loop{index}: {hint} ({err})"),
        )
    }
}
