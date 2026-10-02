//! `mkfs.vfat` 入口。
//!
//! 职责只有三件：解析参数 → 确定区间 → 调用格式化并自检。
//! 退出码与 dosfstools 的惯例一致：成功 `0`，用法错误 `1`。
//!
//! ## 区间如何确定
//!
//! - `--offset SECTOR` 给出起点（字节 = 扇区 × `-S`）；
//! - `BLOCK-COUNT`（单位 **KiB**）给出长度；
//! - 两者缺省时用整个文件。
//!
//! 经 loop 设备调用时，调用方**不应**传 `--offset`：偏移已由 `losetup -o`
//! 表达，此时 `DEVICE` 是块设备，整设备即目标区间。

use std::process::ExitCode;

use gadgetdisk_mkfsvfat::cli::{Cli, CliOutcome, parse_args};
use gadgetdisk_mkfsvfat::format::{format_fat32, normalize_label, verify_path};

/// 求目标设备/文件的容量（字节）。
///
/// ## 为什么不能直接用 `metadata().len()`
///
/// 对**块设备**，`stat(2)` 的 `st_size` 恒为 **0**——它描述的是设备节点本身，
/// 不是设备容量。因此对 loop 设备（我们统一经 loop 格式化的路径）必须改用
/// `ioctl(BLKGETSIZE64)`。
///
/// 这个区别是实测出来的：最初直接用 `metadata().len()`，结果在真机上每个
/// loop 设备都被判成 0 字节，所有分区格式化都失败并报
/// 「--offset 0 超出设备大小（0 字节）」。
fn device_size(path: &str) -> Result<u64, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("cannot stat {path}: {e}"))?;

    if metadata.is_file() {
        // 常规文件：`st_size` 就是真实大小。
        return Ok(metadata.len());
    }

    // 块设备：用 BLKGETSIZE64 取容量。
    //
    // **`cfg` 必须同时覆盖 `android`**：Android 的 `target_os` 是 `"android"`
    // 而非 `"linux"`，只写 `linux` 会让本分支在目标平台上被整个编译掉，
    // 退化成"不支持块设备"——这正是实测踩到的坑。
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::io::AsRawFd;

        // `BLKGETSIZE64` = _IOR(0x12, 114, u64)。
        //
        // 请求参数的类型**随平台而异**：glibc 上是 `u64`/`c_ulong`，Android 的
        // bionic 上是 `i32`。因此不能硬编码任一种——`0x8008_1272` 在两种宽度下
        // 都放得下（最高位之外无溢出），故用 `as _` 让编译器按本地签名推断。
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the ioctl request number is a small compile-time constant and must match the platform's argument width"
        )]
        let request = 0x8008_1272u64 as _;

        let file = std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
        let mut size: u64 = 0;

        // SAFETY: `fd` 来自刚打开且仍存活的 `File`；`size` 是合法的 `u64` 出参，
        // 大小与 ioctl 声明的 `size_t` 一致。内核在成功时写入该值。
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), request, std::ptr::addr_of_mut!(size)) };
        if rc != 0 {
            return Err(format!(
                "cannot get the size of {path} (ioctl BLKGETSIZE64 failed: {})",
                std::io::Error::last_os_error()
            ));
        }
        Ok(size)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Err(format!(
            "{path} is not a regular file, and this platform does not support block device size queries"
        ))
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let cli = match parse_args(&argv) {
        Ok(cli) => cli,
        Err(err) => {
            // `-h/--help` 与 `-V/--version` 是**成功的早退**：输出后以 0 退出。
            // 用显式的种类判断，而不是猜文本前缀。
            match err.outcome {
                CliOutcome::Help | CliOutcome::Version => {
                    println!("{}", err.message);
                    return ExitCode::SUCCESS;
                }
                CliOutcome::Error => {
                    eprintln!("mkfs.vfat: {}", err.message);
                    eprintln!();
                    eprintln!("{}", Cli::help_text());
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mkfs.vfat: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    // 1. 确定区间。
    let total_bytes = device_size(&cli.device)?;

    let start = cli.offset_bytes();
    if start >= total_bytes {
        return Err(format!(
            "--offset {} exceeds the device size ({} bytes)",
            cli.offset_sectors, total_bytes
        ));
    }

    let available = total_bytes - start;
    // `BLOCK-COUNT` 单位是 KiB（与 dosfstools 一致，**不是**扇区数）。
    let end = match cli.block_count {
        Some(blocks) => {
            let wanted = blocks
                .checked_mul(1024)
                .ok_or_else(|| format!("BLOCK-COUNT too large: {blocks}"))?;
            if wanted > available {
                return Err(format!(
                    "BLOCK-COUNT {blocks} ({} bytes) exceeds the available space of {available} bytes",
                    wanted
                ));
            }
            start + wanted
        }
        None => total_bytes,
    };

    if cli.verbose {
        eprintln!(
            "mkfs.vfat: device={} range=[{start}, {end}) size={} bytes sector={} label={:?}",
            cli.device,
            end - start,
            cli.sector_size,
            cli.label.as_deref().unwrap_or("(none)")
        );
    }

    // 2. 打开并以读写方式操作（格式化需要写）。
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&cli.device)
        .map_err(|e| format!("cannot open {} read-write: {e}", cli.device))?;

    let label_text = cli.label.clone().unwrap_or_else(|| {
        // 未给 `-n` 时用本项目的默认卷标，而不是 dosfstools 的"无卷标"：
        // GadgetDisk 创建的镜像应有一致的默认标识。
        gadgetdisk_mkfsvfat::DEFAULT_LABEL.to_string()
    });
    let label = normalize_label(&label_text);

    // 3. 格式化。
    let volume = format_fat32(file, start, end, &label).map_err(|e| e.to_string())?;

    // 4. 自检：确认没有静默降级成 FAT16，并回读真实参数。
    //
    // 这一步不可省：`fatfs` 在簇数不足时即使显式指定 `FatType::Fat32` 也会
    // 产出 FAT16，而我们只支持 FAT32。
    let verified = verify_path(std::path::Path::new(&cli.device), start, end)
        .map_err(|e| format!("{e} (hint: FAT32 needs at least 65525 clusters, about 33 MiB)"))?;

    if cli.verbose {
        eprintln!(
            "mkfs.vfat: done label={} cluster_size={} total_clusters={}",
            verified.label, verified.cluster_bytes, verified.total_clusters
        );
    } else {
        // 与 dosfstools 一样在成功时保持安静（除非 -v）。
        let _ = volume;
    }

    Ok(())
}
