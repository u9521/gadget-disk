//! 仅测试使用的临时目录助手。
//!
//! 与 `gadgetdisk-gdd`/`gadgetdisk-core` 的同名助手一致：不引入
//! `tempfile` 依赖（目标设备与 CI 都不需要），由调用方显式清理。
//!
//! **语义与那两个 crate 不同，且是刻意的**：本模块的 [`cleanup`] 删除
//! **传入的目录本身**，而不是它的父目录。后者是个危险的陷阱——传目录会
//! 删掉系统临时目录（`cargo test` 曾真的因此清空 `/tmp`，连带破坏同机
//! 其他测试与工具链缓存）。这里选更直观的语义，避免同一个坑再踩一次。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 在系统临时目录下创建一个全新的空目录。
pub fn temp_dir(tag: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "gadgetdisk-{tag}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// 递归删除传入的**目录本身**。
///
/// 只接受由 [`temp_dir`] 生成的路径：拒绝删除 `/` 或任何不在系统临时目录
/// 下的路径，避免误删。这条防护本身就是上面那个陷阱的教训。
pub fn cleanup(dir: &Path) {
    let temp = std::env::temp_dir();
    if !dir.starts_with(&temp) || dir == temp {
        // 拒绝越界删除：宁可泄漏一个临时目录，也不能删错东西。
        return;
    }
    std::fs::remove_dir_all(dir).ok();
}

/// 测试用 FAT32 格式化器：**调用本仓库自带的 `mkfsvfat` 二进制**。
///
/// ## 为什么不用 core 里的实现
///
/// 生产路径上 FAT32 的格式化是**外部进程**（模块自带的 `bin/mkfs.vfat`），
/// core 里只保留了测试用的最小实现。CLI 的测试若继续用 core 的实现，
/// 就测不到真实的调用链（进程、参数、退出码）。
///
/// 因此这里直接执行 `target/<profile>/mkfsvfat`。若该二进制不存在（例如只跑
/// 单个 crate 的测试而未先构建它），会**明确报错**而不是静默跳过——
/// 静默跳过会让测试看起来通过而实际什么都没验证。
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TestMkfsFormatter;

#[cfg(test)]
impl gadgetdisk_core::fs::Formatter for TestMkfsFormatter {
    fn format(
        &self,
        image_path: &Path,
        plan: &gadgetdisk_core::fs::FormatPlan,
    ) -> gadgetdisk_core::Result<gadgetdisk_core::fs::FormattedVolume> {
        use gadgetdisk_core::CoreError;

        if plan.filesystem != gadgetdisk_core::FilesystemType::Fat32 {
            // exFAT/ext4 需要设备上才有的系统工具；测试里只覆盖 FAT32。
            return Err(CoreError::UnsupportedFilesystem(format!(
                "test formatter supports FAT32 only, got {}",
                plan.filesystem
            )));
        }

        let tool = mkfsvfat_binary().ok_or_else(|| {
            CoreError::UnsupportedFilesystem(
                "bundled mkfsvfat binary not found; please build it with `cargo build -p gadgetdisk-mkfsvfat` first"
                    .to_string(),
            )
        })?;

        // 经 `--offset` 直接写区间（测试环境不一定有可用的 loop 设备）。
        let mut cmd = std::process::Command::new(&tool);
        if !plan.label.is_empty() {
            cmd.arg("-n").arg(&plan.label);
        }
        cmd.arg("--offset")
            .arg((plan.start_bytes / 512).to_string());
        let blocks = plan.size_bytes() / 1024;
        cmd.arg(image_path).arg(blocks.to_string());

        let out = cmd
            .output()
            .map_err(|e| CoreError::InvalidArgument(format!("failed to execute mkfsvfat: {e}")))?;
        if !out.status.success() {
            return Err(CoreError::InvalidArgument(format!(
                "mkfsvfat failed (exit code {:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        Ok(gadgetdisk_core::fs::FormattedVolume {
            filesystem: gadgetdisk_core::FilesystemType::Fat32,
            label: plan.label.clone(),
            tool: Some(tool.display().to_string()),
        })
    }
}

/// 定位测试用的 `mkfsvfat` 二进制。
///
/// 测试可执行文件位于 `target/<profile>/deps/`，产物在 `target/<profile>/`。
#[cfg(test)]
pub fn mkfsvfat_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/<test-bin> → .../target/<profile>/
    let profile_dir = exe.parent()?.parent()?;
    let candidate = profile_dir.join("mkfsvfat");
    if candidate.is_file() {
        return Some(candidate);
    }
    // 也可能带 `.exe`（非 Unix 宿主）；本仓库只支持 Unix，故仅作兜底。
    let with_exe = profile_dir.join("mkfsvfat.exe");
    if with_exe.is_file() {
        return Some(with_exe);
    }
    None
}
