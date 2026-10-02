//! GadgetDisk USB gadget 侧：configfs 读写、UDC 选择、gadget 备份与还原。
//!
//! ## 按归属切分的两个模块
//!
//! 本 crate 的编排代码刻意分成两半，对应**两个不同的进程**：
//!
//! | 模块 | 归属 | 碰的 configfs 条目 |
//! |---|---|---|
//! | [`mass_storage`] | `gdd` | `functions/mass_storage.gadget-disk` 及 `lun.N/*`、配置链接、`UDC` |
//! | [`identity`] | CLI | `idVendor`/`idProduct`/`strings/*`/`os_desc` |
//!
//! 这样切分的直接好处是 `gdd` 的权限面最小：它连 `idVendor` 都不认识。
//! 违反这条边界的代码会被 `crates/gadgetdisk-gdd` 的源码扫描测试发现。
//!
//! ## 可测试性
//!
//! 全部内核访问经 [`configfs::ConfigFs`] trait，因此「操作顺序」这一
//! 最高风险的逻辑可在主机断言（见 [docs/testing.md](../../../docs/testing.md)）：
//!
//! - `file` 属性必须**最后**写入（`cdrom`/`ro` 先写才有意义）；
//! - `mkdir lun.N` 必须在写 `UDC` **之前**（内核 `fsg_lun_make` 会 `EBUSY`）；
//! - `lun.0` 只能 `clear_lun`，不能 `delete_lun`；
//! - 无 UDC 时**不得**改动 configfs。
//!
//! 规格见 [docs/android-integration.md](../../../docs/android-integration.md)。

pub mod configfs;
pub mod discover;
pub mod error;
pub mod identity;
pub mod jsonfile;
pub mod mass_storage;
pub mod paths;

pub use configfs::{ConfigFs, DirEntryInfo, FsError, MemConfigFs, RealConfigFs};
pub use discover::{DiscoverError, GADGET_PARENT, GadgetTree, RealGadgetTree, discover};
pub use error::{GadgetError, GadgetResult, ensure_configfs, parse_lun_index};
pub use identity::{BACKUP_FILE_NAME, Identity, IdentityBackup, OS_DESC_USE_ATTR};
pub use mass_storage::{
    LunRequest, LunState, MassStorage, Step, UDC_POLL_INTERVAL, UDC_REBIND_TIMEOUT,
};
pub use paths::{
    BACKUP_ATTRS, ConfigChoice, DEFAULT_CONFIG_NAME, DEFAULT_GADGET_DIR, FUNCTION_NAME,
    GADGET_ROOT, GadgetChoice, INQUIRY_STRING_MAX, LINK_NAME, Layout, MAX_LUNS, STRING_ATTRS,
    STRING_MAX_BYTES, Udc,
};
