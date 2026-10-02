//! FAT32 目录遍历封装。
//!
//! **已知陷阱**：`fatfs::Dir::iter()` 会产出 `.` 与 `..`。实测后果是
//! 按 `e.is_dir()` 递归遍历时会对 `.` 反复递归，最终栈溢出崩溃
//! （`thread 'main' has overflowed its stack`）。
//!
//! 因此本模块是唯一允许遍历目录的入口，并在唯一位置强制跳过这两个条目。
//! 规格见 [docs/disk-image-format.md](../../../docs/disk-image-format.md)。

use std::io::{Read, Seek, Write};

use crate::Result;

/// FAT32 目录遍历产出的规整条目。
///
/// 剔除了 `fatfs` 产出的 `.` 与 `..` 伪条目，避免递归统计或遍历时陷入循环。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 文件或目录名（已过滤伪条目）。
    pub name: String,
    /// 是否为目录。
    pub is_dir: bool,
    /// 文件大小（目录固定为 0）。
    pub size: u64,
}

/// 判断目录项名是否为必须跳过的伪条目。
pub fn is_pseudo_entry(name: &str) -> bool {
    name == "." || name == ".."
}

/// 列出目录下的条目，**按契约跳过 `.` 与 `..`**。
pub fn list_dir<T: Read + Write + Seek>(dir: &fatfs::Dir<'_, T>) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in dir.iter() {
        let entry = entry?;
        let name = entry.file_name();

        // 必须在任何递归或大小计算之前跳过，否则会无限递归。
        if is_pseudo_entry(&name) {
            continue;
        }

        entries.push(Entry {
            name,
            is_dir: entry.is_dir(),
            size: if entry.is_dir() { 0 } else { entry.len() },
        });
    }
    Ok(entries)
}

/// 递归统计目录下的文件总字节数。
///
/// 这是 `.`/`..` 陷阱最危险的使用场景：不跳过伪条目会导致栈溢出。
/// 本函数委托 [`list_dir`]，因此天然安全；深度上限作为额外保险。
pub fn total_bytes<T: Read + Write + Seek>(
    dir: &fatfs::Dir<'_, T>,
    max_depth: usize,
) -> Result<u64> {
    walk(dir, max_depth)
}

fn walk<T: Read + Write + Seek>(dir: &fatfs::Dir<'_, T>, remaining_depth: usize) -> Result<u64> {
    if remaining_depth == 0 {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in list_dir(dir)? {
        if entry.is_dir {
            if let Ok(sub) = dir.open_dir(&entry.name) {
                total += walk(&sub, remaining_depth - 1)?;
            }
        } else {
            total += entry.size;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use fatfs::{FatType, FormatVolumeOptions, FsOptions};
    use fscommon::StreamSlice;

    fn volume(
        tag: &str,
    ) -> (
        std::path::PathBuf,
        fatfs::FileSystem<StreamSlice<std::fs::File>>,
    ) {
        let size = 64 * 1024 * 1024;
        let (path, file) = testutil::temp_image(tag, size);
        let slice = StreamSlice::new(file, 0, size).unwrap();
        fatfs::format_volume(
            slice,
            FormatVolumeOptions::new()
                .fat_type(FatType::Fat32)
                .volume_label(*b"GADGETDISK "),
        )
        .unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let slice = StreamSlice::new(file, 0, size).unwrap();
        let fs = fatfs::FileSystem::new(slice, FsOptions::new()).unwrap();
        (path, fs)
    }

    #[test]
    fn pseudo_entries_are_detected() {
        assert!(is_pseudo_entry("."));
        assert!(is_pseudo_entry(".."));
        assert!(!is_pseudo_entry("..."));
        assert!(!is_pseudo_entry("file.txt"));
    }

    #[test]
    fn listing_skips_pseudo_entries() {
        let (path, fs) = volume("dir-list");
        let root = fs.root_dir();

        std::io::Write::write_all(&mut root.create_file("alpha.txt").unwrap(), b"abc").unwrap();
        root.create_dir("sub").unwrap();

        let entries = list_dir(&root).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(!names.contains(&"."));
        assert!(!names.contains(&".."));
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"sub"));
        assert_eq!(entries.len(), 2);

        drop(root);
        drop(fs);
        testutil::cleanup(&path);
    }

    #[test]
    fn recursive_walk_terminates_on_empty_volume() {
        // 回归测试：若不跳过 `.`/`..`，此处会因对 `.` 反复递归而栈溢出。
        let (path, fs) = volume("dir-empty");
        let root = fs.root_dir();
        let total = total_bytes(&root, 16).unwrap();
        assert_eq!(total, 0);

        drop(root);
        drop(fs);
        testutil::cleanup(&path);
    }

    #[test]
    fn recursive_walk_sums_nested_files() {
        let (path, fs) = volume("dir-nested");
        let root = fs.root_dir();

        std::io::Write::write_all(&mut root.create_file("top.txt").unwrap(), b"12345").unwrap();

        let nested = root.create_dir("n1").unwrap();
        std::io::Write::write_all(&mut nested.create_file("a.txt").unwrap(), b"123").unwrap();
        let deeper = nested.create_dir("n2").unwrap();
        std::io::Write::write_all(&mut deeper.create_file("b.txt").unwrap(), b"1234567").unwrap();

        let total = total_bytes(&root, 16).unwrap();
        assert_eq!(total, 5 + 3 + 7);

        drop(deeper);
        drop(nested);
        drop(root);
        drop(fs);
        testutil::cleanup(&path);
    }

    #[test]
    fn walk_respects_depth_limit() {
        let (path, fs) = volume("dir-depth");
        let root = fs.root_dir();
        let nested = root.create_dir("n1").unwrap();
        std::io::Write::write_all(&mut nested.create_file("deep.txt").unwrap(), b"1234567890")
            .unwrap();

        // 深度 0：什么都不统计。
        assert_eq!(total_bytes(&root, 0).unwrap(), 0);
        // 深度 1：进入 n1，但 recursion 已到 0，故不计其文件。
        assert_eq!(total_bytes(&root, 1).unwrap(), 0);
        // 深度 2：完整统计。
        assert_eq!(total_bytes(&root, 2).unwrap(), 10);

        drop(nested);
        drop(root);
        drop(fs);
        testutil::cleanup(&path);
    }
}
