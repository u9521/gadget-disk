//! loop 设备的内核接口抽象。
//!
//! ## 为什么用 ioctl 而不是 `losetup`
//!
//! Android 自带的 `losetup` 是 toybox 实现，**缺少 `-P`**（已实测），
//! 无法表达「按分区表派生分区子设备」。gdd 因此直接用内核 ioctl，
//! 与 Android 自身的 `vold`（`Loop::create()`）走完全相同的接口。
//!
//! ## 抽象的目的
//!
//! 真正容易出错的是 [`crate::attach`] 里的**调用顺序**（先设 fd 再设 status、
//! 释放时先 umount 再 clear_fd）。把内核访问收敛为 [`LoopControl`] trait 后，
//! 顺序逻辑可以在主机上用 [`MemLoop`] 断言，不需要真实 `/dev/loop*`。
//!
//! 规格见 [docs/ondevice-loop-mount.md](../../../../docs/ondevice-loop-mount.md)。

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::{LoopError, LoopResult};

/// `/dev/loop-control`：`LOOP_CTL_GET_FREE` 返回下一个空闲 loop 号。
pub const LOOP_CONTROL: &str = "/dev/loop-control";

// ioctl 请求号的类型在目标之间**不一致**：glibc 是 `c_ulong`，
// bionic（Android）是 `c_int`。常量统一用 `u64` 保存数值，
// 调用点统一写 `as _` 由编译器按目标推断，从而一份源码同时编过两边。
/// `LOOP_CTL_GET_FREE`：分配/查找空闲 loop 设备，返回其序号。
pub const LOOP_CTL_GET_FREE: u64 = 0x4C82;

/// `LOOP_SET_FD`：把文件描述符绑定到 loop 设备。
pub const LOOP_SET_FD: u64 = 0x4C00;

/// `LOOP_CLR_FD`：解除 loop 设备与文件描述符的绑定。
pub const LOOP_CLR_FD: u64 = 0x4C01;

/// `LOOP_SET_STATUS64`：设置偏移、只读标志等。
pub const LOOP_SET_STATUS64: u64 = 0x4C04;

/// `LOOP_GET_STATUS64`：读取当前状态。
pub const LOOP_GET_STATUS64: u64 = 0x4C05;

/// `LOOP_SET_CAPACITY`：让 loop 设备重新读取底层文件大小。
///
/// **当前主流程不再调用它**：它原本是 partscan 的前置步骤（内核要先知道整盘
/// 容量才会解析分区表），而该路径已于 2026-10-06 移除。接口与其测试保留，
/// 因为这是 loop 驱动的一项独立能力，删除它不属于本次改动的范围。
pub const LOOP_SET_CAPACITY: u64 = 0x4C07;

/// `LOOP_CONFIGURE`（Linux 5.8+）：一次性原子配置 fd 与 status。
///
/// 主流程保持采用经典的 `LOOP_SET_FD` + `LOOP_SET_STATUS64` 分步调用，
/// 以保证向下兼容早期 Android 内核版本，并在出错时保留细粒度的阶段 errno。
pub const LOOP_CONFIGURE: u64 = 0x4C0A;

/// 把 loop 序号转成设备路径。
pub fn loop_path(index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/block/loop{index}"))
}

/// 解析 `/dev/block/loopN`、`/dev/loopN` 等路径中的序号。
pub fn parse_loop_index(path: &str) -> Option<u32> {
    let name = Path::new(path).file_name()?.to_str()?;
    name.strip_prefix("loop")?.parse().ok()
}

/// 一个 loop 设备的内核状态（`LOOP_GET_STATUS64` 的子集）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoopStatus {
    /// 绑定的后备文件；未绑定时为 `None`。
    pub backing_file: Option<PathBuf>,
    /// 起始偏移（`lo_offset`）。
    pub offset_bytes: u64,
    /// 只读标志（`lo_flags & LO_FLAGS_READ_ONLY`）。
    pub read_only: bool,
    /// 是否为「自动清除」设备（`LO_FLAGS_AUTOCLEAR`）。
    pub autoclear: bool,
    /// 是否启用分区扫描（`LO_FLAGS_PARTSCAN`）。
    pub partscan: bool,
    /// 容量（字节）。
    pub size_bytes: u64,
}

impl LoopStatus {
    /// 是否已绑定后备文件。
    pub fn is_bound(&self) -> bool {
        self.backing_file.is_some()
    }
}

/// loop 设备的内核访问边界。
///
/// 所有方法都以 **loop 序号**寻址，而不是路径：序号才是内核的稳定标识，
/// 且避免每层都重复拼接 `/dev/block/loopN`。
pub trait LoopControl {
    /// 取得一个空闲 loop 序号（必要时由内核创建新设备）。
    fn get_free(&mut self) -> LoopResult<u32>;

    /// 把已打开的后备文件绑定到 loop 设备。
    fn set_fd(&mut self, index: u32, file: &File) -> LoopResult<()>;

    /// 设置偏移、只读与 partscan 标志。
    ///
    /// 必须在 [`LoopControl::set_fd`] **之后**调用：`LOOP_SET_STATUS64`
    /// 对未绑定的设备返回 `ENXIO`。
    ///
    /// `partscan` 对应 `LO_FLAGS_PARTSCAN`。它必须与偏移**同时**设定：
    /// partscan 会要求偏移为 0（内核需要看到分区表），
    /// 而 `lo_offset` 路径必须清掉该标志，否则内核会在偏移处
    /// 再解析一次分区表，得到错误的子设备。
    fn set_status64(
        &mut self,
        index: u32,
        offset_bytes: u64,
        read_only: bool,
        partscan: bool,
    ) -> LoopResult<()>;

    /// 读取当前状态。
    fn status(&self, index: u32) -> LoopResult<LoopStatus>;

    /// 让内核重新读取底层文件的容量（partscan 前的必要步骤）。
    fn set_capacity(&mut self, index: u32) -> LoopResult<()>;

    /// 打开「最后一个使用者关闭时自动清除」。
    ///
    /// **只能在挂载成功之后调用**（已实测）。`LO_FLAGS_AUTOCLEAR` 的语义是
    /// 「最后一个持有者关闭设备时自动解绑」，而本模块在设置状态的 ioctl
    /// 之后并不保留设备 fd——若在 setup 阶段就置上该标志，ioctl 返回时
    /// 设备立刻被内核自动解绑，后续 `LOOP_SET_CAPACITY` / `mount` 都会
    /// 拿到 `ENXIO`，症状看起来像「loop 设备没绑定」。
    ///
    /// 挂载成功后设备由挂载本身持有，此时置位才是安全的：
    /// 它是 gdd 被杀时的第二道保险（第一道是启动清理）。
    fn set_autoclear(&mut self, index: u32) -> LoopResult<()>;

    /// 解除绑定。返回 `false` 表示设备本就未绑定（幂等）。
    fn clear_fd(&mut self, index: u32) -> LoopResult<bool>;

    /// 枚举当前**已绑定**的 loop 设备。
    fn list_bound(&self) -> LoopResult<Vec<(u32, LoopStatus)>>;
}

// ---------------------------------------------------------------- 真实实现

/// 直接对内核发 ioctl 的实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealLoopControl;

impl RealLoopControl {
    /// 读 `/sys/block/loopN/loop/backing_file`。
    ///
    /// 文件内容形如 `/data/adb/gadget-disk/images/a.img\n`（末行有换行）；
    /// 设备未绑定时该属性为空或不存在，此时返回 `None`。
    fn sysfs_backing_file(index: u32) -> Option<PathBuf> {
        let path = format!("/sys/block/loop{index}/loop/backing_file");
        let text = std::fs::read_to_string(path).ok()?;
        parse_sysfs_backing_file(&text)
    }

    /// 构造。
    pub fn new() -> Self {
        Self
    }

    /// 打开 `/dev/block/loopN`；节点缺失时按内核**实际**的 major:minor 补建。
    ///
    /// **已实测**：Android 只预建有限数量的 loop 节点（本机到 `loop52`），
    /// 而 `LOOP_CTL_GET_FREE` 会返回更大的序号（本机返回 52，而预建节点只到
    /// `loop17` 一类）。此时 `open` 得到 `ENOENT`，与「loop 不可用」完全是
    /// 两回事——内核侧设备存在，只是用户空间没有对应节点（Android 没有 udev）。
    ///
    /// **关键**：minor 号**不是**序号本身。loop 驱动按
    /// `minor = index * (max_part + 1)` 分配，本机 `max_part = 7` 时
    /// `loop1 = 7:8`、`loop52 = 7:416`。若按 `minor = index` 建节点，
    /// 节点会指向**另一个**设备：`open` 成功，但 `LOOP_SET_FD` 以
    /// `ENXIO` 失败，错误信息完全指不到真正的原因。
    /// 因此优先从 `/sys/block/loopN/dev` 读内核给出的真实 major:minor。
    fn open_device(index: u32) -> LoopResult<File> {
        let path = loop_path(index);
        match open_rw(&path) {
            Ok(file) => Ok(file),
            Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {
                create_device_node(index)?;
                open_rw(&path).map_err(|err| LoopError::io("open loop device", &path, err))
            }
            Err(err) => Err(LoopError::io("open loop device", &path, err)),
        }
    }

    /// 执行一次无参数 ioctl，非负返回值原样返回。
    fn ioctl0(file: &File, request: u64, index: u32) -> LoopResult<i32> {
        // SAFETY: fd 来自已打开的 File；无参 ioctl 不触及用户内存。
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _) };
        if rc < 0 {
            return Err(LoopError::ioctl(
                ioctl_name(request),
                index,
                std::io::Error::last_os_error(),
            ));
        }
        Ok(rc)
    }
}

/// 以读写方式打开 loop 设备。
///
/// **必须以 O_RDWR 打开**（已实测）：`losetup` 同样如此。若以只读打开，
/// 内核在 `LOOP_SET_FD` 里会把该 loop 设备标记为只读，随后以读写挂载
/// 其上的文件系统会以 `EACCES` 失败——错误信息看起来像权限/SELinux 问题，
/// 实际原因是打开模式。
fn open_rw(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
}

/// loop 块设备的 major 号（`Documentation/admin-guide/devices.txt`）。
pub const LOOP_MAJOR: u32 = 7;

/// 读取内核为 `loopN` 分配的 `(major, minor)`。
///
/// 以 `/sys/block/loopN/dev` 为准：它是内核自己写出的权威值，
/// 不依赖我们对 `max_part` 的推导。读不到时退回
/// `minor = index * (max_part + 1)`（Linux loop 驱动的分配公式），
/// 再不行才退回 `minor = index`。
fn loop_device_numbers(index: u32) -> (u32, u32) {
    let sysfs = format!("/sys/block/loop{index}/dev");
    if let Ok(text) = std::fs::read_to_string(&sysfs)
        && let Some((major, minor)) = text.trim().split_once(':')
        && let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>())
    {
        return (major, minor);
    }

    // 退回内核公式：minor = index * (max_part + 1)。
    let max_part = std::fs::read_to_string("/sys/module/loop/parameters/max_part")
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(0);
    (LOOP_MAJOR, index * (max_part + 1))
}

/// 补建 `/dev/block/loopN` 设备节点。
///
/// 只在内核已分配该 loop 设备（`LOOP_CTL_GET_FREE` 刚返回过它）时调用，
/// 因此不会凭空造出无效节点。权限按内核惯例设 `0600`：
/// loop 设备只应由 root 打开。
fn create_device_node(index: u32) -> LoopResult<()> {
    let path = loop_path(index);
    let Some(dir) = path.parent() else {
        return Err(LoopError::protocol(format!(
            "cannot determine the parent directory of {}",
            path.display()
        )));
    };
    std::fs::create_dir_all(dir).map_err(|err| LoopError::io("create /dev/block", dir, err))?;

    let (major, minor) = loop_device_numbers(index);
    let dev = rustix::fs::makedev(major, minor);
    // 用 rustix 的安全封装替代裸 `libc::mknod`：无需构造 `CString`，也不再有 `unsafe`。
    match rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::BlockDevice,
        rustix::fs::Mode::from_bits_truncate(0o600),
        dev,
    ) {
        Ok(()) => {}
        // EEXIST 表示别人抢先建好了：竞态下这是成功。
        Err(rustix::io::Errno::EXIST) => {}
        Err(err) => {
            return Err(LoopError::io(
                "create loop device node",
                &path,
                std::io::Error::from(err),
            ));
        }
    }
    // 已存在的节点可能指向**错误的设备**（本模块早期版本按 `minor = index`
    // 建过节点，实测导致 `LOOP_SET_FD` 报 ENXIO），也可能权限不对。
    // 用 mknod 的 EEXIST 无法发现前者，因此显式核对 major:minor 并纠正。
    correct_node_numbers(&path, dev);
    // 收敛到 0600，避免非 root 能直接读写 loop 设备。
    rustix::fs::chmod(&path, rustix::fs::Mode::from_bits_truncate(0o600)).map_err(|err| {
        LoopError::io(
            "set loop device node permissions",
            &path,
            std::io::Error::from(err),
        )
    })?;
    Ok(())
}

/// 若已存在的节点指向的设备号不对，就地纠正。
///
/// 用 `mknod` 覆盖已存在的路径会得到 `EEXIST`，因此只能先删再建。
/// 只在设备号确实不匹配时动手，避免无谓地断开别人正在使用的节点。
fn correct_node_numbers(path: &Path, dev: rustix::fs::Dev) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    use std::os::unix::fs::MetadataExt as _;
    // st_rdev 仅在块设备/字符设备上有意义。
    if meta.rdev() == dev {
        return;
    }
    if std::fs::remove_file(path).is_err() {
        return;
    }
    // 失败即放弃：调用方紧接着会自行 chmod，此处只负责纠正设备号。
    let _ = rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::BlockDevice,
        rustix::fs::Mode::from_bits_truncate(0o600),
        dev,
    );
}

/// ioctl 请求号 → 可读名字，用于错误信息。
fn ioctl_name(request: u64) -> &'static str {
    match request {
        LOOP_CTL_GET_FREE => "LOOP_CTL_GET_FREE",
        LOOP_SET_FD => "LOOP_SET_FD",
        LOOP_CLR_FD => "LOOP_CLR_FD",
        LOOP_SET_STATUS64 => "LOOP_SET_STATUS64",
        LOOP_GET_STATUS64 => "LOOP_GET_STATUS64",
        LOOP_SET_CAPACITY => "LOOP_SET_CAPACITY",
        _ => "unknown",
    }
}

/// 内核的 `struct loop_info64` 中我们关心的字段偏移。
///
/// 不直接定义整个结构体：不同架构上它有 8 字节对齐差异，
/// 直接用 `#[repr(C)]` 复刻是**未验证假设**。这里改用
/// `LOOP_GET_STATUS64` 的原始字节缓冲区 + 手工按 Linux 的布局解析，
/// 布局定义见 `include/uapi/linux/loop.h`：
///
/// ```text
/// struct loop_info64 {
///     __u64 lo_device;        // 0
///     __u64 lo_inode;         // 8
///     __u64 lo_rdevice;       // 16
///     __u64 lo_offset;        // 24
///     __u64 lo_sizelimit;     // 32
///     __u32 lo_number;        // 40  （本实现不读取）
///     __u32 lo_encrypt_type;  // 44
///     __u32 lo_encrypt_key_size; // 48
///     __u32 lo_flags;         // 52
///     __u8  lo_file_name[64]; // 56
///     ...
/// };
/// ```
const OFF_OFFSET: usize = 24;
const OFF_FLAGS: usize = 52;
const OFF_FILE_NAME: usize = 56;
const FILE_NAME_LEN: usize = 64;

/// `lo_flags` 位。
const LO_FLAGS_READ_ONLY: u32 = 1;
/// `lo_flags`：自动清除。
const LO_FLAGS_AUTOCLEAR: u32 = 4;
/// `lo_flags`：按分区表派生分区子设备。
const LO_FLAGS_PARTSCAN: u32 = 8;

/// 从 `LOOP_GET_STATUS64` 的原始缓冲区解析状态。
///
/// `size_bytes` 由调用方另行提供（内核结构体里没有容量字段；
/// 容量取自后备文件或 `BLKGETSIZE64`）。
fn parse_status(raw: &[u8], size_bytes: u64) -> Option<LoopStatus> {
    if raw.len() < OFF_FILE_NAME + FILE_NAME_LEN {
        return None;
    }
    let offset_bytes = u64::from_ne_bytes(raw[OFF_OFFSET..OFF_OFFSET + 8].try_into().ok()?);
    let flags = u32::from_ne_bytes(raw[OFF_FLAGS..OFF_FLAGS + 4].try_into().ok()?);
    let name_bytes = &raw[OFF_FILE_NAME..OFF_FILE_NAME + FILE_NAME_LEN];
    let end = name_bytes
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(FILE_NAME_LEN);
    let name = std::str::from_utf8(&name_bytes[..end]).ok()?;
    let backing_file = if name.is_empty() {
        None
    } else {
        Some(PathBuf::from(name))
    };
    Some(LoopStatus {
        backing_file,
        offset_bytes,
        read_only: flags & LO_FLAGS_READ_ONLY != 0,
        autoclear: flags & LO_FLAGS_AUTOCLEAR != 0,
        partscan: flags & LO_FLAGS_PARTSCAN != 0,
        size_bytes,
    })
}

/// 解析 `/sys/block/loopN/loop/backing_file` 的内容。
///
/// 纯函数（便于主机测试）。sysfs 会在末尾带一个换行；设备未绑定时内容为
/// 空白（或读取失败，由调用方处理），此时返回 `None`——**不得**当成
/// 「绑定到空路径」，否则 `list_bound()` 会报告一个指向空路径的假附件。
fn parse_sysfs_backing_file(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// 组装 `LOOP_SET_STATUS64` 的输入缓冲区。
///
/// 只填 `lo_offset` 与 `lo_flags`；`lo_file_name` **留空**。
/// 其余字段保持 0 表示「不限制」。
///
/// `lo_file_name` 留空没关系：路径的权威来源是 sysfs，不是该字段
/// （见 [`RealLoopControl::status`] 的说明）。
fn build_status(offset_bytes: u64, read_only: bool, partscan: bool) -> Vec<u8> {
    let mut buf = vec![0u8; OFF_FILE_NAME + FILE_NAME_LEN];
    buf[OFF_OFFSET..OFF_OFFSET + 8].copy_from_slice(&offset_bytes.to_ne_bytes());
    // 这里**不**置 AUTOCLEAR：见 `LoopControl::set_autoclear` 的说明。
    let mut flags = 0u32;
    if read_only {
        flags |= LO_FLAGS_READ_ONLY;
    }
    if partscan {
        flags |= LO_FLAGS_PARTSCAN;
    }
    buf[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_ne_bytes());
    buf
}

/// 组装一次「只改 flags」的 `LOOP_SET_STATUS64` 输入。
///
/// 保留既有偏移、只读标志与**后备文件名**（`GET_STATUS64` 的结果），
/// 只追加 `autoclear`。
fn build_autoclear_status(current: &LoopStatus) -> Vec<u8> {
    let mut buf = build_status(current.offset_bytes, current.read_only, current.partscan);
    let flags = u32::from_ne_bytes(buf[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap_or([0; 4]));
    buf[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&(flags | LO_FLAGS_AUTOCLEAR).to_ne_bytes());
    buf
}

impl LoopControl for RealLoopControl {
    fn get_free(&mut self) -> LoopResult<u32> {
        let path = Path::new(LOOP_CONTROL);
        let file = File::open(path).map_err(|err| LoopError::io("open loop-control", path, err))?;
        // SAFETY: fd 来自已打开的 File；LOOP_CTL_GET_FREE 无用户内存参数。
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
        if rc < 0 {
            return Err(LoopError::ioctl(
                "LOOP_CTL_GET_FREE",
                0,
                std::io::Error::last_os_error(),
            ));
        }
        // LOOP_CTL_GET_FREE 返回空闲 loop 序号，已确认 rc >= 0。
        Ok(u32::try_from(rc).expect("LOOP_CTL_GET_FREE return value was checked non-negative"))
    }

    fn set_fd(&mut self, index: u32, file: &File) -> LoopResult<()> {
        let device = Self::open_device(index)?;
        // SAFETY: 两个 fd 都有效；LOOP_SET_FD 以整数参数传递后备文件 fd。
        let rc = unsafe { libc::ioctl(device.as_raw_fd(), LOOP_SET_FD as _, file.as_raw_fd()) };
        if rc < 0 {
            return Err(LoopError::ioctl(
                "LOOP_SET_FD",
                index,
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    fn set_status64(
        &mut self,
        index: u32,
        offset_bytes: u64,
        read_only: bool,
        partscan: bool,
    ) -> LoopResult<()> {
        let device = Self::open_device(index)?;

        // `lo_file_name` 留空即可：内核不靠它记录后备文件（见 `status()` 的说明），
        // 路径的唯一权威来源是 sysfs，因此这里无需读回再写回。
        let mut buf = build_status(offset_bytes, read_only, partscan);

        // SAFETY: buf 至少与内核期望的 struct loop_info64 前缀一样长，
        // 且在调用期间保持存活、可写。
        let rc = unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                LOOP_SET_STATUS64 as _,
                buf.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if rc < 0 {
            return Err(LoopError::ioctl(
                "LOOP_SET_STATUS64",
                index,
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    fn status(&self, index: u32) -> LoopResult<LoopStatus> {
        let device = Self::open_device(index)?;
        let size_bytes = crate::blockdev::size_bytes(&device).unwrap_or(0);
        let mut buf = vec![0u8; OFF_FILE_NAME + FILE_NAME_LEN];
        // SAFETY: buf 可写且足够长；内核只写 struct loop_info64 大小。
        let rc = unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                LOOP_GET_STATUS64 as _,
                buf.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // 未绑定的设备返回 ENXIO：这是正常状态而非错误。
            if err.raw_os_error() == Some(libc::ENXIO) {
                return Ok(LoopStatus::default());
            }
            return Err(LoopError::ioctl("LOOP_GET_STATUS64", index, err));
        }

        let mut status = parse_status(&buf, size_bytes)
            .ok_or_else(|| LoopError::protocol(format!("cannot parse the state of loop{index}")))?;

        // **后备文件路径以 sysfs 为准**（已实测）。
        //
        // `LOOP_GET_STATUS64` 返回的 `lo_file_name` 在现代内核上是**空的**：
        // 内核不再把后备文件路径写进该字段，而 `/sys/block/loopN/loop/backing_file`
        // 是从真正的 `lo_backing_file` 推导出来的。实测：同一个已绑定设备，
        // sysfs 显示 `/data/adb/gadget-disk/images/probe.img`，而 ioctl 读回的
        // `lo_file_name` 为空。
        //
        // 这一差异是功能性的：`list_bound()` 靠该路径判断「哪些 loop 指向本模块的
        // 镜像」。只看 ioctl 会让它把一切都判定为未绑定，于是 `attachments()` 返回空、
        // 互斥判据失效（实测：镜像已被 loop 挂载时仍能导出为 USB 设备），
        // 启动清理也再找不到残留。
        if status.backing_file.is_none() {
            status.backing_file = Self::sysfs_backing_file(index);
        }

        Ok(status)
    }

    fn set_capacity(&mut self, index: u32) -> LoopResult<()> {
        let device = Self::open_device(index)?;
        Self::ioctl0(&device, LOOP_SET_CAPACITY, index)?;
        Ok(())
    }

    fn set_autoclear(&mut self, index: u32) -> LoopResult<()> {
        let device = Self::open_device(index)?;
        let current = self.status(index)?;
        if current.autoclear {
            return Ok(());
        }
        let mut buf = build_autoclear_status(&current);
        // SAFETY: buf 可写且足够长；内核只读 struct loop_info64 大小。
        let rc = unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                LOOP_SET_STATUS64 as _,
                buf.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if rc < 0 {
            return Err(LoopError::ioctl(
                "LOOP_SET_STATUS64(autoclear)",
                index,
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    fn clear_fd(&mut self, index: u32) -> LoopResult<bool> {
        let device = Self::open_device(index)?;
        // SAFETY: fd 有效；LOOP_CLR_FD 无用户内存参数。
        let rc = unsafe { libc::ioctl(device.as_raw_fd(), LOOP_CLR_FD as _) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // 未绑定时内核返回 ENXIO：视为幂等成功。
            if err.raw_os_error() == Some(libc::ENXIO) {
                return Ok(false);
            }
            return Err(LoopError::ioctl("LOOP_CLR_FD", index, err));
        }
        Ok(true)
    }

    fn list_bound(&self) -> LoopResult<Vec<(u32, LoopStatus)>> {
        let mut out = Vec::new();
        for index in 0..MAX_SCAN_LOOPS {
            let path = loop_path(index);
            if !path.exists() {
                continue;
            }
            let Ok(status) = self.status(index) else {
                continue;
            };
            if status.is_bound() {
                out.push((index, status));
            }
        }
        Ok(out)
    }
}

/// `list_bound` 的扫描上界。
///
/// 内核的 `max_loop` 默认为 8，但部分设备调大过；扫 256 个足以覆盖所有已知配置，
/// 且每个未创建设备的 `exists()` 只是一次 `stat`，成本可忽略。
pub const MAX_SCAN_LOOPS: u32 = 256;

// ---------------------------------------------------------------- 内存替身

/// 内存中的 loop 实现，供主机测试断言调用顺序与参数。
#[derive(Debug, Default, Clone)]
pub struct MemLoop {
    devices: BTreeMap<u32, LoopStatus>,
    /// 按顺序记录的每一次调用，用于断言顺序。
    pub trace: Vec<String>,
    /// 下一次 `get_free` 返回的序号。
    next_free: u32,
    /// 该序号集合上的 `set_fd`/`set_status64`/`set_capacity` 会失败，
    /// 用于模拟内核差异。
    pub fail_on: Vec<(u32, &'static str)>,
    /// `get_free` 是否失败（模拟无 loop-control 的内核）。
    pub control_unavailable: bool,
}

impl MemLoop {
    /// 构造空替身。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置一个已绑定的设备（模拟 gdd 重启前的残留）。
    pub fn with_bound(mut self, index: u32, backing: &str, offset_bytes: u64) -> Self {
        self.devices.insert(
            index,
            LoopStatus {
                backing_file: Some(PathBuf::from(backing)),
                offset_bytes,
                read_only: false,
                autoclear: true,
                partscan: false,
                size_bytes: 0,
            },
        );
        self.next_free = self.next_free.max(index + 1);
        self
    }

    /// 让指定操作在指定设备上失败。
    pub fn failing_on(mut self, index: u32, op: &'static str) -> Self {
        self.fail_on.push((index, op));
        self
    }

    fn should_fail(&self, index: u32, op: &str) -> bool {
        self.fail_on.iter().any(|(i, o)| *i == index && *o == op)
    }

    /// 当前已绑定的设备快照。
    pub fn bound(&self) -> Vec<(u32, LoopStatus)> {
        self.devices
            .iter()
            .filter(|(_, s)| s.is_bound())
            .map(|(i, s)| (*i, s.clone()))
            .collect()
    }
}

impl LoopControl for MemLoop {
    fn get_free(&mut self) -> LoopResult<u32> {
        self.trace.push("get_free".into());
        if self.control_unavailable {
            return Err(LoopError::capability("/dev/loop-control is unavailable"));
        }
        let index = self.next_free;
        self.next_free += 1;
        self.devices.entry(index).or_default();
        Ok(index)
    }

    fn set_fd(&mut self, index: u32, file: &File) -> LoopResult<()> {
        // trace 里带上后备文件的大小，便于断言「打开的是同一个文件」。
        let size = file.metadata().map(|m| m.len()).unwrap_or_default();
        self.trace.push(format!("set_fd:{index}:{size}"));
        if self.should_fail(index, "set_fd") {
            return Err(LoopError::ioctl(
                "LOOP_SET_FD",
                index,
                std::io::Error::from_raw_os_error(libc::ENOTTY),
            ));
        }
        let entry = self.devices.entry(index).or_default();
        entry.backing_file = Some(PathBuf::from(format!("backing-of-{index}")));
        Ok(())
    }

    fn set_status64(
        &mut self,
        index: u32,
        offset_bytes: u64,
        read_only: bool,
        partscan: bool,
    ) -> LoopResult<()> {
        self.trace.push(format!(
            "set_status64:{index}:{offset_bytes}:{read_only}:{partscan}"
        ));
        if self.should_fail(index, "set_status64") {
            return Err(LoopError::ioctl(
                "LOOP_SET_STATUS64",
                index,
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }
        let entry = self.devices.entry(index).or_default();
        entry.offset_bytes = offset_bytes;
        entry.read_only = read_only;
        // 刻意**不**置 autoclear：真实实现同样只在挂载成功后置位。
        entry.autoclear = false;
        entry.partscan = partscan;
        Ok(())
    }

    fn status(&self, index: u32) -> LoopResult<LoopStatus> {
        Ok(self.devices.get(&index).cloned().unwrap_or_default())
    }

    fn set_capacity(&mut self, index: u32) -> LoopResult<()> {
        self.trace.push(format!("set_capacity:{index}"));
        if self.should_fail(index, "set_capacity") {
            return Err(LoopError::ioctl(
                "LOOP_SET_CAPACITY",
                index,
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }
        Ok(())
    }

    fn set_autoclear(&mut self, index: u32) -> LoopResult<()> {
        self.trace.push(format!("set_autoclear:{index}"));
        if self.should_fail(index, "set_autoclear") {
            return Err(LoopError::ioctl(
                "LOOP_SET_STATUS64",
                index,
                std::io::Error::from_raw_os_error(libc::ENXIO),
            ));
        }
        if let Some(entry) = self.devices.get_mut(&index) {
            entry.autoclear = true;
        }
        Ok(())
    }

    fn clear_fd(&mut self, index: u32) -> LoopResult<bool> {
        self.trace.push(format!("clear_fd:{index}"));
        let was_bound = self
            .devices
            .get(&index)
            .map(LoopStatus::is_bound)
            .unwrap_or(false);
        if let Some(entry) = self.devices.get_mut(&index) {
            *entry = LoopStatus::default();
        }
        Ok(was_bound)
    }

    fn list_bound(&self) -> LoopResult<Vec<(u32, LoopStatus)>> {
        Ok(self.bound())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_path_uses_block_symlink_dir() {
        // Android 上 /dev/loopN 不存在，只有 /dev/block/loopN。
        assert_eq!(loop_path(3), PathBuf::from("/dev/block/loop3"));
    }

    #[test]
    fn parse_loop_index_accepts_both_paths() {
        assert_eq!(parse_loop_index("/dev/block/loop7"), Some(7));
        assert_eq!(parse_loop_index("/dev/loop12"), Some(12));
        assert_eq!(parse_loop_index("/dev/block/sda"), None);
        assert_eq!(parse_loop_index("loopX"), None);
    }

    #[test]
    fn loop_device_numbers_uses_kernel_assigned_minor() {
        // 已实测：minor **不是**序号本身。本机 max_part = 7 时
        // loop1 = 7:8、loop52 = 7:416（minor = index * (max_part + 1)）。
        // 按 `minor = index` 建节点会指向另一个设备：open 成功但
        // LOOP_SET_FD 报 ENXIO，症状完全指不到真正原因。
        //
        // 本测试只在真机上有效（依赖 /sys/block/loop*）。主机上
        // 退回公式的分支必须仍然自洽：major 恒为 7。
        let (major, _minor) = loop_device_numbers(0);
        assert_eq!(major, LOOP_MAJOR, "loop 设备的 major 必须是 7");
    }

    #[test]
    fn build_status_sets_offset_and_read_only_flag() {
        let buf = build_status(1_048_576, true, false);
        let offset = u64::from_ne_bytes(buf[OFF_OFFSET..OFF_OFFSET + 8].try_into().unwrap());
        let flags = u32::from_ne_bytes(buf[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap());
        assert_eq!(offset, 1_048_576);
        assert_ne!(flags & LO_FLAGS_READ_ONLY, 0);
        // setup 阶段**不得**置 AUTOCLEAR：那会让 ioctl 一返回
        // 设备就被内核自动解绑（已实测为 ENXIO）。
        assert_eq!(flags & LO_FLAGS_AUTOCLEAR, 0);
        // partscan 随参数设置。
        assert_eq!(flags & LO_FLAGS_PARTSCAN, 0);
        let partscan_buf = build_status(0, false, true);
        let ps_flags =
            u32::from_ne_bytes(partscan_buf[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap());
        assert_ne!(ps_flags & LO_FLAGS_PARTSCAN, 0);
    }

    #[test]
    fn sysfs_backing_file_is_parsed_and_trimmed() {
        // 回归：现代内核的 LOOP_GET_STATUS64 返回空的 lo_file_name，
        // 路径只能从 sysfs 取；漏掉它会让 list_bound()/attachments() 全部失明。
        assert_eq!(
            parse_sysfs_backing_file("/data/adb/gadget-disk/images/a.img\n"),
            Some(PathBuf::from("/data/adb/gadget-disk/images/a.img"))
        );
        assert_eq!(
            parse_sysfs_backing_file("  /x.img  "),
            Some(PathBuf::from("/x.img"))
        );
    }

    #[test]
    fn sysfs_backing_file_empty_means_unbound() {
        // 未绑定时属性为空：必须返回 None，而不是 Some("")——
        // 否则会报告一个指向空路径的假附件。
        assert_eq!(parse_sysfs_backing_file(""), None);
        assert_eq!(parse_sysfs_backing_file("\n"), None);
        assert_eq!(parse_sysfs_backing_file("   "), None);
    }

    #[test]
    fn parse_status_still_reads_the_name_when_present() {
        // ioctl 路径仍然解析 lo_file_name（某些内核/工具会填它），
        // 只是不再作为唯一来源。
        let mut buf = build_status(4096, true, false);
        let name = b"/data/adb/gadget-disk/images/a.img";
        buf[OFF_FILE_NAME..OFF_FILE_NAME + name.len()].copy_from_slice(name);
        assert_eq!(
            parse_status(&buf, 1).unwrap().backing_file.as_deref(),
            Some(Path::new("/data/adb/gadget-disk/images/a.img"))
        );
    }

    #[test]
    fn build_status_without_read_only_clears_the_flag() {
        let buf = build_status(0, false, false);
        let flags = u32::from_ne_bytes(buf[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap());
        assert_eq!(flags & LO_FLAGS_READ_ONLY, 0);
    }

    #[test]
    fn parse_status_reads_back_what_build_status_wrote() {
        let mut buf = build_status(4096, true, false);
        let name = b"/data/adb/gadget-disk/images/a.img";
        buf[OFF_FILE_NAME..OFF_FILE_NAME + name.len()].copy_from_slice(name);

        let status = parse_status(&buf, 1234).unwrap();
        assert_eq!(
            status.backing_file.as_deref(),
            Some(Path::new("/data/adb/gadget-disk/images/a.img"))
        );
        assert_eq!(status.offset_bytes, 4096);
        assert!(status.read_only);
        // build_status 不置 autoclear（只在挂载成功后由 set_autoclear 置）。
        assert!(!status.autoclear);
        // build_status(4096, true, false)：offset 路径必须清掉 partscan。
        assert!(!status.partscan);
        assert_eq!(status.size_bytes, 1234);
    }

    #[test]
    fn parse_status_treats_empty_name_as_unbound() {
        let buf = build_status(0, false, false);
        let status = parse_status(&buf, 0).unwrap();
        assert!(!status.is_bound());
        assert_eq!(status.backing_file, None);
    }

    #[test]
    fn parse_status_rejects_short_buffer() {
        // 缓冲区短于内核结构体时必须报错而不是读出垃圾值。
        assert!(parse_status(&[0u8; 8], 0).is_none());
    }

    #[test]
    fn mem_loop_clear_fd_is_idempotent() {
        let mut lo = MemLoop::new();
        let index = lo.get_free().unwrap();
        // 未绑定时 clear 返回 false，但不算失败。
        assert!(!lo.clear_fd(index).unwrap());
    }

    #[test]
    fn mem_loop_records_call_order() {
        let mut lo = MemLoop::new();
        let index = lo.get_free().unwrap();
        let file = File::open("/dev/null").unwrap();
        lo.set_fd(index, &file).unwrap();
        lo.set_status64(index, 0, false, false).unwrap();
        lo.set_capacity(index).unwrap();

        assert_eq!(
            lo.trace,
            vec![
                "get_free".to_string(),
                format!("set_fd:{index}:{index}"),
                format!("set_status64:{index}:0:false:false"),
                format!("set_capacity:{index}"),
            ]
        );
    }

    #[test]
    fn build_autoclear_status_preserves_offset_and_adds_the_flag() {
        let current = LoopStatus {
            backing_file: Some(PathBuf::from("/x.img")),
            offset_bytes: 1_048_576,
            read_only: true,
            autoclear: false,
            partscan: false,
            size_bytes: 0,
        };
        let buf = build_autoclear_status(&current);
        let offset = u64::from_ne_bytes(buf[OFF_OFFSET..OFF_OFFSET + 8].try_into().unwrap());
        let flags = u32::from_ne_bytes(buf[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap());
        // 偏移与只读必须保留，只追加 autoclear。
        assert_eq!(offset, 1_048_576);
        assert_ne!(flags & LO_FLAGS_READ_ONLY, 0);
        assert_ne!(flags & LO_FLAGS_AUTOCLEAR, 0);
    }

    #[test]
    fn mem_loop_set_autoclear_is_recorded_after_mount() {
        let mut lo = MemLoop::new();
        let index = lo.get_free().unwrap();
        let file = File::open("/dev/null").unwrap();
        lo.set_fd(index, &file).unwrap();
        lo.set_status64(index, 0, false, false).unwrap();
        assert!(!lo.status(index).unwrap().autoclear);
        lo.set_autoclear(index).unwrap();
        assert!(lo.status(index).unwrap().autoclear);
    }
}
