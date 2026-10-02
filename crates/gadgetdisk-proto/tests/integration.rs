//! `gadgetdisk-proto` 集成测试：覆盖 [docs/protocol.md](../../../../docs/protocol.md)
//! 消息表中的**每一条**消息。
//!
//! 单个入口（`tests/integration.rs`），内部以 `mod` 划分主题。
//!
//! ## 覆盖面已随协议收缩
//!
//! 协议只服务 **CLI ↔ `gdd`**，而 `gdd` 只做 mass_storage 挂载，因此消息表
//! 从 25 条收缩为 9 条（`Error` + 4 组请求/应答 + `Rebind`）。镜像管理、
//! 导入、loop、能力探测这些操作由 CLI 就地执行，**不经过任何 IPC**，它们的
//! 结构体（`ImageInfo`/`Attachment`/`Capabilities`/`JobStatus`）仍作为 REST
//! 应答体存在，但不再是 socket 消息。
//!
//! 这里同时钉住这一点：`all_messages()` 必须与 `docs/protocol.md` 的消息表
//! 逐条对应，因此新增/删除 socket 消息时本文件会失败提醒。

use gadgetdisk_proto::MAX_FRAME_BYTES;
use gadgetdisk_proto::message::*;

/// 构造一条覆盖所有字段非默认值的示例 LUN。
fn sample_lun() -> LunInfo {
    LunInfo {
        index: 1,
        image_path: "/data/adb/gadget-disk/images/a.img".into(),
        size_bytes: 4 * 1024 * 1024 * 1024,
        mode: Mode::Cdrom,
        inquiry_string: Some("GD TEST".into()),
        attached: true,
        effective: false,
        // lun.1 可删；lun.0 不可（内核 EPERM）。
        deletable: true,
    }
}

/// REST 仍会序列化这些结构体，因此它们必须可往返。
fn sample_image() -> ImageInfo {
    ImageInfo {
        path: "/data/adb/gadget-disk/images/a.img".into(),
        size_bytes: 64 * 1024 * 1024,
        mtime: 1_760_000_000,
        layout: ImageLayout::Gpt,
        partition_offset_bytes: Some(1024 * 1024),
        in_use: ImageState::Gadget,
    }
}

fn sample_attachment() -> Attachment {
    Attachment {
        image: "/data/adb/gadget-disk/images/a.img".into(),
        loop_dev: "/dev/block/loop52".into(),
        loop_part_devs: vec!["/dev/block/loop52p1".into()],
        mountpoint: "/data/adb/gadget-disk/mnt/a".into(),
        read_only: false,
    }
}

fn sample_capabilities() -> Capabilities {
    Capabilities {
        loop_control: true,
        max_part: 7,
        filesystems: vec!["vfat".into(), "exfat".into()],
        mass_storage_supported: true,
        selinux_enforcing: true,
    }
}

fn sample_job() -> JobStatus {
    JobStatus {
        state: JobState::Running,
        bytes_done: 1024,
        bytes_total: 4096,
        error: None,
    }
}

/// 协议里的全部 11 条消息。
fn all_messages() -> Vec<Message> {
    vec![
        Message::Error(ErrorResponse {
            code: ErrorCode::Busy,
            message: "another operation is in progress".into(),
        }),
        Message::StatusRequest,
        Message::StatusResponse(StatusResponse {
            udc: Some("dummy_udc.0".into()),
            devices: vec![sample_lun()],
        }),
        Message::MountRequest(MountRequest {
            devices: vec![
                MountDevice {
                    lun: Some(0),
                    image_path: "/data/adb/gadget-disk/images/a.img".into(),
                    mode: Mode::Ro,
                    inquiry_string: Some("DISK".into()),
                },
                MountDevice {
                    lun: None,
                    image_path: "/data/adb/gadget-disk/images/b.iso".into(),
                    mode: Mode::Cdrom,
                    inquiry_string: None,
                },
            ],
            rebind: true,
        }),
        Message::MountResponse(MountResponse {
            devices: vec![sample_lun()],
        }),
        Message::UnmountRequest(UnmountRequest { lun: Some(1) }),
        Message::UnmountResponse(UnmountResponse {
            released: vec![1],
            devices: Vec::new(),
        }),
        Message::RebindRequest(RebindRequest {}),
        Message::RebindResponse(RebindResponse {
            udc: "dummy_udc.0".into(),
        }),
        Message::DeleteSlotRequest(DeleteSlotRequest { lun: 1 }),
        Message::DeleteSlotResponse(DeleteSlotResponse {
            devices: vec![sample_lun()],
        }),
    ]
}

mod message_ids {
    use super::*;
    use std::collections::BTreeSet;

    /// 消息表里每条消息的 `id` 必须与文档一致，且互不重复。
    #[test]
    fn ids_match_the_documented_table() {
        let expected: &[(u8, &str)] = &[
            (0x01, "ErrorResponse"),
            (0x10, "StatusRequest"),
            (0x11, "StatusResponse"),
            (0x20, "MountRequest"),
            (0x21, "MountResponse"),
            (0x30, "UnmountRequest"),
            (0x31, "UnmountResponse"),
            (0x32, "RebindRequest"),
            (0x33, "RebindResponse"),
            (0x34, "DeleteSlotRequest"),
            (0x35, "DeleteSlotResponse"),
        ];

        let messages = all_messages();
        assert_eq!(
            messages.len(),
            expected.len(),
            "message count disagrees with the documented message table (keep both sides in sync when adding/removing messages)"
        );

        for (message, (id, name)) in messages.iter().zip(expected) {
            assert_eq!(message.id(), *id, "wrong id for {name}");
        }

        // 不得有重复 id。
        let ids: BTreeSet<u8> = messages.iter().map(|m| m.id()).collect();
        assert_eq!(ids.len(), messages.len(), "duplicate message id");
    }

    /// 协议里**不得**再有那些由 CLI 就地执行的操作的消息。
    ///
    /// 这是一条防回退断言：旧的 25 条协议里含 create/delete/import/list/loop/
    /// capabilities，`gdd` 已不再实现它们，保留变体是纯负债（每个都带一个
    /// 永远走不到的错误分支）。
    #[test]
    fn locally_executed_operations_have_no_messages() {
        let ids: BTreeSet<u8> = all_messages().iter().map(|m| m.id()).collect();
        for removed in [
            0x40u8, 0x41, 0x50, 0x51, 0x60, 0x61, 0x80, 0x81, 0x82, 0x83, 0xA0, 0xA1, 0xA2, 0xA3,
            0xA4, 0xA5, 0xB0, 0xB1,
        ] {
            assert!(
                !ids.contains(&removed),
                "id {removed:#04X} belongs to an operation the CLI runs in place; it must not appear in the protocol"
            );
            assert!(
                Message::decode(removed, b"{}").is_none(),
                "id {removed:#04X} must not decode"
            );
        }
    }

    #[test]
    fn unknown_ids_decode_to_none() {
        for id in [0x00u8, 0x02, 0x0F, 0x36, 0x7F, 0xFE, 0xFF] {
            assert!(Message::decode(id, b"{}").is_none(), "id {id:#04X}");
        }
    }
}

mod round_trip {
    use super::*;

    /// 每条消息经 JSON 编码再解码后必须等价。
    #[test]
    fn every_message_survives_a_round_trip() {
        for message in all_messages() {
            let payload = message.encode_payload().expect("编码");
            let decoded = Message::decode(message.id(), &payload)
                .unwrap_or_else(|| panic!("id {:#04X} 无法解码", message.id()))
                .expect("解码");
            assert_eq!(
                decoded,
                message,
                "id {:#04X} does not round-trip",
                message.id()
            );
        }
    }

    /// 无字段的请求序列化为 `{}`（与文档「所有消息均使用 JSON 负载」一致）。
    #[test]
    fn fieldless_requests_encode_as_empty_object() {
        for message in [
            Message::StatusRequest,
            Message::RebindRequest(RebindRequest {}),
        ] {
            let payload = message.encode_payload().unwrap();
            assert_eq!(
                String::from_utf8(payload).unwrap(),
                "{}",
                "id {:#04X} should encode to {{}}",
                message.id()
            );
        }
    }

    /// 帧层往返（`id` + 长度 + 负载）。
    ///
    /// `Message` 本身**不**实现 `Serialize`（它是「id + 载荷」的信封，线格式
    /// 由 `encode_payload` 决定），因此这里用「编码载荷 → `write_frame`」与
    /// 「`read_frame` → `decode`」两条路径，正是生产代码的走法。
    #[test]
    fn frames_round_trip_through_a_buffer() {
        use gadgetdisk_proto::{read_frame, write_frame};

        let mut buffer = Vec::new();
        for message in all_messages() {
            let payload = message.encode_payload().expect("编码载荷");
            write_frame(&mut buffer, message.id(), &payload).expect("写帧");
        }

        let mut cursor = std::io::Cursor::new(buffer);
        for expected in all_messages() {
            let (id, payload) =
                read_frame(&mut cursor).unwrap_or_else(|err| panic!("读帧失败：{err}"));
            let decoded = Message::decode(id, &payload)
                .expect("id 应已知")
                .expect("解码");
            assert_eq!(decoded, expected);
        }
    }
}

mod wire_format {
    use super::*;

    /// 字段名是**对外契约**（另一端的解码依据），不得随意改名。
    #[test]
    fn field_names_match_the_documented_payloads() {
        let mount = Message::MountRequest(MountRequest {
            devices: vec![MountDevice {
                lun: Some(0),
                image_path: "/x.img".into(),
                mode: Mode::Ro,
                inquiry_string: Some("INQ".into()),
            }],
            rebind: false,
        });
        let value: serde_json::Value =
            serde_json::from_slice(&mount.encode_payload().unwrap()).unwrap();
        assert_eq!(value["devices"][0]["image_path"], "/x.img");
        assert_eq!(value["devices"][0]["lun"], 0);
        assert_eq!(value["devices"][0]["mode"], "ro");
        assert_eq!(value["devices"][0]["inquiry_string"], "INQ");
        assert_eq!(value["rebind"], false);

        let status = Message::StatusResponse(StatusResponse {
            udc: Some("u".into()),
            devices: vec![sample_lun()],
        });
        let value: serde_json::Value =
            serde_json::from_slice(&status.encode_payload().unwrap()).unwrap();
        assert_eq!(value["udc"], "u");
        assert_eq!(value["devices"][0]["index"], 1);
        assert_eq!(value["devices"][0]["attached"], true);
        assert_eq!(value["devices"][0]["effective"], false);
        assert_eq!(value["devices"][0]["inquiry_string"], "GD TEST");
        // 槽位可删性必须出现在线上，否则 UI 只能自己复制「0 号特殊」这条内核知识。
        assert_eq!(value["devices"][0]["deletable"], true);

        let delete = Message::DeleteSlotRequest(DeleteSlotRequest { lun: 2 });
        let value: serde_json::Value =
            serde_json::from_slice(&delete.encode_payload().unwrap()).unwrap();
        assert_eq!(value["lun"], 2);
    }

    /// 可选字段为 `None` 时**不得**出现在 JSON 里（减小负载，且让「未设置」
    /// 与「设置为空」在线上可区分）。
    #[test]
    fn optional_fields_are_omitted_when_none() {
        let message = Message::MountRequest(MountRequest {
            devices: vec![MountDevice {
                lun: None,
                image_path: "/x.img".into(),
                mode: Mode::Rw,
                inquiry_string: None,
            }],
            rebind: false,
        });
        let text = String::from_utf8(message.encode_payload().unwrap()).unwrap();
        assert!(
            !text.contains("lun"),
            "the lun field must be absent when no LUN was specified"
        );
        assert!(
            !text.contains("inquiry_string"),
            "that field must be absent when INQUIRY is not set"
        );
    }

    /// 声明超长的帧必须在**分配内存之前**被拒绝。
    #[test]
    fn oversized_declared_length_is_rejected() {
        let mut buffer = Vec::new();
        // 手工构造一个声明长度超限的帧头。
        buffer.push(0x10u8);
        buffer.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());

        let mut cursor = std::io::Cursor::new(buffer);
        let err = gadgetdisk_proto::read_frame(&mut cursor).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("exceeds") || text.contains("too long") || text.contains("max"),
            "the error message should mention the length limit, got {text}"
        );
    }
}

mod defaults {
    use super::*;

    /// `Mode` 的默认值是可读写 —— 与文档「默认」列一致。
    #[test]
    fn mode_defaults_to_read_write() {
        assert_eq!(Mode::default(), Mode::Rw);
        assert_eq!(Mode::Rw.as_str(), "rw");
        assert_eq!(Mode::Ro.as_str(), "ro");
        assert_eq!(Mode::Cdrom.as_str(), "cdrom");
    }

    /// `UnmountRequest` 缺省表示**全部弹出并拆除**（语义见文档）。
    #[test]
    fn unmount_defaults_to_everything() {
        let request = UnmountRequest::default();
        assert_eq!(request.lun, None);
    }

    /// REST 应答体的结构体仍须可往返（它们不再走 socket，但仍是 WebUI 的契约）。
    #[test]
    fn rest_payload_structs_round_trip() {
        let image = serde_json::to_string(&sample_image()).unwrap();
        let back: ImageInfo = serde_json::from_str(&image).unwrap();
        assert_eq!(back, sample_image());

        let attachment = serde_json::to_string(&sample_attachment()).unwrap();
        let back: Attachment = serde_json::from_str(&attachment).unwrap();
        assert_eq!(back, sample_attachment());

        let caps = serde_json::to_string(&sample_capabilities()).unwrap();
        let back: Capabilities = serde_json::from_str(&caps).unwrap();
        assert_eq!(back, sample_capabilities());
        // 已删除的字段不得再出现。
        assert!(!caps.contains("gadget_hal_present"));

        let job = serde_json::to_string(&sample_job()).unwrap();
        let back: JobStatus = serde_json::from_str(&job).unwrap();
        assert_eq!(back, sample_job());
    }

    /// 错误码字符串是**对外契约**（WebUI 据此映射文案），不得改名。
    #[test]
    fn error_code_strings_are_stable() {
        let cases: &[(ErrorCode, &str)] = &[
            (ErrorCode::Busy, "busy"),
            (ErrorCode::ImageNotFound, "image_not_found"),
            (ErrorCode::ImageInUse, "image_in_use"),
            (ErrorCode::NotRegularFile, "not_regular_file"),
            (ErrorCode::UnsupportedLayout, "unsupported_layout"),
            (ErrorCode::NoUdc, "no_udc"),
            (
                ErrorCode::MassStorageUnsupported,
                "mass_storage_unsupported",
            ),
            (ErrorCode::LoopUnsupported, "loop_unsupported"),
            (ErrorCode::FilesystemUnsupported, "filesystem_unsupported"),
            (ErrorCode::SizeBelowMinimum, "size_below_minimum"),
            (ErrorCode::NoSpace, "no_space"),
            (ErrorCode::PermissionDenied, "permission_denied"),
            (ErrorCode::InvalidArgument, "invalid_argument"),
            (ErrorCode::ConfigfsUnavailable, "configfs_unavailable"),
            (ErrorCode::NotActive, "not_active"),
            (ErrorCode::Internal, "internal"),
        ];
        for (code, text) in cases {
            assert_eq!(
                code.as_str(),
                *text,
                "wrong wire-format string for {code:?}"
            );
        }
    }
}
