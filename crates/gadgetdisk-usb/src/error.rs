//! gadget 侧的错误类型与少量共用助手。
//!
//! 具体的 configfs 编排分两处，按**归属**切分：
//!
//! - [`crate::mass_storage`]：function / LUN / 链接 / UDC —— `gdd` 的领地；
//! - [`crate::identity`]：`idVendor` / `strings` / `os_desc` —— CLI 的领地。
//!
//! 本模块只放两者共用的错误与 `parse_lun_index` / `ensure_configfs`。

use std::path::Path;

use crate::configfs::FsError;
use crate::paths;

/// gadget 侧错误。
#[derive(Debug, thiserror::Error)]
pub enum GadgetError {
    /// 无可用 UDC。
    #[error("no USB controller available (sys.usb.controller is empty)")]
    NoUdc,

    /// 内核不支持 mass_storage function。
    #[error("the kernel does not support the mass_storage gadget function")]
    MassStorageUnsupported,

    /// configfs 操作失败。
    #[error(transparent)]
    Fs(#[from] FsError),

    /// 后端镜像不可用。
    #[error("image unavailable: {0}")]
    ImageUnavailable(String),

    /// 备份/还原失败。
    #[error("gadget backup failed: {0}")]
    Backup(String),

    /// 无法确定可用的 gadget/configfs 布局。
    #[error("cannot determine a usable USB gadget: {0}")]
    ConfigfsUnavailable(String),

    /// 绑定流程走完，但配置没有真正生效（主机未接受）。
    #[error("config was built but did not take effect: {0}")]
    NotActive(String),

    /// 参数非法（LUN 序号越界、INQUIRY 超长、身份字段越界等）。
    ///
    /// 在**写入 configfs 之前**拒绝，而不是让内核静默截断或返回一个
    /// 难以解释的错误码。
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// 持久配置无法应用（身份属性读回不一致等）。
    ///
    /// 与 [`Self::Fs`] 的区别：这是「写了但没生效」——必须报错，
    /// 否则用户以为配置生效了。
    #[error("config did not take effect: {0}")]
    Config(String),
}

/// 本 crate 的 gadget 结果类型。
pub type GadgetResult<T> = std::result::Result<T, GadgetError>;

/// 解析 `lun.<n>` 形式的目录名。
pub fn parse_lun_index(name: &str) -> Option<u8> {
    name.strip_prefix("lun.")?.parse().ok()
}

/// 校验 `path` 位于 configfs 上。
pub fn ensure_configfs(path: &Path) -> GadgetResult<()> {
    paths::verify_configfs(path)?;
    Ok(())
}
