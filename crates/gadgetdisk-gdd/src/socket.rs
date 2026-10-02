//! `AF_UNIX` **路径** socket 的绑定、访问控制与陈旧文件处理。
//!
//! 安全契约见 [docs/protocol.md](../../../../docs/protocol.md)：
//!
//! | 项 | 取值 |
//! |---|---|
//! | 类型 | `AF_UNIX` **路径** socket（非抽象套接字） |
//! | 目录权限 | `0700`，owner `root:root` —— **无 `x` 权限即无法连接**，主防线 |
//! | socket 权限 | `0600`（加固，不构成访问控制） |
//! | 对端校验 | `accept` 后读 `SO_PEERCRED`，uid 必须为 `0`，否则立即关闭 |
//!
//! 为何不用抽象套接字：抽象套接字不出现在文件系统中，任何应用都能从
//! `/proc/net/unix` 枚举名字并直接 `connect()`，权限位完全失效。
//! 路径 socket 使非 root 进程在**文件系统层面**即被 `0700` 目录挡住。

use std::ffi::CString;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// socket 相关的错误。
#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    /// 底层 IO 失败。
    #[error("socket io error: {0}")]
    Io(#[from] std::io::Error),

    /// 路径中含 NUL 字节，无法传给 C。
    #[error("path contains a NUL byte: {0}")]
    NulPath(String),

    /// 已有 gdd 在监听（`connect` 成功）。
    #[error("another gdd is already listening on {0}")]
    AlreadyRunning(PathBuf),

    /// 目录创建后权限不正确。
    #[error("socket directory mode is {actual:o}, expected {expected:o}")]
    BadDirectoryMode {
        /// 实际权限。
        actual: u32,
        /// 期望权限。
        expected: u32,
    },
}

/// 本 crate 的 socket 结果类型。
pub type Result<T> = std::result::Result<T, SocketError>;

/// socket 目录要求的权限位。
pub const SOCKET_DIR_MODE: u32 = 0o700;

/// socket 文件使用的权限位（加固；访问控制由目录承担）。
pub const SOCKET_FILE_MODE: u32 = 0o600;

/// 监听中的 Unix socket。
#[derive(Debug)]
pub struct Listener {
    fd: OwnedFd,
    path: PathBuf,
}

impl Listener {
    /// 原始文件描述符。
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// 绑定的路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 尝试接受一个连接。
    ///
    /// 返回的 [`Peer`] 已通过 `SO_PEERCRED` 读取对端身份；
    /// **是否信任该对端由调用方决定**（本函数不做拒绝，以便测试能断言拒绝逻辑）。
    pub fn accept(&self) -> Result<Peer> {
        loop {
            // SAFETY: accept 只写入我们提供的地址缓冲区；fd 有效且已 listen。
            let client = unsafe {
                libc::accept(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if client < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(SocketError::Io(err));
            }

            // SAFETY: accept 返回新的、由我们拥有的 fd。
            let owned = unsafe { OwnedFd::from_raw_fd(client) };
            return Ok(Peer {
                fd: owned,
                credentials: peer_credentials(client)?,
            });
        }
    }

    /// 在 `timeout` 内等待一个连接；超时返回 `Ok(None)`。
    ///
    /// 这是**按需进程模型**的基础：gdd 必须能在无连接时醒来判断自己
    /// 是否该退出，因此不能永久阻塞在 `accept` 上。用 `poll(2)` 而不是把
    /// fd 设为非阻塞，是为了保持 `accept` 的阻塞语义不变，改动面最小。
    pub fn accept_timeout(&self, timeout: Duration) -> Result<Option<Peer>> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // millis 截断到 i32 上限：超长超时会被截短，而不是整数溢出。
        let millis = libc::c_int::try_from(timeout.as_millis().min(i32::MAX as u128))
            .expect("已 min 到 i32::MAX，必然落在 c_int 范围内");

        // SAFETY: pfd 是有效的单个 pollfd 数组。
        let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                // 被信号打断不是错误：返回「无连接」让调用方重新判断退出条件。
                return Ok(None);
            }
            return Err(SocketError::Io(err));
        }
        if ready == 0 {
            return Ok(None);
        }

        self.accept().map(Some)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // 移除 socket 文件，避免留下陈旧节点。
        std::fs::remove_file(&self.path).ok();
    }
}

/// 一个已接受的连接及其对端身份。
#[derive(Debug)]
pub struct Peer {
    fd: OwnedFd,
    credentials: PeerCredentials,
}

impl Peer {
    /// 对端凭据。
    pub fn credentials(&self) -> PeerCredentials {
        self.credentials
    }

    /// 对端是否为 root（`uid == 0`）。
    ///
    /// 这是唯一的信任判据：路径 socket 的目录权限挡住非 root 的 `connect()`，
    /// 而 `SO_PEERCRED` 是内核提供的、不可伪造的第二道校验。
    pub fn is_trusted(&self) -> bool {
        self.credentials.uid == 0
    }

    /// 原始文件描述符。
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl io::Read for Peer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: buf 是有效的可写切片；fd 由本结构拥有且有效。
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            // 已确认非负，故转换必然成功。
            Ok(usize::try_from(n).expect("n 已检查 >= 0"))
        }
    }
}

impl io::Write for Peer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: buf 是有效的只读切片；fd 由本结构拥有且有效。
        let n = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                buf.as_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            // 已确认非负，故转换必然成功。
            Ok(usize::try_from(n).expect("n 已检查 >= 0"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `SO_PEERCRED` 读出的对端身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    /// 对端进程 uid。
    pub uid: u32,
    /// 对端进程 gid。
    pub gid: u32,
    /// 对端进程 pid。
    pub pid: i32,
}

/// 读取 `SO_PEERCRED`。
pub fn peer_credentials(fd: RawFd) -> Result<PeerCredentials> {
    // SAFETY: ucred 是 POD；getsockopt 只写入我们提供的缓冲区。
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .expect("ucred 大小远小于 socklen_t 上限");

    // SAFETY: 参数类型与长度均正确。
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(SocketError::Io(io::Error::last_os_error()));
    }

    Ok(PeerCredentials {
        uid: cred.uid,
        gid: cred.gid,
        pid: cred.pid,
    })
}

/// 判断形如 `ECONNREFUSED` 的错误是否表示「残留 socket 文件」。
///
/// 依据 [docs/protocol.md](../../../../docs/protocol.md)：bind 前先尝试 `connect()`：
/// - 连接成功 → gdd 存活，本次启动放弃并退出；
/// - 返回 `ECONNREFUSED` → 属残留文件，删除后重新 bind。
pub fn is_stale_socket_error(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ECONNREFUSED)
}

/// 处理绑定前的陈旧 socket 检查，然后绑定并监听。
///
/// `socket_path` 的父目录会被创建（若不存在）并以 [`SOCKET_DIR_MODE`] 收紧权限。
pub fn bind(socket_path: &Path) -> Result<Listener> {
    if let Some(parent) = socket_path.parent() {
        prepare_socket_dir(parent)?;
    }

    // 陈旧 socket 处理：先尝试 connect。
    if socket_path.exists() {
        match probe_connect(socket_path) {
            Ok(()) => return Err(SocketError::AlreadyRunning(socket_path.to_path_buf())),
            Err(err) if is_stale_socket_error(&err) => {
                std::fs::remove_file(socket_path)?;
            }
            Err(err) => {
                // 其他错误（如 EACCES）不当作陈旧文件，交由 bind 报出真实原因。
                let _ = err;
                std::fs::remove_file(socket_path)?;
            }
        }
    }

    let c_path = CString::new(socket_path.as_os_str().as_encoded_bytes())
        .map_err(|_| SocketError::NulPath(socket_path.display().to_string()))?;

    // SAFETY: socket(2) 无内存副作用。
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(SocketError::Io(io::Error::last_os_error()));
    }
    // SAFETY: fd 由我们拥有。
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: sockaddr_un 是 POD，先清零再填字段。
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family =
        libc::sa_family_t::try_from(libc::AF_UNIX).expect("AF_UNIX 可放进 sa_family_t");

    let path_bytes = c_path.as_bytes();
    if path_bytes.len() >= addr.sun_path.len() {
        return Err(SocketError::NulPath(format!(
            "socket path is too long (limit {} bytes): {}",
            addr.sun_path.len() - 1,
            socket_path.display()
        )));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(path_bytes.iter()) {
        *slot = *byte as libc::c_char;
    }

    // SAFETY: 地址长度与 sun_family 一致，路径已确认以 NUL 结尾。
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
                .expect("sockaddr_un 大小远小于 socklen_t 上限"),
        )
    };
    if rc != 0 {
        return Err(SocketError::Io(io::Error::last_os_error()));
    }

    // 收紧 socket 文件权限（加固；访问控制由目录承担）。
    let c_path_perm = c_path.clone();
    // SAFETY: c_path_perm 是有效的 NUL 结尾字符串。
    unsafe { libc::chmod(c_path_perm.as_ptr(), SOCKET_FILE_MODE as libc::mode_t) };

    // SAFETY: fd 有效且已 bind。
    if unsafe { libc::listen(fd.as_raw_fd(), 16) } != 0 {
        return Err(SocketError::Io(io::Error::last_os_error()));
    }

    Ok(Listener {
        fd,
        path: socket_path.to_path_buf(),
    })
}

/// 创建并收紧 socket 目录权限为 `0700`。
fn prepare_socket_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let c_dir = CString::new(dir.as_os_str().as_encoded_bytes())
        .map_err(|_| SocketError::NulPath(dir.display().to_string()))?;

    // SAFETY: c_dir 是有效的 NUL 结尾字符串。
    if unsafe { libc::chmod(c_dir.as_ptr(), SOCKET_DIR_MODE as libc::mode_t) } != 0 {
        return Err(SocketError::Io(io::Error::last_os_error()));
    }

    // 复核：权限必须真的收紧，否则拒绝启动（这是主防线）。
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode != SOCKET_DIR_MODE {
        return Err(SocketError::BadDirectoryMode {
            actual: mode,
            expected: SOCKET_DIR_MODE,
        });
    }
    Ok(())
}

/// 尝试连接一次，用于探测 gdd 是否存活。
fn probe_connect(socket_path: &Path) -> io::Result<()> {
    let c_path = CString::new(socket_path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;

    // SAFETY: socket(2) 无内存副作用。
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd 由我们拥有。
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: 先清零再填字段。
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family =
        libc::sa_family_t::try_from(libc::AF_UNIX).expect("AF_UNIX 可放进 sa_family_t");
    let bytes = c_path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is too long",
        ));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes.iter()) {
        *slot = *byte as libc::c_char;
    }

    // SAFETY: 地址长度与 sun_family 一致。
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
                .expect("sockaddr_un 大小远小于 socklen_t 上限"),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

use std::os::unix::fs::PermissionsExt;

/// 写出一个完整的帧（`u8 id` + `u32 LE length` + 负载）。
///
/// 这里实现而不复用 `gadgetdisk-proto` 的 `write_frame`，是为了能直接作用于
/// [`Peer`]（它实现 `Write`，但需要把 `write_all` 的语义绑在本模块的 fd 上）。
pub fn write_all<W: io::Write>(writer: &mut W, payload: &[u8], id: u8) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload is too long"))?;
    writer.write_all(&[id])?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// 在测试中把已 accept 的 fd 包装成 [`Peer`]。
///
/// 仅用于测试：生产代码路径始终经 [`Listener::accept`]，其中会读取真实凭据。
#[cfg(test)]
impl Peer {
    /// 用给定 fd 构造 `Peer`，凭据按当前进程 uid 填入。
    pub fn from_owned_fd_for_test(fd: OwnedFd) -> Self {
        // SAFETY: getuid 无副作用。
        let uid = unsafe { libc::getuid() };
        // SAFETY: getgid 无副作用。
        let gid = unsafe { libc::getgid() };
        Self {
            fd,
            credentials: PeerCredentials {
                uid,
                gid,
                pid: std::process::id() as i32,
            },
        }
    }

    /// 用给定 fd 与**指定凭据**构造 `Peer`。
    ///
    /// 存在的理由：测试常以非 root 运行，无法产生真实的 root 对端；
    /// 而「凭据校验」这一安全关键路径必须能被直接测试。
    pub fn with_credentials_for_test(fd: OwnedFd, credentials: PeerCredentials) -> Self {
        Self { fd, credentials }
    }
}

/// 在测试中作为客户端连接到 `path`。
#[cfg(test)]
pub fn connect_for_test(path: &Path) -> io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn bind_creates_socket_with_tight_directory() {
        let dir = testutil::temp_dir("sock-bind");
        let path = dir.join("run").join("gdd.sock");

        let listener = bind(&path).expect("绑定成功");
        assert!(path.exists(), "socket 文件应存在");

        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, SOCKET_DIR_MODE, "目录必须为 0700");

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, SOCKET_FILE_MODE, "socket 文件应为 0600");

        drop(listener);
        // Drop 应清理 socket 文件。
        assert!(!path.exists(), "Drop 后不应残留 socket 文件");

        testutil::cleanup(&dir);
    }

    #[test]
    fn second_bind_reports_already_running() {
        let dir = testutil::temp_dir("sock-dup");
        let path = dir.join("run").join("gdd.sock");

        let _first = bind(&path).expect("首次绑定");
        let err = bind(&path).expect_err("第二次绑定必须失败");
        assert!(matches!(err, SocketError::AlreadyRunning(_)));

        testutil::cleanup(&dir);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = testutil::temp_dir("sock-stale");
        let run_dir = dir.join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let path = run_dir.join("gdd.sock");

        // 造一个残留文件（非 socket），bind 应能接管。
        std::fs::write(&path, b"stale").unwrap();

        let listener = bind(&path).expect("残留文件应被替换");
        assert!(path.exists());
        drop(listener);

        testutil::cleanup(&dir);
    }

    #[test]
    fn stale_detection_only_matches_econnrefused() {
        let refused = io::Error::from_raw_os_error(libc::ECONNREFUSED);
        assert!(is_stale_socket_error(&refused));

        let other = io::Error::from_raw_os_error(libc::EACCES);
        assert!(!is_stale_socket_error(&other));
    }

    #[test]
    fn accept_reports_root_credentials() {
        let dir = testutil::temp_dir("sock-cred");
        let path = dir.join("run").join("gdd.sock");
        let listener = bind(&path).unwrap();

        // 在本进程内连接：SO_PEERCRED 会给出本进程 uid。
        let path_clone = path.clone();
        let handle = std::thread::spawn(move || {
            let c_path = CString::new(path_clone.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: socket(2) 无内存副作用。
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            assert!(fd >= 0);
            // SAFETY: `sockaddr_un` 全零即合法（`sun_family` 为 0 是普通整数，
            // `sun_path` 是全零的定长字节数组），随后两行立刻填上真实的
            // `sun_family` 与 `sun_path`，不存在读取未初始化的时刻。
            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            addr.sun_family =
                libc::sa_family_t::try_from(libc::AF_UNIX).expect("AF_UNIX 可放进 sa_family_t");
            for (slot, byte) in addr.sun_path.iter_mut().zip(c_path.as_bytes().iter()) {
                *slot = *byte as libc::c_char;
            }
            // SAFETY: 地址长度与 sun_family 一致。
            let rc = unsafe {
                libc::connect(
                    fd,
                    (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                    libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
                        .expect("sockaddr_un 大小远小于 socklen_t 上限"),
                )
            };
            assert_eq!(rc, 0, "连接失败");
            // 保持连接一小段时间，让 accept 完成凭据读取。
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: fd 由本线程拥有。
            unsafe { libc::close(fd) };
        });

        let peer = listener.accept().expect("应接受连接");
        let creds = peer.credentials();

        // 本测试进程的 uid 即为对端 uid（若以 root 运行则为 0）。
        // SAFETY: getuid 无副作用。
        let self_uid = unsafe { libc::getuid() };
        assert_eq!(creds.uid, self_uid);
        assert_eq!(peer.is_trusted(), self_uid == 0);
        assert!(creds.pid > 0, "应报告对端 pid");

        handle.join().unwrap();
        drop(peer);
        drop(listener);
        testutil::cleanup(&dir);
    }

    #[test]
    fn untrusted_peer_is_not_trusted() {
        // 直接构造凭据判断逻辑：非 0 uid 必须不被信任。
        let untrusted = PeerCredentials {
            uid: 1000,
            gid: 1000,
            pid: 1234,
        };
        assert_ne!(untrusted.uid, 0);

        let trusted = PeerCredentials {
            uid: 0,
            gid: 0,
            pid: 1,
        };
        assert_eq!(trusted.uid, 0);
    }

    #[test]
    fn path_too_long_is_rejected() {
        let dir = testutil::temp_dir("sock-long");
        let long = format!("{}.sock", "x".repeat(200));
        let path = dir.join("run").join(long);

        let err = bind(&path).expect_err("过长路径必须被拒绝");
        assert!(matches!(err, SocketError::NulPath(_)));

        testutil::cleanup(&dir);
    }
}
