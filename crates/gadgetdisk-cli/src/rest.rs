//! REST 路由：把 HTTP 请求分发到「CLI 直做」或「转发 gdd」。
//!
//! 设计理由见
//! [按需进程模型 Note](../../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。
//!
//! ## 职责划分（按需进程模型）
//!
//! | 操作 | 由谁做 | 为什么 |
//! |---|---|---|
//! | `mount` / `unmount`（导出为 USB 设备） | **gdd** | 它是已导出镜像的守卫，必须持有该事实 |
//! | loop 挂载/卸载 | **本进程直做** | 一次性内核操作，无需常驻状态 |
//! | 镜像增删查、导入、只读工具、能力探测 | **本进程直做** | 纯文件系统/探测操作 |
//!
//! gdd **只**负责「导出期间不被读写」这一件事，因此可以在镜像卸载后退出。
//!
//! ## 安全
//!
//! 回环端口对**全设备**开放（实测：uid 2000 可连），因此每个请求都必须带
//! 正确的 Bearer token，除预检 `OPTIONS`（浏览器不为其带凭据）。token 用
//! 常数时间比较，避免时序侧信道。

use std::path::{Path, PathBuf};

use gadgetdisk_proto::{ErrorCode, Message};

use crate::http::{self, Method, Request, Response};

/// `api.json` 的内容：WebUI 引导所需的全部信息。
///
/// 由 `serve` 启动时写入**模块的 `webroot/`**（`0600`）：那是 WebView 唯一能
/// 同源读到、而其他应用读不到的位置（`/data/adb` 为 `0700`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApiInfo {
    /// 监听端口（临时端口，每次启动可能不同）。
    pub port: u16,
    /// Bearer token。
    pub token: String,
}

/// 需要 `gdd` 参与的操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GddAction {
    /// 导出为 USB 设备（可多 LUN）。
    Mount(Message),
    /// 解除导出（单个 LUN 或全部）。
    Unmount(Message),
    /// 重新绑定 UDC（让身份改动生效）。
    Rebind(Message),
    /// 删除一个空闲槽位。
    DeleteSlot(Message),
}

/// 后端能力：把一次 REST 调用变成具体动作。
///
/// 抽象成 trait 是为了让**路由与鉴权逻辑可主机测试**——那是本模块唯一
/// 容易出错的部分（路径匹配、方法匹配、401/404/405 的边界）。
pub trait Backend {
    /// `GET /api/v1/status`
    fn status(&mut self) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/images`
    fn images(&mut self) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/image/partitions?path=`
    ///
    /// 返回镜像的分区列表，供 UI 让用户选择挂载哪个分区。
    /// 无分区表时 `partitions` 为空数组（**不是**错误）。
    fn image_partitions(&mut self, path: &str) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/loop`
    fn loop_attachments(&mut self) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/capabilities`
    fn capabilities(&mut self) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/jobs/{id}`
    fn job_status(&mut self, id: &str) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/tool/{df,ls,stat}`
    fn tool(&mut self, tool: Tool, path: &str) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/create`
    fn create(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/delete`
    fn delete(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/upload/begin`
    ///
    /// 受理一次分块上传：校验目标名、同名冲突与空间，登记 job（**必须登记**，
    /// 否则 `serve` 会在上传期间判定空闲而退出），返回 `upload_id`。
    fn upload_begin(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/upload/chunk?upload_id=&offset=`
    ///
    /// 顺序追加一块镜像字节。**流式读体、不设上限**——这正是本端点的存在理由
    /// （镜像可远超 [`http::MAX_BODY_BYTES`]）。
    fn upload_chunk(
        &mut self,
        upload_id: &str,
        offset: u64,
        body: impl std::io::Read,
    ) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/upload/commit`
    ///
    /// 收尾：把暂存文件**原子改名**为 `images/<dest_name>`。
    fn upload_commit(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/upload/abort`
    ///
    /// 放弃上传并清理暂存文件。
    fn upload_abort(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/loop/attach`
    fn loop_attach(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/loop/detach`
    fn loop_detach(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/config`
    ///
    /// 回 `config/gadget.json` 的内容，并在缺文件时用**内核当前生效值**预填，
    /// 使用户看到的是「现在是什么」而非空白表单。
    fn config_get(&mut self) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/config`
    ///
    /// 写文件、立即应用到 configfs，并在 UDC 已绑定时请 `gdd` 重绑。
    fn config_set(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `GET /api/v1/config/security`
    ///
    /// 镜像文件的 SELinux **目标上下文**：保存值、解析后的生效值与内置默认。
    ///
    /// 与 [`Self::config_get`] 拆分为不同端点：二者的**写入**语义存在本质
    /// 差异（USB 身份支持按字段增量合并，而安全上下文为完整取值替换），合并至同一端点
    /// 存在误覆盖 USB 设备身份配置的风险。
    fn config_security_get(&mut self) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/config/security`
    ///
    /// 设置或清除目标上下文（请求体：`{"image_context": "…"}` 或 `{"reset": true}`）。
    /// 落盘遵循原子**读—改—写**机制，绝不覆盖同一配置文件中的 USB 身份。
    fn config_security_set(
        &mut self,
        body: &[u8],
    ) -> Result<serde_json::Value, (ErrorCode, String)>;

    /// `POST /api/v1/{mount,unmount,rebind}`：交给 `gdd`（必要时先拉起它）。
    fn gdd_op(&mut self, action: GddAction) -> Result<serde_json::Value, (ErrorCode, String)>;
}

/// 只读工具。
///
/// 曾经还有 `Ls`/`Stat`，只服务于 WebUI 的内置路径浏览器；该浏览器已由
/// 系统文件选择器取代（见 [上传与导入](../../../../docs/image-upload-and-import.md)），
/// 因此一并移除——留着只会是没有调用方的攻击面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// 可用空间。
    Df,
}

/// 解析后的路由目标。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Status,
    Images,
    ImagePartitions,
    Loop,
    Capabilities,
    Job(String),
    Tool(Tool),
    Create,
    Delete,
    LoopAttach,
    LoopDetach,
    Mount,
    Unmount,
    Rebind,
    SlotDelete,
    Config,
    ConfigSecurity,
    UploadBegin,
    UploadChunk,
    UploadCommit,
    UploadAbort,
    NotFound,
}

/// 把方法 + 路径解析为路由。
///
/// **纯函数**：路径匹配的所有边界都能在主机上穷举，不必起 HTTP 服务。
fn route(method: Method, path: &str) -> Route {
    // 先按「方法是否匹配」分组：同一个路径用错方法要回 405 而不是 404。
    match (method, path) {
        (Method::Get, "/api/v1/status") => Route::Status,
        (Method::Get, "/api/v1/images") => Route::Images,
        (Method::Get, "/api/v1/image/partitions") => Route::ImagePartitions,
        (Method::Get, "/api/v1/loop") => Route::Loop,
        (Method::Get, "/api/v1/capabilities") => Route::Capabilities,
        (Method::Get, "/api/v1/tool/df") => Route::Tool(Tool::Df),
        (Method::Post, "/api/v1/create") => Route::Create,
        (Method::Post, "/api/v1/delete") => Route::Delete,
        (Method::Post, "/api/v1/loop/attach") => Route::LoopAttach,
        (Method::Post, "/api/v1/loop/detach") => Route::LoopDetach,
        (Method::Post, "/api/v1/upload/begin") => Route::UploadBegin,
        (Method::Post, "/api/v1/upload/chunk") => Route::UploadChunk,
        (Method::Post, "/api/v1/upload/commit") => Route::UploadCommit,
        (Method::Post, "/api/v1/upload/abort") => Route::UploadAbort,
        (Method::Post, "/api/v1/mount") => Route::Mount,
        (Method::Post, "/api/v1/unmount") => Route::Unmount,
        (Method::Post, "/api/v1/rebind") => Route::Rebind,
        (Method::Post, "/api/v1/slot/delete") => Route::SlotDelete,
        (Method::Get, "/api/v1/config") => Route::Config,
        (Method::Post, "/api/v1/config") => Route::Config,
        (Method::Get, "/api/v1/config/security") => Route::ConfigSecurity,
        (Method::Post, "/api/v1/config/security") => Route::ConfigSecurity,
        _ => match path.strip_prefix("/api/v1/jobs/") {
            // job id 不允许含 `/`：避免把 `/jobs/a/b` 误当成一个 id。
            Some(id) if method == Method::Get && !id.is_empty() && !id.contains('/') => {
                Route::Job(id.to_string())
            }
            _ => Route::NotFound,
        },
    }
}

/// 该路径是否存在（用于区分 404 与 405）。
fn path_exists(path: &str) -> bool {
    const PATHS: &[&str] = &[
        "/api/v1/status",
        "/api/v1/images",
        "/api/v1/image/partitions",
        "/api/v1/loop",
        "/api/v1/capabilities",
        "/api/v1/tool/df",
        "/api/v1/create",
        "/api/v1/delete",
        "/api/v1/upload/begin",
        "/api/v1/upload/chunk",
        "/api/v1/upload/commit",
        "/api/v1/upload/abort",
        "/api/v1/loop/attach",
        "/api/v1/loop/detach",
        "/api/v1/mount",
        "/api/v1/unmount",
        "/api/v1/config",
        "/api/v1/config/security",
    ];
    PATHS.contains(&path) || path.starts_with("/api/v1/jobs/")
}

/// 常数时间比较两个字节串是否相等。
///
/// 逐字节异或累积，**不提前返回**：提前返回会泄漏「前缀匹配了多少字节」，
/// 使攻击者可以逐字节爆破 token。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // 长度不同必然不等；长度本身不是秘密，可以直接返回。
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 校验 `Authorization: Bearer <token>`。
pub fn token_ok(request: &Request, expected: &str) -> bool {
    let Some(raw) = request.header("authorization") else {
        return false;
    };
    let Some(token) = raw.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.trim().as_bytes(), expected.as_bytes())
}

/// 处理一个请求：预检 → 鉴权 → 路由 → 调用 backend。
///
/// **请求体的读法按路由决定**（见 [`http::RequestBody`]）：控制类端点走
/// [`json_body`]（一次读完并限 [`http::MAX_BODY_BYTES`]），上传端点走
/// [`http::RequestBody::stream`]（流式、不设上限）。
///
/// 这样分层是必需的：体的读法只有读之前才知道（有人要 JSON，有人要字节流），
/// 而体在 HTTP 里只能读一次。
pub fn handle<B: Backend>(
    request: &Request,
    body: http::RequestBody<'_>,
    token: &str,
    backend: &mut B,
) -> Response {
    // 预检不带凭据，必须先于鉴权处理，否则浏览器永远过不了预检。
    if request.method == Method::Options {
        return Response::preflight();
    }

    if !token_ok(request, token) {
        return json_error(
            401,
            ErrorCode::PermissionDenied,
            "missing or wrong access token",
        );
    }

    let target = route(request.method, &request.path);

    // 路径存在但方法不对 → 405；两者都不对 → 404。
    if target == Route::NotFound {
        if path_exists(&request.path) {
            return json_error(
                405,
                ErrorCode::InvalidArgument,
                &format!(
                    "path {} does not support method {}",
                    request.path,
                    request.method.as_str()
                ),
            );
        }
        return json_error(
            404,
            ErrorCode::InvalidArgument,
            &format!("unknown path: {}", request.path),
        );
    }

    /// 读 JSON 体，失败即提前返回一个可直接返回的响应。
    ///
    /// 用局部函数而非宏：宏展开在表达式位置时，`return` 会让类型推断失去参照，
    /// 落成一堆 `[u8]` 尺寸未知的报错。
    macro_rules! body_bytes {
        () => {
            match body.read_json() {
                Ok(raw) => raw,
                Err(err) => {
                    return json_error(err.status(), ErrorCode::InvalidArgument, &err.to_string());
                }
            }
        };
    }

    let result = match target {
        Route::Status => backend.status(),
        Route::Images => backend.images(),
        Route::ImagePartitions => {
            let Some(path) = request.query_param("path") else {
                return json_error(
                    400,
                    ErrorCode::InvalidArgument,
                    "missing the path query parameter (image path)",
                );
            };
            backend.image_partitions(&path)
        }
        Route::Loop => backend.loop_attachments(),
        Route::Capabilities => backend.capabilities(),
        Route::Job(id) => backend.job_status(&id),
        Route::Tool(tool) => {
            let Some(path) = request.query_param("path") else {
                return json_error(
                    400,
                    ErrorCode::InvalidArgument,
                    "missing the path query parameter",
                );
            };
            backend.tool(tool, &path)
        }
        Route::Create => backend.create(&body_bytes!()),
        Route::Delete => backend.delete(&body_bytes!()),
        Route::LoopAttach => backend.loop_attach(&body_bytes!()),
        Route::LoopDetach => backend.loop_detach(&body_bytes!()),
        Route::Mount => backend.gdd_op(GddAction::Mount(Message::MountRequest(match decode_body(
            &body_bytes!(),
        ) {
            Ok(v) => v,
            Err(resp) => return resp,
        }))),
        Route::Unmount => backend.gdd_op(GddAction::Unmount(Message::UnmountRequest(
            match decode_body(&body_bytes!()) {
                Ok(v) => v,
                Err(resp) => return resp,
            },
        ))),
        Route::Rebind => backend.gdd_op(GddAction::Rebind(Message::RebindRequest(
            Default::default(),
        ))),
        Route::SlotDelete => backend.gdd_op(GddAction::DeleteSlot(Message::DeleteSlotRequest(
            match decode_body(&body_bytes!()) {
                Ok(v) => v,
                Err(resp) => return resp,
            },
        ))),
        Route::Config => {
            if request.method == Method::Get {
                backend.config_get()
            } else {
                backend.config_set(&body_bytes!())
            }
        }
        // 与 `Config` 同款分流：同一路径、按方法决定读写。
        Route::ConfigSecurity => {
            if request.method == Method::Get {
                backend.config_security_get()
            } else {
                backend.config_security_set(&body_bytes!())
            }
        }

        // ---- 分块上传 ----
        //
        // `begin`/`commit`/`abort` 是控制类端点（小 JSON）；
        // 只有 `chunk` 流式读体——那正是这个端点存在的理由。
        Route::UploadBegin => backend.upload_begin(&body_bytes!()),
        Route::UploadCommit => backend.upload_commit(&body_bytes!()),
        Route::UploadAbort => backend.upload_abort(&body_bytes!()),
        Route::UploadChunk => {
            let Some(upload_id) = request.query_param("upload_id") else {
                return json_error(
                    400,
                    ErrorCode::InvalidArgument,
                    "missing the upload_id query parameter",
                );
            };
            let Some(offset) = request.query_param("offset") else {
                return json_error(
                    400,
                    ErrorCode::InvalidArgument,
                    "missing the offset query parameter",
                );
            };
            let Ok(offset) = offset.parse::<u64>() else {
                return json_error(
                    400,
                    ErrorCode::InvalidArgument,
                    &format!("offset must be a non-negative integer, got {offset:?}"),
                );
            };
            backend.upload_chunk(&upload_id, offset, body.stream())
        }

        // 上面已排除。
        Route::NotFound => unreachable!("NotFound 已在前面返回"),
    };

    match result {
        Ok(value) => Response::json(200, value.to_string().into_bytes()),
        Err((code, message)) => json_error(status_for(code), code, &message),
    }
}

/// 解析 JSON 请求体；失败时返回一个可直接返回的响应。
fn decode_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|err| {
        json_error(
            400,
            ErrorCode::InvalidArgument,
            &format!("the request body is not valid JSON: {err}"),
        )
    })
}

/// 错误码 → HTTP 状态码。
///
/// 只做**能确定**的映射；其余一律 `500`——猜错状态码会把排查引向错误方向。
pub const fn status_for(code: ErrorCode) -> u16 {
    match code {
        ErrorCode::InvalidArgument | ErrorCode::SizeBelowMinimum | ErrorCode::UnsupportedLayout => {
            400
        }
        ErrorCode::PermissionDenied => 401,
        ErrorCode::ImageNotFound => 404,
        // 409 表示「与当前资源状态冲突」——同名冲突正属此类（换个名字就能成功），
        // 与 400（请求本身不合法）区分开，前端可据此给「改名或先删除」的指引。
        ErrorCode::ImageInUse
        | ErrorCode::Busy
        | ErrorCode::NotRegularFile
        | ErrorCode::AlreadyExists => 409,
        ErrorCode::NoSpace => 507,
        ErrorCode::NoUdc
        | ErrorCode::LoopUnsupported
        | ErrorCode::FilesystemUnsupported
        | ErrorCode::MassStorageUnsupported
        // 环境/内核层面的失败：不是调用方能改的，也不是「没找到」。
        | ErrorCode::ConfigfsUnavailable
        | ErrorCode::NotActive
        | ErrorCode::Internal => 500,
    }
}

/// 构造一个错误响应。
///
/// 字段名用 `error`/`message`，与既有 CLI 契约一致，使 WebUI 的
/// `parseExecResult` 与错误文案映射可原样复用。
fn json_error(status: u16, code: ErrorCode, message: &str) -> Response {
    Response::json(
        status,
        serde_json::json!({ "error": code.as_str(), "message": message })
            .to_string()
            .into_bytes(),
    )
}

/// 解析镜像名到 `images/` 下的绝对路径。
///
/// **安全**：只接受单一文件名，绝不接受含 `/`、`..` 或空的输入，
/// 避免经 REST 越出 images 目录。复用 gdd 既有的安全判据。
pub fn resolve_image_path(dirs: &gadgetdisk_gdd::DataDirs, name: &str) -> Option<PathBuf> {
    if !gadgetdisk_gdd::paths::is_safe_component(name) {
        return None;
    }
    Some(dirs.images().join(name))
}

/// 从 `/dev/urandom` 读 32 字节并转成 64 位十六进制串。
///
/// 不引 `rand`：这是唯一需要随机性的地方，读内核 CSPRNG 足够且零依赖。
pub fn random_token() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    let mut file = std::fs::File::open("/dev/urandom")?;
    file.read_exact(&mut bytes)?;
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

/// 把 `api.json` 原子写入 `webroot/`。
///
/// 先写临时文件再 `rename`：WebView 可能在任意时刻读取该文件，
/// 绝不能让它读到「写了一半」的内容。
pub fn write_api_info(path: &Path, info: &ApiInfo) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(info).map_err(std::io::Error::other)?;

    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }

    // 权限：token 在文件里，必须仅 root 可读。
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpVersion;

    // ------------------------------------------------------------ 路由

    #[test]
    fn routes_read_only_endpoints() {
        assert_eq!(route(Method::Get, "/api/v1/status"), Route::Status);
        assert_eq!(route(Method::Get, "/api/v1/images"), Route::Images);
        assert_eq!(route(Method::Get, "/api/v1/loop"), Route::Loop);
        assert_eq!(
            route(Method::Get, "/api/v1/capabilities"),
            Route::Capabilities
        );
        assert_eq!(route(Method::Get, "/api/v1/tool/df"), Route::Tool(Tool::Df));
    }

    /// 路径浏览器的端点必须**不存在**。
    ///
    /// 回归：`ls`/`stat` 只为 WebUI 的内置路径浏览器服务；该浏览器被系统文件选择器
    /// 取代后，这两条路由若残留，就是没有调用方的攻击面（可枚举设备任意目录）。
    #[test]
    fn path_browser_endpoints_are_gone() {
        assert_eq!(route(Method::Get, "/api/v1/tool/ls"), Route::NotFound);
        assert_eq!(route(Method::Get, "/api/v1/tool/stat"), Route::NotFound);
        assert_eq!(route(Method::Post, "/api/v1/import"), Route::NotFound);
        assert!(!path_exists("/api/v1/tool/ls"));
        assert!(!path_exists("/api/v1/tool/stat"));
        assert!(!path_exists("/api/v1/import"));
    }

    /// 分块上传的四条路由。
    #[test]
    fn routes_chunked_upload() {
        assert_eq!(
            route(Method::Post, "/api/v1/upload/begin"),
            Route::UploadBegin
        );
        assert_eq!(
            route(Method::Post, "/api/v1/upload/chunk"),
            Route::UploadChunk
        );
        assert_eq!(
            route(Method::Post, "/api/v1/upload/commit"),
            Route::UploadCommit
        );
        assert_eq!(
            route(Method::Post, "/api/v1/upload/abort"),
            Route::UploadAbort
        );
        // GET 不得落进这些路由（否则会走成 405 而不是 404）。
        assert_eq!(route(Method::Get, "/api/v1/upload/begin"), Route::NotFound);
        // `path_exists` 必须认识它们：否则方法用错时回 404 而不是 405。
        for path in [
            "/api/v1/upload/begin",
            "/api/v1/upload/chunk",
            "/api/v1/upload/commit",
            "/api/v1/upload/abort",
        ] {
            assert!(path_exists(path), "{path} 必须在 PATHS 中");
        }
    }

    /// 新增的 `config` 与 `rebind` 路由。
    ///
    /// `config` 是**同一路径两种方法**：GET 读、POST 写。这与其余端点不同，
    /// 因此单独钉住，避免将来有人只加了一半。
    #[test]
    fn routes_config_and_rebind() {
        assert_eq!(route(Method::Get, "/api/v1/config"), Route::Config);
        assert_eq!(route(Method::Post, "/api/v1/config"), Route::Config);
        assert_eq!(route(Method::Post, "/api/v1/rebind"), Route::Rebind);
        // 其余方法不得落进这两个路由。
        assert_eq!(route(Method::Options, "/api/v1/config"), Route::NotFound);
        assert_eq!(route(Method::Get, "/api/v1/rebind"), Route::NotFound);
    }

    /// `/api/v1/config/security`：镜像 SELinux 目标上下文的读与写。
    ///
    /// 与 `/api/v1/config`（USB 身份）**分开**是刻意的：两者的写入语义不同，
    /// 且共用端点会让「只想改标签」有覆盖身份的风险。这里同时钉住
    /// `path_exists`（否则方法用错会回 404 而不是 405）。
    #[test]
    fn routes_config_security() {
        assert_eq!(
            route(Method::Get, "/api/v1/config/security"),
            Route::ConfigSecurity
        );
        assert_eq!(
            route(Method::Post, "/api/v1/config/security"),
            Route::ConfigSecurity
        );
        assert_eq!(
            route(Method::Options, "/api/v1/config/security"),
            Route::NotFound
        );
        assert!(path_exists("/api/v1/config/security"));
        // 不能因为前缀匹配把别的路径也吞进来。
        assert_eq!(
            route(Method::Get, "/api/v1/config/security/extra"),
            Route::NotFound
        );
        assert!(!path_exists("/api/v1/config/security/extra"));
    }

    /// `config/security` 的 GET/POST 必须分派到不同的后端方法。
    #[test]
    fn config_security_route_dispatches_by_method() {
        let mut backend = FakeBackend::default();

        let response = call(&get("/api/v1/config/security", "t"), "t", &mut backend);
        assert_eq!(response.status, 200);
        assert_eq!(
            backend.calls.last().map(String::as_str),
            Some("config.security.get")
        );

        let response = call(
            &post("/api/v1/config/security", "t", br#"{"reset":true}"#),
            "t",
            &mut backend,
        );
        assert_eq!(response.status, 200);
        assert_eq!(
            backend.calls.last().map(String::as_str),
            Some("config.security.set")
        );

        // 该端点是「同路径两种方法」，因此没有第三种方法可用来验 405；
        // `path_exists` 单独断言（它正是 404/405 的分界），OPTIONS 仍是预检。
        assert!(path_exists("/api/v1/config/security"));
        let response = call(
            &TestRequest {
                head: Request {
                    method: Method::Options,
                    path: "/api/v1/config/security".into(),
                    query: None,
                    headers: vec![("authorization".into(), "Bearer t".into())],
                    content_length: None,
                    version: HttpVersion::Http11,
                },
                body: Vec::new(),
            },
            "t",
            &mut backend,
        );
        assert_eq!(response.status, 204, "预检不带凭据，必须先于鉴权");
    }

    #[test]
    fn routes_slot_delete() {
        assert_eq!(
            route(Method::Post, "/api/v1/slot/delete"),
            Route::SlotDelete
        );
        // 只接受 POST（它是改状态的操作）。
        assert_eq!(route(Method::Get, "/api/v1/slot/delete"), Route::NotFound);
    }

    #[test]
    fn routes_job_status_by_id() {
        assert_eq!(
            route(Method::Get, "/api/v1/jobs/abc123"),
            Route::Job("abc123".into())
        );
    }

    #[test]
    fn rejects_job_id_containing_a_slash() {
        // `/jobs/a/b` 不是合法 id，不能把它当成 id "a/b"。
        assert_eq!(route(Method::Get, "/api/v1/jobs/a/b"), Route::NotFound);
        assert_eq!(route(Method::Get, "/api/v1/jobs/"), Route::NotFound);
    }

    #[test]
    fn routes_state_changing_endpoints() {
        assert_eq!(route(Method::Post, "/api/v1/create"), Route::Create);
        assert_eq!(route(Method::Post, "/api/v1/delete"), Route::Delete);
        assert_eq!(route(Method::Post, "/api/v1/mount"), Route::Mount);
        assert_eq!(route(Method::Post, "/api/v1/unmount"), Route::Unmount);
        assert_eq!(
            route(Method::Post, "/api/v1/loop/attach"),
            Route::LoopAttach
        );
        assert_eq!(
            route(Method::Post, "/api/v1/loop/detach"),
            Route::LoopDetach
        );
    }

    /// `config` 的 GET/POST 必须分派到不同的后端方法。
    #[test]
    fn config_route_dispatches_by_method() {
        let mut backend = FakeBackend::default();
        let token = "t";

        let get_request = get("/api/v1/config", token);
        let response = call(&get_request, token, &mut backend);
        assert_eq!(response.status, 200);
        assert_eq!(backend.calls, vec!["config.get".to_string()]);

        backend.calls.clear();
        // 必须是合法 JSON：`config_set` 会解析它。
        let post_request = post("/api/v1/config", token, br#"{"id_vendor":4660}"#);
        let response = call(&post_request, token, &mut backend);
        assert_eq!(response.status, 200);
        assert_eq!(backend.calls, vec!["config.set".to_string()]);
    }

    /// 删除槽位走 `gdd` 通道（它要改 configfs）。
    #[test]
    fn slot_delete_route_dispatches_to_gdd() {
        let mut backend = FakeBackend::default();
        let token = "t";
        let request = post("/api/v1/slot/delete", token, br#"{"lun":1}"#);
        let response = call(&request, token, &mut backend);
        assert_eq!(response.status, 200);
        assert_eq!(backend.calls, vec!["gdd.slot_delete".to_string()]);
    }

    /// `rebind` 走 `gdd` 通道。
    #[test]
    fn rebind_route_dispatches_to_gdd() {
        let mut backend = FakeBackend::default();
        let token = "t";
        let request = post("/api/v1/rebind", token, b"{}");
        let response = call(&request, token, &mut backend);
        assert_eq!(response.status, 200);
        assert_eq!(backend.calls, vec!["gdd.rebind".to_string()]);
    }

    #[test]
    fn wrong_method_on_known_path_is_not_not_found() {
        // 关键：GET 一个只支持 POST 的路径，必须能区分出来（回 405）。
        assert_eq!(route(Method::Get, "/api/v1/mount"), Route::NotFound);
        assert!(path_exists("/api/v1/mount"), "路径本身存在，应回 405");
        assert!(!path_exists("/api/v1/nope"), "未知路径应回 404");
    }

    // ------------------------------------------------------------ 鉴权

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    fn request_with_auth(value: Option<&str>) -> Request {
        let mut headers = Vec::new();
        if let Some(v) = value {
            headers.push(("authorization".to_string(), v.to_string()));
        }
        Request {
            method: Method::Get,
            path: "/api/v1/status".into(),
            query: None,
            headers,
            content_length: None,
            version: HttpVersion::Http11,
        }
    }

    #[test]
    fn token_check_accepts_only_the_exact_bearer_token() {
        assert!(token_ok(
            &request_with_auth(Some("Bearer s3cret")),
            "s3cret"
        ));
        // 错误 token、缺 Bearer 前缀、缺失头部都必须拒绝。
        assert!(!token_ok(
            &request_with_auth(Some("Bearer wrong")),
            "s3cret"
        ));
        assert!(!token_ok(&request_with_auth(Some("s3cret")), "s3cret"));
        assert!(!token_ok(
            &request_with_auth(Some("Basic s3cret")),
            "s3cret"
        ));
        assert!(!token_ok(&request_with_auth(None), "s3cret"));
    }

    // ------------------------------------------------------------ 分发

    /// 记录被调用路由的替身后端。
    #[derive(Default)]
    struct FakeBackend {
        calls: Vec<String>,
        fail: bool,
    }

    impl FakeBackend {
        fn ok(&mut self, name: &str) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.calls.push(name.to_string());
            if self.fail {
                return Err((ErrorCode::ImageInUse, "占用了".into()));
            }
            Ok(serde_json::json!({ "handled": name }))
        }
    }

    impl Backend for FakeBackend {
        fn status(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("status")
        }
        fn images(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("images")
        }
        fn image_partitions(
            &mut self,
            path: &str,
        ) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok(&format!("partitions:{path}"))
        }
        fn loop_attachments(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("loop")
        }
        fn capabilities(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("capabilities")
        }
        fn job_status(&mut self, id: &str) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok(&format!("job:{id}"))
        }
        fn tool(
            &mut self,
            tool: Tool,
            path: &str,
        ) -> Result<serde_json::Value, (ErrorCode, String)> {
            let name = match tool {
                Tool::Df => "df",
            };
            self.ok(&format!("{name}:{path}"))
        }
        fn create(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("create")
        }
        fn delete(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("delete")
        }
        fn upload_begin(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("upload.begin")
        }
        fn upload_chunk(
            &mut self,
            upload_id: &str,
            offset: u64,
            mut body: impl std::io::Read,
        ) -> Result<serde_json::Value, (ErrorCode, String)> {
            let mut buf = Vec::new();
            let _ = body.read_to_end(&mut buf);
            self.ok(&format!("upload.chunk:{upload_id}:{offset}:{}", buf.len()))
        }
        fn upload_commit(
            &mut self,
            _body: &[u8],
        ) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("upload.commit")
        }
        fn upload_abort(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("upload.abort")
        }
        fn loop_attach(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("loop.attach")
        }
        fn loop_detach(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("loop.detach")
        }
        fn config_get(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("config.get")
        }
        fn config_set(&mut self, _body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("config.set")
        }
        fn config_security_get(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("config.security.get")
        }
        fn config_security_set(
            &mut self,
            _body: &[u8],
        ) -> Result<serde_json::Value, (ErrorCode, String)> {
            self.ok("config.security.set")
        }
        fn gdd_op(&mut self, action: GddAction) -> Result<serde_json::Value, (ErrorCode, String)> {
            let name = match action {
                GddAction::Mount(_) => "gdd.mount",
                GddAction::Unmount(_) => "gdd.unmount",
                GddAction::Rebind(_) => "gdd.rebind",
                GddAction::DeleteSlot(_) => "gdd.slot_delete",
            };
            self.ok(name)
        }
    }

    /// 构造 GET 请求。
    ///
    /// **必须像真实解析器那样拆出查询串**：`http::parse_request` 会把 `?`
    /// 之后的部分放进 `query`，助手若不拆，路由看到的就是带 `?` 的路径。
    /// 一个测试请求：头 + 体字节。
    ///
    /// `http::Request` 本身已不再携带请求体（体由 `http::RequestBody` 惰性管），
    /// 因此测试用一个薄封装把两者放在一起，助手就不必把体塞进头部。
    struct TestRequest {
        head: Request,
        body: Vec<u8>,
    }

    fn get(path_and_query: &str, token: &str) -> TestRequest {
        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (path_and_query.to_string(), None),
        };
        TestRequest {
            head: Request {
                method: Method::Get,
                path,
                query,
                headers: vec![("authorization".into(), format!("Bearer {token}"))],
                content_length: None,
                version: HttpVersion::Http11,
            },
            body: Vec::new(),
        }
    }

    fn post(path: &str, token: &str, body: &[u8]) -> TestRequest {
        TestRequest {
            head: Request {
                method: Method::Post,
                path: path.into(),
                query: None,
                headers: vec![("authorization".into(), format!("Bearer {token}"))],
                content_length: Some(body.len() as u64),
                version: HttpVersion::Http11,
            },
            body: body.to_vec(),
        }
    }

    /// 调一次 `handle`（体的读法由 `handle` 内部按路由决定）。
    fn call(request: &TestRequest, token: &str, backend: &mut FakeBackend) -> Response {
        let body = http::RequestBody::from_bytes(request.body.clone(), request.head.content_length);
        handle(&request.head, body, token, backend)
    }

    fn body_of(response: &Response) -> String {
        String::from_utf8(response.body.clone()).unwrap()
    }

    #[test]
    fn preflight_skips_authentication_and_returns_204() {
        // 浏览器不会给预检带 Authorization，若在此处鉴权则永远过不了预检。
        let request = TestRequest {
            head: Request {
                method: Method::Options,
                path: "/api/v1/mount".into(),
                query: None,
                headers: Vec::new(),
                content_length: None,
                version: HttpVersion::Http11,
            },
            body: Vec::new(),
        };
        let mut backend = FakeBackend::default();
        let response = call(&request, "tok", &mut backend);

        assert_eq!(response.status, 204);
        assert!(response.cors.preflight);
        assert!(backend.calls.is_empty(), "预检不得触发任何后端动作");
    }

    #[test]
    fn missing_or_wrong_token_is_401_and_touches_nothing() {
        for request in [
            get("/api/v1/status", "wrong"),
            TestRequest {
                head: Request {
                    method: Method::Get,
                    path: "/api/v1/status".into(),
                    query: None,
                    headers: Vec::new(),
                    content_length: None,
                    version: HttpVersion::Http11,
                },
                body: Vec::new(),
            },
        ] {
            let mut backend = FakeBackend::default();
            let response = call(&request, "right", &mut backend);
            assert_eq!(response.status, 401, "必须拒绝");
            assert!(backend.calls.is_empty(), "未鉴权不得触发后端动作");
        }
    }

    #[test]
    fn image_partitions_without_path_is_400() {
        let mut backend = FakeBackend::default();
        let response = call(&get("/api/v1/image/partitions", "t"), "t", &mut backend);
        assert_eq!(response.status, 400, "缺少 path 必须是 400 而不是 404");
        assert!(backend.calls.is_empty(), "参数缺失时不应调用后端");
    }

    #[test]
    fn image_partitions_rejects_wrong_method_with_405() {
        let mut backend = FakeBackend::default();
        let response = call(
            &post("/api/v1/image/partitions", "t", b"{}"),
            "t",
            &mut backend,
        );
        assert_eq!(response.status, 405, "路径存在但方法不对应为 405");
    }

    #[test]
    fn dispatches_each_route_to_its_backend_method() {
        let mut backend = FakeBackend::default();
        let cases: Vec<(TestRequest, &str)> = vec![
            (get("/api/v1/status", "t"), "status"),
            (get("/api/v1/images", "t"), "images"),
            (
                get(
                    "/api/v1/image/partitions?path=%2Fdata%2Fimages%2Fa.img",
                    "t",
                ),
                "partitions:/data/images/a.img",
            ),
            (get("/api/v1/loop", "t"), "loop"),
            (get("/api/v1/capabilities", "t"), "capabilities"),
            (get("/api/v1/jobs/j1", "t"), "job:j1"),
            (get("/api/v1/tool/df?path=%2Fdata", "t"), "df:/data"),
            (post("/api/v1/create", "t", b"{}"), "create"),
            (post("/api/v1/delete", "t", b"{}"), "delete"),
            (post("/api/v1/upload/begin", "t", b"{}"), "upload.begin"),
            (post("/api/v1/upload/commit", "t", b"{}"), "upload.commit"),
            (post("/api/v1/upload/abort", "t", b"{}"), "upload.abort"),
            (post("/api/v1/loop/attach", "t", b"{}"), "loop.attach"),
            (post("/api/v1/loop/detach", "t", b"{}"), "loop.detach"),
            (
                post("/api/v1/mount", "t", br#"{"devices":[]}"#),
                "gdd.mount",
            ),
            (
                post("/api/v1/unmount", "t", br#"{"lun":null}"#),
                "gdd.unmount",
            ),
            (get("/api/v1/config", "t"), "config.get"),
            (get("/api/v1/config/security", "t"), "config.security.get"),
            (
                post("/api/v1/config/security", "t", br#"{"reset":true}"#),
                "config.security.set",
            ),
        ];

        for (request, expected) in cases {
            let response = call(&request, "t", &mut backend);
            assert_eq!(response.status, 200, "{expected} 应成功");
            assert_eq!(backend.calls.last().map(String::as_str), Some(expected));
        }
    }

    #[test]
    fn unknown_path_is_404_and_wrong_method_is_405() {
        let mut backend = FakeBackend::default();

        let response = call(&get("/api/v1/nope", "t"), "t", &mut backend);
        assert_eq!(response.status, 404);

        // GET 一个只支持 POST 的路径 → 405，且不触发后端。
        let response = call(&get("/api/v1/mount", "t"), "t", &mut backend);
        assert_eq!(response.status, 405);
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn tool_without_path_parameter_is_400() {
        let mut backend = FakeBackend::default();
        let response = call(&get("/api/v1/tool/df", "t"), "t", &mut backend);
        assert_eq!(response.status, 400);
        assert!(body_of(&response).contains("path"));
    }

    #[test]
    fn malformed_json_body_is_400_not_500() {
        let mut backend = FakeBackend::default();
        let response = call(&post("/api/v1/mount", "t", b"not json"), "t", &mut backend);
        assert_eq!(response.status, 400);
        assert!(backend.calls.is_empty(), "解析失败不得触发后端动作");
    }

    #[test]
    fn error_responses_carry_error_and_message_fields() {
        // 字段名必须与 CLI 契约一致，否则 WebUI 的错误文案映射失效。
        let mut backend = FakeBackend {
            fail: true,
            ..Default::default()
        };
        let response = call(&get("/api/v1/status", "t"), "t", &mut backend);
        let body = body_of(&response);

        assert_eq!(response.status, 409, "image_in_use 应映射为 409");
        assert!(body.contains("\"error\":\"image_in_use\""), "得到 {body}");
        assert!(body.contains("\"message\""), "得到 {body}");
    }

    // ------------------------------------------------------------ 错误码映射

    #[test]
    fn status_mapping_covers_every_error_code() {
        // 穷举所有错误码，确保没有遗漏的 `match` 分支（编译器已强制），
        // 且映射结果落在合理区间。
        let cases = [
            (ErrorCode::InvalidArgument, 400),
            (ErrorCode::SizeBelowMinimum, 400),
            (ErrorCode::UnsupportedLayout, 400),
            (ErrorCode::PermissionDenied, 401),
            (ErrorCode::ImageNotFound, 404),
            (ErrorCode::ImageInUse, 409),
            (ErrorCode::Busy, 409),
            (ErrorCode::NotRegularFile, 409),
            (ErrorCode::NoSpace, 507),
            (ErrorCode::NoUdc, 500),
            (ErrorCode::LoopUnsupported, 500),
            (ErrorCode::FilesystemUnsupported, 500),
            (ErrorCode::MassStorageUnsupported, 500),
            (ErrorCode::Internal, 500),
        ];
        for (code, expected) in cases {
            assert_eq!(status_for(code), expected, "{code:?}");
        }
    }

    // ------------------------------------------------------------ 工具函数

    #[test]
    fn resolve_image_path_rejects_traversal() {
        let dirs = gadgetdisk_gdd::DataDirs::new("/tmp/gd-rest");
        assert!(resolve_image_path(&dirs, "ok.img").is_some());
        // 目录穿越与绝对路径都必须拒绝。
        assert!(resolve_image_path(&dirs, "../etc/passwd").is_none());
        assert!(resolve_image_path(&dirs, "a/b.img").is_none());
        assert!(resolve_image_path(&dirs, "").is_none());
        assert!(resolve_image_path(&dirs, "..").is_none());
    }

    #[test]
    fn random_token_is_hex_and_unguessable_length() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 64, "32 字节应为 64 个十六进制字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次生成必须不同");
    }

    #[test]
    fn write_api_info_is_atomic_and_owner_only() {
        let dir = crate::testutil::temp_dir("rest-apiinfo");
        let path = dir.join("webroot/api.json");
        let info = ApiInfo {
            port: 39001,
            token: "abc".into(),
        };

        write_api_info(&path, &info).unwrap();

        let read: ApiInfo = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, info);

        // 权限必须是 0600：token 在里面。
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "api.json 必须仅 root 可读");

        // 临时文件不得残留。
        assert!(!path.with_extension("json.tmp").exists());

        crate::testutil::cleanup(&dir);
    }

    #[test]
    fn api_info_round_trips_through_json() {
        let info = ApiInfo {
            port: 1234,
            token: "deadbeef".into(),
        };
        let text = serde_json::to_string(&info).unwrap();
        assert!(text.contains("\"port\":1234"));
        assert_eq!(serde_json::from_str::<ApiInfo>(&text).unwrap(), info);
    }
}
