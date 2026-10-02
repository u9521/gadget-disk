//! 镜像文件的 SELinux 上下文：**所有会挂载镜像的路径**共用的检查入口。
//!
//! ## 为什么单独一个模块
//!
//! 该检查原本仅服务于 gadget 导出（原实现位于 `gadget_adapter`，模块文档对应
//! 「gadget 接线」）。真机实测发现**本地 loop 挂载同样受内核线程安全域限制**：后备镜像由内核
//! 线程读写，SELinux 依据**该线程所属安全域**判定访问权限，与“谁发起了挂载”无关。
//! 因此包含三条涉及内核读写镜像的路径：
//!
//! | 路径 | 内核侧读写者 | 入口 |
//! |---|---|---|
//! | 导出为 USB 设备（gadget LUN） | `file-storage` 内核线程 | `run_mount` / `serve::gdd_op` |
//! | 本地挂载（loop） | loop 工作线程 | `run_attach_loop` / `serve::loop_attach` |
//! | 经 loop 的格式化（`mkfs`） | loop 工作线程 | `MkfsFormatter::format` |
//!
//! 三者统一复用 [`check`]，避免任一分支遗漏——未修正上下文导致的后果（Host 端读不出
//! 内容、写入被静默丢弃、`mkfs` 报 EIO）均极难从表面现象反推根因。
//!
//! ## 为什么不在 `gdd`
//!
//! 这是**用户可见的策略决策**（是否修正用户文件标签及修正为何值），而非
//! mass_storage 的底层执行细节。`gdd` 的职责被严格限定为管理 configfs 状态与 LUN 配置，
//! 若在其中修改文件标签将破坏职责边界（`crates/gadgetdisk-gdd/tests/scope.rs` 亦会因此失效）。
//!
//! ## 为什么不在 `gadgetdisk-loop`
//!
//! 该 crate 专注于**内核 loop 接口**（ioctl / `mount(2)` / 释放顺序），不感知
//! SELinux，亦不应为扩展属性（xattr）操作引入模块配置依赖。检查由外部调用方在打开后备文件**之前**
//! 完成，顺序由调用点保证（loop 侧的顺序断言见 `gadgetdisk-loop` 的 `attach`）。
//!
//! ## 边界
//!
//! 仅修改本模块 `images/` 目录内的文件；目录外部仅输出告警。修正失败不阻断挂载流程。
//! 判定规则与实测依据见 [`crate::selinux`] 的模块文档。

use std::path::Path;

use gadgetdisk_gdd::DataDirs;

/// 检查并（必要时）修正镜像的 SELinux 上下文，返回**用户可见的警告文本**。
///
/// 返回空 `Vec` 表示无需告警：上下文已符合预期，或当前环境不支持 SELinux（无 xattr 支持，属于正常情况）。
///
/// `images/` 之外的文件只警告、绝不修改标签（用户文件可能被系统其他策略依赖）。
///
/// 批量操作（一次导出/挂载多个镜像、创建时格式化多个分区）应当先取一次
/// [`target_for`]，再对每个镜像调用 [`check_in`]——配置只读一次，确保同一批操作
/// 不会因配置在途中被 WebUI 修改而使用不同的目标标签。
pub fn check(dirs: &DataDirs, image: &Path) -> Vec<String> {
    check_in(&dirs.images(), image, &target_for(dirs))
}

/// 当前生效的目标上下文（`config/gadget.json` 的 `image_context`，缺省用内置默认）。
pub fn target_for(dirs: &DataDirs) -> String {
    crate::cli_paths::GadgetConfig::load(dirs).resolved_image_context()
}

/// 用**调用方给定的**目标上下文与镜像目录执行检查。
///
/// 不接收 `DataDirs`：`MkfsFormatter` 拿不到它（formatter 被注入
/// `gadgetdisk-core` 的创建流程，而 `DataDirs` 归 CLI 的挂载编排层），
/// 因此镜像目录与目标值由调用方传入。
pub fn check_in(images_dir: &Path, image: &Path, target: &str) -> Vec<String> {
    let outcome =
        crate::selinux::check_and_fix(&crate::selinux::RealContextStore, images_dir, image, target);
    outcome.warning(image).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dirs(tag: &str) -> (DataDirs, PathBuf) {
        let root = crate::testutil::temp_dir(tag);
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        (dirs, root)
    }

    /// 配置里设了值 → 目标就是它；没设 → 内置默认。
    #[test]
    fn target_comes_from_the_config_file() {
        let (dirs, root) = dirs("image-context-target");
        assert_eq!(target_for(&dirs), crate::selinux::DEFAULT_IMAGE_CONTEXT);

        crate::cli_paths::GadgetConfig::store_image_context(
            &dirs,
            Some("u:object_r:vendor_file:s0"),
        )
        .unwrap();
        assert_eq!(target_for(&dirs), "u:object_r:vendor_file:s0");

        crate::testutil::cleanup(&root);
    }

    /// 非 SELinux 宿主（xattr 读不到）→ **无警告**，不是错误。
    ///
    /// 这条在开发机上直接可跑：`/tmp` 通常没有 `security.selinux`，
    /// 宿主上 `lgetxattr` 会以 `ENODATA`/`ENOTSUP` 失败，正好是 `Unknown` 分支。
    #[test]
    fn missing_xattr_support_is_silent() {
        let (dirs, root) = dirs("image-context-no-selinux");
        let image = dirs.images().join("a.img");
        std::fs::write(&image, b"x").unwrap();

        let warnings = check(&dirs, &image);
        // 有 SELinux 的宿主上这个文件会被真的打上标签 → 此时也**不该**有警告
        // （`AlreadyCorrect` 同样静默）。两种宿主上都必须为空。
        assert!(warnings.is_empty(), "得到 {warnings:?}");

        crate::testutil::cleanup(&root);
    }

    /// `images/` 之外的文件**只警告、绝不改标签**——这条边界必须从本模块也能看到。
    ///
    /// 判据取「警告非空」；是否真的改了由 `selinux::check_and_fix` 的单测
    /// （`files_outside_the_images_dir_are_never_relabeled`）用替身存储断言。
    /// 这里的文件没有 xattr，因此会走 `Unknown` 而不是 `OutsideImagesDir`；
    /// 在真机上它才有意义——所以本测试只断言「不 panic、不阻断」。
    #[test]
    fn outside_images_dir_never_blocks_the_caller() {
        let (dirs, root) = dirs("image-context-outside");
        let outside = root.join("outside.img");
        std::fs::write(&outside, b"x").unwrap();

        // 关键：函数必须**返回**（不 panic、不 Err），调用方才能继续挂载。
        let _ = check(&dirs, &outside);
        let _ = check_in(
            &dirs.images(),
            &outside,
            crate::selinux::DEFAULT_IMAGE_CONTEXT,
        );

        crate::testutil::cleanup(&root);
    }

    /// `check_in` 与 `check` 必须使用**同一套**判定（同一个 `check_and_fix`）。
    ///
    /// 两者被不同路径使用（loop 挂载用 `check`，格式化用 `check_in`），漂移会让
    /// 「同一条策略」在不同入口表现不一致。
    #[test]
    fn check_in_agrees_with_check() {
        let (dirs, root) = dirs("image-context-agree");
        let image = dirs.images().join("a.img");
        std::fs::write(&image, b"x").unwrap();
        let target = target_for(&dirs);

        assert_eq!(
            check_in(&dirs.images(), &image, &target),
            check(&dirs, &image),
            "两个入口必须给出同样的结论"
        );

        crate::testutil::cleanup(&root);
    }
}
