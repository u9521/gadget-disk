//! 帧编解码与版本握手。
//!
//! 帧格式（[docs/protocol.md](../../../docs/protocol.md)）：
//!
//! ```text
//! +--------+------------------+---------------------+
//! | u8 id  | u32 LE length    | length 字节 UTF-8 JSON |
//! +--------+------------------+---------------------+
//! ```
//!
//! `length` 上限为 [`MAX_FRAME_BYTES`]；实现必须校验并拒绝异常大的声明长度，
//! 避免内存放大（例如声明 4 GiB 后只发 1 字节）。

use std::io::{Read, Write};

/// 协议版本。
pub const PROTOCOL_VERSION: u8 = 1;

/// 单帧负载上限：1 MiB。
///
/// 依据：本模块所有负载都是小型状态描述（镜像列表、LUN 状态、job 进度），
/// 1 MiB 已有两个数量级的余量；上限存在的目的是让「声明超大长度」的攻击
/// 在读取前就被拒绝，而不是先分配内存。
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// 帧编解码错误。
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// 底层 I/O 失败。
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// 声明的负载长度超过 [`MAX_FRAME_BYTES`]。
    #[error("frame length {declared} exceeds limit {limit}")]
    LengthTooLarge {
        /// 对端声明的长度。
        declared: u32,
        /// 允许的上限。
        limit: u32,
    },

    /// 对端在帧中途关闭了连接。
    #[error("incomplete frame: expected {expected} bytes, got {actual}")]
    Truncated {
        /// 期望字节数。
        expected: usize,
        /// 实际读取字节数。
        actual: usize,
    },

    /// 负载不是合法 UTF-8。
    #[error("payload is not valid UTF-8: {0}")]
    NotUtf8(#[from] std::str::Utf8Error),

    /// 负载不是合法 JSON。
    #[error("payload is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// 协议版本不受支持。
    #[error("unsupported protocol version: peer {peer}, local {local}")]
    VersionMismatch {
        /// 对端版本。
        peer: u8,
        /// 本端版本。
        local: u8,
    },
}

/// 本 crate 的帧层结果类型。
pub type Result<T> = std::result::Result<T, FrameError>;

/// 写握手：client 发送自己的版本，gdd 回 `1`（支持）或 `0`（不支持）。
pub fn write_handshake<W: Write>(writer: &mut W, supported: bool) -> Result<()> {
    let byte = if supported { PROTOCOL_VERSION } else { 0 };
    writer.write_all(&[byte])?;
    writer.flush()?;
    Ok(())
}

/// 读握手字节。
///
/// 返回值 `Some(version)` 表示对端声明的版本；`None` 表示对端声明不支持。
pub fn read_handshake<R: Read>(reader: &mut R) -> Result<Option<u8>> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(if byte[0] == 0 { None } else { Some(byte[0]) })
}

/// 写一帧。
pub fn write_frame<W: Write>(writer: &mut W, id: u8, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::LengthTooLarge {
        declared: u32::MAX,
        limit: MAX_FRAME_BYTES,
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::LengthTooLarge {
            declared: len,
            limit: MAX_FRAME_BYTES,
        });
    }

    writer.write_all(&[id])?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// 写一帧 JSON 消息。
pub fn write_json<W: Write, T: serde::Serialize>(writer: &mut W, id: u8, value: &T) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    write_frame(writer, id, &payload)
}

/// 读一帧，返回 `(id, payload)`。
///
/// **长度校验在分配之前完成**：先读 5 字节头，确认 `length <= MAX_FRAME_BYTES`
/// 后才按该长度分配缓冲区。
pub fn read_frame<R: Read>(reader: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    read_exact_counted(reader, &mut header)?;

    let id = header[0];
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::LengthTooLarge {
            declared: len,
            limit: MAX_FRAME_BYTES,
        });
    }

    let mut payload = vec![0u8; len as usize];
    read_exact_counted(reader, &mut payload)?;
    Ok((id, payload))
}

/// 读一帧并解析为 JSON。
pub fn read_json<R: Read, T: serde::de::DeserializeOwned>(reader: &mut R) -> Result<(u8, T)> {
    let (id, payload) = read_frame(reader)?;
    let text = std::str::from_utf8(&payload)?;
    let value = serde_json::from_str(text)?;
    Ok((id, value))
}

/// `read_exact` 的包装：把「帧中途 EOF」区分为 [`FrameError::Truncated`]，
/// 便于 CLI 诊断，而不是笼统报 `UnexpectedEof`。
fn read_exact_counted<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<()> {
    let expected = buf.len();
    let mut filled = 0usize;
    while filled < expected {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(FrameError::Truncated {
                    expected,
                    actual: filled,
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ErrorCode;

    #[test]
    fn frame_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x10, b"{\"a\":1}").unwrap();

        assert_eq!(buf[0], 0x10);
        assert_eq!(u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]), 7);
        assert_eq!(&buf[5..], b"{\"a\":1}");

        let mut cursor = std::io::Cursor::new(buf);
        let (id, payload) = read_frame(&mut cursor).unwrap();
        assert_eq!(id, 0x10);
        assert_eq!(payload, b"{\"a\":1}");
    }

    #[test]
    fn json_round_trip() {
        let mut buf = Vec::new();
        let value = crate::message::ErrorResponse {
            code: ErrorCode::Busy,
            message: "另一操作正在进行".into(),
        };
        write_json(&mut buf, 0x01, &value).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (id, decoded): (u8, crate::message::ErrorResponse) = read_json(&mut cursor).unwrap();
        assert_eq!(id, 0x01);
        assert_eq!(decoded, value);
    }

    #[test]
    fn rejects_oversized_declared_length_without_allocating() {
        // 声明 4 GiB，但只提供 5 字节头。必须在分配前拒绝。
        let mut header = vec![0x11u8];
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(header);

        let err = read_frame(&mut cursor).unwrap_err();
        match err {
            FrameError::LengthTooLarge { declared, limit } => {
                assert_eq!(declared, u32::MAX);
                assert_eq!(limit, MAX_FRAME_BYTES);
            }
            other => panic!("期望 LengthTooLarge，得到 {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_payload_on_write() {
        let payload = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        let mut sink = Vec::new();
        let err = write_frame(&mut sink, 0x11, &payload).unwrap_err();
        assert!(matches!(err, FrameError::LengthTooLarge { .. }));
        assert!(sink.is_empty(), "拒绝时不得写出任何字节");
    }

    #[test]
    fn accepts_payload_exactly_at_limit() {
        let payload = vec![0xABu8; MAX_FRAME_BYTES as usize];
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x11, &payload).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (id, decoded) = read_frame(&mut cursor).unwrap();
        assert_eq!(id, 0x11);
        assert_eq!(decoded.len(), MAX_FRAME_BYTES as usize);
    }

    #[test]
    fn detects_truncated_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x11, b"0123456789").unwrap();
        buf.truncate(buf.len() - 3); // 去掉尾部 3 字节

        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        match err {
            FrameError::Truncated { expected, actual } => {
                assert_eq!(expected, 10);
                assert_eq!(actual, 7);
            }
            other => panic!("期望 Truncated，得到 {other:?}"),
        }
    }

    #[test]
    fn detects_truncated_header() {
        let mut cursor = std::io::Cursor::new(vec![0x11u8, 0x00]);
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(
            err,
            FrameError::Truncated {
                expected: 5,
                actual: 2
            }
        ));
    }

    #[test]
    fn rejects_eof_at_header_start() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(
            err,
            FrameError::Truncated {
                expected: 5,
                actual: 0
            }
        ));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x11, &[0xFF, 0xFE, 0xFD]).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_json::<_, serde_json::Value>(&mut cursor).unwrap_err();
        assert!(matches!(err, FrameError::NotUtf8(_)));
    }

    #[test]
    fn rejects_invalid_json() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x11, b"{not json}").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_json::<_, serde_json::Value>(&mut cursor).unwrap_err();
        assert!(matches!(err, FrameError::Json(_)));
    }

    #[test]
    fn handshake_supported_round_trip() {
        let mut buf = Vec::new();
        write_handshake(&mut buf, true).unwrap();
        assert_eq!(buf, vec![PROTOCOL_VERSION]);

        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_handshake(&mut cursor).unwrap(), Some(PROTOCOL_VERSION));
    }

    #[test]
    fn handshake_unsupported_is_zero_byte() {
        let mut buf = Vec::new();
        write_handshake(&mut buf, false).unwrap();
        assert_eq!(buf, vec![0]);

        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_handshake(&mut cursor).unwrap(), None);
    }

    #[test]
    fn frame_id_is_preserved_for_all_values() {
        for id in [0x00u8, 0x01, 0x10, 0x83, 0xA5, 0xB1, 0xFF] {
            let mut buf = Vec::new();
            write_frame(&mut buf, id, b"{}").unwrap();
            let mut cursor = std::io::Cursor::new(buf);
            let (read_id, _) = read_frame(&mut cursor).unwrap();
            assert_eq!(read_id, id);
        }
    }

    #[test]
    fn two_frames_read_back_to_back() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x10, b"{}").unwrap();
        write_frame(&mut buf, 0x11, b"{\"udc\":null}").unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (id1, p1) = read_frame(&mut cursor).unwrap();
        let (id2, p2) = read_frame(&mut cursor).unwrap();
        assert_eq!(id1, 0x10);
        assert_eq!(p1, b"{}");
        assert_eq!(id2, 0x11);
        assert_eq!(p2, b"{\"udc\":null}");
    }
}
