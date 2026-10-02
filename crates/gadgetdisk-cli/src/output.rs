//! JSON 输出与退出码。
//!
//! 契约见 [docs/protocol.md](../../../../docs/protocol.md) 的「CLI 契约」：
//!
//! - **成功**：stdout 输出**单个 JSON 对象**，退出码 `0`；
//! - **失败**：stdout（或 stderr）输出含 `error` 字段的 JSON，退出码非零；
//! - WebUI 必须容忍 stdout 尾随换行与可能混入的额外空白。
//!
//! 本模块只负责把消息编码为 JSON 文本，不做 IO，因此完全可主机测试。

use gadgetdisk_proto::{ErrorCode, Message};

/// 进程退出码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// 成功。
    Success,
    /// 用法错误（参数非法、容量越界等）。
    Usage,
    /// 无法与 `gdd` 通信。
    Unreachable,
    /// `gdd` 返回了错误响应。
    Server,
}

impl ExitCode {
    /// 数值退出码。
    pub const fn as_i32(self) -> i32 {
        match self {
            ExitCode::Success => 0,
            ExitCode::Usage => 2,
            ExitCode::Unreachable => 3,
            ExitCode::Server => 4,
        }
    }

    /// 供 `std::process::exit` 使用的 `u8` 退出码。
    ///
    /// 取值范围在 [`Self::as_i32`] 中固定为 0/2/3/4，转 `u8` 必然成功；
    /// 用 `expect` 而不是 `as` 是为了不引入 `clippy::cast_possible_truncation`
    /// 告警，同时把「不可能失败」写成可执行的断言。
    pub fn as_u8(self) -> u8 {
        u8::try_from(self.as_i32()).expect("exit codes are fixed at 0/2/3/4, always within u8")
    }
}

/// 一个 JSON 输出（文本 + 应使用的 stdout/stderr 与退出码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonOutput {
    /// JSON 文本（不含尾随换行）。
    pub text: String,
    /// 是否应写到 stderr（错误走 stderr 便于脚本区分）。
    pub to_stderr: bool,
    /// 建议的退出码。
    pub exit_code: ExitCode,
}

impl JsonOutput {
    /// 输出文本加尾随换行。
    ///
    /// WebUI 会 `trim()`，但脚本期望一个完整行；两者兼得的方式是总是补一个 `\n`。
    pub fn line(&self) -> String {
        format!("{}\n", self.text)
    }
}

/// 把成功应答编码为 JSON。
///
/// 对 `Message` 直接序列化其载荷（与线上格式一致），使 WebUI 能直接消费。
pub fn success(message: &Message) -> Result<JsonOutput, serde_json::Error> {
    let payload = message.encode_payload()?;
    // 重新格式化为紧凑文本；`encode_payload` 已是紧凑 JSON，这里直接复用。
    let text = String::from_utf8(payload).unwrap_or_else(|_| "{}".to_string());
    Ok(JsonOutput {
        text,
        to_stderr: false,
        exit_code: ExitCode::Success,
    })
}

/// 把一个已经构造好的 JSON 值编码为成功输出。
///
/// 供「不经 gdd 的只读工具」（`ls`/`stat`/`df`）使用。与 [`success`]
/// 共用同一个 [`JsonOutput`] 构造，避免调用点各写一份（曾经因此出现
/// 两处独立的成功输出构造，改一处漏一处）。
pub fn success_value(value: &serde_json::Value) -> JsonOutput {
    JsonOutput {
        text: value.to_string(),
        to_stderr: false,
        exit_code: ExitCode::Success,
    }
}

/// 把错误编码为 JSON。
///
/// 错误对象**总是**含 `error` 字段，其值为稳定错误码字符串，
/// 便于 WebUI 分支处理与文案映射。
pub fn failure(code: ErrorCode, message: impl AsRef<str>) -> JsonOutput {
    let value = serde_json::json!({
        "error": code.as_str(),
        "message": message.as_ref(),
    });
    JsonOutput {
        text: value.to_string(),
        to_stderr: true,
        exit_code: match code {
            ErrorCode::InvalidArgument | ErrorCode::SizeBelowMinimum => ExitCode::Usage,
            _ => ExitCode::Server,
        },
    }
}

/// 未能与 `gdd` 通信时的 JSON。
///
/// 错误码 `gdd_unreachable`：CLI 连不上 `gdd` 的 socket。
///
/// 名字必须指向**实际存在的东西**（`gdd`），否则排查时会在进程表里找一个
/// 不存在的 `gdd`。WebUI 的离线状态机以这个字符串为判据，两端需同时改。
pub fn unreachable(path: &str, reason: &str) -> JsonOutput {
    let value = serde_json::json!({
        "error": "gdd_unreachable",
        "message": format!("cannot connect to gdd ({path}): {reason}"),
        "path": path,
    });
    JsonOutput {
        text: value.to_string(),
        to_stderr: true,
        exit_code: ExitCode::Unreachable,
    }
}

/// 用法错误（参数本身非法，未发出任何请求）。
pub fn usage(message: impl AsRef<str>) -> JsonOutput {
    let value = serde_json::json!({
        "error": "invalid_argument",
        "message": message.as_ref(),
    });
    JsonOutput {
        text: value.to_string(),
        to_stderr: true,
        exit_code: ExitCode::Usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gadgetdisk_proto::{LunInfo, Mode, StatusResponse};

    fn sample_lun() -> LunInfo {
        LunInfo {
            index: 0,
            image_path: "/x.img".into(),
            size_bytes: 1024,
            mode: Mode::Rw,
            inquiry_string: Some("GD".into()),
            attached: true,
            effective: true,
            deletable: true,
        }
    }

    #[test]
    fn success_emits_single_json_object() {
        let message = Message::StatusResponse(StatusResponse {
            udc: Some("dummy_udc.0".into()),
            devices: vec![sample_lun()],
        });

        let output = success(&message).unwrap();
        assert!(!output.to_stderr);
        assert_eq!(output.exit_code, ExitCode::Success);
        assert_eq!(output.exit_code.as_i32(), 0);

        // 必须是单个 JSON 对象。
        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["udc"], "dummy_udc.0");
        assert_eq!(parsed["devices"][0]["image_path"], "/x.img");
        // 多 LUN 的新字段必须出现在输出里，否则 UI 无从显示它。
        assert_eq!(parsed["devices"][0]["inquiry_string"], "GD");

        // 行输出只多一个换行。
        assert_eq!(output.line(), format!("{}\n", output.text));
        assert!(output.line().ends_with('\n'));
    }

    #[test]
    fn failure_always_has_error_field() {
        let output = failure(ErrorCode::Busy, "另一操作正在进行");
        assert!(output.to_stderr);
        assert_eq!(output.exit_code, ExitCode::Server);
        assert_ne!(output.exit_code.as_i32(), 0);

        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(parsed["error"], "busy");
        assert_eq!(parsed["message"], "另一操作正在进行");
    }

    #[test]
    fn usage_errors_exit_with_two() {
        let output = failure(ErrorCode::InvalidArgument, "参数非法");
        assert_eq!(output.exit_code, ExitCode::Usage);
        assert_eq!(output.exit_code.as_i32(), 2);
    }

    #[test]
    fn size_below_minimum_is_a_usage_error() {
        let output = failure(ErrorCode::SizeBelowMinimum, "太小");
        assert_eq!(output.exit_code, ExitCode::Usage);
        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(parsed["error"], "size_below_minimum");
    }

    #[test]
    fn unreachable_keeps_the_documented_error_code() {
        // 错误码是 WebUI 离线状态机的判据，**不得**随进程改名而变。
        let output = unreachable("/data/adb/gadget-disk/run/gdd.sock", "No such file");
        assert!(output.to_stderr);
        assert_eq!(output.exit_code, ExitCode::Unreachable);
        assert_eq!(output.exit_code.as_i32(), 3);

        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(parsed["error"], "gdd_unreachable");
        assert_eq!(parsed["path"], "/data/adb/gadget-disk/run/gdd.sock");
    }

    #[test]
    fn usage_helper_emits_parseable_json() {
        let output = usage("未知布局：foo");
        assert!(output.to_stderr);
        assert_eq!(output.exit_code, ExitCode::Usage);
        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(parsed["error"], "invalid_argument");
    }

    #[test]
    fn every_socket_message_encodes_as_an_object() {
        // socket 通道上的每一种应答都必须是可解析的 JSON **对象**。
        let messages = vec![
            Message::StatusResponse(StatusResponse {
                udc: None,
                devices: Vec::new(),
            }),
            Message::UnmountResponse(gadgetdisk_proto::UnmountResponse {
                released: vec![0, 1],
                devices: vec![sample_lun()],
            }),
            Message::MountResponse(gadgetdisk_proto::MountResponse {
                devices: vec![sample_lun()],
            }),
            Message::RebindResponse(gadgetdisk_proto::RebindResponse {
                udc: "dummy_udc.0".into(),
            }),
        ];

        for message in messages {
            let output = success(&message).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
            assert!(
                parsed.is_object(),
                "id {:#04X} 的输出不是 JSON 对象：{}",
                message.id(),
                output.text
            );
        }
    }

    /// 就地执行的命令用 `success_value`：它必须与 socket 通道输出同构。
    #[test]
    fn success_value_emits_a_compact_json_object() {
        let value = serde_json::json!({ "images": [], "count": 0 });
        let output = success_value(&value);
        assert!(!output.to_stderr);
        assert_eq!(output.exit_code, ExitCode::Success);
        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["count"], 0);
    }
}
