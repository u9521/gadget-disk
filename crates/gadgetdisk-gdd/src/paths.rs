//! 数据目录布局与运行期路径。
//!
//! 规格见 [docs/architecture.md](../../../../docs/architecture.md) 的「运行期布局」。
//!
//! 把路径集中在此处，是为了让测试可以用临时目录替换根目录，
//! 同时保证生产路径只有一处定义。**CLI 与 `gdd` 共用本模块**：两者必须看到
//! 同一个数据根与同一个 socket 路径，否则会各自连到自己以为的那一套。

use std::path::{Path, PathBuf};

/// 默认数据根目录。
pub const DEFAULT_DATA_ROOT: &str = "/data/adb/gadget-disk";

/// 默认模块根目录。
///
/// 与 [`DEFAULT_DATA_ROOT`] **不同源**：模块安装目录由 root 管理器决定，
/// 数据目录是本模块自己约定的。二者不可互相推导，所以各自定义。
pub const DEFAULT_MODULE_ROOT: &str = "/data/adb/modules/gadget-disk";

/// `serve` 发布给 WebUI 的文件名（位于模块 `webroot/` 下）。
pub const API_JSON_NAME: &str = "api.json";

/// socket 文件名。
pub const SOCKET_NAME: &str = "gdd.sock";

/// 跨进程操作锁文件名。
pub const OPS_LOCK_NAME: &str = "ops.lock";

/// 数据目录布局。
///
/// ```text
/// <root>/
/// ├── images/     # 用户镜像
/// ├── run/        # 0700 root:root；运行期状态（socket、锁、意图、缓存）
/// ├── config/     # 持久配置（用户可见、可手改）
/// ├── logs/       # 0700 root:root；日志（cli.log、gdd.log、service.log、serve.log）
/// ├── mnt/        # loop 挂载点
/// └── tmp/        # 导入过程中的临时文件（完成后原子改名）
/// ```
///
/// ## `run/` 与 `config/` 为什么分开
///
/// 两者都跨重启保留，但**归属与语义**完全不同：
///
/// - `config/` 是**用户意图**（我想让设备叫什么名字）：用户可见、可手改，
///   删掉它只意味着「回到 Android 的默认身份」；
/// - `run/` 是**运行期事实与意图**（socket、锁、导出意图、缓存）：由程序管理，
///   删掉它会让「上次导出到哪」这一信息丢失。
///
/// 早期实现把两者混在 `state/` 下，用户无法区分「哪些文件我能删」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    /// 数据根目录。
    pub root: PathBuf,
}

impl Default for DataDirs {
    fn default() -> Self {
        Self::new(DEFAULT_DATA_ROOT)
    }
}

impl DataDirs {
    /// 以 `root` 为数据根目录。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 数据根目录本身。
    ///
    /// 需要它才能把同一个数据根传给子进程（`serve` 按需拉起 `gdd` 时必须让
    /// 两者看到同一份数据），而不是从 `images().parent()` 反推——
    /// 那种反推在布局变化时会静默指错。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `images/`：用户镜像。
    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }

    /// `run/`：socket、运行期锁、导出意图、缓存。**必须以 `0700 root:root` 创建。**
    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `config/`：持久配置（用户可见）。
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    /// `logs/`：日志。**必须以 `0700 root:root` 创建。**
    ///
    /// 与 `run/` 同级的防护：日志里会含镜像路径、LUN 参数、SELinux 上下文等
    /// 信息，不该让非 root 读到。
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `mnt/`：loop 挂载点。
    pub fn mnt(&self) -> PathBuf {
        self.root.join("mnt")
    }

    /// `tmp/`：导入过程中的临时文件。
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// socket 完整路径。
    pub fn socket_path(&self) -> PathBuf {
        self.run().join(SOCKET_NAME)
    }

    /// 跨进程操作锁路径。
    pub fn ops_lock(&self) -> PathBuf {
        self.run().join(OPS_LOCK_NAME)
    }

    // 注：**CLI 拥有的**那些文件路径（导出意图、身份备份、分区偏移缓存、
    // loop 登记、持久配置、对账日志）刻意**不在这里**，而在
    // `gadgetdisk-cli` 的 `cli_paths` 模块。
    //
    // `gdd` 无状态：它不认识这些文件。把它们定义在这里会让「gdd 是否碰到状态
    // 文件」变成需要靠自觉遵守的约定，而分开定义则由
    // `crates/gadgetdisk-gdd/tests/scope.rs` 的源码扫描强制。

    /// 全部需要存在的子目录。
    pub fn all(&self) -> [PathBuf; 6] {
        [
            self.images(),
            self.run(),
            self.config(),
            self.logs(),
            self.mnt(),
            self.tmp(),
        ]
    }

    /// 创建全部子目录，并把 `run/` 与 `logs/` 收紧到 `0700`。
    ///
    /// `run/` 的权限至关重要：它是 socket 所在目录，其访问控制完全依赖
    /// 「无 `x` 权限即无法 connect」这一文件系统层防线（见 docs/protocol.md）。
    /// `logs/` 同样收紧——日志里含镜像路径等敏感信息。
    pub fn create_all(&self) -> std::io::Result<()> {
        for dir in self.all() {
            std::fs::create_dir_all(&dir)?;
        }
        set_dir_mode_0700(&self.run())?;
        set_dir_mode_0700(&self.logs())?;
        Ok(())
    }

    /// 把 `name` 解析为 `images/` 下的完整路径。
    ///
    /// 拒绝任何会跳出 `images/` 的名字（`..`、绝对路径、路径分隔符），
    /// 以免 RPC 传入的目标名造成目录穿越。
    pub fn image_path(&self, name: &str) -> Option<PathBuf> {
        if !is_safe_component(name) {
            return None;
        }
        Some(self.images().join(name))
    }

    /// 把 `name` 解析为 `mnt/` 下的挂载点。
    pub fn mountpoint(&self, name: &str) -> Option<PathBuf> {
        if !is_safe_component(name) {
            return None;
        }
        Some(self.mnt().join(name))
    }
}

/// 把一个目录收紧到 `0700`。
///
/// 失败**不**向上传播为致命错误：目录恰好在只读挂载上时，socket 的 `bind`
/// 会因权限不足而明确报错，比在启动期中断更好诊断。
pub fn set_dir_mode_0700(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// 判断字符串是否为安全的单一路径组件。
///
/// 允许字母、数字、`.`、`_`、`-`；拒绝空串、`.`、`..` 与任何路径分隔符。
pub fn is_safe_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 把安全的组件拼成用于挂载点的名字（去掉扩展名，替换不安全字符）。
pub fn component_from_file_name(file_name: &str) -> Option<String> {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);

    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if is_safe_component(&sanitized) {
        Some(sanitized)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn layout_matches_documented_paths() {
        let dirs = DataDirs::default();
        assert_eq!(dirs.root, PathBuf::from("/data/adb/gadget-disk"));
        assert_eq!(dirs.images(), PathBuf::from("/data/adb/gadget-disk/images"));
        assert_eq!(dirs.run(), PathBuf::from("/data/adb/gadget-disk/run"));
        assert_eq!(dirs.config(), PathBuf::from("/data/adb/gadget-disk/config"));
        assert_eq!(dirs.mnt(), PathBuf::from("/data/adb/gadget-disk/mnt"));
        assert_eq!(dirs.tmp(), PathBuf::from("/data/adb/gadget-disk/tmp"));
        assert_eq!(
            dirs.socket_path(),
            PathBuf::from("/data/adb/gadget-disk/run/gdd.sock")
        );
    }

    #[test]
    fn logs_dir_is_part_of_the_layout_and_tightened() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("gd-logs-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();

        assert_eq!(dirs.logs(), root.join("logs"));
        let mode = std::fs::metadata(dirs.logs()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "logs/ 权限应为 0700");
        // 布局里必须含 logs/，否则它不会被创建。
        assert!(dirs.all().contains(&dirs.logs()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ops_lock_lives_in_run() {
        let dirs = DataDirs::new("/tmp/gd");
        assert_eq!(dirs.ops_lock(), PathBuf::from("/tmp/gd/run/ops.lock"));
    }

    #[test]
    fn safe_components_are_accepted() {
        for name in ["a.img", "disk-1.img", "my_disk", "IMG.2026"] {
            assert!(is_safe_component(name), "{name} 应被接受");
        }
    }

    #[test]
    fn traversal_and_separators_are_rejected() {
        for name in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "/abs",
            "with space",
            "with:colon",
        ] {
            assert!(!is_safe_component(name), "{name} 应被拒绝");
        }
    }

    #[test]
    fn image_path_rejects_traversal() {
        let dirs = DataDirs::new("/tmp/gd");
        assert!(dirs.image_path("ok.img").is_some());
        assert!(dirs.image_path("../../etc/passwd").is_none());
        assert!(dirs.image_path("/etc/passwd").is_none());
        assert!(dirs.mountpoint("../x").is_none());
    }

    #[test]
    fn component_from_file_name_strips_extension() {
        assert_eq!(
            component_from_file_name("my disk.img").as_deref(),
            Some("my_disk")
        );
        assert_eq!(
            component_from_file_name("plain.img").as_deref(),
            Some("plain")
        );
        assert_eq!(component_from_file_name("..").as_deref(), None);
        assert_eq!(
            component_from_file_name(".hidden").as_deref(),
            Some(".hidden")
        );
    }

    #[test]
    fn create_all_builds_every_subdir_and_tightens_run() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("gd-dirs-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();

        let dirs = DataDirs::new(&root);
        dirs.create_all().expect("创建目录");

        for dir in dirs.all() {
            assert!(dir.is_dir(), "{dir:?} 应存在");
        }
        // `run/` 必须是 0700：socket 的访问控制全靠它。
        let mode = std::fs::metadata(dirs.run()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "run/ 权限应为 0700");

        std::fs::remove_dir_all(&root).ok();
    }
}
