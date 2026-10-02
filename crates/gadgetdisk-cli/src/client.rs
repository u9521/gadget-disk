//! 把子命令映射为协议请求消息。
//!
//! 契约见 [docs/protocol.md](../../../../docs/protocol.md) 的「CLI 契约」：
//!
//! ```text
//! gadgetdisk <subcommand> [args...]
//! ```
//!
//! 把「解析」与「发送」分开，使子命令到消息的映射可以纯主机测试。

use std::path::PathBuf;

use gadgetdisk_proto::{
    ErrorCode, ImageLayout as ProtoLayout, Message, Mode as ProtoMode, MountDevice, MountRequest,
    UnmountRequest,
};

/// 子命令解析失败。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 未知布局名。
    #[error("unknown layout: {0} (available: raw, gpt, mbr)")]
    UnknownLayout(String),

    /// 未知模式。
    #[error("unknown mode: {0} (available: rw, ro, cdrom)")]
    UnknownMode(String),

    /// 缺少必需参数。
    #[error("missing argument: {0}")]
    MissingArgument(&'static str),

    /// 该子命令应当由 CLI 就地执行，不应经 socket 发给 `gdd`。
    ///
    /// 出现这个错误意味着**分派逻辑有 bug**（本该走 `run_client_locally` 的
    /// 命令被交给了 socket 通道），因此文案要指明是内部错误，免得用户去查
    /// 网络或 gdd。
    #[error("internal error: {0} must run in place in the CLI, not be sent to gdd over the socket")]
    NotOverSocket(String),

    /// 与 gdd 通信失败。
    ///
    /// 只渲染底层原因：调用方（`output::unreachable`）会补上路径，
    /// 否则错误文本里会出现两次路径。
    #[error("{source}")]
    Connect {
        /// socket 路径。
        path: PathBuf,
        /// 底层原因。
        source: std::io::Error,
    },

    /// 协议层失败。
    #[error("protocol error: {0}")]
    Protocol(String),

    /// gdd 返回了错误响应。
    ///
    /// `code` 保持为 [`ErrorCode`] 而不是 `String`：曾经这里转成字符串、
    /// 调用方再用一张 14 分支的表转回枚举，绕一圈既无收益，又会在新增错误码
    /// 时静默退化成 `Internal`。`ErrorCode::as_str()` 是唯一的字符串归属。
    #[error("{}: {message}", .code.as_str())]
    Server {
        /// 稳定错误码。
        code: ErrorCode,
        /// 说明。
        message: String,
    },
}

/// 解析布局名。
pub fn parse_layout(value: &str) -> Result<ProtoLayout, ClientError> {
    match value.to_ascii_lowercase().as_str() {
        "raw" => Ok(ProtoLayout::Raw),
        "gpt" => Ok(ProtoLayout::Gpt),
        "mbr" => Ok(ProtoLayout::Mbr),
        other => Err(ClientError::UnknownLayout(other.to_string())),
    }
}

/// 解析设备模式名。
pub fn parse_mode(value: &str) -> Result<ProtoMode, ClientError> {
    match value.to_ascii_lowercase().as_str() {
        "rw" => Ok(ProtoMode::Rw),
        "ro" => Ok(ProtoMode::Ro),
        "cdrom" => Ok(ProtoMode::Cdrom),
        other => Err(ClientError::UnknownMode(other.to_string())),
    }
}

/// 一条 `mount` 里的单个设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDevice {
    /// 镜像路径。
    pub image: PathBuf,
    /// 设备模式。
    pub mode: ProtoMode,
    /// 指定 LUN 序号；缺省由 `gdd` 分配。
    pub lun: Option<u8>,
    /// SCSI INQUIRY 字符串。
    pub inquiry_string: Option<String>,
}

/// CLI 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 查询状态。
    Status,
    /// 挂载镜像为 USB 设备（可多 LUN）。
    Mount {
        /// 待挂载的设备列表。
        devices: Vec<CommandDevice>,
        /// 是否强制重绑 UDC 让身份改动生效。
        rebind: bool,
    },
    /// 卸载。
    Unmount {
        /// 指定 LUN。
        lun: Option<u8>,
    },
    /// 创建镜像。
    Create {
        /// 目标路径。
        path: PathBuf,
        /// 容量。
        size_bytes: u64,
        /// 布局。
        layout: ProtoLayout,
        /// 文件系统。
        filesystem: String,
        /// 卷标。
        label: String,
    },
    /// 列出镜像。
    ListImages,
    /// 删除镜像。
    Delete {
        /// 镜像路径。
        path: PathBuf,
    },
    /// 导入镜像。
    Import {
        /// 源路径。
        source: PathBuf,
        /// 目标名。
        dest_name: String,
        /// 移动而非复制。
        mv: bool,
    },
    /// 本地 loop 挂载。
    AttachLoop {
        /// 镜像路径。
        image: PathBuf,
        /// 模式。
        mode: String,
        /// 分区序号。
        partition_index: Option<u32>,
        /// 只读。
        read_only: bool,
    },
    /// 释放 loop。
    DetachLoop {
        /// 镜像路径。
        image: Option<PathBuf>,
        /// loop 设备。
        loop_dev: Option<String>,
    },
    /// 删除一个空闲槽位。
    DeleteSlot {
        /// 槽位序号（`lun.0` 不可删）。
        lun: u8,
    },
    /// 重新绑定 UDC，促使主机重新枚举（让新的 USB 身份立即生效）。
    ///
    /// 与 `Mount { rebind: true }` 的区别是**不需要任何镜像**：它只重绑一次。
    /// 两条路径最终都落到 `RebindRequest`/`MountRequest.rebind`，但语义不同——
    /// 「挂载顺便重绑」与「只为让身份生效而重绑」是两件事。
    Rebind,
    /// 列出 loop 附件。
    ListLoop,
    /// 能力探测。
    Capabilities,
}

/// 把子命令映射为**经 UDS 发给 `gdd`** 的请求消息。
///
/// ## 只有与 configfs 写入有关的命令能到这里
///
/// `gdd` 只做 mass_storage 挂载，因此只有 `mount`/`unmount`/`delete-slot`/`rebind`
/// 需要过 socket。
/// 其余子命令（创建/删除/导入/loop/能力/配置）由 CLI **就地执行**，不经任何
/// IPC——它们走到这里说明分派逻辑写错了，属于内部错误而非用户错误。
///
/// 这样设计而不是「都发过去、让 gdd 回一个不支持」，是因为后者会让每个
/// 子命令都有一个永远走不到的错误分支（旧实现正是如此），既增加维护面，
/// 也让「这条命令到底走哪条通道」变得难以从代码上看清。
pub fn build_request(command: &Command) -> Result<Message, ClientError> {
    match command {
        Command::Mount { devices, rebind } => {
            if devices.is_empty() {
                return Err(ClientError::MissingArgument(
                    "at least one image path is required",
                ));
            }
            Ok(Message::MountRequest(MountRequest {
                devices: devices
                    .iter()
                    .map(|device| MountDevice {
                        lun: device.lun,
                        image_path: device.image.to_string_lossy().into_owned(),
                        mode: device.mode,
                        inquiry_string: device.inquiry_string.clone(),
                    })
                    .collect(),
                rebind: *rebind,
            }))
        }
        Command::Rebind => Ok(Message::RebindRequest(
            gadgetdisk_proto::RebindRequest::default(),
        )),
        Command::Unmount { lun } => Ok(Message::UnmountRequest(UnmountRequest { lun: *lun })),
        Command::DeleteSlot { lun } => Ok(Message::DeleteSlotRequest(
            gadgetdisk_proto::DeleteSlotRequest { lun: *lun },
        )),
        other => Err(ClientError::NotOverSocket(format!("{other:?}"))),
    }
}

pub use connection::Connection;

mod connection {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};

    use gadgetdisk_proto::{
        Message, PROTOCOL_VERSION, read_frame, read_handshake, write_frame, write_handshake,
    };

    use super::ClientError;

    /// 与 gdd 的一次性连接。
    pub struct Connection {
        stream: UnixStream,
        path: PathBuf,
    }

    impl Connection {
        /// 连接到 `path`。
        pub fn connect(path: &Path) -> Result<Self, ClientError> {
            let stream = UnixStream::connect(path).map_err(|source| ClientError::Connect {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(Self {
                stream,
                path: path.to_path_buf(),
            })
        }

        /// socket 路径。
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// 握手；版本不匹配时返回明确错误。
        pub fn handshake(&mut self) -> Result<(), ClientError> {
            write_handshake(&mut self.stream, true)
                .map_err(|err| ClientError::Protocol(err.to_string()))?;

            match read_handshake(&mut self.stream) {
                Ok(Some(PROTOCOL_VERSION)) => Ok(()),
                Ok(Some(other)) => Err(ClientError::Protocol(format!(
                    "protocol version mismatch: gdd is {other}, local is {PROTOCOL_VERSION}. \
                     Make sure the CLI and gdd come from the same install."
                ))),
                Ok(None) => Err(ClientError::Protocol(
                    "gdd reports an unsupported protocol version; update the module.".into(),
                )),
                Err(err) => Err(ClientError::Protocol(err.to_string())),
            }
        }

        /// 发送请求并读取应答。
        pub fn request(&mut self, request: &Message) -> Result<Message, ClientError> {
            let payload = request
                .encode_payload()
                .map_err(|err| ClientError::Protocol(err.to_string()))?;
            write_frame(&mut self.stream, request.id(), &payload)
                .map_err(|err| ClientError::Protocol(err.to_string()))?;

            let (id, payload) = read_frame(&mut self.stream)
                .map_err(|err| ClientError::Protocol(err.to_string()))?;

            let message = Message::decode(id, &payload)
                .ok_or_else(|| {
                    ClientError::Protocol(format!("gdd returned an unknown message id: {id:#04X}"))
                })?
                .map_err(|err| ClientError::Protocol(err.to_string()))?;

            // gdd 的错误响应转成 CLI 错误，使退出码非零。
            if let Message::Error(ref err) = message {
                return Err(ClientError::Server {
                    // 原样传递枚举，不再经字符串来回转换。
                    code: err.code,
                    message: err.message.clone(),
                });
            }

            Ok(message)
        }
    }

    impl Read for Connection {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for Connection {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.stream.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.stream.flush()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gadgetdisk_proto::ErrorCode;

    #[test]
    fn layout_names_parse_case_insensitively() {
        assert_eq!(parse_layout("raw").unwrap(), ProtoLayout::Raw);
        assert_eq!(parse_layout("GPT").unwrap(), ProtoLayout::Gpt);
        assert_eq!(parse_layout("Mbr").unwrap(), ProtoLayout::Mbr);
        assert!(matches!(
            parse_layout("exfat"),
            Err(ClientError::UnknownLayout(_))
        ));
    }

    #[test]
    fn mode_names_parse_case_insensitively() {
        assert_eq!(parse_mode("rw").unwrap(), ProtoMode::Rw);
        assert_eq!(parse_mode("RO").unwrap(), ProtoMode::Ro);
        assert_eq!(parse_mode("CdRom").unwrap(), ProtoMode::Cdrom);
        assert!(matches!(
            parse_mode("write"),
            Err(ClientError::UnknownMode(_))
        ));
    }

    #[test]
    fn mount_maps_to_a_device_list() {
        let message = build_request(&Command::Mount {
            devices: vec![
                CommandDevice {
                    image: PathBuf::from("/data/adb/gadget-disk/images/a.img"),
                    mode: ProtoMode::Cdrom,
                    lun: Some(0),
                    inquiry_string: Some("ISO".into()),
                },
                CommandDevice {
                    image: PathBuf::from("/data/adb/gadget-disk/images/b.img"),
                    mode: ProtoMode::Rw,
                    lun: None,
                    inquiry_string: None,
                },
            ],
            rebind: true,
        })
        .unwrap();

        match message {
            Message::MountRequest(request) => {
                assert_eq!(request.devices.len(), 2);
                assert_eq!(request.devices[0].lun, Some(0));
                assert_eq!(request.devices[0].mode, ProtoMode::Cdrom);
                assert_eq!(request.devices[0].inquiry_string.as_deref(), Some("ISO"));
                // 未指定 LUN 时保持 `None`，由 `gdd` 分配。
                assert_eq!(request.devices[1].lun, None);
                assert!(request.rebind);
            }
            other => panic!("期望 MountRequest，得到 {other:?}"),
        }
    }

    #[test]
    fn mount_without_devices_is_a_usage_error() {
        let err = build_request(&Command::Mount {
            devices: Vec::new(),
            rebind: false,
        })
        .unwrap_err();
        assert!(matches!(err, ClientError::MissingArgument(_)));
    }

    #[test]
    fn unmount_without_lun_means_all() {
        match build_request(&Command::Unmount { lun: None }).unwrap() {
            Message::UnmountRequest(request) => assert_eq!(request.lun, None),
            other => panic!("期望 UnmountRequest，得到 {other:?}"),
        }
        match build_request(&Command::Unmount { lun: Some(1) }).unwrap() {
            Message::UnmountRequest(request) => assert_eq!(request.lun, Some(1)),
            other => panic!("期望 UnmountRequest，得到 {other:?}"),
        }
    }

    #[test]
    fn delete_slot_maps_to_a_delete_slot_request() {
        match build_request(&Command::DeleteSlot { lun: 2 }).unwrap() {
            Message::DeleteSlotRequest(request) => assert_eq!(request.lun, 2),
            other => panic!("期望 DeleteSlotRequest，得到 {other:?}"),
        }
    }

    /// **只有**改 configfs 的命令会经 socket。其余命令必须被明确拒绝，
    /// 且错误要指出这是分派 bug 而不是用户的用法问题。
    #[test]
    fn only_configfs_writes_go_over_the_socket() {
        let locally_executed = vec![
            Command::Status,
            Command::ListImages,
            Command::ListLoop,
            Command::Capabilities,
            Command::Delete {
                path: PathBuf::from("/x.img"),
            },
            Command::Create {
                path: PathBuf::from("x.img"),
                size_bytes: gadgetdisk_core::MIN_FAT32_BYTES,
                layout: ProtoLayout::Raw,
                filesystem: "fat32".into(),
                label: "X".into(),
            },
            Command::AttachLoop {
                image: PathBuf::from("/x.img"),
                mode: "rw".into(),
                partition_index: None,
                read_only: false,
            },
            Command::DetachLoop {
                image: Some(PathBuf::from("/x.img")),
                loop_dev: None,
            },
        ];
        for command in locally_executed {
            let err = build_request(&command).unwrap_err();
            match err {
                // 显示文案由 `#[error]` 模板负责，这里只关心变体正确。
                ClientError::NotOverSocket(_) => {}
                other => panic!("{command:?} 不应经 socket，得到 {other:?}"),
            }
        }
    }

    #[test]
    fn error_code_strings_match_protocol() {
        // 错误码字符串是**对外契约**（WebUI 据此映射文案），不得随意改名。
        // 唯一归属是 `ErrorCode::as_str()`；这里钉住几个关键值。
        assert_eq!(ErrorCode::Busy.as_str(), "busy");
        assert_eq!(ErrorCode::ImageNotFound.as_str(), "image_not_found");
        assert_eq!(ErrorCode::ImageInUse.as_str(), "image_in_use");
        assert_eq!(ErrorCode::SizeBelowMinimum.as_str(), "size_below_minimum");
        assert_eq!(ErrorCode::PermissionDenied.as_str(), "permission_denied");
    }

    #[test]
    fn server_error_keeps_the_typed_code() {
        // 回归：错误码必须保持为枚举。曾在 `ClientError::Server` 里转成
        // 字符串、调用方再用一张表转回，新增错误码时会静默退化为 Internal。
        let err = ClientError::Server {
            code: ErrorCode::ImageInUse,
            message: "占用了".into(),
        };
        match err {
            ClientError::Server { code, .. } => assert_eq!(code, ErrorCode::ImageInUse),
            other => panic!("期望 Server，得到 {other:?}"),
        }
    }
}
