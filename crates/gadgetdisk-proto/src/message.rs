//! 协议消息定义。
//!
//! 每个消息类型对应 [docs/protocol.md](../../../docs/protocol.md) 消息表中的一个 `id`。
//! 字段名与文档**逐字一致**，以便 WebUI 直接消费。
//!
//! 结构安排：每种消息的**载荷**是唯一权威定义（例如 [`StatusResponse`]），
//! [`Message`] 只是把载荷与线格式 `id` 配对的枚举，不重复声明字段。

use serde::{Deserialize, Serialize};

/// 协议版本。client 首字节发送，gdd 以 `1` 表示支持、`0` 表示不支持。
pub const PROTOCOL_VERSION: u8 = 1;

/// 设备模式：`rw` / `ro` / `cdrom`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 可读写 U 盘（默认）。
    #[default]
    Rw,
    /// 只读（写保护）。
    Ro,
    /// 光驱。
    Cdrom,
}

impl Mode {
    /// configfs `lun.N/ro` 属性取值。
    ///
    /// 光驱必须只读，故 `Cdrom` 也返回 `1`。
    pub const fn ro_attr(self) -> u8 {
        match self {
            Mode::Rw => 0,
            Mode::Ro | Mode::Cdrom => 1,
        }
    }

    /// configfs `lun.N/cdrom` 属性取值。
    pub const fn cdrom_attr(self) -> u8 {
        match self {
            Mode::Cdrom => 1,
            Mode::Rw | Mode::Ro => 0,
        }
    }

    /// 该模式是否要求只读打开后端文件。
    pub const fn is_read_only(self) -> bool {
        matches!(self, Mode::Ro | Mode::Cdrom)
    }

    /// 线格式字符串，与 `docs/protocol.md` 的设备模式表一致。
    ///
    /// 用于把内核真值回写成可持久化的意图（`run/state.json`）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Rw => "rw",
            Mode::Ro => "ro",
            Mode::Cdrom => "cdrom",
        }
    }
}

/// 磁盘布局：`raw` / `gpt` / `mbr` / `unknown`。
///
/// 比 `gadgetdisk_core::ImageLayout` 多一个 `Unknown`，用于描述未被识别的既有镜像。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageLayout {
    /// 无分区表。
    Raw,
    /// GPT 分区表。
    Gpt,
    /// MBR 分区表。
    Mbr,
    /// 无法识别。
    #[default]
    Unknown,
}

/// 镜像占用状态：`none` / `gadget` / `loop` / `importing`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageState {
    /// 空闲。
    #[default]
    None,
    /// 作为 gadget LUN 挂载中。
    Gadget,
    /// 作为 loop 附件挂载中。
    Loop,
    /// 正在被导入任务写入。
    Importing,
}

/// 长任务状态：`running` / `done` / `failed`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// 进行中。
    Running,
    /// 已完成。
    Done,
    /// 已失败。
    Failed,
}

/// 稳定错误码。WebUI 依据它做文案映射，故不得随意改名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 另一操作正在进行。
    Busy,
    /// 镜像不存在。
    ImageNotFound,
    /// 镜像已被 gadget 或 loop 占用。
    ImageInUse,
    /// 路径不是常规文件。
    NotRegularFile,
    /// 无法识别的磁盘布局。
    UnsupportedLayout,
    /// 无可用 USB 控制器。
    NoUdc,
    /// 内核不支持 mass_storage function。
    MassStorageUnsupported,
    /// loop 能力不可用。
    LoopUnsupported,
    /// 内核缺少所需文件系统。
    FilesystemUnsupported,
    /// 容量低于 FAT32 下限。
    SizeBelowMinimum,
    /// 目标文件系统空间不足。
    NoSpace,
    /// 权限或 SELinux 拒绝。
    PermissionDenied,
    /// 参数非法。
    InvalidArgument,
    /// 目标已存在。
    ///
    /// 用于「创建镜像时同名文件已存在」：**拒绝而不是覆盖**，因为覆盖会静默
    /// 丢掉用户镜像里的全部数据。WebUI 据此提示用户改名或先删除。
    AlreadyExists,
    /// 无法确定可用的 gadget / configfs 未就绪。
    ///
    /// 与 [`ErrorCode::NoUdc`] 的区别：前者表示「未检测到可用的 UDC 控制器」，
    /// 后者表示「虽存在 UDC 但无法确定目标 gadget」——例如检测到多个 gadget 且无法裁决，
    /// 或 `/config/usb_gadget` 不可读。
    ConfigfsUnavailable,
    /// USB 配置已建立，但主机没有接受（未生效）。
    ///
    /// 真机实测的典型形态：`lun.0/file` 已绑定、链接也在，但
    /// `/sys/class/udc/<udc>/state` 停在 `addressed`——主机侧表现为
    /// 「该设备无法启动（代码 10）」。**绝不能**把它报成成功。
    NotActive,
    /// 内部错误（文档未列出，用于兜底而非隐藏问题）。
    Internal,
}

impl ErrorCode {
    /// 线格式字符串，与文档错误码表一致。
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Busy => "busy",
            ErrorCode::ImageNotFound => "image_not_found",
            ErrorCode::ImageInUse => "image_in_use",
            ErrorCode::NotRegularFile => "not_regular_file",
            ErrorCode::UnsupportedLayout => "unsupported_layout",
            ErrorCode::NoUdc => "no_udc",
            ErrorCode::MassStorageUnsupported => "mass_storage_unsupported",
            ErrorCode::LoopUnsupported => "loop_unsupported",
            ErrorCode::FilesystemUnsupported => "filesystem_unsupported",
            ErrorCode::SizeBelowMinimum => "size_below_minimum",
            ErrorCode::NoSpace => "no_space",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::AlreadyExists => "already_exists",
            ErrorCode::ConfigfsUnavailable => "configfs_unavailable",
            ErrorCode::NotActive => "not_active",
            ErrorCode::Internal => "internal",
        }
    }
}

// ---------------------------------------------------------------- 结构定义

/// LUN 信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LunInfo {
    /// LUN 序号。
    pub index: u8,
    /// 后端镜像路径。
    pub image_path: String,
    /// 容量字节数。
    pub size_bytes: u64,
    /// 设备模式。
    pub mode: Mode,
    /// SCSI INQUIRY 字符串；未设置时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inquiry_string: Option<String>,
    /// 是否已挂到配置上。
    pub attached: bool,
    /// 是否已实际生效；`false` 表示已配置但未生效（如 USB 未连接 / 无 UDC）。
    pub effective: bool,
    /// 该槽位能否删除。
    ///
    /// `lun.0` 由内核随 function 创建，**只能弹出不能删除**（`rmdir` 返回
    /// `EPERM`，已实测）。把这条内核知识放进协议，是为了让 UI 不必自己复制它
    /// ——否则每个前端都要记得「0 号特殊」。
    pub deletable: bool,
}

/// 镜像信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// 镜像路径。
    pub path: String,
    /// 容量字节数。
    pub size_bytes: u64,
    /// 修改时间（Unix 秒）。
    pub mtime: i64,
    /// 磁盘布局。
    pub layout: ImageLayout,
    /// 分区起始偏移；未知布局为 `null`。
    pub partition_offset_bytes: Option<u64>,
    /// 占用状态。
    pub in_use: ImageState,
}

/// loop 附件信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// 镜像路径。
    pub image: String,
    /// loop 设备路径。
    pub loop_dev: String,
    /// 派生出的分区子设备。
    ///
    /// **恒为空**：这些子设备只由 `LO_FLAGS_PARTSCAN` 路径产生，而该路径
    /// 已于 2026-10-06 移除（Android 无 devtmpfs，内核不建 `loopNpM`，
    /// 恒回退）。字段保留仅为兼容既有 REST 消费者，**不应**据此判断
    /// 挂载方式；实际挂载的设备见 [`Attachment::loop_dev`]。
    pub loop_part_devs: Vec<String>,
    /// 挂载点。
    pub mountpoint: String,
    /// 是否只读挂载。
    pub read_only: bool,
}

/// job 状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    /// 任务状态。
    pub state: JobState,
    /// 已复制字节数。
    pub bytes_done: u64,
    /// 总字节数。
    pub bytes_total: u64,
    /// 失败原因；仅 `failed` 时有值。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// 能力探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `/dev/loop-control` 是否可打开。
    pub loop_control: bool,
    /// loop 驱动的 `max_part` 参数值。
    ///
    /// **仍在使用**：`loopdev` 依赖它推导 loop 设备次设备号
    /// （`minor = index * (max_part + 1)`），以补建 Android 未预建的
    /// `/dev/block/loopN` 节点。它**不再**用于判断分区扫描。
    ///
    /// 曾由它派生出 `partscan_supported`，但该布尔值只反映
    /// 「`max_part > 0`」，而 Android 无 udev/devtmpfs、内核不建
    /// `loopNpM` 节点，该路径**恒回退**。字段与整条 partscan 挂载路径
    /// 已于 2026-10-06 一并移除，详见
    /// [本地 loop 挂载](../../../docs/ondevice-loop-mount.md)。
    pub max_part: u32,
    /// 内核支持的文件系统（例如 `["vfat","exfat"]`）。
    pub filesystems: Vec<String>,
    /// 内核是否支持 mass_storage function。
    pub mass_storage_supported: bool,
    /// SELinux 是否处于 enforcing。
    pub selinux_enforcing: bool,
}

/// `MountRequest` 中的单个设备描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountDevice {
    /// LUN 序号；缺省由 gdd 分配到第一个空闲 LUN。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lun: Option<u8>,
    /// 镜像路径。
    pub image_path: String,
    /// 设备模式。
    pub mode: Mode,
    /// SCSI INQUIRY 字符串（最长 28 字符，见 `gadgetdisk_usb::INQUIRY_STRING_MAX`）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inquiry_string: Option<String>,
}

// ---------------------------------------------------------------- 消息载荷

/// `0x01` gdd → client：错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// 稳定错误码。
    pub code: ErrorCode,
    /// 人类可读说明。
    pub message: String,
}

/// `0x11` gdd → client：状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    /// 当前 UDC 名；无可用控制器时为 `None`。
    pub udc: Option<String>,
    /// 各 LUN 状态。
    pub devices: Vec<LunInfo>,
}

/// `0x20` client → gdd：挂载请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRequest {
    /// 待挂载设备列表。
    pub devices: Vec<MountDevice>,
    /// 即使没有结构性改动也强制走一次「断 UDC → 重绑」。
    ///
    /// 用途：身份（`idVendor`/字符串描述符）被 CLI 改写后，需要一次重新 bind
    /// 才能让主机看到新描述符。
    #[serde(default)]
    pub rebind: bool,
}

/// `0x21` gdd → client：挂载结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountResponse {
    /// 挂载后的 LUN 状态。
    pub devices: Vec<LunInfo>,
}

/// `0x30` client → gdd：卸载请求。
///
/// **语义分两种，区别很大**：
///
/// - `lun = Some(n)`：只弹出第 `n` 个 LUN 的**介质**。LUN 目录、配置链接、
///   function 与 UDC **全部保留**，主机侧只看到该介质消失。要恢复只需重新
///   写 `lun.n/file`，无需重配 USB。
/// - `lun = None`：**全部弹出并拆除**——清全部 file、删链接、删 function。
///   即完全退出「USB 大容量存储 (UMS)」模式的语义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UnmountRequest {
    /// 指定 LUN；`None` 表示全部弹出并拆除。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lun: Option<u8>,
}

/// `0x31` gdd → client：卸载结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmountResponse {
    /// 已释放的 LUN 序号（全部弹出时是当时存在的全部）。
    pub released: Vec<u8>,
    /// 操作后的内核真值。
    ///
    /// 返回它而不是让调用方再发一次 `StatusRequest`：两次请求之间状态可能变化，
    /// 而调用方（CLI）要据此写 `state.json`——必须写自己刚造成的那份事实。
    pub devices: Vec<LunInfo>,
}

/// `0x32` client → gdd：重新绑定 UDC。
///
/// 用途：身份（`idVendor`/`idProduct`/字符串描述符）由 CLI 直接写 configfs，
/// 但改动**只在下次 bind 时生效**。CLI 改完身份后发这条消息，让 gdd 走一次
/// 「断 UDC → 等 Android 绑回 / 自己绑」，使新描述符被主机看到。
///
/// 为什么不让 CLI 自己断 UDC：UDC 的写入是 `gdd` 的领地（它是配置的唯一写入者），
/// 两个进程都改 UDC 会引入「谁先谁后」的竞态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RebindRequest {}

/// `0x33` gdd → client：重绑结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebindResponse {
    /// 重绑后实际生效的 UDC 名。
    pub udc: String,
}

/// `0x34` client → gdd：删除一个空闲槽位。
///
/// ## 语义
///
/// 删除 = 弹掉介质 + `rmdir lun.N`，使该序号回到「不存在」状态。之后重新挂载
/// 同号会走一次 LUN 创建（与首次创建同样的内核约束）。
///
/// **只允许删空闲槽位**：`file` 非空时先 `forced_eject` 再删。调用方不必自己
/// 先弹出——但界面仍会先弹再删，让用户看清每一步。
///
/// ## 为什么 `lun.0` 不能删
///
/// 它由内核在创建 function 时自动生成（`fsg_alloc_inst` 里注册为默认组），
/// `rmdir` 返回 `EPERM`（已实测）。因此 `lun.0` 只能「弹出后空闲」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSlotRequest {
    /// 待删除的 LUN 序号。
    pub lun: u8,
}

/// `0x35` gdd → client：删除槽位结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSlotResponse {
    /// 操作后的内核真值。
    ///
    /// 与 `MountResponse`/`UnmountResponse` 同样的理由：调用方（CLI）要据此写
    /// `run/state.json`，而删除槽位会**隐式解绑 UDC**（内核 `fsg_lun_drop` 调
    /// `unregister_gadget_item`），因此操作后的状态必须由内核现读回报，不能由
    /// 调用方推断。
    pub devices: Vec<LunInfo>,
}

// ---------------------------------------------------------------- 信封

/// 协议消息总枚举：将线格式指令 ID（Opcode）与其对应的强类型负载结构体进行映射。
///
/// 协议严格限定为 CLI 与 `gdd` 之间的私有控制链路，仅包含 mass_storage 导出所必需的消息；
/// 其余镜像创建、导入与本地 loop 挂载均由 CLI 就地执行，不经过 socket IPC。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// `0x01` gdd → client：错误。
    Error(ErrorResponse),
    /// `0x10` client → gdd：查询状态。
    ///
    /// 保留它是必要的：CLI 的 `ensure_gdd` 用它做**真正的**健康检查
    /// （只看 socket 文件存在会误判崩溃留下的陈旧 socket）。
    StatusRequest,
    /// `0x11` gdd → client：状态。
    StatusResponse(StatusResponse),
    /// `0x20` client → gdd：挂载/更新 LUN。
    MountRequest(MountRequest),
    /// `0x21` gdd → client：挂载结果。
    MountResponse(MountResponse),
    /// `0x30` client → gdd：卸载（单个 LUN 或全部）。
    UnmountRequest(UnmountRequest),
    /// `0x31` gdd → client：卸载结果。
    UnmountResponse(UnmountResponse),
    /// `0x32` client → gdd：重新绑定 UDC。
    RebindRequest(RebindRequest),
    /// `0x33` gdd → client：重绑结果。
    RebindResponse(RebindResponse),
    /// `0x34` client → gdd：删除槽位。
    DeleteSlotRequest(DeleteSlotRequest),
    /// `0x35` gdd → client：删除槽位结果。
    DeleteSlotResponse(DeleteSlotResponse),
}

impl Message {
    /// 该消息的线格式 `id`。
    pub const fn id(&self) -> u8 {
        match self {
            Message::Error(_) => 0x01,
            Message::StatusRequest => 0x10,
            Message::StatusResponse(_) => 0x11,
            Message::MountRequest(_) => 0x20,
            Message::MountResponse(_) => 0x21,
            Message::UnmountRequest(_) => 0x30,
            Message::UnmountResponse(_) => 0x31,
            Message::RebindRequest(_) => 0x32,
            Message::RebindResponse(_) => 0x33,
            Message::DeleteSlotRequest(_) => 0x34,
            Message::DeleteSlotResponse(_) => 0x35,
        }
    }

    /// 该消息的 JSON 载荷字节。
    ///
    /// 无字段的请求（如 `StatusRequest`）序列化为 `{}`，与文档「所有消息均使用
    /// JSON 负载」一致。
    pub fn encode_payload(&self) -> serde_json::Result<Vec<u8>> {
        fn empty(_: &()) -> serde_json::Result<Vec<u8>> {
            Ok(b"{}".to_vec())
        }
        match self {
            Message::Error(v) => serde_json::to_vec(v),
            Message::StatusRequest => empty(&()),
            Message::StatusResponse(v) => serde_json::to_vec(v),
            Message::MountRequest(v) => serde_json::to_vec(v),
            Message::MountResponse(v) => serde_json::to_vec(v),
            Message::UnmountRequest(v) => serde_json::to_vec(v),
            Message::UnmountResponse(v) => serde_json::to_vec(v),
            Message::RebindRequest(v) => serde_json::to_vec(v),
            Message::RebindResponse(v) => serde_json::to_vec(v),
            Message::DeleteSlotRequest(v) => serde_json::to_vec(v),
            Message::DeleteSlotResponse(v) => serde_json::to_vec(v),
        }
    }

    /// 由线格式 `id` 与 JSON 载荷解码。
    ///
    /// 未知 `id` 返回 `None`，由上层决定回什么错误（不 panic）。
    pub fn decode(id: u8, payload: &[u8]) -> Option<serde_json::Result<Self>> {
        fn parse<T: serde::de::DeserializeOwned>(
            payload: &[u8],
            wrap: fn(T) -> Message,
        ) -> Option<serde_json::Result<Message>> {
            Some(serde_json::from_slice(payload).map(wrap))
        }

        match id {
            0x01 => parse(payload, Message::Error),
            0x10 => Some(Ok(Message::StatusRequest)),
            0x11 => parse(payload, Message::StatusResponse),
            0x20 => parse(payload, Message::MountRequest),
            0x21 => parse(payload, Message::MountResponse),
            0x30 => parse(payload, Message::UnmountRequest),
            0x31 => parse(payload, Message::UnmountResponse),
            0x32 => parse(payload, Message::RebindRequest),
            0x33 => parse(payload, Message::RebindResponse),
            0x34 => parse(payload, Message::DeleteSlotRequest),
            0x35 => parse(payload, Message::DeleteSlotResponse),
            _ => None,
        }
    }
}
