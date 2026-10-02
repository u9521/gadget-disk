//! socket 服务循环：接受连接、握手、逐请求应答。
//!
//! 连接模型（[docs/protocol.md](../../../../docs/protocol.md)）：
//! **一请求一连接** —— `connect` → 握手 → 请求 → 响应 → `close`，无跨请求特权状态。

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gadgetdisk_proto::{Message, PROTOCOL_VERSION, read_frame, read_handshake, write_handshake};

use crate::kernel::MassStorageOps;
use crate::logging;
use crate::paths::DataDirs;
use crate::service::{Service, error};
use crate::socket::{self, Listener, SocketError};

/// 非阻塞 `accept` 的轮询间隔。
///
/// 决定两件事的及时性：`shutdown` 标志的响应，以及「设备弹出」被察觉的延迟。
/// 取 100ms：足够及时，空转开销可忽略（10 次/秒）。
pub const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// gdd 参数。
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// 数据目录。
    pub dirs: DataDirs,
    /// socket 路径；`None` 表示用 `dirs.socket_path()`。
    pub socket_path: Option<PathBuf>,
    /// 无挂载且无请求多久后自行退出。
    ///
    /// 按需进程模型的核心：gdd 不是常驻服务，而只在「有镜像被导出为
    /// USB 设备」期间存在。有挂载时**永不**因空闲退出（否则会失去守卫）。
    pub idle_timeout: Duration,
}

/// 默认空闲退出时长（秒）。
///
/// 经验值：足够覆盖用户在 WebUI 上连续操作的间隔，又不至于长时间驻留。
/// 可用 `--idle-timeout` 覆盖。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

impl DaemonConfig {
    /// 以默认数据根目录构造。
    pub fn new(dirs: DataDirs) -> Self {
        Self {
            dirs,
            socket_path: None,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// 覆盖 socket 路径。
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = Some(path.into());
        self
    }

    /// 覆盖空闲退出时长。
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// 生效的 socket 路径。
    pub fn resolved_socket_path(&self) -> PathBuf {
        self.socket_path
            .clone()
            .unwrap_or_else(|| self.dirs.socket_path())
    }
}

/// 是否应因空闲而退出。
///
/// **纯函数**，便于在主机上穷举判定逻辑而不必真的等 60 秒。
///
/// 规则：**只要有镜像仍被导出为 gadget LUN 就永不退出**——那时 gdd 是
/// 该镜像的唯一守卫，退出会让「同一镜像不可同时为 gadget LUN 与 loop 附件」
/// 这条数据安全底线失去执行者。只有在无挂载、且静默超过 `timeout` 时才退出。
pub fn idle_should_exit(idle_for: Duration, timeout: Duration, has_active_gadget: bool) -> bool {
    if has_active_gadget {
        return false;
    }
    idle_for >= timeout
}

/// 若检测到「设备已弹出」，清理我们自己的链接与 function。
///
/// **不解绑 UDC**：内核在 gadget deactivate（拔线）时会把 `lun.0/file` 清空，
/// 此时只需撤掉自己的痕迹；一旦解绑，Android 的 `init` 会立刻按
/// `sys.usb.config` 重装它自己的配置，反而把状态搅乱。
///
/// 抽成独立函数是为了**可测**：`run` 的循环会阻塞，无法在主机上端到端断言，
/// 而「弹出后是否真的清理了」是本模块最该被钉住的行为。
///
/// 返回 `true` 表示本次确实执行了清理。
pub fn handle_eject_if_needed<G: MassStorageOps>(gadget: &mut G) -> bool {
    if !gadget.is_ejected() {
        return false;
    }
    let failures = gadget.cleanup_after_eject();
    for failure in &failures {
        logging::warn(&format!("eject cleanup incomplete: {failure}"));
    }
    logging::info("device eject detected; cleaned up the function and links (UDC left bound)");
    true
}

/// 运行期错误。
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// socket 层失败。
    #[error(transparent)]
    Socket(#[from] SocketError),

    /// 目录准备失败。
    #[error("failed to prepare the data directory: {0}")]
    Io(#[from] std::io::Error),
}

/// 处理一个已接受的连接。
///
/// 返回 `Ok(())` 表示连接被正常处理（含被拒绝的情况）；对端不可信时立即关闭。
pub fn serve_connection<G>(
    service: &Arc<Mutex<Service<G>>>,
    peer: &mut socket::Peer,
) -> std::io::Result<()>
where
    G: MassStorageOps,
{
    // 访问控制：SO_PEERCRED 的 uid 必须为 0，否则立即关闭。
    //
    // 这是第二道防线（第一道是 0700 目录让非 root 无法 connect）。
    // 两道独立防线，任一生效即可挡住非 root 客户端。
    if !peer.is_trusted() {
        // 不回应任何内容，直接关闭；把原因写入 stderr 便于排查。
        let creds = peer.credentials();
        logging::warn(&format!(
            "rejected a non-root connection (uid={} pid={})",
            creds.uid, creds.pid
        ));
        return Ok(());
    }

    // 握手：读客户端版本，回 1（支持）或 0（不支持）。
    match read_handshake(peer) {
        Ok(Some(PROTOCOL_VERSION)) => {
            write_handshake_io(peer, true)?;
        }
        Ok(Some(other)) => {
            // 版本不匹配时以 0 应答，而非静默断开。
            logging::warn(&format!(
                "protocol version mismatch (peer {other}, local {PROTOCOL_VERSION})"
            ));
            write_handshake_io(peer, false)?;
            return Ok(());
        }
        Ok(None) => {
            // 对端声明不支持。
            write_handshake_io(peer, false)?;
            return Ok(());
        }
        Err(err) => {
            logging::warn(&format!("handshake read failed: {err}"));
            return Ok(());
        }
    }

    // 读一条请求帧。
    let (id, payload) = match read_frame(peer) {
        Ok(frame) => frame,
        Err(err) => {
            logging::warn(&format!("request read failed: {err}"));
            return Ok(());
        }
    };

    // 按 id 解码。未知 id 与负载非法都要给出明确的错误应答，而不是断开。
    let request = match Message::decode(id, &payload) {
        None => {
            let response = error(
                gadgetdisk_proto::ErrorCode::InvalidArgument,
                format!("unknown message id: {id:#04X}"),
            );
            return write_response(peer, response);
        }
        Some(Err(err)) => {
            let response = error(
                gadgetdisk_proto::ErrorCode::InvalidArgument,
                format!("cannot parse the request: {err}"),
            );
            return write_response(peer, response);
        }
        Some(Ok(message)) => message,
    };

    // 交给 service 处理。
    let response = {
        let mut guard = service
            .lock()
            .map_err(|_| std::io::Error::other("service lock poisoned"))?;
        guard.handle(request)
    };

    write_response(peer, response)
}

/// 把应答编码并写出。
fn write_response(peer: &mut socket::Peer, response: Message) -> std::io::Result<()> {
    let payload = response
        .encode_payload()
        .map_err(|err| std::io::Error::other(format!("failed to encode the response: {err}")))?;
    socket::write_all(peer, &payload, response.id())?;
    peer.flush()
}

/// [`gadgetdisk_proto::write_handshake`] 的 `io::Result` 包装。
fn write_handshake_io(peer: &mut socket::Peer, supported: bool) -> std::io::Result<()> {
    write_handshake(peer, supported).map_err(|err| std::io::Error::other(err.to_string()))
}

/// 绑定并返回监听器（供调用方自行驱动 accept 循环）。
pub fn bind(config: &DaemonConfig) -> Result<Listener, DaemonError> {
    config.dirs.create_all()?;
    // 数据根目录权限：仅 root 可进入（与 socket 目录同级防护）。
    let listener = socket::bind(&config.resolved_socket_path())?;
    Ok(listener)
}

/// 驱动 accept 循环，直到 `shutdown` 返回真**或**空闲退出条件满足。
///
/// ## 退出条件
///
/// 1. `shutdown()` 返回真（信号驱动的优雅退出）；
/// 2. [`idle_should_exit`] 为真：无 gadget 挂载且静默超过 `idle_timeout`。
///
/// 循环用 [`Listener::accept_timeout`] 而非阻塞 `accept`，否则无连接时
/// 永远醒不过来、也观察不到 `shutdown` 标志。
pub fn run<G>(
    config: DaemonConfig,
    service: Arc<Mutex<Service<G>>>,
    mut shutdown: impl FnMut() -> bool,
) -> Result<(), DaemonError>
where
    G: MassStorageOps,
{
    let listener = bind(&config)?;
    logging::info(&format!(
        "listening on {} (exits after {}s idle with no mounts)",
        listener.path().display(),
        config.idle_timeout.as_secs()
    ));

    // 轮询间隔**必须与空闲超时解耦且足够短**。
    //
    // 早期实现取 `idle_timeout / 4`（默认 120s / 4 = 30s），于是「设备弹出」
    // 之后最长要 30 秒才会被察觉并清理；同一错误在 `serve` 上更严重——它让每个
    // REST 请求排队等到下一次轮询（实测 15 秒）。两处都已改为固定短间隔。
    let tick = ACCEPT_POLL_INTERVAL;
    let mut last_activity = Instant::now();

    loop {
        if shutdown() {
            logging::info("exit signal received");
            break;
        }

        // 有镜像被导出时 gdd 是它的唯一守卫，此时不允许因空闲退出。
        let has_active_gadget = {
            let mut guard = service
                .lock()
                .map_err(|_| std::io::Error::other("service lock poisoned"))?;

            // 「设备弹出」的收尾（见 handle_eject_if_needed 的说明）。
            handle_eject_if_needed(&mut guard.gadget);

            !guard.gadget.mounted_images().is_empty()
        };

        if idle_should_exit(
            last_activity.elapsed(),
            config.idle_timeout,
            has_active_gadget,
        ) {
            logging::info(&format!(
                "no mounts and idle for {}s, exiting",
                config.idle_timeout.as_secs()
            ));
            break;
        }

        match listener.accept_timeout(tick) {
            Ok(Some(mut peer)) => {
                // 任何连接都算活动，包括被拒绝的——否则外部轮询会不断
                // 把 gdd 的退出时间往后推，而它其实没做任何有用的事。
                last_activity = Instant::now();
                if let Err(err) = serve_connection(&service, &mut peer) {
                    logging::warn(&format!("connection handling failed: {err}"));
                }
            }
            Ok(None) => {} // 超时：回到循环顶部重新判断退出条件
            Err(err) => {
                logging::warn(&format!("accept failed: {err}"));
            }
        }
    }

    // `Listener` 的 `Drop` 会 unlink socket 文件；此处显式说明这一依赖，
    // 避免将来有人把 listener 挪进更长的生命周期而留下陈旧节点。
    drop(listener);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{GadgetView, KernelResult, MassStorageOps, NullBackend};
    use crate::testutil;
    use gadgetdisk_proto::{LunInfo, MountDevice};
    use gadgetdisk_proto::{Message, StatusResponse, write_json as proto_write_json};

    type TestService = Service<NullBackend>;

    fn test_setup(tag: &str) -> (DaemonConfig, Arc<Mutex<TestService>>, PathBuf) {
        let root = testutil::temp_dir(tag);
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let config = DaemonConfig::new(dirs.clone());
        let service = Arc::new(Mutex::new(Service::new(
            dirs,
            NullBackend::new(Some("dummy_udc.0".into())),
        )));
        (config, service, root)
    }

    /// 可脚本化的假 gadget 后端：只用来断言「弹出时是否调用了清理」。
    #[derive(Default)]
    struct FakeGadget {
        ejected: bool,
        cleanup_calls: usize,
        cleanup_failures: Vec<String>,
        mounted: Vec<std::path::PathBuf>,
    }

    impl GadgetView for FakeGadget {
        fn udc(&self) -> Option<String> {
            Some("dummy_udc.0".into())
        }
        fn luns(&self) -> Vec<LunInfo> {
            Vec::new()
        }
        fn is_mounted(&self, _image: &std::path::Path) -> bool {
            false
        }
        fn mounted_images(&self) -> Vec<std::path::PathBuf> {
            self.mounted.clone()
        }
    }

    impl MassStorageOps for FakeGadget {
        fn mount(
            &mut self,
            _devices: &[MountDevice],
            _force_rebind: bool,
        ) -> KernelResult<Vec<LunInfo>> {
            Ok(Vec::new())
        }
        fn unmount_lun(&mut self, _lun: u8) -> KernelResult<Vec<LunInfo>> {
            Ok(Vec::new())
        }
        fn eject_all(&mut self) -> KernelResult<Vec<LunInfo>> {
            Ok(Vec::new())
        }
        fn delete_slot(&mut self, _lun: u8) -> KernelResult<Vec<LunInfo>> {
            Ok(Vec::new())
        }
        fn teardown(&mut self) -> Vec<String> {
            Vec::new()
        }
        fn rebind(&mut self) -> KernelResult<String> {
            Ok("dummy_udc.0".into())
        }
        fn is_ejected(&self) -> bool {
            self.ejected
        }
        fn cleanup_after_eject(&mut self) -> Vec<String> {
            self.cleanup_calls += 1;
            self.cleanup_failures.clone()
        }
    }

    /// 弹出时必须清理；未弹出时**不得**清理。
    #[test]
    fn eject_triggers_cleanup_exactly_once_per_detection() {
        let mut gadget = FakeGadget {
            ejected: true,
            ..Default::default()
        };
        assert!(handle_eject_if_needed(&mut gadget), "弹出时应报告已清理");
        assert_eq!(gadget.cleanup_calls, 1);

        // 还没弹出 → 不清理。
        gadget.ejected = false;
        assert!(!handle_eject_if_needed(&mut gadget));
        assert_eq!(gadget.cleanup_calls, 1, "未弹出不得调用清理");
    }

    /// 弹出判定的语义（在 `UsbGadget` 上实现，这里锁住「判据必须同时看
    /// 我们自己的痕迹与内核状态」这一约定）。
    ///
    /// 回归（AVD 实测）：早期实现先要求「`luns()` 里有 attached 的项」再看
    /// `file` 是否为空——但弹出**就是** `file` 变空，那一刻 `attached` 已经是
    /// false，条件永远不成立，清理从不触发。
    #[test]
    fn eject_requires_our_artifacts_to_still_exist() {
        // 未导出（没有我们的 function/链接）→ 不算弹出。
        let mut not_exported = FakeGadget {
            ejected: false,
            ..Default::default()
        };
        assert!(!handle_eject_if_needed(&mut not_exported));
        assert_eq!(not_exported.cleanup_calls, 0);

        // 导出过且已被弹出 → 必须清理。
        let mut exported_then_ejected = FakeGadget {
            ejected: true,
            ..Default::default()
        };
        assert!(handle_eject_if_needed(&mut exported_then_ejected));
        assert_eq!(exported_then_ejected.cleanup_calls, 1);
    }

    /// 清理失败只记录，不影响「已处理过弹出」的判定。
    #[test]
    fn eject_cleanup_failures_do_not_panic() {
        let mut gadget = FakeGadget {
            ejected: true,
            cleanup_failures: vec!["删除 function 失败：EBUSY".into()],
            ..Default::default()
        };
        assert!(handle_eject_if_needed(&mut gadget));
        assert_eq!(gadget.cleanup_calls, 1);
    }

    /// 读取一条应答消息（用 `Message::decode`，因为 `Message` 不作为整体反序列化）。
    fn read_message<R: std::io::Read>(reader: &mut R) -> (u8, Message) {
        let (id, payload) = read_frame(reader).unwrap();
        let message = Message::decode(id, &payload)
            .unwrap_or_else(|| panic!("未知 id {id:#04X}"))
            .unwrap();
        (id, message)
    }

    /// 在后台接受一次连接并处理，返回 join 句柄。
    ///
    /// `credentials` 用于模拟对端身份：测试常以非 root 运行，无法产生真实的
    /// root 对端，而凭据校验是安全关键路径，必须能直接测试。
    fn accept_once_with(
        listener: &Listener,
        service: &Arc<Mutex<TestService>>,
        credentials: socket::PeerCredentials,
    ) -> std::thread::JoinHandle<()> {
        // 需要一个能跨线程使用的 listener：用原始 fd 复制。
        let fd = listener.as_raw_fd();
        // SAFETY: `fd` 借自仍存活的 `listener`，是打开状态的有效 fd；`dup` 只读取
        // 它并返回一个**新的** fd，不取走原 fd 的所有权。
        let dup = unsafe { libc::dup(fd) };
        assert!(dup >= 0);
        let service = Arc::clone(service);
        std::thread::spawn(move || {
            // SAFETY: `dup` 由本线程独占持有（原 fd 归调用方，此处只移动了 dup 的
            // 所有权进来），且已断言 >= 0；地址与长度参数传空指针是 accept 的合法用法。
            let client = unsafe { libc::accept(dup, std::ptr::null_mut(), std::ptr::null_mut()) };
            assert!(client >= 0, "accept 失败");
            use std::os::unix::io::FromRawFd as _;
            // SAFETY: `client` 是刚由 `accept` 返回的、当前无人持有的新 fd（已断言
            // >= 0）；交给 `OwnedFd` 后由它独占并在 drop 时关闭。
            let owned = unsafe { std::os::unix::io::OwnedFd::from_raw_fd(client) };
            let mut peer = socket::Peer::with_credentials_for_test(owned, credentials);
            serve_connection(&service, &mut peer).unwrap();
            // SAFETY: `dup` 是我们自己复制出来的 fd，所有权在本闭包内，关闭后不再使用。
            unsafe { libc::close(dup) };
        })
    }

    /// 以 root 身份接受一次连接（模拟经 `ksu.exec` 发起的 CLI）。
    fn accept_once(
        listener: &Listener,
        service: &Arc<Mutex<TestService>>,
    ) -> std::thread::JoinHandle<()> {
        accept_once_with(
            listener,
            service,
            socket::PeerCredentials {
                uid: 0,
                gid: 0,
                pid: 4242,
            },
        )
    }

    #[test]
    fn round_trip_status_over_real_socket() {
        let (config, service, root) = test_setup("srv-status");
        let listener = bind(&config).unwrap();
        let handle = accept_once(&listener, &service);

        // 客户端：连接、握手、发请求、读应答。
        let mut client = socket::connect_for_test(&config.resolved_socket_path()).unwrap();
        write_handshake(&mut client, true).unwrap();
        assert_eq!(read_handshake(&mut client).unwrap(), Some(PROTOCOL_VERSION));

        let request = Message::StatusRequest;
        let value: serde_json::Value =
            serde_json::from_slice(&request.encode_payload().unwrap()).unwrap();
        proto_write_json(&mut client, request.id(), &value).unwrap();

        let (_id, response) = read_message(&mut client);
        match response {
            Message::StatusResponse(StatusResponse { udc, devices }) => {
                assert_eq!(udc.as_deref(), Some("dummy_udc.0"));
                assert!(devices.is_empty());
            }
            other => panic!("期望 StatusResponse，得到 {other:?}"),
        }

        handle.join().unwrap();
        testutil::cleanup(&root);
    }

    #[test]
    fn version_mismatch_gets_zero_reply() {
        let (config, service, root) = test_setup("srv-version");
        let listener = bind(&config).unwrap();
        let handle = accept_once(&listener, &service);

        let mut client = socket::connect_for_test(&config.resolved_socket_path()).unwrap();
        // 发送一个不支持的版本。
        client.write_all(&[99]).unwrap();
        client.flush().unwrap();

        // gdd 必须明确回 0（不支持），而不是静默断开。
        assert_eq!(read_handshake(&mut client).unwrap(), None);

        handle.join().unwrap();
        testutil::cleanup(&root);
    }

    #[test]
    fn garbage_request_gets_error_response() {
        let (config, service, root) = test_setup("srv-garbage");
        let listener = bind(&config).unwrap();
        let handle = accept_once(&listener, &service);

        let mut client = socket::connect_for_test(&config.resolved_socket_path()).unwrap();
        write_handshake(&mut client, true).unwrap();
        let _ = read_handshake(&mut client).unwrap();

        // 发一帧非法 JSON。
        gadgetdisk_proto::write_frame(&mut client, 0x11, b"{not json}").unwrap();

        let (_id, response) = read_message(&mut client);
        match response {
            Message::Error(err) => {
                assert_eq!(err.code, gadgetdisk_proto::ErrorCode::InvalidArgument);
            }
            other => panic!("期望 Error，得到 {other:?}"),
        }

        handle.join().unwrap();
        testutil::cleanup(&root);
    }

    #[test]
    fn bind_refuses_second_instance() {
        let (config, _, root) = test_setup("srv-second");
        let _first = bind(&config).unwrap();

        let err = bind(&config).expect_err("第二个实例必须拒绝启动");
        assert!(matches!(
            err,
            DaemonError::Socket(SocketError::AlreadyRunning(_))
        ));

        testutil::cleanup(&root);
    }

    #[test]
    fn untrusted_peer_is_rejected_without_response() {
        // SO_PEERCRED 的 uid != 0 必须立即关闭，且不回应任何内容。
        let (config, service, root) = test_setup("srv-untrusted");
        let listener = bind(&config).unwrap();

        let handle = accept_once_with(
            &listener,
            &service,
            socket::PeerCredentials {
                uid: 1000,
                gid: 1000,
                pid: 31337,
            },
        );

        // 客户端连上后应先收到「对端关闭」而不是正常握手应答。
        let mut client = socket::connect_for_test(&config.resolved_socket_path()).unwrap();

        // 尝试读握手字节：应得到 EOF（连接被关闭）而非任何数据。
        use std::io::Read as _;
        let mut byte = [0u8; 1];
        let outcome = client.read(&mut byte);
        match outcome {
            Ok(0) => {} // 正常的关闭
            Ok(n) => panic!("对端不应收到任何数据，却读到 {n} 字节"),
            Err(err) => {
                // 某些内核会返回 ConnectionReset；同样是正确的拒绝行为。
                assert_eq!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset,
                    "期望 EOF 或 ConnectionReset，得到 {err:?}"
                );
            }
        }

        handle.join().unwrap();
        testutil::cleanup(&root);
    }

    #[test]
    fn config_resolves_default_socket_path() {
        let dirs = DataDirs::new("/tmp/gd-test");
        let config = DaemonConfig::new(dirs);
        assert_eq!(
            config.resolved_socket_path(),
            PathBuf::from("/tmp/gd-test/run/gdd.sock")
        );

        let overridden = config.with_socket_path("/tmp/other.sock");
        assert_eq!(
            overridden.resolved_socket_path(),
            PathBuf::from("/tmp/other.sock")
        );
    }

    // ------------------------------------------------------------ 空闲退出

    #[test]
    fn idle_exit_requires_no_gadget_and_full_timeout() {
        // 静默不足时不退出。
        assert!(!idle_should_exit(
            Duration::from_secs(30),
            Duration::from_secs(60),
            false
        ));
        // 静默足够且无挂载 → 退出。
        assert!(idle_should_exit(
            Duration::from_secs(60),
            Duration::from_secs(60),
            false
        ));
        // 恰好在边界上即退出（`>=`，避免无限期驻留）。
        assert!(idle_should_exit(
            Duration::from_secs(61),
            Duration::from_secs(60),
            false
        ));
    }

    #[test]
    fn idle_exit_never_happens_while_a_gadget_is_mounted() {
        // 这是**数据安全底线**的一部分：gdd 是已导出镜像的唯一守卫，
        // 它退出后就没有人阻止同一镜像再被 loop 挂载。
        for idle_for in [0, 1, 60, 3_600, 86_400] {
            assert!(
                !idle_should_exit(Duration::from_secs(idle_for), Duration::from_secs(60), true),
                "有 gadget 挂载时静默 {idle_for}s 也不得退出"
            );
        }
    }

    #[test]
    fn config_carries_a_default_idle_timeout_and_can_override_it() {
        let dirs = DataDirs::new("/tmp/gd-test");
        assert_eq!(
            DaemonConfig::new(dirs.clone()).idle_timeout,
            DEFAULT_IDLE_TIMEOUT
        );

        let custom = DaemonConfig::new(dirs).with_idle_timeout(Duration::from_secs(5));
        assert_eq!(custom.idle_timeout, Duration::from_secs(5));
    }

    #[test]
    fn accept_timeout_returns_none_when_no_connection_arrives() {
        // 按需模型依赖这个行为：没有连接时必须能醒来判断是否退出，
        // 而不是永久阻塞在 accept 上。
        let root = testutil::temp_dir("srv-accept-timeout");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let config = DaemonConfig::new(dirs);
        let listener = bind(&config).unwrap();

        let started = Instant::now();
        let got = listener.accept_timeout(Duration::from_millis(150)).unwrap();
        assert!(got.is_none(), "无连接时应返回 None");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "应真的等待了超时时长，而不是立刻返回"
        );

        drop(listener);
        testutil::cleanup(&root);
    }

    #[test]
    fn dropping_the_listener_unlinks_the_socket_file() {
        // 空闲退出后不得留下陈旧 socket 节点。
        let root = testutil::temp_dir("srv-unlink");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let config = DaemonConfig::new(dirs);
        let path = config.resolved_socket_path();

        let listener = bind(&config).unwrap();
        assert!(path.exists(), "绑定后 socket 文件应存在");

        drop(listener);
        assert!(!path.exists(), "退出后 socket 文件必须被 unlink");

        testutil::cleanup(&root);
    }
}
