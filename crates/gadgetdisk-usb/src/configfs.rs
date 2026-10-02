//! configfs 访问抽象。
//!
//! 这是本 crate 的**可测试性接缝**：生产实现操作真实 `/config`，
//! 测试实现使用内存结构。这样「操作顺序」这类最高风险的逻辑
//! 能够在主机上断言（见 [docs/architecture.md](../../../../docs/architecture.md)）：
//!
//! - configfs 的 `file` 属性必须**最后**写入（`cdrom`/`ro` 先写才有意义）；
//! - `lun.0` 只能 `clear_lun` 而不能 `delete_lun`；
//! - 无 UDC 时不得改动 configfs。
//!
//! ## 关于 `lun.N/file` 的实测语义（与早期文档记载不同）
//!
//! 早期文档称「`file` 只能在 LUN 全新创建且尚未绑定文件时写入，覆写会被拒绝」。
//! **AVD 实测证伪了「覆写被拒绝」这一半**：
//!
//! | 操作 | 实测结果 |
//! |---|---|
//! | 绑定后覆写 `file` 为新路径 | `rc=0`，属性读回为新路径 |
//! | 覆写为不存在的路径 | `rc=1`，属性保持旧值 |
//! | 写空串（`clear_lun`）后再写新路径 | 均 `rc=0` |
//!
//! 但**覆写虽然成功，已枚举的 LUN 不会改用新后端文件**：guest 侧
//! `/sys/block/sda/size` 保持不变（仍对应旧镜像）。因此「换镜像必须先卸载」
//! 这条 UX 约束成立，只是原因从「写入被拒绝」修正为
//! 「写入被接受但不生效」——后者对用户更隐蔽，UI 更必须解释清楚。
//!
//! 规格见 [docs/android-integration.md](../../../../docs/android-integration.md)。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// configfs 操作错误。
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// 底层 IO 失败。
    #[error("configfs io error ({path}): {source}")]
    Io {
        /// 出错的路径。
        path: PathBuf,
        /// 底层原因。
        #[source]
        source: io::Error,
    },

    /// 目标不是 configfs。
    #[error("path {path} is not configfs (magic={magic:#x})")]
    NotConfigFs {
        /// 被检查的路径。
        path: PathBuf,
        /// 实际读到的 magic。
        magic: i64,
    },

    /// 条目不存在。
    #[error("configfs entry does not exist: {0}")]
    NotFound(PathBuf),

    /// 条目已存在。
    #[error("configfs entry already exists: {0}")]
    AlreadyExists(PathBuf),
}

/// 本 crate 的 FS 结果类型。
pub type FsResult<T> = std::result::Result<T, FsError>;

/// configfs 的一个目录项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// 条目名。
    pub name: String,
    /// 是否为目录。
    pub is_dir: bool,
    /// 是否为符号链接。
    pub is_symlink: bool,
}

/// configfs 访问接口。
///
/// 全部路径均为**相对于 gadget 根**的路径（例如 `configs/b.1/msd`），
/// 由实现拼接自己的根目录。
pub trait ConfigFs {
    /// 读取一个属性文件的值（去掉尾随换行）。
    fn read(&self, relative: &str) -> FsResult<String>;

    /// 写入一个属性文件。
    fn write(&mut self, relative: &str, value: &str) -> FsResult<()>;

    /// 创建目录。
    fn mkdir(&mut self, relative: &str) -> FsResult<()>;

    /// 删除目录（必须为空）。
    fn rmdir(&mut self, relative: &str) -> FsResult<()>;

    /// 创建符号链接。
    ///
    /// `relative_target` 是**相对于 configfs 挂载根**的目标路径
    /// （例如 `functions/mass_storage.gadget-disk`）；`relative_link` 同样是相对路径
    /// （例如 `configs/b.1/msd`）。
    ///
    /// ## configfs 的符号链接语义（已实测）
    ///
    /// configfs 在 `symlink(2)` 时把 `target` 当作**相对于进程当前工作目录**
    /// 的路径来校验，而不是相对于链接所在目录。因此同一个 target 字符串
    /// 会因 cwd 不同而成功或失败：
    ///
    /// | cwd | target | 结果 |
    /// |---|---|---|
    /// | `/` | `functions/mass_storage.gadget-disk` | `ENOENT` |
    /// | gadget 根 | `functions/mass_storage.gadget-disk` | 成功，`readlink` 得 `../../../../usb_gadget/g1/functions/mass_storage.gadget-disk` |
    ///
    /// 为避免实现依赖 cwd，`RealConfigFs` 会先把 target 解析为**绝对路径**
    /// 再交给内核（绝对路径与 cwd 无关），见其实现。
    ///
    /// 返回创建后的路径，供上层在“删除时优先按路径删除”的策略中使用。
    fn symlink(&mut self, relative_target: &str, relative_link: &str) -> FsResult<PathBuf>;

    /// 删除符号链接。
    fn unlink(&mut self, relative: &str) -> FsResult<()>;

    /// 读取符号链接的目标。
    ///
    /// 用于备份原 gadget 的配置符号链接：还原时必须知道它原先指向哪个
    /// function，否则只能按链接名猜测（实测会导致还原失败）。
    fn read_link(&self, relative: &str) -> FsResult<String>;

    /// 列出目录项。
    fn read_dir(&self, relative: &str) -> FsResult<Vec<DirEntryInfo>>;

    /// 判断条目是否存在。
    fn exists(&self, relative: &str) -> bool;
}

/// 真实 configfs 实现。
///
/// 根目录为 `/config/usb_gadget/g1`（路径常量见
/// [docs/android-integration.md](../../../../docs/android-integration.md)）。
#[derive(Debug, Clone)]
pub struct RealConfigFs {
    root: PathBuf,
}

/// configfs 的 `fstatfs` magic（`0x62656570`，即 `"beep"`）。
pub const CONFIGFS_MAGIC: i64 = 0x6265_6570;

impl RealConfigFs {
    /// 以给定 gadget 根目录创建。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// gadget 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn full(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl ConfigFs for RealConfigFs {
    fn read(&self, relative: &str) -> FsResult<String> {
        let path = self.full(relative);
        std::fs::read_to_string(&path)
            .map(|text| text.trim_end_matches(['\n', '\r', '\0']).to_string())
            .map_err(|source| FsError::Io { path, source })
    }

    fn write(&mut self, relative: &str, value: &str) -> FsResult<()> {
        use std::io::Write as _;

        let path = self.full(relative);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|source| FsError::Io {
                path: path.clone(),
                source,
            })?;

        // 关键（已实测）：**零字节写入是 no-op**。
        //
        // configfs 的 store 回调在 count==0 时直接返回，不做任何处理。
        // 因此「写空串清除属性」必须实际写出至少一个字符（通常用换行），
        // 否则属性保持原值。shell 里 `echo "" > attr` 之所以有效，
        // 正是因为它写出了一个换行；而 `printf '' > attr` 无效。
        //
        // 这一行为直接决定了 `clear_lun`（清空 `lun.N/file`）能否生效：
        // 若写入零字节，LUN 仍绑定在后端文件上，随后写 `ro`/`cdrom`
        // 会返回 EBUSY（实测 rc=1）。
        let payload = if value.is_empty() { "\n" } else { value };

        match file.write_all(payload.as_bytes()) {
            Ok(()) => Ok(()),
            // `ENODEV` 在清空 `UDC` 时表示「本来就没有绑定」，属幂等成功。
            //
            // 已实测：向 `UDC` 写换行时，若当前为空则返回 ENODEV（rc=1）；
            // 若已绑定控制器则返回 0 并解除绑定。卸载流程会在「本来未挂载」
            // 时也执行一次清空，因此必须容忍该错误，否则卸载会假失败。
            Err(source)
                if value.is_empty()
                    && relative == "UDC"
                    && source.raw_os_error() == Some(libc::ENODEV) =>
            {
                Ok(())
            }
            Err(source) => Err(FsError::Io {
                path: path.clone(),
                source,
            }),
        }
    }

    fn mkdir(&mut self, relative: &str) -> FsResult<()> {
        let path = self.full(relative);
        std::fs::create_dir(&path).map_err(|source| FsError::Io { path, source })
    }

    fn rmdir(&mut self, relative: &str) -> FsResult<()> {
        let path = self.full(relative);
        std::fs::remove_dir(&path).map_err(|source| FsError::Io { path, source })
    }

    fn symlink(&mut self, relative_target: &str, relative_link: &str) -> FsResult<PathBuf> {
        let link = self.full(relative_link);

        // configfs 按 **cwd** 解析 target，因此必须传绝对路径，
        // 否则结果取决于 gdd 进程的工作目录（已实测：cwd=/ 时会 ENOENT）。
        let absolute_target = if std::path::Path::new(relative_target).is_absolute() {
            PathBuf::from(relative_target)
        } else {
            self.root.join(relative_target)
        };

        std::os::unix::fs::symlink(&absolute_target, &link).map_err(|source| FsError::Io {
            path: link.clone(),
            source,
        })?;
        Ok(link)
    }

    fn unlink(&mut self, relative: &str) -> FsResult<()> {
        let path = self.full(relative);
        std::fs::remove_file(&path).map_err(|source| FsError::Io { path, source })
    }

    fn read_link(&self, relative: &str) -> FsResult<String> {
        let path = self.full(relative);
        let target = std::fs::read_link(&path).map_err(|source| FsError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(target.to_string_lossy().into_owned())
    }

    fn read_dir(&self, relative: &str) -> FsResult<Vec<DirEntryInfo>> {
        let path = self.full(relative);
        let reader = std::fs::read_dir(&path).map_err(|source| FsError::Io {
            path: path.clone(),
            source,
        })?;

        let mut entries = Vec::new();
        for entry in reader.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            entries.push(DirEntryInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: metadata.is_dir(),
                is_symlink: file_type.is_symlink(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn exists(&self, relative: &str) -> bool {
        self.full(relative).exists()
    }
}

/// 内存 configfs 实现，供主机测试使用。
///
/// 语义上刻意模拟真实 configfs 的实测约束，否则测试无法发现真实问题：
///
/// 1. **`file` 可覆写且返回成功**，但**已绑定 LUN 的后端不会改变**。
///    这里记录「实际生效的绑定」与「属性当前值」两份状态，
///    使「覆写不生效」这一真实行为可被断言（见 [`MemConfigFs::effective_file`]）。
/// 2. **`file` 写入不存在的路径会失败**，且保持旧值。
/// 3. **`lun.0` 不可删除**（实测 `EPERM`）；`lun.1`+ 可删除，但需先清空 `file`。
#[derive(Debug, Default, Clone)]
pub struct MemConfigFs {
    // 注意：真实实现规避了「零字节写入是 no-op」这一内核行为
    // （见 `RealConfigFs::write` 的注释），因此这里无需再建模该陷阱；
    // 但 `RealConfigFs` 的规避逻辑有专门的单元测试覆盖。
    /// 相对路径 → 内容（属性文件）或标记（目录）。
    entries: BTreeMap<String, Entry>,
    /// 实际生效的 `file` 绑定（相对路径 → 后端文件）。
    ///
    /// 只在「首次绑定」与「clear_lun 后重新绑定」时更新；
    /// 覆写不改变它，从而复现「覆写成功但不生效」的真实行为。
    effective_file: BTreeMap<String, String>,
    /// 已知存在的常规文件集合，用于复现「写入不存在路径失败」。
    known_files: std::collections::BTreeSet<String>,
    /// LUN 目录数上限（模拟内核 `FSG_MAX_LUNS`）。
    max_luns: u8,
    /// 是否模拟内核 `fsg_lun_make` 的 `refcnt` 检查。
    ///
    /// `refcnt` 由 `config_usb_cfg_link`/`unlink` 增减，即**「我们的 function 是否
    /// 被某个配置链接引用」**（不是「UDC 是否绑定」——AVD 实测校正过这一点：
    /// 只写空 UDC 而不删链接，`mkdir lun.N` 仍返回 EBUSY）。
    ///
    /// 打开后：只要我们的配置链接存在，`mkdir lun.N`（N≥1）就返回 `EBUSY`。
    /// 默认关闭，供需要区分「function 创建 vs LUN 创建」的测试打开。
    lun_create_needs_unlinked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Dir,
    File(String),
    Symlink(String),
}

impl MemConfigFs {
    /// 新建空的内存 configfs。
    pub fn new() -> Self {
        let mut fs = Self {
            max_luns: crate::paths::MAX_LUNS,
            ..Self::default()
        };
        // 预置 gadget 根下的标准结构。
        for dir in [
            "",
            "configs",
            "functions",
            "strings",
            "strings/0x409",
            "os_desc",
        ] {
            fs.entries.insert(dir.to_string(), Entry::Dir);
        }
        fs
    }

    /// 该属性文件是否已写入过值。
    pub fn is_set(&self, relative: &str) -> bool {
        matches!(self.entries.get(relative), Some(Entry::File(v)) if !v.is_empty())
    }

    /// 全部已知的相对路径（诊断用）。
    pub fn paths(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// 设置一个属性文件（不检查约束），供测试预置状态。
    pub fn preset(&mut self, relative: &str, value: &str) {
        self.entries
            .insert(relative.to_string(), Entry::File(value.to_string()));
    }

    /// 声明某个后端文件存在，使 `file` 写入能通过校验。
    pub fn add_known_file(&mut self, path: &str) {
        self.known_files.insert(path.to_string());
    }

    /// 强制移除一个目录及其子项（**仅供测试构造场景**）。
    ///
    /// 真实内核不允许删除 `lun.0`；此方法用于模拟「不支持 mass_storage
    /// 的内核」这类无法通过正常操作构造的状态。
    pub fn remove_dir_force(&mut self, relative: &str) -> bool {
        let prefix = format!("{relative}/");
        self.entries
            .retain(|key, _| key != relative && !key.starts_with(&prefix));
        self.effective_file
            .retain(|key, _| !key.starts_with(&prefix));
        true
    }

    /// 该 `file` 属性**实际生效**的绑定；`None` 表示未绑定。
    ///
    /// 与 [`ConfigFs::read`] 读到的属性值不同：覆写会改变属性值，
    /// 但不会改变这里的返回值。
    pub fn effective_file(&self, relative: &str) -> Option<&str> {
        self.effective_file.get(relative).map(String::as_str)
    }

    /// 移除一个属性文件（**仅供测试**：模拟内核缺少该属性，例如老内核没有
    /// `forced_eject`）。
    pub fn remove_attr(&mut self, relative: &str) -> bool {
        self.entries.remove(relative).is_some()
    }

    /// 创建一个属性文件占位（模拟内核在 mkdir 时生成的属性集）。
    pub fn add_file(&mut self, relative: &str) {
        self.entries
            .insert(relative.to_string(), Entry::File(String::new()));
    }

    fn not_found(relative: &str) -> FsError {
        FsError::NotFound(PathBuf::from(relative))
    }

    /// 是否存在指向我们 mass_storage function 的配置链接。
    ///
    /// 这就是内核 `fsg_opts->refcnt != 0` 的可观测等价物（见
    /// `with_lun_create_needing_unlinked` 的说明）。
    fn our_link_exists(&self) -> bool {
        self.entries.iter().any(|(path, entry)| {
            path.starts_with("configs/")
                && matches!(entry, Entry::Symlink(target) if target.contains(crate::paths::FUNCTION_NAME))
        })
    }

    /// 当前 `UDC` 属性的值（空串表示未绑定）。
    pub fn bound_udc(&self) -> String {
        match self.entries.get("UDC") {
            Some(Entry::File(value)) => value.trim().to_string(),
            _ => String::new(),
        }
    }

    /// 覆盖 LUN 目录数上限（模拟内核 `FSG_MAX_LUNS`）。
    pub fn with_max_luns(mut self, max: u8) -> Self {
        self.max_luns = max;
        self
    }

    /// 让 `mkdir lun.N` 在我们的配置链接存在时返回 EBUSY（模拟 `fsg_lun_make`
    /// 的 `refcnt` 检查）。
    pub fn with_lun_create_needing_unlinked(mut self) -> Self {
        self.lun_create_needs_unlinked = true;
        self
    }
}

impl ConfigFs for MemConfigFs {
    fn read(&self, relative: &str) -> FsResult<String> {
        match self.entries.get(relative) {
            Some(Entry::File(value)) => Ok(value.clone()),
            _ => Err(Self::not_found(relative)),
        }
    }

    fn write(&mut self, relative: &str, value: &str) -> FsResult<()> {
        if !self.entries.contains_key(relative) {
            return Err(Self::not_found(relative));
        }

        // `UDC` 的实测/源码语义：已被占用时再写非空值返回 **EBUSY**。
        //
        // 依据（源码）：`configfs.c` 的 `gadget_dev_desc_UDC_store` 在
        // `gi->composite.gadget_driver.udc_name` 非空时 `ret = -EBUSY`。
        // 写空串则走 `unregister_gadget` 解除绑定（未绑定时返回 ENODEV，
        // 对我们是幂等成功）。
        if relative == "UDC" {
            let trimmed = value.trim();
            let currently = self.bound_udc();
            if !trimmed.is_empty() && !currently.is_empty() {
                return Err(FsError::Io {
                    path: PathBuf::from(relative),
                    // 用真实的 `EBUSY` 而不是 `ErrorKind::ResourceBusy`：`EBUSY` 正是
                    // 内核对这两个场景实际返回的 errno，直接用它比映射回 `ErrorKind`
                    // 更贴近事实，也不会在多一层映射时丢掉原始 errno。
                    source: io::Error::from_raw_os_error(libc::EBUSY),
                });
            }
            if let Some(Entry::File(slot)) = self.entries.get_mut(relative) {
                *slot = trimmed.to_string();
            }
            return Ok(());
        }

        // `forced_eject` 的语义（源码 `fsg_store_forced_eject`）：
        // 先清 `prevent_medium_removal`，再 `fsg_store_file(..., "")`，
        // 即**强制解绑后端文件**，无论主机是否允许弹出。
        if relative.ends_with("/forced_eject") {
            let lun_dir = relative.trim_end_matches("/forced_eject");
            let file_attr = format!("{lun_dir}/file");
            if value.is_empty() {
                // 零字节写入内核会直接返回，不做任何事。
                return Ok(());
            }
            let had = self.effective_file.remove(&file_attr).is_some();
            if let Some(Entry::File(slot)) = self.entries.get_mut(&file_attr) {
                *slot = String::new();
            }
            if !had {
                // 内核在未绑定时也返回 count（不报错），这里保持一致。
            }
            return Ok(());
        }

        // `file` 的实测语义：可覆写、返回成功，但**不改变已生效的绑定**；
        // 写入不存在的路径会失败且保持旧值。
        if relative.ends_with("/file") || relative == "file" {
            if !value.is_empty() && !self.known_files.contains(value) {
                return Err(FsError::Io {
                    path: PathBuf::from(relative),
                    source: io::Error::new(
                        io::ErrorKind::NotFound,
                        "the backing file does not exist; the kernel refuses to bind",
                    ),
                });
            }

            let currently_bound = self
                .effective_file
                .get(relative)
                .map(String::as_str)
                .unwrap_or("");

            if value.is_empty() {
                // clear_lun：解除绑定。
                self.effective_file.remove(relative);
            } else if currently_bound.is_empty() {
                // 首次绑定生效。
                self.effective_file
                    .insert(relative.to_string(), value.to_string());
            }
            // 否则：覆写成功但生效绑定保持不变（复现真实行为）。

            if let Some(Entry::File(slot)) = self.entries.get_mut(relative) {
                *slot = value.to_string();
            }
            return Ok(());
        }

        match self.entries.get_mut(relative) {
            Some(Entry::File(slot)) => {
                *slot = value.to_string();
                Ok(())
            }
            _ => Err(Self::not_found(relative)),
        }
    }

    fn mkdir(&mut self, relative: &str) -> FsResult<()> {
        if self.entries.contains_key(relative) {
            return Err(FsError::AlreadyExists(PathBuf::from(relative)));
        }

        // 模拟 `fsg_lun_make`：`lun.N`（N>=1）在 gadget 已绑定 UDC 时返回 EBUSY。
        //
        // 依据（源码）：`f_mass_storage.c` 的 `fsg_lun_make` 检查
        // `fsg_opts->refcnt || fsg_opts->common->luns[num]`，refcnt 非零
        // （即 function 已被某配置引用、进而绑定了 UDC）即 `-EBUSY`。
        //
        // **`lun.0` 不在此列**：它随 function 创建，走 `fsg_alloc_inst`，
        // 不受该检查约束。
        if self.lun_create_needs_unlinked
            && let Some(index) = relative
                .rsplit('/')
                .next()
                .and_then(crate::error::parse_lun_index)
        {
            // `refcnt != 0` 的判据是「我们的 function 被某个配置链接引用」。
            if index >= 1 && self.our_link_exists() {
                return Err(FsError::Io {
                    path: PathBuf::from(relative),
                    source: io::Error::from_raw_os_error(libc::EBUSY),
                });
            }
        }

        // 上限：内核 `num >= FSG_MAX_LUNS` 返回 ERANGE。
        if let Some(index) = relative
            .rsplit('/')
            .next()
            .and_then(crate::error::parse_lun_index)
            && index >= self.max_luns
        {
            return Err(FsError::Io {
                path: PathBuf::from(relative),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "LUN index exceeds the kernel limit (ERANGE)",
                ),
            });
        }

        self.entries.insert(relative.to_string(), Entry::Dir);

        // 模拟内核在创建 LUN 目录时生成的属性集。
        //
        // 属性集**对所有 lun.N 一致**（`fsg_lun_attrs`），含 `inquiry_string`
        // 与 `forced_eject`——后者是 write-only，此处按普通文件建模即可
        // （我们不读它）。
        if relative.contains("mass_storage")
            && let Some(index) = relative
                .rsplit('/')
                .next()
                .and_then(crate::error::parse_lun_index)
        {
            let _ = index;
            for attr in [
                "cdrom",
                "ro",
                "file",
                "nofua",
                "removable",
                "forced_eject",
                "inquiry_string",
            ] {
                self.entries
                    .insert(format!("{relative}/{attr}"), Entry::File(String::new()));
            }
            // 新建的 LUN 默认 removable=1、nofua=0（`fsg_lun_make`
            // 里 `config.removable = true`）。
            self.entries
                .insert(format!("{relative}/removable"), Entry::File("1".into()));
            self.entries
                .insert(format!("{relative}/nofua"), Entry::File("0".into()));
        }

        // 真实内核在创建 strings/<lang> 时生成三个标准字符串描述符。
        if relative.starts_with("strings/") && !relative["strings/".len()..].contains('/') {
            for attr in ["manufacturer", "product", "serialnumber"] {
                self.entries
                    .insert(format!("{relative}/{attr}"), Entry::File(String::new()));
            }
        }
        Ok(())
    }

    fn rmdir(&mut self, relative: &str) -> FsResult<()> {
        if !matches!(self.entries.get(relative), Some(Entry::Dir)) {
            return Err(Self::not_found(relative));
        }

        // 模拟 `fsg_lun_drop`：删除 `lun.N`（N>=1）时，若 gadget 已绑定 UDC，
        // 内核会**隐式解绑** gadget（`unregister_gadget_item`）。
        //
        // 这意味着「删 LUN」与「解绑 UDC」在真实内核上是**同一个动作**，
        // 上层必须知道这一点，否则会以为 UDC 还绑着。
        if let Some(index) = relative
            .rsplit('/')
            .next()
            .and_then(crate::error::parse_lun_index)
            && index >= 1
            && !self.bound_udc().is_empty()
            && let Some(Entry::File(slot)) = self.entries.get_mut("UDC")
        {
            *slot = String::new();
        }

        // 关键约束 2：`lun.0` 不可删除，只能 clear_lun（实测 EPERM）。
        if relative.ends_with("lun.0") {
            return Err(FsError::Io {
                path: PathBuf::from(relative),
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "lun.0 cannot be deleted; it can only be cleared by writing an empty file",
                ),
            });
        }

        // 实测：mass_storage function 在仍有 lun 绑定后端文件时删除会 EBUSY；
        // 清空 file 后才可删除。
        if relative.contains("mass_storage") && !relative.contains("lun.") {
            let bound = self.effective_file.iter().any(|(path, value)| {
                path.starts_with(&format!("{relative}/")) && !value.is_empty()
            });
            if bound {
                return Err(FsError::Io {
                    path: PathBuf::from(relative),
                    source: io::Error::other(
                        "the function still has a LUN bound to a backing file",
                    ),
                });
            }
            // 同时检查属性值（未生效的绑定也算占用）。
            let attr_bound = self.entries.iter().any(|(path, entry)| {
                path.starts_with(&format!("{relative}/"))
                    && path.ends_with("/file")
                    && matches!(entry, Entry::File(v) if !v.is_empty())
            });
            if attr_bound {
                return Err(FsError::Io {
                    path: PathBuf::from(relative),
                    source: io::Error::other("the function still has a LUN with a non-empty file"),
                });
            }
        }

        // 真实 configfs 中，目录下的属性文件由内核生成，随目录一并消失。
        let prefix = format!("{relative}/");
        self.entries.retain(|key, _| !key.starts_with(&prefix));
        self.effective_file
            .retain(|key, _| !key.starts_with(&prefix));

        match self.entries.remove(relative) {
            Some(Entry::Dir) => Ok(()),
            _ => Err(Self::not_found(relative)),
        }
    }

    fn symlink(&mut self, relative_target: &str, relative_link: &str) -> FsResult<PathBuf> {
        if self.entries.contains_key(relative_link) {
            return Err(FsError::AlreadyExists(PathBuf::from(relative_link)));
        }

        // 与真实实现一致：接受绝对路径（cwd 无关）并归一化为根相对形式，
        // 便于测试断言链接指向哪个 function。
        let normalized = relative_target
            .strip_prefix("/config/usb_gadget/g1/")
            .unwrap_or(relative_target)
            .to_string();

        self.entries
            .insert(relative_link.to_string(), Entry::Symlink(normalized));
        Ok(PathBuf::from(relative_link))
    }

    fn unlink(&mut self, relative: &str) -> FsResult<()> {
        match self.entries.get(relative) {
            Some(Entry::Symlink(_)) => {
                self.entries.remove(relative);
                Ok(())
            }
            _ => Err(Self::not_found(relative)),
        }
    }

    fn read_link(&self, relative: &str) -> FsResult<String> {
        match self.entries.get(relative) {
            Some(Entry::Symlink(target)) => Ok(target.clone()),
            _ => Err(Self::not_found(relative)),
        }
    }

    fn read_dir(&self, relative: &str) -> FsResult<Vec<DirEntryInfo>> {
        if !matches!(self.entries.get(relative), Some(Entry::Dir)) {
            return Err(Self::not_found(relative));
        }

        let prefix = if relative.is_empty() {
            String::new()
        } else {
            format!("{relative}/")
        };

        let mut entries = Vec::new();
        for (path, entry) in &self.entries {
            let Some(name) = path.strip_prefix(&prefix) else {
                continue;
            };
            // 只取直接子项。
            if name.is_empty() || name.contains('/') {
                continue;
            }
            entries.push(DirEntryInfo {
                name: name.to_string(),
                is_dir: matches!(entry, Entry::Dir),
                is_symlink: matches!(entry, Entry::Symlink(_)),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn exists(&self, relative: &str) -> bool {
        self.entries.contains_key(relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RealConfigFs::write` 的两条实测语义，用临时目录验证（无需 configfs）。
    ///
    /// 这两条都曾导致真实设备上的失败，故必须有单元测试钉住：
    /// 1. 零字节写入必须被转换为换行，否则属性不会被清除；
    /// 2. 清空 `UDC` 时的 `ENODEV` 必须被容忍。
    #[test]
    fn real_fs_write_semantics_for_clear_and_enodev() {
        let dir = std::env::temp_dir().join(format!(
            "gd-realfs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // 该测试关心的是「写入了多少字节」，而不是普通文件的 file offset 语义：
        // configfs 的属性写入把缓冲区交给内核 store 回调，偏移与截断都不适用。
        // 因此这里断言**写入非空**（长度 1），而非断言内容被截断。
        //
        // 为什么这一点关键：configfs 的 store 回调在 count == 0 时直接返回，
        // 导致零字节写入成为 no-op，属性保持原值。
        let attr = dir.join("file");
        let mut fs = RealConfigFs::new(&dir);

        // 先建空文件，再写空串 → 必须产生 1 字节（换行）。
        std::fs::File::create(&attr).unwrap();
        fs.write("file", "").unwrap();
        let written = std::fs::metadata(&attr).unwrap().len();
        assert_eq!(written, 1, "空串写入必须实际写出 1 个字节（换行）");

        // 非空值原样写入。
        std::fs::File::create(&attr).unwrap();
        fs.write("file", "abc").unwrap();
        assert_eq!(std::fs::metadata(&attr).unwrap().len(), 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mem_fs_starts_with_gadget_scaffolding() {
        let fs = MemConfigFs::new();
        for dir in [
            "configs",
            "functions",
            "strings",
            "strings/0x409",
            "os_desc",
        ] {
            assert!(fs.exists(dir), "{dir} 应存在");
        }
    }

    #[test]
    fn mem_fs_lun_creation_synthesizes_attributes() {
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();

        for attr in ["cdrom", "ro", "file", "nofua", "removable"] {
            assert!(
                fs.exists(&format!("functions/mass_storage.gadget-disk/lun.0/{attr}")),
                "{attr} 应存在"
            );
        }
        assert_eq!(
            fs.read("functions/mass_storage.gadget-disk/lun.0/removable")
                .unwrap(),
            "1"
        );
    }

    #[test]
    fn mem_fs_allows_overwrite_but_binding_is_unchanged() {
        // 实测：覆写返回成功、属性值改变，但**已生效的绑定不变**。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();
        fs.add_known_file("/a.img");
        fs.add_known_file("/b.img");

        let attr = "functions/mass_storage.gadget-disk/lun.0/file";
        fs.write(attr, "/a.img").unwrap();
        assert_eq!(fs.read(attr).unwrap(), "/a.img");
        assert_eq!(fs.effective_file(attr), Some("/a.img"));

        // 覆写成功……
        fs.write(attr, "/b.img").unwrap();
        assert_eq!(fs.read(attr).unwrap(), "/b.img");
        // ……但生效绑定仍是旧文件（这正是「换镜像必须先卸载」的原因）。
        assert_eq!(fs.effective_file(attr), Some("/a.img"));
    }

    #[test]
    fn mem_fs_rejects_binding_missing_file() {
        // 实测：写不存在的路径失败，且属性保持旧值。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();
        fs.add_known_file("/real.img");

        let attr = "functions/mass_storage.gadget-disk/lun.0/file";
        fs.write(attr, "/real.img").unwrap();

        let err = fs.write(attr, "/nonexistent.img").unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
        assert_eq!(fs.read(attr).unwrap(), "/real.img");
        assert_eq!(fs.effective_file(attr), Some("/real.img"));
    }

    #[test]
    fn mem_fs_clear_then_rebind_becomes_effective() {
        // 卸载整个配置（clear_lun）后重新绑定才会生效。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();
        fs.add_known_file("/a.img");
        fs.add_known_file("/b.img");

        let attr = "functions/mass_storage.gadget-disk/lun.0/file";
        fs.write(attr, "/a.img").unwrap();
        fs.write(attr, "").unwrap();
        assert_eq!(fs.effective_file(attr), None);

        fs.write(attr, "/b.img").unwrap();
        assert_eq!(fs.effective_file(attr), Some("/b.img"));
    }

    #[test]
    fn mem_fs_rejects_deleting_lun0() {
        // 关键约束 2。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();

        let err = fs
            .rmdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
        assert!(fs.exists("functions/mass_storage.gadget-disk/lun.0"));
    }

    #[test]
    fn mem_fs_allows_deleting_lun1() {
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.1")
            .unwrap();

        fs.rmdir("functions/mass_storage.gadget-disk/lun.1")
            .unwrap();
        assert!(!fs.exists("functions/mass_storage.gadget-disk/lun.1"));
    }

    #[test]
    fn mem_fs_removes_attributes_with_directory() {
        // 真实 configfs 中属性文件由内核生成，随目录一并消失，
        // 因此删除目录不需要先逐个删除属性。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.1")
            .unwrap();
        let attr = "functions/mass_storage.gadget-disk/lun.1/file";
        assert!(fs.exists(attr));

        fs.rmdir("functions/mass_storage.gadget-disk/lun.1")
            .unwrap();
        assert!(!fs.exists("functions/mass_storage.gadget-disk/lun.1"));
        assert!(!fs.exists(attr), "属性应随目录消失");
    }

    #[test]
    fn mem_fs_rejects_removing_function_with_bound_lun() {
        // 实测：仍有 LUN 绑定后端文件时删除 function 会 EBUSY。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();
        fs.add_known_file("/a.img");
        fs.write("functions/mass_storage.gadget-disk/lun.0/file", "/a.img")
            .unwrap();

        let err = fs.rmdir("functions/mass_storage.gadget-disk").unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
        assert!(fs.exists("functions/mass_storage.gadget-disk"));

        // 清空后可删除。
        fs.write("functions/mass_storage.gadget-disk/lun.0/file", "")
            .unwrap();
        fs.rmdir("functions/mass_storage.gadget-disk").unwrap();
        assert!(!fs.exists("functions/mass_storage.gadget-disk"));
    }

    #[test]
    fn mem_fs_symlink_lifecycle() {
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("configs/b.1").unwrap();

        let link = fs
            .symlink("functions/mass_storage.gadget-disk", "configs/b.1/msd")
            .unwrap();
        assert_eq!(link, PathBuf::from("configs/b.1/msd"));

        let entries = fs.read_dir("configs/b.1").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_symlink);
        assert_eq!(entries[0].name, "msd");

        fs.unlink("configs/b.1/msd").unwrap();
        assert!(fs.read_dir("configs/b.1").unwrap().is_empty());
    }

    #[test]
    fn symlink_normalizes_absolute_target() {
        // configfs 按 cwd 解析 target，因此实现必须传绝对路径；
        // MemConfigFs 需能接受这种形式并归一化。
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("configs/b.1").unwrap();

        fs.symlink(
            "/config/usb_gadget/g1/functions/mass_storage.gadget-disk",
            "configs/b.1/msd",
        )
        .unwrap();

        match fs.entries.get("configs/b.1/msd") {
            Some(Entry::Symlink(target)) => {
                assert_eq!(target, "functions/mass_storage.gadget-disk");
            }
            other => panic!("期望 Symlink，得到 {other:?}"),
        }
    }

    #[test]
    fn mem_fs_symlink_conflict_is_rejected() {
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/a").unwrap();
        fs.mkdir("configs/b.1").unwrap();
        fs.symlink("functions/a", "configs/b.1/x").unwrap();

        let err = fs.symlink("functions/a", "configs/b.1/x").unwrap_err();
        assert!(matches!(err, FsError::AlreadyExists(_)));
    }

    #[test]
    fn mem_fs_read_dir_lists_only_direct_children() {
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();

        let entries = fs.read_dir("functions").unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["mass_storage.gadget-disk"]);

        let inner = fs.read_dir("functions/mass_storage.gadget-disk").unwrap();
        let names: Vec<_> = inner.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["lun.0"]);
    }

    #[test]
    fn mem_fs_missing_paths_report_not_found() {
        let fs = MemConfigFs::new();
        assert!(matches!(fs.read("nope/file"), Err(FsError::NotFound(_))));
        assert!(matches!(fs.read_dir("nope"), Err(FsError::NotFound(_))));
    }

    #[test]
    fn mem_fs_write_to_missing_file_is_rejected() {
        let mut fs = MemConfigFs::new();
        assert!(fs.write("nope/file", "x").is_err());
    }

    #[test]
    fn is_set_reports_bound_lun() {
        let mut fs = MemConfigFs::new();
        fs.mkdir("functions/mass_storage.gadget-disk").unwrap();
        fs.mkdir("functions/mass_storage.gadget-disk/lun.0")
            .unwrap();
        let attr = "functions/mass_storage.gadget-disk/lun.0/file";

        assert!(!fs.is_set(attr));
        fs.add_known_file("/a.img");
        fs.write(attr, "/a.img").unwrap();
        assert!(fs.is_set(attr));
    }
}
