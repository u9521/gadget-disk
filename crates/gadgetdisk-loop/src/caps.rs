//! 能力探测阶梯。
//!
//! 规格（[docs/ondevice-loop-mount.md](../../../../docs/ondevice-loop-mount.md)
//! 的「能力探测阶梯」）定义了四级探测，结果经 `CapabilitiesResponse`
//! 暴露给 WebUI，决定走哪条挂载路径。
//!
//! ## 为什么探测要可替身
//!
//! 探测结果**决定分支**（`-P` 整盘 vs `lo_offset` 单分区），
//! 而分支逻辑是本模块最需要测试的部分。真实探测需要 root 与特定内核，
//! 因此探测的**输入**（文件是否存在、参数值）被抽成 [`CapabilitySource`]，
//! 由 [`probe`] 消费。

use std::path::Path;

use gadgetdisk_proto::Capabilities;

use crate::loopdev::LOOP_CONTROL;

/// 探测所需的外部事实。
pub trait CapabilitySource {
    /// 某个路径是否存在（且可打开）。
    fn exists(&self, path: &Path) -> bool;

    /// 读取一个小文本文件；失败返回 `None`。
    fn read_text(&self, path: &Path) -> Option<String>;

    /// 内核支持的文件系统列表（来自 `/proc/filesystems`）。
    fn filesystems(&self) -> Vec<String>;
}

/// 真实实现：直接读文件系统。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealSource;

impl CapabilitySource for RealSource {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read_text(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn filesystems(&self) -> Vec<String> {
        parse_filesystems(&std::fs::read_to_string("/proc/filesystems").unwrap_or_default())
    }
}

/// 解析 `/proc/filesystems`，返回**非 `nodev`**（即需要块设备）的类型名。
///
/// 只取非 `nodev` 项：`vfat`、`exfat` 都挂块设备，而 `tmpfs`/`proc` 之类
/// 的 `nodev` 项对 loop 挂载毫无意义。把它们也算作「支持」会误导用户。
pub fn parse_filesystems(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or("");
        if first == "nodev" {
            continue;
        }
        if !first.is_empty() {
            out.push(first.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 解析 `/sys/module/loop/parameters/max_part`。
///
/// 该参数在**内核内置 loop** 时只读（实测为 `-r--r--r--`，值 `0`）。
///
/// **用途已收窄**：不再用于判断分区扫描（那条路径已移除），而是供
/// [`crate::loopdev`] 推导 loop 设备次设备号（`minor = index * (max_part + 1)`），
/// 以便在 Android 未预建 `/dev/block/loopN` 时补建**正确**的节点。
pub fn parse_max_part(text: &str) -> u32 {
    text.trim().parse().unwrap_or(0)
}

/// 执行探测，产出 [`Capabilities`]。
///
/// `mass_storage_supported` 不在此处判断：它属于 USB 侧，由 `usb_adapter`
/// 填充后合并，避免本 crate 依赖 configfs。
///
/// **不再探测分区扫描**：曾由此派生 `partscan_supported`
/// （`loop_control && max_part > 0`），但该布尔值只反映内核参数，
/// 而 Android 无 udev/devtmpfs、内核不建 `loopNpM` 节点，路径恒回退。
/// 该字段与整条 partscan 挂载路径已于 2026-10-06 移除——探测返回
/// `true` 而实际不可用，正是「未验证即标注」要防的乐观报告。
pub fn probe(source: &dyn CapabilitySource) -> Capabilities {
    let loop_control = source.exists(Path::new(LOOP_CONTROL));
    let max_part = source
        .read_text(Path::new("/sys/module/loop/parameters/max_part"))
        .map(|t| parse_max_part(&t))
        .unwrap_or(0);
    let filesystems = source.filesystems();
    Capabilities {
        loop_control,
        max_part,
        filesystems,
        mass_storage_supported: false,
        selinux_enforcing: false,
    }
}

/// 内存替身。
#[derive(Debug, Default, Clone)]
pub struct MemSource {
    paths: Vec<String>,
    files: Vec<(String, String)>,
    /// `/proc/filesystems` 的内容。
    pub filesystems_text: String,
}

impl MemSource {
    /// 构造空替身。
    pub fn new() -> Self {
        Self::default()
    }

    /// 让某路径「存在」。
    pub fn with_path(mut self, path: &str) -> Self {
        self.paths.push(path.to_string());
        self
    }

    /// 让某路径可读出内容。
    pub fn with_text(mut self, path: &str, text: &str) -> Self {
        self.paths.push(path.to_string());
        self.files.push((path.to_string(), text.to_string()));
        self
    }

    /// 设置 `/proc/filesystems` 内容。
    pub fn with_filesystems(mut self, text: &str) -> Self {
        self.filesystems_text = text.to_string();
        self
    }
}

impl CapabilitySource for MemSource {
    fn exists(&self, path: &Path) -> bool {
        self.paths.iter().any(|p| Path::new(p) == path)
    }

    fn read_text(&self, path: &Path) -> Option<String> {
        self.files
            .iter()
            .find(|(p, _)| Path::new(p) == path)
            .map(|(_, t)| t.clone())
    }

    fn filesystems(&self) -> Vec<String> {
        parse_filesystems(&self.filesystems_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filesystems_skips_nodev_entries() {
        let text = "nodev\tsysfs\nnodev\tproc\n\tvfat\n\texfat\n";
        assert_eq!(parse_filesystems(text), vec!["exfat", "vfat"]);
    }

    #[test]
    fn parse_filesystems_tolerates_blank_lines() {
        assert!(parse_filesystems("\n\n").is_empty());
    }

    #[test]
    fn parse_max_part_falls_back_to_zero() {
        assert_eq!(parse_max_part("7\n"), 7);
        assert_eq!(parse_max_part(""), 0);
        assert_eq!(parse_max_part("not-a-number"), 0);
    }

    #[test]
    fn probe_reports_loop_control_and_max_part_independently() {
        // 删除 `partscan_supported` 后，探测只回报两个**原始事实**，不再由它们
        // 派生布尔结论。这正是本次修复的核心：此前 `loop_control && max_part > 0`
        // 会在 Android 上报 `true`，而该平台恒回退——探测不该替内核下结论。
        let base = MemSource::new().with_path(LOOP_CONTROL);

        // 只有 loop-control，max_part 缺失 → 如实报 0，不派生出「支持」。
        let caps = probe(
            &base
                .clone()
                .with_path("/sys/module/loop/parameters/max_part"),
        );
        assert!(caps.loop_control);
        assert_eq!(caps.max_part, 0);

        // max_part 为 0（内核内置 loop 的实测情况）。
        let caps = probe(
            &base
                .clone()
                .with_text("/sys/module/loop/parameters/max_part", "0\n"),
        );
        assert!(caps.loop_control);
        assert_eq!(caps.max_part, 0);

        // max_part > 0：AVD 实测即为 7，但挂载仍走 lo_offset。
        // 探测如实回报 7，**不再**由此宣称分区扫描可用。
        let caps = probe(&base.with_text("/sys/module/loop/parameters/max_part", "7\n"));
        assert!(caps.loop_control);
        assert_eq!(caps.max_part, 7);
    }

    #[test]
    fn probe_without_loop_control_still_reports_max_part() {
        // 两个事实互不覆盖：没有 loop-control 时 max_part 仍如实回报。
        let caps = probe(
            &MemSource::new()
                .with_text("/sys/module/loop/parameters/max_part", "7\n")
                .with_filesystems("\tvfat\n"),
        );
        assert!(!caps.loop_control);
        assert_eq!(caps.max_part, 7);
        assert_eq!(caps.filesystems, vec!["vfat"]);
    }

    #[test]
    fn probe_reports_only_block_device_filesystems() {
        let caps = probe(
            &MemSource::new()
                .with_path(LOOP_CONTROL)
                .with_filesystems("nodev\ttmpfs\n\texfat\n\tf2fs\n"),
        );
        assert_eq!(caps.filesystems, vec!["exfat", "f2fs"]);
    }
}
