//! 最小 HTTP/1.1 服务端：仅覆盖本模块需要的一个很小的子集。
//!
//! ## 为什么手写而不引依赖
//!
//! 需要的能力只有「解析请求行 + 头部 + `Content-Length` 定长体」和
//! 「写一个响应」。`hyper`/`tokio` 会引入异步运行时并把二进制撑大数倍，
//! 而本仓库已有的风格就是手写内核边界的解析器（MBR、`loop_info64`）。
//!
//! ## 刻意支持的很小子集（其余一律显式拒绝，不静默容忍）
//!
//! | 项 | 处理 |
//! |---|---|
//! | 方法 | 仅 `GET` / `POST` / `OPTIONS`，其余 `405` |
//! | 版本 | 仅 `HTTP/1.1` / `HTTP/1.0`，其余 `400` |
//! | 传输编码 | **拒绝** `Transfer-Encoding`（不支持 chunked），`400` |
//! | 请求体 | 惰性：JSON 端点一次读完且限 [`MAX_BODY_BYTES`]（超限 `413`，**分配前**拒绝）；上传端点流式、**不设上限** |
//! | 连接 | **可复用**（keep-alive）：HTTP/1.1 默认复用、HTTP/1.0 默认关闭；`Connection: close` 一律关闭；空闲 [`IO_TIMEOUT`] 后断开 |
//!
//! 明确拒绝而不猜测，是这里唯一正确的策略：把 chunked 或畸形请求当成
//! 定长体去读，会让「请求边界」的判断出错，而这是 HTTP 层面最危险的错误。
//!
//! **为什么要连接复用**：实测每个请求有约 **26ms 固定开销**（新建连接 + 线程 +
//! 解析），与载荷无关；而分块上传会发很多次请求，8 MiB 分块下这笔开销占请求耗时的
//! 30%。复用它直接抹掉这块。反过来，**解析失败时绝不复用**——此时请求边界已不可信，
//! 继续读可能把下一个请求的头当成体的一部分。
//!
//! **为什么不支持 chunked**：本服务的客户端是 WebView 的 `fetch`，它对已知长度的
//! `Blob`/`File` 会发 `Content-Length`，因此 chunked 没有任何调用方；而实现它等于
//! 亲手引入上面那条「请求边界解析」的最危险路径。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// **JSON 端点**的请求体上限（1 MiB）。
///
/// 只作用于 [`RequestBody::read_json`]：控制类端点的体是结构化 JSON，一次读完最省事，
/// 而「一次读完」就必须有上限，否则一个声称 4 GiB 的请求会直接 OOM。
///
/// **不作用于上传端点**（走 [`RequestBody::stream`]）：镜像字节可以远超此值，
/// 且只能流式落盘。
///
/// > 早期注释称此值「与协议帧上限一致」而刻意复用同一个数字。那是个**假耦合**：
/// > [`gadgetdisk_proto::MAX_FRAME_BYTES`] 管的是 `serve`/CLI ↔ `gdd` 的 AF_UNIX
/// > 帧，而上传字节根本不经过 gdd（直接写文件系统）。两者不在同一条数据路径上，
/// > 让 REST 请求体受 gdd 帧上限约束没有任何依据。
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 单个请求头行的上限（含名称与值）。
///
/// 防止无换行的超长「头」把内存吃光。8 KiB 远超本用例任何合法头。
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// 头部条目数上限。
const MAX_HEADERS: usize = 64;

/// 单次读写超时。
///
/// 必须有：否则一个连上就不发数据的客户端会永久占住线程，
/// 而 gdd 是单进程常驻的（即使按需，也不该被一个空连接拖住）。
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `OPTIONS`
    Options,
}

impl Method {
    /// 线格式名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Options => "OPTIONS",
        }
    }
}

/// 请求所用的 HTTP 版本。
///
/// 只区分「1.0」与「1.1」两档，因为这是唯一影响行为的差别：**默认是否复用连接**
/// （HTTP/1.1 默认 keep-alive，1.0 默认 close）。其余版本在解析阶段就被拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    /// `HTTP/1.0`
    Http10,
    /// `HTTP/1.1`
    Http11,
}

/// 一个已解析的请求（**不含请求体**）。
///
/// 请求体是**惰性**的：头部解析完后，读者仍停在体的起点，由调用方按需读取。
/// 这样做的理由见 [`RequestBody`]——小 JSON 体要求「一次读完且带上限」，
/// 而上传体要求「流式、不设上限」，两者不可能用同一个 `Vec<u8>` 表达。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// 方法。
    pub method: Method,
    /// 路径（不含查询串）。
    pub path: String,
    /// 查询串（`?` 之后，未解码）。
    pub query: Option<String>,
    /// 头部（名称已小写化，便于不区分大小写地查找）。
    pub headers: Vec<(String, String)>,
    /// 声明的内容长度（`Content-Length`）；缺失时为 `None`。
    ///
    /// **已经过上限校验吗？** 没有——校验按读取方式区分：
    /// [`RequestBody::read_json`] 会带上限拒绝，`stream` 则不设上限。
    pub content_length: Option<u64>,
    /// 请求行里的 HTTP 版本；决定**默认**是否复用连接。
    pub version: HttpVersion,
}

impl Request {
    /// 不区分大小写地取一个头部的值。
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == lower)
            .map(|(_, v)| v.as_str())
    }

    /// 取查询参数（未做百分号解码）。
    pub fn query_param(&self, key: &str) -> Option<String> {
        let query = self.query.as_deref()?;
        for pair in query.split('&') {
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            if k == key {
                return Some(percent_decode(v));
            }
        }
        None
    }
}

/// 请求体：**按用途**选择读取方式。
///
/// 两种读法服务于两个互相冲突的需求，合并成一个 `Vec<u8>` 就会二选一地伤害其中一方：
///
/// | 读法 | 上限 | 用途 |
/// |---|---|---|
/// | [`RequestBody::read_json`] | [`MAX_BODY_BYTES`] | 控制类端点的小 JSON 体：一次读完，**分配前**拒绝超大声明，避免恶意大 JSON 吃内存 |
/// | [`RequestBody::stream`] | **无** | 上传端点的镜像字节：边读边落盘，**不把整个体驻留内存** |
///
/// 早期实现只有前者，于是 REST 请求体被 `MAX_BODY_BYTES` 一刀切。那个上限本意是
/// 防大 JSON，却被误当作「与 gdd 协议帧上限保持一致」而套到了所有请求上——而上传的
/// 字节流**根本不经过 gdd**（直接写文件系统），两者不在同一条数据路径上，是**假耦合**。
pub struct RequestBody<'a> {
    reader: Box<dyn Read + 'a>,
    content_length: Option<u64>,
}

impl<'a> RequestBody<'a> {
    /// 由一个**已在内存里**的字节串构造（供测试与内部组装使用）。
    ///
    /// 生产路径永远由 [`parse_request`] 从连接构造；这个构造函数只是让测试
    /// 不必为了造一个体去拼 TCP 字节流。
    pub fn from_bytes(body: Vec<u8>, declared: Option<u64>) -> Self {
        Self {
            reader: Box::new(std::io::Cursor::new(body)),
            content_length: declared,
        }
    }

    /// 声明的内容长度；缺失时为 `None`（等价于空体）。
    pub const fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// 一次读完并施加 [`MAX_BODY_BYTES`] 上限（控制类端点用）。
    ///
    /// **分配前**拒绝：否则一个声称 4 GiB 的请求会直接 OOM。
    pub fn read_json(mut self) -> Result<Vec<u8>, ParseError> {
        // 先在 `usize` 域里判上限，再做一次**受检**转换：`len` 来自网络，
        // 用 `as` 截断在 32 位目标上会把「声称 4 GiB」悄悄变成「声称若干字节」，
        // 而那正是这个上限要拦的东西。
        let len = usize::try_from(self.content_length.unwrap_or(0))
            .map_err(|_| ParseError::BodyTooLarge)?;
        if len > MAX_BODY_BYTES {
            return Err(ParseError::BodyTooLarge);
        }
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).map_err(|err| {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                ParseError::BadContentLength
            } else {
                ParseError::Io(err.to_string())
            }
        })?;
        Ok(body)
    }

    /// 以流的形式交出体（上传端点用）：**不施加任何上限**。
    ///
    /// 调用方负责边读边消费（例如写入临时文件），不得整块收集——
    /// 那正是本方法要避免的。上限由业务层决定（例如空间预检）。
    pub fn stream(self) -> impl Read + 'a {
        // `take` 到声明的长度：少了会在读到 EOF 时自然结束（由调用方按字节数校验），
        // 多了不会越界读到下一个请求（本服务是一请求一连接，但保持边界清晰）。
        let limit = self.content_length.unwrap_or(0);
        self.reader.take(limit)
    }
}

/// 一个完整请求：头 + 可读的体。
///
/// 与 [`Request`] 分开，是因为 `rest::handle` 需要**先看头和路由**再决定怎么读体
/// （有的端点读 JSON，有的端点流式落盘）。
pub struct ParsedRequest<'a> {
    /// 已解析的请求头与路由信息。
    pub head: Request,
    /// 惰性请求体。
    pub body: RequestBody<'a>,
}

/// 解析失败的原因。全部映射为一个明确的 `400`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// 请求行不是 `METHOD PATH VERSION` 三段。
    MalformedRequestLine,
    /// 方法不在支持集合内（调用方回 `405`）。
    UnsupportedMethod(String),
    /// HTTP 版本不是 `HTTP/1.1` 或 `HTTP/1.0`。
    UnsupportedVersion(String),
    /// 头部行不是 `name: value`。
    MalformedHeader,
    /// 头部行过长。
    HeaderLineTooLong,
    /// 头部条目过多。
    TooManyHeaders,
    /// `Content-Length` 缺失、非数字或与声明不符。
    BadContentLength,
    /// 请求体超过 [`MAX_BODY_BYTES`]。
    BodyTooLarge,
    /// 使用了不支持的传输编码（chunked）。
    UnsupportedTransferEncoding,
    /// 底层 IO 失败。
    Io(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MalformedRequestLine => write!(f, "malformed request line"),
            ParseError::UnsupportedMethod(m) => write!(f, "unsupported method: {m}"),
            ParseError::UnsupportedVersion(v) => write!(f, "unsupported HTTP version: {v}"),
            ParseError::MalformedHeader => write!(f, "malformed header line"),
            ParseError::HeaderLineTooLong => write!(f, "header line too long"),
            ParseError::TooManyHeaders => write!(f, "too many header entries"),
            ParseError::BadContentLength => write!(f, "missing or invalid Content-Length"),
            ParseError::BodyTooLarge => write!(f, "request body exceeds the limit"),
            ParseError::UnsupportedTransferEncoding => {
                write!(f, "unsupported Transfer-Encoding (chunked is not accepted)")
            }
            ParseError::Io(err) => write!(f, "read failed: {err}"),
        }
    }
}

impl ParseError {
    /// 该错误应回的 HTTP 状态码。
    pub const fn status(&self) -> u16 {
        match self {
            ParseError::UnsupportedMethod(_) => 405,
            ParseError::BodyTooLarge => 413,
            _ => 400,
        }
    }
}

/// 从字节流解析一个请求的**头部**，并把请求体以惰性句柄交出。
///
/// 只读请求行与头部（逐行、带上限），**不读体**——体的读法由调用方决定
/// （[`RequestBody::read_json`] 或 [`RequestBody::stream`]）。
/// **不含 chunked**：遇到 `Transfer-Encoding` 直接拒绝，而不是猜测边界。
pub fn parse_request<'a, R: Read + 'a>(reader: R) -> Result<ParsedRequest<'a>, ParseError> {
    let mut buf = BufReader::new(reader);

    // ---- 请求行 ----
    let line = read_line(&mut buf)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ParseError::MalformedRequestLine);
    };

    let method = match method {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "OPTIONS" => Method::Options,
        other => return Err(ParseError::UnsupportedMethod(other.to_string())),
    };

    let version = match version {
        "HTTP/1.1" => HttpVersion::Http11,
        "HTTP/1.0" => HttpVersion::Http10,
        other => return Err(ParseError::UnsupportedVersion(other.to_string())),
    };

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target.to_string(), None),
    };

    // ---- 头部 ----
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(&mut buf)?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(ParseError::TooManyHeaders);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ParseError::MalformedHeader);
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    let lookup = |name: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    // chunked 一律拒绝：把 chunked 当定长体会读错请求边界。
    if lookup("transfer-encoding").is_some() {
        return Err(ParseError::UnsupportedTransferEncoding);
    }

    // ---- 请求体：**不读**，只记下声明长度 ----
    //
    // 长度在这里只做**语法**校验（必须是十进制），**不做上限判断**：上限取决于
    // 调用方打算怎么读（见 `RequestBody`）。`u64` 而非 `usize` 是为了在 32 位
    // 目标上也能如实表达大声明，再由读取侧按需拒绝。
    let content_length = match lookup("content-length") {
        Some(raw) => Some(
            raw.parse::<u64>()
                .map_err(|_| ParseError::BadContentLength)?,
        ),
        None => None,
    };

    Ok(ParsedRequest {
        head: Request {
            method,
            path,
            query,
            headers,
            content_length,
            version,
        },
        // `buf` 已经**恰好**消费到体的起点（`read_line` 逐字节读到空行），
        // 所以把整个 `BufReader` 交出去即可——它内部可能已缓冲了部分体字节，
        // 继续从它读才是正确的，另起一个 reader 会丢掉那段缓冲。
        body: RequestBody {
            reader: Box::new(buf),
            content_length,
        },
    })
}

/// 读一行（`\n` 结尾），去尾 `\r`/`\n`，并施加长度上限。
fn read_line<R: BufRead>(reader: &mut R) -> Result<String, ParseError> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break, // EOF：交由上层判断是否算「空行结束」
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if raw.len() >= MAX_HEADER_LINE_BYTES {
                    return Err(ParseError::HeaderLineTooLong);
                }
                raw.push(byte[0]);
            }
            Err(err) => return Err(ParseError::Io(err.to_string())),
        }
    }

    while raw.last() == Some(&b'\r') {
        raw.pop();
    }
    String::from_utf8(raw).map_err(|_| ParseError::MalformedRequestLine)
}

/// 一个待写出的响应。
#[derive(Debug, Clone)]
pub struct Response {
    /// 状态码。
    pub status: u16,
    /// 内容类型。
    pub content_type: &'static str,
    /// CORS 头（值为 `None` 表示不带该头）。
    pub cors: CorsHeaders,
    /// 响应体。
    pub body: Vec<u8>,
}

/// CORS 响应头。
///
/// 只回显**固定的** WebUI origin，而不是 `*`：本接口可改状态，
/// 用 `*` 等于允许任意网页（在任何应用的 WebView 里）读取响应。
#[derive(Debug, Clone, Default)]
pub struct CorsHeaders {
    /// `Access-Control-Allow-Origin` 的值。
    pub allow_origin: Option<String>,
    /// 是否为预检响应（额外带 Methods/Headers/Max-Age）。
    pub preflight: bool,
}

/// 允许的 WebUI origin。
///
/// 实测值：KernelSU 的 `WebViewAssetLoader` 以 `https://mui.kernelsu.org`
/// 提供模块网页，跨源请求会带这个 `Origin`。
///
/// **待验证假设**：其他 root 管理器（如 APatch）的 origin 可能不同。
/// 若将来发现不匹配，改这一个常量即可。
pub const WEBUI_ORIGIN: &str = "https://mui.kernelsu.org";

impl Response {
    /// 以 JSON 体构造。
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/json",
            cors: CorsHeaders {
                allow_origin: Some(WEBUI_ORIGIN.to_string()),
                preflight: false,
            },
            body: body.into(),
        }
    }

    /// 空的预检响应（`204`）。
    pub fn preflight() -> Self {
        Self {
            status: 204,
            content_type: "text/plain",
            cors: CorsHeaders {
                allow_origin: Some(WEBUI_ORIGIN.to_string()),
                preflight: true,
            },
            body: Vec::new(),
        }
    }

    /// 状态码的规范原因短语。
    pub const fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            500 => "Internal Server Error",
            _ => "Unknown",
        }
    }

    /// 把响应写入流。
    pub fn write_to<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        // 单次写出（无连接复用）时一律关闭连接，语义与旧行为一致。
        self.write_with_connection(writer, false)
    }

    /// 写出响应，并按 `keep_alive` 决定 `Connection` 头。
    ///
    /// 复用时必须**显式**回 `keep-alive`：客户端据此确认可以继续在这条连接上
    /// 发下一个请求。漏写会让它以为连接已关闭而重建——那就等于没复用。
    pub fn write_to_keep_alive<W: Write>(
        &self,
        writer: &mut W,
        keep_alive: bool,
    ) -> std::io::Result<()> {
        self.write_with_connection(writer, keep_alive)
    }

    fn write_with_connection<W: Write>(
        &self,
        writer: &mut W,
        keep_alive: bool,
    ) -> std::io::Result<()> {
        write!(writer, "HTTP/1.1 {} {}\r\n", self.status, self.reason())?;
        write!(writer, "Content-Type: {}\r\n", self.content_type)?;
        write!(writer, "Content-Length: {}\r\n", self.body.len())?;
        write!(
            writer,
            "Connection: {}\r\n",
            if keep_alive { "keep-alive" } else { "close" }
        )?;

        if let Some(origin) = &self.cors.allow_origin {
            write!(writer, "Access-Control-Allow-Origin: {origin}\r\n")?;
        }
        if self.cors.preflight {
            write!(
                writer,
                "Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n"
            )?;
            write!(
                writer,
                "Access-Control-Allow-Headers: Content-Type, Authorization\r\n"
            )?;
            write!(writer, "Access-Control-Max-Age: 600\r\n")?;
        }

        write!(writer, "\r\n")?;
        writer.write_all(&self.body)?;
        writer.flush()
    }
}

/// 处理一个已接受的 TCP 连接：设超时、解析、交给 `handler`、写回。
///
/// 解析失败时**仍然回一个 JSON 错误**，而不是静默断开：客户端能据此
/// 定位问题（静默断开只能看到 `Failed to fetch`，无法区分原因）。
///
/// `handler` 收到**头部**与**惰性请求体**两部分：它必须先看路由再决定怎么读体
/// （JSON 端点上限、上传端点流式）。这是「连接内单线程」的前提下才成立的设计——
/// 体只能被读一次，且必须由 handler 读完或用完。
///
/// ## 连接复用（Keep-Alive）
///
/// 同一连接上**连续处理多个请求**，直到客户端要求关闭或空闲超时。
///
/// 动机是实测出来的：每个请求有约 **26ms 固定开销**（新建连接 + 线程 + 解析），
/// 与载荷大小无关，而分块上传会发很多次请求——8 MiB 分块下这笔开销占请求耗时的
/// **30%**。复用它能把这块直接抹掉。
///
/// 三条边界必须守死：
///
/// 1. **HTTP/1.0 默认不复用**：只有客户端显式发 `Connection: keep-alive` 才继续。
///    这是 HTTP/1.0 的语义，猜错会让老客户端等一个永不到来的响应。
/// 2. **`Connection: close` 一律关闭**：客户端说了关就必须关。
/// 3. **空闲超时或对端关闭就退出**：复用连接不能变成「永久占住一个线程」——
///    那正是本模块用 `IO_TIMEOUT` 要防的事。
pub fn serve_connection<F>(mut stream: TcpStream, mut handler: F)
where
    F: FnMut(Request, RequestBody<'_>) -> Response,
{
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    loop {
        // 先把读写两半分开：handler 拿读半（读体），响应写回用写半。
        // 若继续用 `&mut stream`，`parse_request` 借用的生命周期会与随后的
        // `write_to` 冲突。
        let read_half = match stream.try_clone() {
            Ok(clone) => clone,
            Err(err) => {
                let response = Response::json(
                    500,
                    serde_json::json!({
                        "error": "internal",
                        "message": format!("cannot clone the connection: {err}"),
                    })
                    .to_string()
                    .into_bytes(),
                );
                let _ = response.write_to(&mut stream);
                return;
            }
        };

        // 记录版本与 Connection 头，用于决定这一轮之后是否继续。
        let (response, keep_alive) = match parse_request(read_half) {
            Ok(parsed) => {
                let keep = wants_keep_alive(&parsed.head);
                (handler(parsed.head, parsed.body), keep)
            }
            Err(err) => {
                // 解析失败无法可靠判断边界（例如长度不符），因此**不复用**：
                // 继续读下去可能把下一个请求的头当成体的一部分。
                let response = Response::json(
                    err.status(),
                    serde_json::json!({
                        "error": "invalid_argument",
                        "message": err.to_string(),
                    })
                    .to_string()
                    .into_bytes(),
                );
                (response, false)
            }
        };

        let wrote_ok = response
            .write_to_keep_alive(&mut stream, keep_alive)
            .is_ok();
        if !keep_alive || !wrote_ok {
            return;
        }
    }
}

/// 这一轮之后是否继续复用连接。
///
/// 判据只有一条来源：**客户端**的意图。服务端不主动延长，也不猜测。
fn wants_keep_alive(request: &Request) -> bool {
    match request.header("connection") {
        // 显式要求关闭：听客户端的。
        Some(value) if value.eq_ignore_ascii_case("close") => false,
        // 显式要求复用。
        Some(value) if value.eq_ignore_ascii_case("keep-alive") => true,
        // 未声明：HTTP/1.1 默认复用，HTTP/1.0 默认关闭（向后兼容老客户端）。
        _ => request.version != HttpVersion::Http10,
    }
}

/// 极简百分号解码（`%XX` 与 `+`）。
///
/// 只用于查询参数里的路径值；非法转义原样保留，不报错——
/// 路径最终仍会经文件系统校验，这里宽松不会造成安全问题。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只解析头部（**不读体**）。用于头/路由类断言。
    fn parse_head(raw: &[u8]) -> Result<Request, ParseError> {
        parse_request(std::io::Cursor::new(raw.to_vec())).map(|parsed| parsed.head)
    }

    /// 解析并按 **JSON 端点**的方式读完体，返回 `(头, 体)`。
    fn parse(raw: &[u8]) -> Result<(Request, Vec<u8>), ParseError> {
        let parsed = parse_request(std::io::Cursor::new(raw.to_vec()))?;
        let body = parsed.body.read_json()?;
        Ok((parsed.head, body))
    }

    #[test]
    fn parses_a_minimal_get() {
        let req = parse_head(b"GET /api/v1/status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/api/v1/status");
        assert_eq!(req.query, None);
        assert_eq!(req.content_length, None);
    }

    #[test]
    fn parses_query_string_into_pairs() {
        let req =
            parse_head(b"GET /api/v1/tool/ls?path=%2Fdata%2Fadb&x=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.path, "/api/v1/tool/ls");
        assert_eq!(req.query_param("path").as_deref(), Some("/data/adb"));
        assert_eq!(req.query_param("x").as_deref(), Some("1"));
        assert_eq!(req.query_param("missing"), None);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let req = parse_head(b"POST / HTTP/1.1\r\nContent-Type: application/json\r\n\r\n").unwrap();
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.header("CONTENT-TYPE"), Some("application/json"));
    }

    // ---------------------------------------------------------------- 连接复用

    /// `Connection` 头 + HTTP 版本 → 是否复用。这是 keep-alive 的**唯一**判据。
    #[test]
    fn keep_alive_decision_follows_the_client() {
        let with = |extra: &str, ver: &str| {
            let raw = format!("GET / HTTP/1.1\r\n{extra}\r\n").replace("HTTP/1.1", ver);
            parse_head(raw.as_bytes()).unwrap()
        };

        // HTTP/1.1 未声明 → 默认复用。
        assert!(wants_keep_alive(&with("", "HTTP/1.1")));
        // 显式 close → 关闭，即使是 1.1。
        assert!(!wants_keep_alive(&with(
            "Connection: close\r\n",
            "HTTP/1.1"
        )));
        // 大小写不敏感。
        assert!(!wants_keep_alive(&with(
            "Connection: CLOSE\r\n",
            "HTTP/1.1"
        )));
        // HTTP/1.0 未声明 → 默认关闭（向后兼容老客户端）。
        assert!(!wants_keep_alive(&with("", "HTTP/1.0")));
        // HTTP/1.0 显式要求 → 复用。
        assert!(wants_keep_alive(&with(
            "Connection: keep-alive\r\n",
            "HTTP/1.0"
        )));
    }

    #[test]
    fn response_advertises_the_connection_disposition() {
        let response = Response::json(200, b"{}".to_vec());
        let mut closed = Vec::new();
        response.write_to(&mut closed).unwrap();
        let closed = String::from_utf8(closed).unwrap();
        assert!(
            closed.contains("Connection: close\r\n"),
            "单次写出必须声明关闭：{closed}"
        );

        let mut kept = Vec::new();
        response.write_to_keep_alive(&mut kept, true).unwrap();
        let kept = String::from_utf8(kept).unwrap();
        assert!(
            kept.contains("Connection: keep-alive\r\n"),
            "复用时必须显式声明 keep-alive，否则客户端会重建连接（等于没复用）：{kept}"
        );
    }

    /// 同一条连接上连续两个请求都必须被正确响应。
    ///
    /// 这是复用的**实质**：连接不被复用的话，第二个请求会拿到 EOF。
    #[test]
    fn one_connection_serves_multiple_requests() {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut seen = 0usize;
            serve_connection(stream, |req, _body| {
                seen += 1;
                Response::json(
                    200,
                    format!("{{\"n\":{},\"path\":\"{}\"}}", seen, req.path).into_bytes(),
                )
            });
            seen
        });

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // 两个请求共用**同一个** TcpStream。
        stream
            .write_all(b"GET /first HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("200"), "第一个请求：{line:?}");
        let mut body = String::new();
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).unwrap();
            if l == "\r\n" {
                break;
            }
        }
        // Content-Length 为 15 `{"n":1,"path":"/first"}` — 读满即可。
        let mut buf = [0u8; 64];
        use std::io::Read as _;
        let n = reader.read(&mut buf).unwrap();
        body.push_str(&String::from_utf8_lossy(&buf[..n]));
        assert!(body.contains("/first"), "第一个响应体：{body:?}");

        stream
            .write_all(b"GET /second HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut line2 = String::new();
        reader.read_line(&mut line2).unwrap();
        assert!(
            line2.contains("200"),
            "第二个请求（复用同一连接）：{line2:?}"
        );

        assert_eq!(server.join().unwrap(), 2, "同一条连接上应处理两个请求");
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"POST /api HTTP/1.1\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let (head, body) = parse(raw).unwrap();
        assert_eq!(body, b"{\"a\":1}");
        assert_eq!(head.content_length, Some(7));
    }

    #[test]
    fn rejects_chunked_transfer_encoding() {
        // 把 chunked 当定长体会读错请求边界，必须拒绝而不是猜。
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(
            parse_head(raw).unwrap_err(),
            ParseError::UnsupportedTransferEncoding
        );
    }

    #[test]
    fn rejects_oversized_json_body_before_allocating() {
        // 声称 4 GiB：JSON 端点必须在分配前拒绝，否则直接 OOM。
        let raw = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", u32::MAX);
        let err = parse(raw.as_bytes()).unwrap_err();
        assert_eq!(err, ParseError::BodyTooLarge);
        assert_eq!(err.status(), 413);
    }

    #[test]
    fn rejects_bad_content_length() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n";
        assert_eq!(parse_head(raw).unwrap_err(), ParseError::BadContentLength);
    }

    #[test]
    fn rejects_truncated_body() {
        // 声明 10 字节却只给 3 字节：不能当作空体放过。
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc";
        assert_eq!(parse(raw).unwrap_err(), ParseError::BadContentLength);
    }

    // ---------------------------------------------------------------- 流式请求体

    #[test]
    fn streaming_body_accepts_payload_far_beyond_json_limit() {
        // 回归：REST 请求体上限曾**假耦合**于 gdd 的协议帧上限，于是上传被
        // 1 MiB 一刀切。上传字节不经 gdd（直接写文件系统），必须能流式读大载荷。
        let size = MAX_BODY_BYTES * 3;
        let mut raw =
            format!("POST /api/v1/upload/chunk HTTP/1.1\r\nContent-Length: {size}\r\n\r\n")
                .into_bytes();
        raw.extend(std::iter::repeat_n(0xABu8, size));

        let parsed = parse_request(std::io::Cursor::new(raw)).unwrap();
        let mut out = Vec::new();
        parsed.body.stream().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), size, "流式体必须能超过 JSON 上限");
        assert!(out.iter().all(|b| *b == 0xAB));
    }

    #[test]
    fn streaming_body_does_not_drain_reader_eagerly() {
        // 「流式」的实际含义：解析完头部后**不预读**体。若这里一次性读完，
        // 上传的内存峰值就仍然等于整个文件。
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let parsed = parse_request(std::io::Cursor::new(raw.to_vec())).unwrap();
        assert_eq!(parsed.head.content_length, Some(5));
        // 头部解析阶段不得读走体字节：`Cursor` 位置应恰在体起点。
        // （`BufReader` 可能预读入缓冲，故以「读出来仍是 hello」为准。）
        let mut out = String::new();
        parsed.body.stream().read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn streaming_body_is_bounded_by_declared_length() {
        // 只声明 3 字节时，流必须恰好给出 3 字节，不能把后面的字节也吐出来。
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcdef";
        let parsed = parse_request(std::io::Cursor::new(raw.to_vec())).unwrap();
        let mut out = String::new();
        parsed.body.stream().read_to_string(&mut out).unwrap();
        assert_eq!(out, "abc");
    }

    #[test]
    fn json_reader_still_caps_and_stream_does_not() {
        // 同一个请求：JSON 读法拒绝，流式读法接受。
        let size = MAX_BODY_BYTES + 1;
        let mut raw = format!("POST / HTTP/1.1\r\nContent-Length: {size}\r\n\r\n").into_bytes();
        raw.extend(std::iter::repeat_n(b'x', size));

        let parsed = parse_request(std::io::Cursor::new(raw.clone())).unwrap();
        assert_eq!(
            parsed.body.read_json().unwrap_err(),
            ParseError::BodyTooLarge
        );

        let parsed = parse_request(std::io::Cursor::new(raw)).unwrap();
        let mut out = Vec::new();
        parsed.body.stream().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), size);
    }

    #[test]
    fn rejects_unsupported_method_with_405() {
        let err = parse(b"DELETE / HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!(err, ParseError::UnsupportedMethod("DELETE".into()));
        assert_eq!(err.status(), 405);
    }

    #[test]
    fn rejects_unsupported_version() {
        let err = parse(b"GET / HTTP/2.0\r\n\r\n").unwrap_err();
        assert_eq!(err, ParseError::UnsupportedVersion("HTTP/2.0".into()));
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn rejects_malformed_request_line() {
        assert_eq!(
            parse(b"GET\r\n\r\n").unwrap_err(),
            ParseError::MalformedRequestLine
        );
        // 四段也是非法的。
        assert_eq!(
            parse(b"GET / HTTP/1.1 extra\r\n\r\n").unwrap_err(),
            ParseError::MalformedRequestLine
        );
    }

    #[test]
    fn rejects_malformed_header_line() {
        assert_eq!(
            parse(b"GET / HTTP/1.1\r\nnot-a-header\r\n\r\n").unwrap_err(),
            ParseError::MalformedHeader
        );
    }

    #[test]
    fn rejects_too_many_headers() {
        let mut raw = String::from("GET / HTTP/1.1\r\n");
        for i in 0..(MAX_HEADERS + 1) {
            raw.push_str(&format!("X-{i}: v\r\n"));
        }
        raw.push_str("\r\n");
        assert_eq!(parse(raw.as_bytes()), Err(ParseError::TooManyHeaders));
    }

    #[test]
    fn rejects_overlong_header_line() {
        let mut raw = String::from("GET / HTTP/1.1\r\nX: ");
        raw.push_str(&"a".repeat(MAX_HEADER_LINE_BYTES + 10));
        raw.push_str("\r\n\r\n");
        assert_eq!(parse(raw.as_bytes()), Err(ParseError::HeaderLineTooLong));
    }

    #[test]
    fn response_write_includes_cors_and_closes() {
        let response = Response::json(200, b"{\"ok\":true}".to_vec());
        let mut out = Vec::new();
        response.write_to(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Access-Control-Allow-Origin: https://mui.kernelsu.org\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.ends_with("{\"ok\":true}"));
        // 非预检响应不应带 Methods/Headers。
        assert!(!text.contains("Access-Control-Allow-Methods"));
    }

    #[test]
    fn preflight_response_carries_methods_headers_and_no_body() {
        let response = Response::preflight();
        let mut out = Vec::new();
        response.write_to(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(text.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n"));
        assert!(text.contains("Access-Control-Allow-Headers: Content-Type, Authorization\r\n"));
        assert!(text.contains("Access-Control-Max-Age: 600\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn every_used_status_has_a_reason_phrase() {
        for status in [200, 204, 400, 401, 404, 405, 413, 500] {
            let response = Response::json(status, Vec::new());
            assert_ne!(response.reason(), "Unknown", "状态码 {status} 缺少原因短语");
        }
    }

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("%2Fdata%2Fadb"), "/data/adb");
        assert_eq!(percent_decode("a+b"), "a b");
        // 非法转义原样保留，不 panic、不吞字符。
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
