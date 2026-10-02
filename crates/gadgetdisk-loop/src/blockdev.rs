//! 块设备容量查询（`BLKGETSIZE64`）。
//!
//! 内核的 `loop_info64` **不含容量字段**，容量必须从块设备本身取。
//! `ioctl(fd, BLKGETSIZE64)` 返回字节数（`_IOR(0x12,114,size_t)`）。

use std::fs::File;
use std::os::unix::io::AsRawFd;

use crate::error::LoopResult;

/// `BLKGETSIZE64`：把块设备容量（字节）写入 `u64`。
pub const BLKGETSIZE64: u64 = 0x8008_1272;

/// 读取块设备容量；失败返回 `None`（容量只用于展示，不是关键路径）。
pub fn size_bytes(device: &File) -> Option<u64> {
    let mut size: u64 = 0;
    // SAFETY: out 是有效的 u64，内核只写 8 字节。
    let rc = unsafe {
        libc::ioctl(
            device.as_raw_fd(),
            BLKGETSIZE64 as _,
            &mut size as *mut u64 as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return None;
    }
    Some(size)
}

/// 便捷包装：按路径读取容量。
pub fn size_bytes_of(path: &std::path::Path) -> LoopResult<Option<u64>> {
    let file = std::fs::File::open(path)
        .map_err(|err| crate::error::LoopError::io("open block device", path, err))?;
    Ok(size_bytes(&file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_of_regular_file_is_none_not_an_error() {
        // 常规文件不支持 BLKGETSIZE64：必须安静地返回 None，
        // 而不是让整个挂载流程失败。
        let file = File::open("/dev/null").unwrap();
        assert_eq!(size_bytes(&file), None);
    }
}
