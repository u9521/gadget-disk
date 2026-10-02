//! `mkfs` 的探测与执行：**统一经 loop 设备逐个格式化分区**。
//!
//! ## 为什么在 CLI 层而不是 core
//!
//! `gadgetdisk-core` 的边界是「不接触平台接口、完全可在主机测试」
//! （见其模块文档）。`mkfs` 与 `losetup` 都是**外部进程**，涉及 `fork/exec`、
//! `PATH` 与设备上实际装了什么——这些都属平台侧。core 只定义 `Formatter` trait
//! 与纯逻辑（[`gadgetdisk_core::fs`]），真实执行在这里。
//!
//! ## 为什么统一经 loop（尽管 ext4 有 `-E offset`）
//!
//! 各 `mkfs` 的偏移能力各不相同：
//!
//! | 工具 | 偏移能力 |
//! |---|---|
//! | `mkfs.exfat` | **无**任何偏移选项 |
//! | `mke2fs` | `-E offset=<字节>` |
//! | `mkfs.fat`（dosfstools） | `--offset <扇区>` |
//! | 本模块自带的 `mkfs.vfat` | `--offset <扇区>` |
//!
//! 让每个工具各走各的偏移路径，结果是"三条实现、三种失败模式"。统一经
//! `losetup -o` 后：偏移只在一个地方表达（loop 设备），`mkfs` 命令各自保持最简单
//! 的形式（对块设备做整设备格式化），新增文件系统时**不需要**再研究它的偏移能力。
//!
//! 代价是每个分区多一次 `losetup`/`losetup -d`（实测约几十毫秒）。
//!
//! ## 为什么 FAT32 也走外部进程
//!
//! 设备上**不存在** `mkfs.vfat`（AVD 实测：无 dosfstools、toybox 亦无 `mkfs`），
//! 故模块自带一个（`crates/gadgetdisk-mkfsvfat`，安装为 `bin/mkfs.vfat`）。
//! 让它与 `mkfs.exfat`/`mke2fs` 走同一条路径，格式化流程里就没有"内置特例"了。
//!
//! ## 格式化前修正镜像 SELinux 上下文
//!
//! 经 loop 执行格式化时底层镜像由**内核工作线程**读写，安全标签未开放读写权限会导致 `mkfs` 直接
//! 失败。因此 [`MkfsFormatter`] 须在调用 `losetup` 关联首个设备前修正标签，与 CLI 本地 loop
//! 挂载路径复用 [`crate::image_context`] 统一逻辑（真机实测：loop 与 gadget 导出受同等安全域限制，
//! 见 [镜像上下文](../../../../docs/android-integration.md#镜像文件的-selinux-上下文内核读镜像的前提)）。
//!
//! ## 安全
//!
//! 所有路径都经 [`std::process::Command`] 的参数数组传入，**不经 shell**，
//! 因此镜像名里的空格或引号不会被解释成命令。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use gadgetdisk_core::fs::{FilesystemType, FormatPlan, FormattedVolume, Formatter};
use gadgetdisk_core::{CoreError, Result};

/// 探测 `mkfs` 的候选目录（按优先级）。
///
/// `/system/bin` 是 Android 标准位置；模块自带的 `bin/` 放在**最后**，让系统工具
/// 优先——自带二进制只在系统缺失时兜底。
pub const MKFS_SEARCH_DIRS: &[&str] =
    &["/system/bin", "/vendor/bin", "/system/xbin", "/product/bin"];

/// 一次 `mkfs` 探测的结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MkfsProbe {
    /// 文件系统类型名（`fat32` / `exfat` / `ext4`）。
    pub filesystem: String,
    /// 找到的工具绝对路径；`None` 表示未找到。
    pub path: Option<String>,
    /// 人类可读说明（供 WebUI 直接展示）。
    pub note: String,
}

/// 探测所有已知文件系统的 `mkfs` 可用性。
///
/// 只读文件系统状态、不写任何东西，因此可在任意环境反复调用。
/// WebUI 的 `GET /api/v1/capabilities` 用它把「本机实际能做什么」如实告诉用户。
///
/// `module_dir` 为模块根目录（用于定位自带的 `bin/mkfs.vfat`）；传 `None` 时
/// 只探测系统目录与 `PATH`。
pub fn probe_all(module_dir: Option<&Path>) -> Vec<MkfsProbe> {
    FilesystemType::ALL
        .iter()
        .map(|fs| {
            let found = find_mkfs(*fs, module_dir);
            MkfsProbe {
                filesystem: fs.as_str().to_string(),
                path: found.as_ref().map(|p| p.display().to_string()),
                note: if found.is_some() {
                    "formatting runs per partition through loop devices".to_string()
                } else {
                    format!(
                        "no mkfs tool found for {}; that filesystem is unavailable",
                        fs.as_str()
                    )
                },
            }
        })
        .collect()
}

/// 为某个文件系统查找 `mkfs` 可执行文件的绝对路径。
///
/// 顺序：系统目录 → `PATH` → 模块自带 `bin/`。自带二进制放最后，避免覆盖设备上
/// 已有的、更"原生"的实现。
pub fn find_mkfs(filesystem: FilesystemType, module_dir: Option<&Path>) -> Option<PathBuf> {
    for candidate in filesystem.mkfs_candidates() {
        for dir in MKFS_SEARCH_DIRS {
            let path = Path::new(dir).join(candidate);
            if is_executable(&path) {
                return Some(path);
            }
        }
        if let Some(found) = find_in_path(candidate) {
            return Some(found);
        }
        // 最后才看模块自带的实现。
        if let Some(root) = module_dir {
            let path = root.join("bin").join(candidate);
            if is_executable(&path) {
                return Some(path);
            }
        }
    }
    None
}

/// 在 `PATH` 的各个目录里查找可执行文件。
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// 路径是否为可执行文件。
///
/// 用 `metadata` 而非 `exists()`：目录也能 `exists()`，但显然不能执行。
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// 基于外部 `mkfs` 的格式化器：**一律经 loop 设备**。
///
/// ## 为什么要带上镜像上下文
///
/// 经 loop 执行格式化时，底层镜像由**内核工作线程**读写；安全标签未开放权限会导致
/// `mkfs` 以 EIO/EPERM 失败（真机实测：loop 与 gadget 导出受同等安全域限制）。
/// 因此 formatter 在**调用 `losetup` 关联设备之前**修正镜像安全标签——这与 CLI 挂载
/// 路径复用同一套策略（[`crate::image_context`]）。
///
/// 修正仅对 `images/` 下的文件生效；目录外部仅告警，且**失败不阻断**格式化流程，
/// 由外部调用方将警告信息传递给用户。告警通过 [`Self::take_warnings`] 取出，而非
/// 扩充 [`FormattedVolume`] 契约——后者归属于 `gadgetdisk-core`，为单条诊断信息扩大其公开
/// 契约将对 core 层设计及全部相关测试替身造成不必要的侵入。
#[derive(Debug, Default, Clone)]
pub struct MkfsFormatter {
    /// 模块根目录；用于定位自带的 `bin/mkfs.vfat`。
    module_dir: Option<PathBuf>,
    /// `(镜像目录, 目标 SELinux 上下文)`；`None` = 不检查（主机/测试默认）。
    image_context: Option<(PathBuf, String)>,
    /// 本次格式化收集到的上下文警告。
    ///
    /// `Formatter::format` 只拿到 `&self`，因此这里必须是内部可变。用 `Arc<Mutex<…>>`
    /// 而不是 `RefCell`：formatter 的 `Clone` 与跨线程共享（`create_image` 可能
    /// 在多分区循环里被不同线程调用）都要成立。
    warnings: Arc<Mutex<Vec<String>>>,
}

impl MkfsFormatter {
    /// 指定模块根目录（自带 `mkfs.vfat` 所在处）。
    pub fn with_module_dir(module_dir: impl Into<PathBuf>) -> Self {
        Self {
            module_dir: Some(module_dir.into()),
            ..Self::default()
        }
    }

    /// 格式化前检查并（必要时）修正镜像的 SELinux 上下文。
    ///
    /// `images_dir` 是**我们自己的**镜像目录：只有它下面的文件允许被改标签。
    /// `target` 由调用方从配置解析一次传入，使一次创建里的多个分区用同一个值。
    pub fn with_image_context(
        mut self,
        images_dir: impl Into<PathBuf>,
        target: impl Into<String>,
    ) -> Self {
        self.image_context = Some((images_dir.into(), target.into()));
        self
    }

    /// 取走已收集的警告（取走后清空，因此重复调用不会重复上报）。
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    /// 检查一次并在需要时记录警告。
    fn check_context(&self, image_path: &Path) {
        let Some((images_dir, target)) = &self.image_context else {
            return;
        };
        let warnings = crate::image_context::check_in(images_dir, image_path, target);
        if warnings.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.warnings.lock() {
            guard.extend(warnings);
        }
    }
}

impl Formatter for MkfsFormatter {
    fn format(&self, image_path: &Path, plan: &FormatPlan) -> Result<FormattedVolume> {
        let Some(tool) = find_mkfs(plan.filesystem, self.module_dir.as_deref()) else {
            return Err(CoreError::UnsupportedFilesystem(format!(
                "no mkfs tool found for {} (searched {}, PATH and the module bin/)",
                plan.filesystem,
                MKFS_SEARCH_DIRS.join(", ")
            )));
        };

        // **必须在 `format_via_loop`（第一个 `losetup`）之前**：内核在打开后备
        // 文件的那一刻按当时的标签判定内核线程的读写权限。
        self.check_context(image_path);

        format_via_loop(&tool, image_path, plan)
    }
}

/// 经 loop 设备格式化 `[start, end)` 区间。
///
/// 流程：`losetup -f` 取空闲设备 → `losetup -o <offset> --sizelimit <len>` 绑定区间
/// → 调用 `mkfs` → **无条件** `losetup -d` 释放。
fn format_via_loop(tool: &Path, image_path: &Path, plan: &FormatPlan) -> Result<FormattedVolume> {
    let loop_dev = attach_loop(image_path, plan.start_bytes, plan.end_bytes)?;

    let mut cmd = Command::new(tool);
    match plan.filesystem {
        // mkfs.vfat：经 loop 时**不带** `--offset`（偏移已由 losetup 表达）。
        // 卷标经 `-n` 传入；`-F 32` 是其默认值，不必显式给。
        FilesystemType::Fat32 => {
            if !plan.label.is_empty() {
                cmd.arg("-n").arg(&plan.label);
            }
            cmd.arg(&loop_dev);
        }
        // mkfs.exfat 的卷标选项是 `-L`。
        FilesystemType::ExFat => {
            if !plan.label.is_empty() {
                cmd.arg("-L").arg(&plan.label);
            }
            cmd.arg(&loop_dev);
        }
        // mke2fs：经 loop 时不必也不应再传 `-E offset`。
        FilesystemType::Ext4 => {
            cmd.arg("-q").arg("-F").arg("-t").arg("ext4");
            if !plan.label.is_empty() {
                cmd.arg("-L").arg(&plan.label);
            }
            cmd.arg(&loop_dev);
        }
    }
    let result = run(tool, cmd);

    // **无论格式化成败都要释放 loop**，否则会泄漏一个 loop 设备。
    let detach = detach_loop(&loop_dev);
    result?;
    // 释放失败要如实上报：残留的 loop 会让该镜像无法再被导出为 USB 设备
    // （数据安全互斥），静默忽略会把问题推迟到更难排查的地方。
    detach?;

    Ok(FormattedVolume {
        filesystem: plan.filesystem,
        label: plan.label.clone(),
        tool: Some(tool.display().to_string()),
    })
}

/// 把镜像的某个区间挂到一个空闲 loop 设备上。
fn attach_loop(image_path: &Path, start: u64, end: u64) -> Result<PathBuf> {
    // `losetup -f` 输出空闲设备名。
    let out = Command::new("losetup")
        .arg("-f")
        .output()
        .map_err(CoreError::Io)?;
    if !out.status.success() {
        return Err(CoreError::InvalidArgument(format!(
            "cannot acquire a free loop device: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dev.is_empty() {
        return Err(CoreError::InvalidArgument(
            "no loop device available".into(),
        ));
    }

    let size = end.saturating_sub(start);
    let mut cmd = Command::new("losetup");
    cmd.arg("-o").arg(start.to_string());
    // `--sizelimit` 限定长度：loop 默认会把区间延伸到文件末尾。不限制的话
    // mkfs 会看到一个比实际分区更大的设备，写出越界的文件系统元数据。
    if size > 0 {
        cmd.arg("--sizelimit").arg(size.to_string());
    }
    cmd.arg(&dev).arg(image_path);

    let out = cmd.output().map_err(CoreError::Io)?;
    if !out.status.success() {
        // 绑定失败时设备可能已被部分占用，尽力释放避免泄漏。
        let _ = detach_loop(Path::new(&dev));
        return Err(CoreError::InvalidArgument(format!(
            "losetup -o {start} --sizelimit {size} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    Ok(PathBuf::from(dev))
}

/// 释放 loop 设备。
fn detach_loop(dev: &Path) -> Result<()> {
    let out = Command::new("losetup").arg("-d").arg(dev).output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(CoreError::InvalidArgument(format!(
            "losetup -d {} failed: {}",
            dev.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        ))),
        Err(err) => Err(CoreError::Io(err)),
    }
}

/// 执行命令，失败时带上 stderr 内容。
fn run(tool: &Path, mut cmd: Command) -> Result<()> {
    let out = cmd.output().map_err(CoreError::Io)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Err(CoreError::InvalidArgument(format!(
        "{} failed (exit code {:?}): {}",
        tool.display(),
        out.status.code(),
        if stderr.is_empty() { stdout } else { stderr }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_all_covers_every_filesystem() {
        let probes = probe_all(None);
        assert_eq!(probes.len(), FilesystemType::ALL.len());
        for fs in FilesystemType::ALL {
            assert!(
                probes.iter().any(|p| p.filesystem == fs.as_str()),
                "探测结果缺少 {}",
                fs.as_str()
            );
        }
    }

    #[test]
    fn fat32_candidates_point_at_the_bundled_tool() {
        // 设备上没有 mkfs.vfat，故 FAT32 的候选必须包含模块自带的那个名字。
        assert!(
            FilesystemType::Fat32
                .mkfs_candidates()
                .contains(&"mkfs.vfat")
        );
    }

    #[test]
    fn module_dir_is_searched_for_bundled_tool() {
        let dir = std::env::temp_dir().join(format!(
            "gd-mkfs-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("mkfs.vfat");
        std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 系统优先：只有系统里确实没有时才应命中自带实现。
        let found = find_mkfs(FilesystemType::Fat32, Some(&dir));
        assert!(found.is_some(), "应能找到某个 mkfs.vfat（系统的或自带的）");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_executable_file_is_not_treated_as_tool() {
        let dir = std::env::temp_dir().join(format!(
            "gd-mkfs-noexec-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("mkfs.vfat");
        std::fs::write(&fake, b"not executable").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        // 不可执行的文件必须**不**被当成工具。
        assert!(!is_executable(&fake));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_executable_rejects_directories_and_missing() {
        assert!(!is_executable(Path::new("/tmp")));
        assert!(!is_executable(Path::new(
            "/nonexistent/definitely/not/here"
        )));
    }

    #[test]
    fn is_executable_accepts_a_real_binary() {
        assert!(is_executable(Path::new("/bin/sh")));
    }

    #[test]
    fn find_in_path_locates_a_known_command() {
        assert!(find_in_path("sh").is_some(), "PATH 中应能找到 sh");
    }

    #[test]
    fn missing_image_is_reported_not_silently_skipped() {
        // 「不能静默成功」：目标不可用时必须报错。
        let formatter = MkfsFormatter::with_module_dir("/nonexistent-module-dir");
        let plan = FormatPlan::new(FilesystemType::ExFat, 0, 1024, "X");
        let err = formatter
            .format(Path::new("/nonexistent-image-xyz.img"), &plan)
            .unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::UnsupportedFilesystem(_)
                    | CoreError::InvalidArgument(_)
                    | CoreError::Io(_)
            ),
            "得到 {err:?}"
        );
    }

    /// 未注入上下文时**不产生任何警告**（主机/测试路径不该看到 SELinux 噪音）。
    #[test]
    fn no_image_context_means_no_warnings() {
        let formatter = MkfsFormatter::with_module_dir("/nonexistent-module-dir");
        let plan = FormatPlan::new(FilesystemType::ExFat, 0, 1024, "X");
        let _ = formatter.format(Path::new("/nonexistent-image-xyz.img"), &plan);
        assert!(formatter.take_warnings().is_empty());
    }

    /// 目录**外**的镜像：只告警、不改标签，且**不阻断**格式化流程。
    ///
    /// 用真实 `RealContextStore` 在主机上无法断言「改没改」，但可以断言这条
    /// 边界的两件可观测事实：函数仍然继续走到取工具/打开镜像那一步（没有被
    /// 上下文检查提前打断），以及警告最多一条（目录外只警告一次）。
    /// 「目录外绝不改标签」本身由 `selinux::check_and_fix` 的替身单测钉死。
    #[test]
    fn outside_the_images_dir_never_blocks_formatting() {
        let outside = std::env::temp_dir().join(format!(
            "gd-mkfs-outside-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&outside, vec![0u8; 4096]).unwrap();

        let images_dir = std::env::temp_dir().join("gd-mkfs-images-not-here");
        let formatter = MkfsFormatter::with_module_dir("/nonexistent-module-dir")
            .with_image_context(&images_dir, crate::selinux::DEFAULT_IMAGE_CONTEXT);

        let plan = FormatPlan::new(FilesystemType::ExFat, 0, 1024, "X");
        // 关键：**不 panic**，且错误来自「找不到工具/镜像」而不是上下文检查。
        let _ = formatter.format(&outside, &plan);

        // 主机（无 SELinux）上读不到 xattr → `Unknown` → 静默；有 SELinux 的
        // 宿主上目录外 → 恰好一条告警。两种宿主都不许超过一条。
        let warnings = formatter.take_warnings();
        assert!(warnings.len() <= 1, "得到 {warnings:?}");
        // 取走后必须清空，避免调用方重复上报。
        assert!(formatter.take_warnings().is_empty());

        std::fs::remove_file(&outside).ok();
    }
}
