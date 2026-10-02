//! 只读文件系统查询：可用空间。
//!
//! 该操作**不经 gdd**（不涉及 gadget/loop 状态），因此放在 core 供
//! CLI 与 REST 传输层共用。它是纯查询：不创建、不删除、不修改任何东西。
//!
//! 这里曾有 `list_entries` 与 `stat_path`（目录列举、单路径信息）。它们只服务于
//! WebUI 的**内置路径浏览器**；该浏览器已被系统文件选择器取代，故一并移除——
//! 留着就是「可枚举设备任意目录」的没有调用方的攻击面。
//!
//! 输出字段名与 [docs/protocol.md](../../../docs/protocol.md) 的 CLI 契约
//! **逐字一致**，因为 WebUI 直接消费这些 JSON。

use std::path::{Path, PathBuf};

use crate::CoreError;

/// 找到 `path` 自身或**最近的已存在祖先**。
///
/// ## 设计依据
///
/// WebUI 在初次创建 `images/` 目录前需预检可用空间，此时目标路径尚不存在。
/// 必须递归向上检索真实存在的最近目录以供 `statvfs` 测量：
///
/// - 最近已存在的祖先路径与目标路径位于同一文件系统，测量结果真实有效；
/// - 检索过程递归向上，避免仅回溯单层在多级未建路径场景下失败。
///
/// 返回的路径为实际参与测量的目标，调用端可直接用于诊断回显。
/// 根目录兜底返回 `/`，相对路径返回 `.`。
pub fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();

    loop {
        if current.exists() {
            return current;
        }
        match current.parent() {
            // 还有父目录可退：继续向上。
            Some(parent) if parent != current => current = parent.to_path_buf(),
            // 已到根（或相对路径的尽头）仍不存在。
            _ => {
                return if path.is_absolute() {
                    PathBuf::from("/")
                } else {
                    PathBuf::from(".")
                };
            }
        }
    }
}

/// 目标路径所在文件系统的可用字节数。
///
/// 目标可能尚不存在（WebUI 要在创建 `images/` 之前预检），因此先经
/// [`nearest_existing_ancestor`] 找到真实存在的祖先再测量。返回的元组第一项
/// 是**实际测量的对象**，调用方必须回显它。
pub fn available_bytes(path: &Path) -> Result<(PathBuf, u64, u64), CoreError> {
    let target = nearest_existing_ancestor(path);

    // `rustix::fs::statvfs` 返回 Result 且字段是 u64，不需要裸 libc 调用或手工零初始化。
    // rustix 的错误可无损转成 `std::io::Error`（保留 errno）。
    let stat = rustix::fs::statvfs(&target).map_err(std::io::Error::from)?;

    let frsize = stat.f_frsize;
    Ok((target, stat.f_bavail * frsize, stat.f_blocks * frsize))
}

/// `CoreError` → 该场景下的**协议无关**错误分类。
///
/// core 不依赖 `gadgetdisk-proto`（那会把「协议」沉进最底层、且让 core
/// 无法被非协议场景复用），因此这里只返回一个很窄的枚举，由传输层
/// （CLI / REST）映射成各自的错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// 目标不存在。
    NotFound,
    /// 权限或 SELinux 拒绝。
    PermissionDenied,
    /// 参数非法（例如路径含 NUL 字节）。
    Invalid,
    /// 其余。
    Other,
}

/// 把 [`CoreError`] 归类。
///
/// 只映射**能够确定**的几种；其余归为 `Other`，而不是猜成权限问题——
/// 错误的分类会把排查引向错误方向，比没有分类更糟。
pub fn classify(err: &CoreError) -> FsError {
    match err {
        CoreError::NotFound(_) => FsError::NotFound,
        CoreError::InvalidArgument(_) => FsError::Invalid,
        CoreError::Io(io) => match io.kind() {
            std::io::ErrorKind::NotFound => FsError::NotFound,
            std::io::ErrorKind::PermissionDenied => FsError::PermissionDenied,
            _ => FsError::Other,
        },
        _ => FsError::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn existing_path_is_returned_as_is() {
        let dir = std::env::temp_dir();
        assert_eq!(nearest_existing_ancestor(&dir), dir);
    }

    #[test]
    fn missing_deep_path_resolves_to_existing_ancestor() {
        // 这是修复的核心场景：连续多层目录都不存在。
        let dir = std::env::temp_dir();
        let deep = dir.join("gd-not-exist-a").join("b").join("c");
        assert!(!deep.exists());
        assert_eq!(nearest_existing_ancestor(&deep), dir);
    }

    #[test]
    fn missing_direct_child_resolves_to_parent() {
        let dir = std::env::temp_dir();
        let child = dir.join("gd-not-exist-single");
        assert!(!child.exists());
        assert_eq!(nearest_existing_ancestor(&child), dir);
    }

    #[test]
    fn unresolvable_absolute_path_returns_root() {
        // 根总是存在，因此这个断言实际验证的是「不会死循环且返回绝对路径」。
        let mut p = PathBuf::from("/");
        for _ in 0..40 {
            p.push("gd-definitely-missing");
        }
        let resolved = nearest_existing_ancestor(&p);
        assert!(resolved.is_absolute(), "得到 {resolved:?}");
        assert!(resolved.exists(), "结果必须真实存在");
    }

    #[test]
    fn unresolvable_relative_path_returns_current_dir() {
        let p = PathBuf::from("gd-not-exist-rel").join("x").join("y");
        let resolved = nearest_existing_ancestor(&p);
        assert!(!resolved.is_absolute());
        assert!(resolved.exists());
    }

    #[test]
    fn available_bytes_reports_measured_target_for_missing_path() {
        let dir = testutil::temp_dir("fsinfo-avail");
        let missing = dir.join("images");
        assert!(!missing.exists());

        let (target, available, total) = available_bytes(&missing).unwrap();
        // 必须回显**实际测量**的对象，而不是请求的路径。
        assert_eq!(target, dir);
        assert!(available > 0, "可用空间应为正");
        assert!(total >= available, "总量应不小于可用量");

        // 注意：不能把 `dir` 本身交给 `testutil::cleanup`——那个助手删除的是
        // 传入路径的**父目录**（供「传入镜像文件路径」的用法），传目录会删掉
        // 系统临时目录。
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn classify_maps_only_certain_kinds() {
        use std::io::ErrorKind;
        let not_found = CoreError::Io(std::io::Error::from(ErrorKind::NotFound));
        assert_eq!(classify(&not_found), FsError::NotFound);

        let denied = CoreError::Io(std::io::Error::from(ErrorKind::PermissionDenied));
        assert_eq!(classify(&denied), FsError::PermissionDenied);

        // 不确定的一律 Other：不猜成权限问题。
        let other = CoreError::Io(std::io::Error::other("boom"));
        assert_eq!(classify(&other), FsError::Other);

        assert_eq!(
            classify(&CoreError::InvalidArgument("x".into())),
            FsError::Invalid
        );
        assert_eq!(
            classify(&CoreError::NotFound("x".into())),
            FsError::NotFound
        );
    }
}
