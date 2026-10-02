//! JSON 文件的原子写入。
//!
//! ## 为什么必须原子
//!
//! `run/` 下的文件都是**跨重启**恢复的依据：写一半就掉电，下次开机读到的就是
//! 一个残缺的 JSON。用「同目录临时文件 + `rename(2)`」保证读者只会看到
//! 旧内容或新内容，绝不会看到中间态。
//!
//! ## 为什么必须是同目录
//!
//! `rename(2)` 只在**同一文件系统**内原子。`/data/adb/gadget-disk/run/` 与
//! 系统临时目录未必同源（后者可能是 tmpfs），因此临时文件必须落在目标目录里。
//!
//! ## 为什么带 pid 后缀
//!
//! CLI 是**一次性进程**，多个 `gadgetdisk` 可能并发（WebUI 的两次点击）。
//! 固定临时文件名会让两者互相覆盖，写出不属于自己的内容。pid 后缀让并发写
//! 各写各的，最后一个 `rename` 胜出——这正是「最后写入者获胜」的期望语义。

use std::io::Write;
use std::path::Path;

/// 把 `bytes` 原子写入 `path`。
///
/// 步骤：写 `<path>.<pid>.tmp` → `fsync` → `rename` 到 `path`。
///
/// `fsync` 不能省：`rename` 只保证目录项替换是原子的，不保证数据已落盘。
/// 掉电后可能得到一个「已改名但内容为空」的文件。
pub fn write_json_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let temp = temp_path(path);
    {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            // 改名失败（例如目标是个目录）时清掉临时文件，避免留下垃圾。
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// 删除一个文件，不存在视为成功。
pub fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn temp_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    path.with_file_name(format!("{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gd-json-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_content_and_leaves_no_temp_file() {
        let dir = temp_dir("write");
        let path = dir.join("state.json");

        write_json_atomic(&path, br#"{"version":1}"#).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"version":1}"#);
        // 临时文件不得残留。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件：{leftovers:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overwrites_existing_content_atomically() {
        let dir = temp_dir("overwrite");
        let path = dir.join("state.json");
        std::fs::write(&path, b"old").unwrap();

        write_json_atomic(&path, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn creates_missing_parent_directory() {
        let dir = temp_dir("mkdir");
        let path = dir.join("nested/deeper/state.json");

        write_json_atomic(&path, b"{}").unwrap();

        assert!(path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_if_exists_is_idempotent() {
        let dir = temp_dir("remove");
        let path = dir.join("x.json");
        // 不存在时也返回 Ok（语义：确保它不存在）。
        remove_if_exists(&path).unwrap();
        std::fs::write(&path, b"x").unwrap();
        remove_if_exists(&path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
