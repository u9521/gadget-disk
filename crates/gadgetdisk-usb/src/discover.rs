//! gadget / config 目录**探测**。
//!
//! ## 为什么必须探测而不是硬编码
//!
//! AOSP 的 [init.usb.configfs.rc][rc] 全文件只引用 `usb_gadget/g1` 与
//! `configs/b.1`（逐行核对，140 行），所以它们是**最可靠的兜底**；但
//! **不能当作唯一事实**：
//!
//! - 实测（红魔9 Pro / Android 12）`/config/usb_gadget/` 下同时存在 `g1` 与
//!   名为 `g2` 的 **vendor 私有** gadget——`g2` 不是 AOSP 的产物（AOSP 从不
//!   创建它），说明厂商会自行增删 gadget。
//! - 因此「用哪个 gadget、哪个 config」必须在运行时探测，并在选不出来时
//!   **报错而不是盲写**：猜错会写坏别的模块/框架正在用的 gadget。
//!
//! [rc]: https://android.googlesource.com/platform/system/core/+/refs/heads/main/rootdir/init.usb.configfs.rc
//!
//! ## 与 `ConfigFs` 的分工
//!
//! [`ConfigFs`](crate::configfs::ConfigFs) 的路径**相对于某一个 gadget 根**，
//! 无法表达「列出同级 gadget」这类跨 gadget 的访问。探测因此单独用一个更窄的
//! [`GadgetTree`] 抽象：生产实现走 `std::fs`，测试实现走内存树，二者都能在
//! 主机上跑。

use std::path::PathBuf;

use crate::paths::{
    ConfigChoice, DEFAULT_CONFIG_NAME, DEFAULT_GADGET_DIR, GadgetChoice, Layout, UDC_ATTR,
};

/// gadget 根目录的绝对父路径。
pub const GADGET_PARENT: &str = "/config/usb_gadget";

/// 探测所需的「gadget 父目录」只读视图。
///
/// 刻意做窄：只需要列出 gadget、读某个 gadget 的 UDC、列出某个 gadget 的
/// config 目录，以及看某个 config 里有没有符号链接。
pub trait GadgetTree {
    /// 列出 `/config/usb_gadget` 下的**目录**名（gadget 名）。
    fn gadget_dirs(&self) -> Result<Vec<String>, DiscoverError>;

    /// 读 `<gadget>/UDC`；读不到时返回空串（视为未绑定）。
    fn read_udc(&self, gadget: &str) -> String;

    /// 列出 `<gadget>/configs` 下的**目录**名。
    fn config_dirs(&self, gadget: &str) -> Vec<String>;

    /// `<gadget>/configs/<config>` 下是否存在符号链接。
    fn config_has_symlink(&self, gadget: &str, config: &str) -> bool;
}

/// 探测失败的明确原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverError {
    /// `/config/usb_gadget` 不存在或不可读——configfs 未就绪，**不得盲写**。
    NoUsableGadget(String),
    /// 目录存在但一个 gadget 都没有。
    NoGadget,
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverError::NoUsableGadget(why) => {
                write!(
                    f,
                    "cannot determine a usable USB gadget (configfs not ready): {why}"
                )
            }
            DiscoverError::NoGadget => {
                write!(
                    f,
                    "no USB gadget found in /config/usb_gadget (configfs is not initialized)"
                )
            }
        }
    }
}

/// 探测可用的 gadget 与配置目录。
///
/// 选择顺序（理由记入 [`Layout`]）：
/// 1. gadget：**已绑定到 `udc_name` 的那个** → 名为 `g1`（AOSP 默认）→ 唯一的那个；
/// 2. config：**含至少一个符号链接的那个**（Android 正在用）→ 名为 `b.1`
///    → 唯一的那个 → 都没有时按 `b.1` 待创建。
///
/// 探测失败返回 [`DiscoverError`]；调用方必须**报错而不是盲写**。
pub fn discover(tree: &impl GadgetTree, udc_name: Option<&str>) -> Result<Layout, DiscoverError> {
    let (gadget, gadget_reason) = choose_gadget(tree, udc_name)?;
    let gadget_root = PathBuf::from(format!("{GADGET_PARENT}/{gadget}"));
    let (config_name, config_reason) = choose_config(tree, &gadget);

    Ok(Layout {
        gadget_root,
        config_name,
        gadget_reason,
        config_reason,
    })
}

/// 选 gadget：绑定中的 → `g1` → 唯一的一个。
fn choose_gadget(
    tree: &impl GadgetTree,
    udc_name: Option<&str>,
) -> Result<(String, GadgetChoice), DiscoverError> {
    let mut gadgets = tree.gadget_dirs()?;
    gadgets.sort();
    if gadgets.is_empty() {
        return Err(DiscoverError::NoGadget);
    }

    // 1. 已绑定到目标 UDC 的那个最可信。
    if let Some(udc) = udc_name.filter(|name| !name.is_empty()) {
        for name in &gadgets {
            if tree.read_udc(name).trim() == udc {
                return Ok((name.clone(), GadgetChoice::BoundToUdc));
            }
        }
    }

    // 2. AOSP 默认。
    if let Some(name) = gadgets
        .iter()
        .find(|name| name.as_str() == DEFAULT_GADGET_DIR)
    {
        return Ok((name.clone(), GadgetChoice::DefaultG1));
    }

    // 3. 唯一的一个。
    if gadgets.len() == 1 {
        return Ok((gadgets[0].clone(), GadgetChoice::SoleGadget));
    }

    // 多个 gadget 且都判不出来：宁可报错也不猜（猜错会写坏别人的 gadget）。
    Err(DiscoverError::NoUsableGadget(format!(
        "multiple gadgets exist ({}) but none can be chosen",
        gadgets.join(", ")
    )))
}

/// 选 config 目录：有链接的 → `b.1` → 唯一的一个 → `b.1`（待创建）。
fn choose_config(tree: &impl GadgetTree, gadget: &str) -> (String, ConfigChoice) {
    let mut configs = tree.config_dirs(gadget);
    configs.sort();

    // 1. 含符号链接 → Android 正在使用。用「有没有链接」判定即可：即使某个
    //    链接是悬空的，也说明该配置曾被启用过，比另选一个空配置更可信。
    for name in &configs {
        if tree.config_has_symlink(gadget, name) {
            return (name.clone(), ConfigChoice::HasLinks);
        }
    }

    // 2. AOSP 默认。
    if configs.iter().any(|name| name == DEFAULT_CONFIG_NAME) {
        return (DEFAULT_CONFIG_NAME.to_string(), ConfigChoice::DefaultB1);
    }

    // 3. 唯一的一个。
    if configs.len() == 1 {
        return (configs[0].clone(), ConfigChoice::SoleConfig);
    }

    // 4. 没有（或全是空的多选）→ 用 b.1，由挂载流程创建。
    (DEFAULT_CONFIG_NAME.to_string(), ConfigChoice::ToCreate)
}

/// 生产实现：直接读 `/config/usb_gadget`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealGadgetTree {
    parent: PathBuf,
}

impl Default for RealGadgetTree {
    fn default() -> Self {
        Self::new(GADGET_PARENT)
    }
}

impl RealGadgetTree {
    /// 以给定父目录构造（测试可指向临时目录）。
    pub fn new(parent: impl Into<PathBuf>) -> Self {
        Self {
            parent: parent.into(),
        }
    }

    fn dir_entries(&self, path: &std::path::Path) -> Result<Vec<String>, DiscoverError> {
        let entries = std::fs::read_dir(path)
            .map_err(|err| DiscoverError::NoUsableGadget(format!("{}: {err}", path.display())))?;
        Ok(entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect())
    }
}

impl GadgetTree for RealGadgetTree {
    fn gadget_dirs(&self) -> Result<Vec<String>, DiscoverError> {
        self.dir_entries(&self.parent)
    }

    fn read_udc(&self, gadget: &str) -> String {
        std::fs::read_to_string(self.parent.join(gadget).join(UDC_ATTR)).unwrap_or_default()
    }

    fn config_dirs(&self, gadget: &str) -> Vec<String> {
        self.dir_entries(&self.parent.join(gadget).join("configs"))
            .unwrap_or_default()
    }

    fn config_has_symlink(&self, gadget: &str, config: &str) -> bool {
        let dir = self.parent.join(gadget).join("configs").join(config);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_symlink())
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个 gadget 的测试描述：名字、绑定的 UDC、以及它的 config 列表
    /// （每项为「config 名, 是否含符号链接」）。
    type MemGadget = (String, String, Vec<(String, bool)>);

    /// 内存版 gadget 树，用于主机测试探测逻辑。
    #[derive(Default)]
    struct MemTree {
        /// 各 gadget 的描述。
        gadgets: Vec<MemGadget>,
        /// 为 `true` 时 `gadget_dirs` 返回错误（模拟 /config 未挂载）。
        fail: bool,
    }

    impl MemTree {
        fn single() -> Self {
            Self {
                gadgets: vec![("g1".into(), String::new(), vec![("b.1".into(), false)])],
                fail: false,
            }
        }

        fn with(mut self, gadget: &str, udc: &str, configs: &[(&str, bool)]) -> Self {
            self.gadgets.push((
                gadget.into(),
                udc.into(),
                configs.iter().map(|(n, l)| (n.to_string(), *l)).collect(),
            ));
            self
        }
    }

    impl GadgetTree for MemTree {
        fn gadget_dirs(&self) -> Result<Vec<String>, DiscoverError> {
            if self.fail {
                return Err(DiscoverError::NoUsableGadget("模拟失败".into()));
            }
            Ok(self.gadgets.iter().map(|(n, ..)| n.clone()).collect())
        }

        fn read_udc(&self, gadget: &str) -> String {
            self.gadgets
                .iter()
                .find(|(n, ..)| n == gadget)
                .map(|(_, udc, _)| udc.clone())
                .unwrap_or_default()
        }

        fn config_dirs(&self, gadget: &str) -> Vec<String> {
            self.gadgets
                .iter()
                .find(|(n, ..)| n == gadget)
                .map(|(_, _, configs)| configs.iter().map(|(n, _)| n.clone()).collect())
                .unwrap_or_default()
        }

        fn config_has_symlink(&self, gadget: &str, config: &str) -> bool {
            self.gadgets
                .iter()
                .find(|(n, ..)| n == gadget)
                .and_then(|(_, _, configs)| configs.iter().find(|(n, _)| n == config))
                .map(|(_, has)| *has)
                .unwrap_or(false)
        }
    }

    #[test]
    fn aosp_default_is_used_when_nothing_is_bound() {
        let layout = discover(&MemTree::single(), None).unwrap();
        assert_eq!(layout.gadget_root, PathBuf::from("/config/usb_gadget/g1"));
        assert_eq!(layout.config_name, "b.1");
        assert_eq!(layout.gadget_reason, GadgetChoice::DefaultG1);
        assert_eq!(layout.config_reason, ConfigChoice::DefaultB1);
    }

    /// 回归（真机实测）：红魔9 Pro 上同时有 `g1` 与 vendor 私有的 `g2`，
    /// **已绑定 UDC 的那个才是我们要用的**，不能盲目选 `g1`。
    #[test]
    fn bound_gadget_wins_over_aosp_default() {
        let tree = MemTree::default().with("g1", "", &[("b.1", false)]).with(
            "g2",
            "a600000.dwc3",
            &[("b.1", true)],
        );
        let layout = discover(&tree, Some("a600000.dwc3")).unwrap();
        assert_eq!(layout.gadget_root, PathBuf::from("/config/usb_gadget/g2"));
        assert_eq!(layout.gadget_reason, GadgetChoice::BoundToUdc);
        // 该 config 含链接 → 判为 Android 正在用。
        assert_eq!(layout.config_reason, ConfigChoice::HasLinks);
    }

    /// 只有一个 gadget 时直接用，不必叫 `g1`。
    #[test]
    fn sole_gadget_with_unusual_name_is_accepted() {
        let tree = MemTree::default().with("gadgetdisk", "", &[("b.1", false)]);
        let layout = discover(&tree, None).unwrap();
        assert_eq!(
            layout.gadget_root,
            PathBuf::from("/config/usb_gadget/gadgetdisk")
        );
        assert_eq!(layout.gadget_reason, GadgetChoice::SoleGadget);
    }

    /// 配置目录名不叫 `b.1` 且含链接时必须选它。
    #[test]
    fn non_default_config_with_links_is_chosen() {
        let tree = MemTree::default().with("g1", "", &[("b.1", false), ("b.2", true)]);
        let layout = discover(&tree, None).unwrap();
        assert_eq!(layout.config_name, "b.2");
        assert_eq!(layout.config_reason, ConfigChoice::HasLinks);
    }

    /// `b.1` 存在但为空 → 仍选 `b.1`（AOSP 默认），理由为 `DefaultB1`。
    #[test]
    fn empty_b1_is_chosen_as_aosp_default() {
        let tree = MemTree::default().with("g1", "", &[("b.1", false), ("b.2", false)]);
        let layout = discover(&tree, None).unwrap();
        assert_eq!(layout.config_name, DEFAULT_CONFIG_NAME);
        assert_eq!(layout.config_reason, ConfigChoice::DefaultB1);
    }

    /// 多个空配置且**没有** `b.1`：确定性地落到 `b.1`（待创建），
    /// 不去猜某个已有空目录——猜错会把链接建到一个 Android 不用的配置里。
    #[test]
    fn multiple_empty_configs_without_b1_still_target_b1() {
        let tree = MemTree::default().with("g1", "", &[("b.2", false), ("b.3", false)]);
        let layout = discover(&tree, None).unwrap();
        assert_eq!(layout.config_name, DEFAULT_CONFIG_NAME);
        assert_eq!(layout.config_reason, ConfigChoice::ToCreate);
    }

    /// 一个 config 都没有 → 用 `b.1` 待创建（首次挂载的常见情形）。
    #[test]
    fn no_config_at_all_means_to_create_b1() {
        let tree = MemTree::default().with("g1", "", &[]);
        let layout = discover(&tree, None).unwrap();
        assert_eq!(layout.config_name, "b.1");
        assert_eq!(layout.config_reason, ConfigChoice::ToCreate);
    }

    /// 多个 gadget 且都无法判定 → **报错**，绝不猜。
    #[test]
    fn ambiguous_gadgets_are_an_error_not_a_guess() {
        let tree = MemTree::default()
            .with("alpha", "", &[("b.1", false)])
            .with("beta", "", &[("b.1", false)]);
        let err = discover(&tree, Some("nothing-bound")).unwrap_err();
        assert!(matches!(err, DiscoverError::NoUsableGadget(_)), "{err:?}");
        // 错误信息要列出候选，便于用户排查。
        assert!(err.to_string().contains("alpha"), "{err}");
        assert!(err.to_string().contains("beta"), "{err}");
    }

    /// 一个 gadget 都没有 → `NoGadget`。
    #[test]
    fn empty_gadget_parent_is_reported() {
        let tree = MemTree::default();
        assert_eq!(discover(&tree, None).unwrap_err(), DiscoverError::NoGadget);
    }

    /// 读不到父目录（configfs 未挂载）→ 明确错误，不得盲写。
    #[test]
    fn unreadable_gadget_parent_is_reported() {
        let tree = MemTree {
            gadgets: vec![],
            fail: true,
        };
        assert!(matches!(
            discover(&tree, None).unwrap_err(),
            DiscoverError::NoUsableGadget(_)
        ));
    }
}
