//! `gadgetdisk serve`：WebUI 的 REST 后端（按需启动、空闲自动退出）。
//!
//! 设计理由见
//! [按需进程模型 Note](../../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。
//!
//! ## 与 gdd 的分工
//!
//! 本进程做**除 gadget 导出之外**的一切：镜像增删查、导入、loop 挂载、
//! 只读工具、能力探测。只有 `mount`/`unmount` 转发给 gdd —— 因为
//! gdd 是「已导出镜像」这件事的持有者与守卫。
//!
//! ## 什么时候需要问 gdd
//!
//! 删除/导入/loop 挂载都必须先确认目标镜像**没有**正被导出为 USB 设备。
//! 判据是向 gdd 要 `StatusResponse`（其中含各 LUN 的镜像路径）。
//! gdd 不在运行时说明**没有任何镜像在被导出**，因此无需拦截——
//! 这个降级方向是安全的：守卫缺席等价于无人占用。
//!
//! ## 生命周期
//!
//! 启动时把 `{port, token}` 原子写入模块 `webroot/api.json`（`0600`），
//! 那是 WebView 唯一能同源读到、而其他应用读不到的位置。空闲超过
//! `idle_timeout` 且无进行中的导入时退出，并删除该文件。

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::job::{self, JobRegistry};
use gadgetdisk_gdd::paths::DataDirs;

use crate::cli_paths::Offsets;
use gadgetdisk_proto::{ErrorCode, ImageLayout, ImageState, JobState, Message};

use crate::client::Connection;
use crate::gadget_adapter::{self, IdentityEditor};
use crate::http;
use crate::loop_adapter::LoopMounts;
use crate::rest::{self, ApiInfo, Backend, GddAction, Tool};
use crate::upload::UploadRegistry;

/// 非阻塞 `accept` 的轮询间隔。
///
/// **必须与空闲超时解耦且足够短**。早期实现用 `idle_timeout / 4`
/// （默认 60s / 4 = **15s**），于是每个请求最多要等 15 秒才被 `accept`：
/// 实测 `GET /api/v1/status` 稳定耗时 15000ms，三次连续请求各等 15s。
/// 这正是「点了没反应、要等刷新」的主因——不是任务慢，是监听循环睡着了。
///
/// 代价：每 25ms 一次空转唤醒（约 40 次/秒），相对一次请求处理可忽略。
pub const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// `serve` 的参数。
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// 数据目录。
    pub dirs: DataDirs,
    /// 模块根目录（`api.json` 写到它的 `webroot/` 下）。
    pub module_dir: PathBuf,
    /// 请求的端口；`0` 表示由内核分配临时端口。
    pub port: u16,
    /// 空闲多久后退出。
    pub idle_timeout: Duration,
}

impl ServeConfig {
    /// 以默认数据根与模块根构造。
    pub fn new(dirs: DataDirs, module_dir: PathBuf) -> Self {
        Self {
            dirs,
            module_dir,
            port: 0,
            idle_timeout: Duration::from_secs(60),
        }
    }

    /// `api.json` 的路径。
    pub fn api_json_path(&self) -> PathBuf {
        self.module_dir.join("webroot").join("api.json")
    }

    /// gdd 的 socket 路径。
    pub fn socket_path(&self) -> PathBuf {
        self.dirs.socket_path()
    }
}

/// 执行一次 REST 请求所需的全部真实能力。
///
/// `gadget` 只在需要探测导出状态时读取 configfs；`loop_mounts`
/// 负责 loop 挂载与内核真值探测。
pub struct LiveBackend {
    dirs: DataDirs,
    socket_path: PathBuf,
    loop_mounts: LoopMounts,
    jobs: JobRegistry,
    /// 进行中的分块上传（分块续写状态；`jobs` 管客户端可见的任务状态）。
    uploads: UploadRegistry,
    /// 模块根目录；用于定位自带的 `bin/mkfs.vfat`。
    ///
    /// 一次性 CLI 可能不在模块目录下运行（例如 `gd-deploy` 之外的手工调用），
    /// 故为 `Option`：`None` 时只探测系统工具。
    module_dir: Option<PathBuf>,
}

impl LiveBackend {
    /// 构造。
    pub fn new(dirs: DataDirs, socket_path: PathBuf) -> Self {
        Self {
            loop_mounts: LoopMounts::new(dirs.clone()),
            dirs,
            socket_path,
            jobs: JobRegistry::new(),
            uploads: UploadRegistry::new(),
            module_dir: None,
        }
    }

    /// 指定模块根目录，使自带 `mkfs.vfat` 可被探测到。
    pub fn with_module_dir(mut self, module_dir: impl Into<PathBuf>) -> Self {
        self.module_dir = Some(module_dir.into());
        self
    }

    /// 当前正被导出为 USB 设备的镜像路径（读 configfs 真值）。
    ///
    /// **不依赖 gdd 是否在运行**：`lun.0/file` 是内核状态，
    /// 任何时刻读到的都是事实。
    fn exported_images(&self) -> Vec<String> {
        // 只读 configfs 真值，因此「gdd 不在跑」时也能正确回答——这是刻意的：
        // 判据若是「问 gdd」，那么 gdd 缺席就会被误读成「没有导出」。
        gadget_adapter::read_export_state()
            .1
            .into_iter()
            .filter(|lun| !lun.image_path.is_empty())
            .map(|lun| lun.image_path)
            .collect()
    }

    /// 目标镜像是否正被导出；是则返回拒绝响应。
    fn ensure_not_exported(&self, image: &str) -> Result<(), (ErrorCode, String)> {
        if self
            .exported_images()
            .iter()
            .any(|exported| exported == image)
        {
            return Err((
                ErrorCode::ImageInUse,
                format!(
                    "image {image} is currently exported as a USB device; unmount the USB export first"
                ),
            ));
        }
        Ok(())
    }

    /// 是否有导入任务在跑（有则不因空闲退出）。
    fn has_running_jobs(&self) -> bool {
        self.jobs.running_count() > 0
    }
}

impl Backend for LiveBackend {
    fn status(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
        // 直接读 configfs 真值，因此 gdd 未运行（没有导出）时也能正确回答
        // 「未挂载」，而不是报「后端不可达」。
        let (udc, luns) = gadget_adapter::read_export_state();

        // 「有导出意图、但内核里没有我们的导出」= 意图尚未兑现（例如设备刚被
        // 拔出、gdd 已清理，而 CLI 还没被叫到）。**如实报告，不静默改写**：
        // 改写会掩盖「你的 U 盘已经掉了」这一事实。
        let intent = gadget_adapter::load_intent(&self.dirs);
        let pending = intent
            .as_ref()
            .map(|intent| !intent.is_empty() && luns.iter().all(|lun| !lun.attached))
            .unwrap_or(false);

        Ok(serde_json::json!({
            "udc": udc,
            "devices": luns,
            "pending_intent": pending,
            "intent": intent.map(|i| i.luns),
        }))
    }

    fn images(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
        let dir = self.dirs.images();
        let mut images: Vec<serde_json::Value> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                images.push(serde_json::json!({
                    "path": path.to_string_lossy(),
                    "size_bytes": metadata.len(),
                    "mtime": crate::job::mtime_secs(&metadata),
                    "layout": crate::job::detect_layout(&path),
                    "partition_offset_bytes": self.partition_offset(name),
                    "in_use": self.image_state(name),
                }));
            }
        }

        images.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(serde_json::json!({ "images": images }))
    }

    fn image_partitions(&mut self, path: &str) -> Result<serde_json::Value, (ErrorCode, String)> {
        let image = PathBuf::from(path);

        // 与 delete 同一条边界：只接受 images/ 下的直接子项，避免经 REST 读任意路径。
        let Some(name) = image.file_name().and_then(|n| n.to_str()) else {
            return Err((
                ErrorCode::InvalidArgument,
                "the path has no file name".to_string(),
            ));
        };
        if self.dirs.image_path(name).as_deref() != Some(image.as_path()) {
            return Err((
                ErrorCode::InvalidArgument,
                "only images under images/ can be read".to_string(),
            ));
        }

        let scan = gadgetdisk_core::read_partitions(&image).map_err(core_err)?;
        let partitions: Vec<serde_json::Value> = scan
            .partitions
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "index": entry.index,
                    "start_lba": entry.start_lba,
                    "offset_bytes": entry.offset_bytes(),
                    "size_bytes": entry.size_bytes,
                    "type_label": entry.type_label,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "path": image.to_string_lossy(),
            "layout": scan.layout.as_str(),
            "partitions": partitions,
            "default_index": scan.default_index(),
        }))
    }

    fn loop_attachments(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
        Ok(serde_json::json!({ "attachments": self.loop_mounts.attachments() }))
    }

    fn capabilities(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
        let caps = self.loop_mounts.capabilities();
        // `mass_storage_supported` 要读内核真值，而不是硬编码：
        // 已经有 LUN 或已探测到 UDC 就说明这颗内核支持它。
        let (udc, luns) = gadget_adapter::read_export_state();
        Ok(serde_json::json!({
            "loop_control": caps.loop_control,
            "max_part": caps.max_part,
            "filesystems": caps.filesystems,
            "mass_storage_supported": !luns.is_empty() || udc.is_some(),
            "selinux_enforcing": selinux_enforcing(),
            // 多 LUN 上限，供 UI 限制「添加设备」的行数。
            "max_luns": gadgetdisk_usb::MAX_LUNS,
            // INQUIRY 长度上限，供 UI 做输入校验（超长会被内核静默截断）。
            "inquiry_string_max": gadgetdisk_usb::INQUIRY_STRING_MAX,
            // 格式化工具探测结果：让 WebUI 如实展示"本机能用什么格式化"，
            // 而不是让用户提交后才发现某个文件系统不可用。
            "mkfs": crate::mkfs::probe_all(self.module_dir.as_deref()),
            // 各布局的分区数上限，供 UI 限制"添加分区"按钮。
            "max_partitions": {
                "raw": 1,
                "mbr": gadgetdisk_core::partspec::MBR_MAX_PRIMARY,
                "gpt": gadgetdisk_core::partspec::MAX_PARTITIONS,
            },
        }))
    }

    fn job_status(&mut self, id: &str) -> Result<serde_json::Value, (ErrorCode, String)> {
        match self.jobs.status(id) {
            Some(status) => Ok(serde_json::to_value(status).unwrap_or(serde_json::Value::Null)),
            None => Err((ErrorCode::InvalidArgument, format!("no such job: {id}"))),
        }
    }

    fn tool(&mut self, tool: Tool, path: &str) -> Result<serde_json::Value, (ErrorCode, String)> {
        let path = PathBuf::from(path);
        match tool {
            Tool::Df => {
                let (target, available, total) =
                    gadgetdisk_core::fsinfo::available_bytes(&path).map_err(core_err)?;
                Ok(df_payload(&target, available, total))
            }
        }
    }

    fn create(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            path: String,
            size_bytes: u64,
            #[serde(default)]
            layout: Option<ImageLayout>,
            #[serde(default)]
            filesystem: Option<String>,
            #[serde(default)]
            volume_label: Option<String>,
            #[serde(default)]
            partitions: Option<Vec<PartitionRequest>>,
        }

        /// 请求体里的单个分区。
        #[derive(serde::Deserialize)]
        struct PartitionRequest {
            /// 容量字节数；`0` 或缺省表示占满剩余空间。
            #[serde(default)]
            size_bytes: u64,
            /// GPT 类型线格式名（`gpt:<短名>` 或 `gpt:<GUID>`）。
            ///
            /// 与 `mbr_type` **互不相关**：哪个生效由 `layout` 决定。
            #[serde(default)]
            gpt_type: Option<String>,
            /// MBR 类型线格式名（`mbr:<短名>` 或 `mbr:0x<字节>`）。
            #[serde(default)]
            mbr_type: Option<String>,
            /// 分区名；缺省用 `MAIN`。**仅 GPT 会写入镜像**。
            #[serde(default)]
            name: Option<String>,
            /// 该分区的文件系统；`"none"` 表示**不格式化**；缺省继承全局默认。
            #[serde(default)]
            filesystem: Option<String>,
            /// MBR 下该分区是主分区还是逻辑分区（GPT/raw 下必须留空）。
            ///
            /// 容器（扩展分区）由写入侧自动创建——GPT 下给 `logical` 会被明确
            /// 拒绝：GPT 没有这个概念，静默当作主分区会让用户在 Host 上得到
            /// 与预期不符的分区表。
            #[serde(default)]
            kind: Option<String>,
        }

        let req: Body = parse_body(body)?;
        let target = crate::job::resolve_target(&self.dirs, &req.path);

        // 目标名正在被导入时不可同时创建。
        if let Some(name) = target.file_name().and_then(|n| n.to_str())
            && self.jobs.is_importing(name)
        {
            return Err((ErrorCode::ImageInUse, format!("{name} is being imported")));
        }
        self.ensure_not_exported(&target.to_string_lossy())?;

        // 同名阻断的**预检**：core 也会拒绝（那是权威判定），但在这里先查一次
        // 可以给出更具体的指引，并避免已经写了一半才发现。
        if target.exists() {
            return Err((
                ErrorCode::AlreadyExists,
                format!(
                    "{} already exists; choose another file name or delete that image first",
                    target.display()
                ),
            ));
        }

        let layout = match req.layout {
            Some(ImageLayout::Raw) => gadgetdisk_core::ImageLayout::Raw,
            Some(ImageLayout::Mbr) => gadgetdisk_core::ImageLayout::Mbr,
            // 缺省与 `gpt` 都走 GPT。
            Some(ImageLayout::Gpt) | None => gadgetdisk_core::ImageLayout::Gpt,
            Some(ImageLayout::Unknown) => {
                return Err((
                    ErrorCode::UnsupportedLayout,
                    "create does not support layout unknown".to_string(),
                ));
            }
        };

        // 全局文件系统：作为**各分区的默认值**。分区可各自覆盖，或显式设为
        // "不格式化"（`filesystem: "none"`）。
        let filesystem = match req.filesystem.as_deref() {
            None => gadgetdisk_core::FilesystemType::Fat32,
            Some("none") => gadgetdisk_core::FilesystemType::Fat32,
            Some(value) => gadgetdisk_core::FilesystemType::parse(value).ok_or_else(|| {
                (
                    ErrorCode::UnsupportedLayout,
                    format!("unsupported filesystem: {value}"),
                )
            })?,
        };

        // 分区列表缺省时留空，由 core 展开为"单分区占满剩余空间"——
        // 这保证既有调用方（不带 partitions 的请求体）行为完全不变。
        let mut partitions = Vec::new();
        for entry in req.partitions.unwrap_or_default() {
            // 类型分域解析：GPT 类型与 MBR 类型**互不相关**，各自独立给出。
            // 两者都可选——哪个生效由布局决定，无关的那个被忽略。
            let gpt_type = match entry.gpt_type.as_deref() {
                None => None,
                Some(value) => Some(gadgetdisk_core::GptPartitionType::parse(value).ok_or_else(
                    || {
                        (
                            ErrorCode::InvalidArgument,
                            format!("unsupported GPT partition type: {value}"),
                        )
                    },
                )?),
            };
            let mbr_type = match entry.mbr_type.as_deref() {
                None => None,
                Some(value) => Some(gadgetdisk_core::MbrPartitionType::parse(value).ok_or_else(
                    || {
                        (
                            ErrorCode::InvalidArgument,
                            format!("unsupported MBR partition type: {value}"),
                        )
                    },
                )?),
            };

            // 每分区文件系统**三态**：缺省 → 继承全局默认；`"none"` → 不格式化；
            // 其他 → 显式指定。用三态而非 `Option` 是因为后两者都是 `None`，
            // 合并会导致"请求不格式化"被套上全局默认。
            let partition_fs = match entry.filesystem.as_deref() {
                None => gadgetdisk_core::partspec::PartitionFilesystem::Inherit,
                Some("none") => gadgetdisk_core::partspec::PartitionFilesystem::None,
                Some(value) => gadgetdisk_core::partspec::PartitionFilesystem::Some(
                    gadgetdisk_core::FilesystemType::parse(value).ok_or_else(|| {
                        (
                            ErrorCode::UnsupportedLayout,
                            format!("unsupported filesystem: {value}"),
                        )
                    })?,
                ),
            };

            let name = entry
                .name
                .unwrap_or_else(|| gadgetdisk_core::create::DEFAULT_PARTITION_NAME.to_string());

            // 归属段：留空/`primary` → 主分区；`logical` → 逻辑分区。
            let kind = match entry.kind.as_deref() {
                None | Some("primary") => gadgetdisk_core::PartitionKind::Primary,
                Some("logical") => gadgetdisk_core::PartitionKind::Logical,
                Some(other) => {
                    return Err((
                        ErrorCode::InvalidArgument,
                        format!("unsupported kind: {other} (only primary / logical are accepted)"),
                    ));
                }
            };

            partitions.push(gadgetdisk_core::PartitionSpec {
                size_bytes: entry.size_bytes,
                gpt_type,
                mbr_type,
                name,
                filesystem: partition_fs,
                kind,
            });
        }

        let mut options = gadgetdisk_core::create::CreateOptions::new(&target)
            .with_size(req.size_bytes)
            .with_layout(layout)
            .with_filesystem(filesystem);
        if let Some(label) = req.volume_label {
            options = options.with_label(label);
        }
        if !partitions.is_empty() {
            options = options.with_partitions(partitions);
        }

        // 自带 `mkfs.vfat` 位于模块的 `bin/` 下；把它一并交给探测。
        //
        // 同时注入镜像 SELinux 上下文：创建时的格式化**经 loop 设备**完成，
        // 底层镜像由内核工作线程读写，标签未开放权限会导致 `mkfs` 直接失败（真机实测：
        // loop 与 gadget 导出受同等安全域限制）。目标值在此处统一解析一次，确保一次创建里的
        // 多个分区应用相同的上下文。
        let mut formatter = match &self.module_dir {
            Some(dir) => crate::mkfs::MkfsFormatter::with_module_dir(dir.clone()),
            None => crate::mkfs::MkfsFormatter::default(),
        };
        formatter = formatter.with_image_context(
            self.dirs.images(),
            crate::image_context::target_for(&self.dirs),
        );

        match gadgetdisk_core::create::create_image(options, &formatter) {
            Ok(created) => {
                // 持久化分区偏移，供 loop 的 lo_offset 路径直接使用。
                if created.layout.has_partition_table()
                    && let Some(name) = created.path.file_name().and_then(|n| n.to_str())
                {
                    let _ = Offsets::set(&self.dirs, name, created.first_partition_offset_bytes());
                }
                // 上下文警告（目录外 / 修正失败）随应答回报——**不阻断**创建流程：
                // 已经创建好的镜像本身仍然具备完整可用性（可导出、可后续再格式化）。
                let warnings = formatter.take_warnings();
                for warning in &warnings {
                    eprintln!("gadgetdisk serve: warning: {warning}");
                }
                Ok(serde_json::json!({
                    "path": created.path.to_string_lossy(),
                    "size_bytes": created.size_bytes,
                    "layout": created.layout.as_str(),
                    // 兼容字段：既有前端只读这一个偏移。
                    "partition_offset_bytes": created.first_partition_offset_bytes(),
                    // 英文诊断文本，UI 只作排查线索（与协议里 `message` 的地位一致）。
                    "warnings": warnings,
                    "partitions": created.partitions.iter().map(|p| {
                        // 类型按布局回**对应那一套**线格式；无关的那套为 null。
                        let gpt_wire = p.gpt_type.as_ref().map(|t| t.as_wire());
                        let mbr_wire = p.mbr_type.map(|t| t.as_wire());
                        let type_wire = match created.layout {
                            gadgetdisk_core::ImageLayout::Gpt
                            | gadgetdisk_core::ImageLayout::Raw => gpt_wire.clone(),
                            gadgetdisk_core::ImageLayout::Mbr => mbr_wire.clone(),
                        };
                        serde_json::json!({
                            "index": p.index,
                            "name": p.name,
                            "type": type_wire,
                            "gpt_type": gpt_wire,
                            "mbr_type": mbr_wire,
                            "offset_bytes": p.offset_bytes,
                            "size_bytes": p.size_bytes,
                            // `null` = 该分区未格式化（只写了分区表）。
                            "filesystem": p.filesystem.map(|f| f.as_str()),
                            "label": p.volume.as_ref().map(|v| v.label.clone()),
                            "tool": p.volume.as_ref().and_then(|v| v.tool.clone()),
                        })
                    }).collect::<Vec<_>>(),
                }))
            }
            Err(err) => Err(core_err(err)),
        }
    }

    fn delete(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            path: String,
        }

        let req: Body = parse_body(body)?;
        let path = PathBuf::from(&req.path);

        // 只允许删除 images/ 下的直接子项，避免经 REST 删任意路径。
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Err((
                ErrorCode::InvalidArgument,
                "the path has no file name".to_string(),
            ));
        };
        if self.dirs.image_path(name).as_deref() != Some(path.as_path()) {
            return Err((
                ErrorCode::InvalidArgument,
                "only images under images/ can be deleted".to_string(),
            ));
        }

        self.ensure_not_exported(&path.to_string_lossy())?;
        if self.jobs.is_importing(name) {
            return Err((ErrorCode::ImageInUse, format!("{name} is being imported")));
        }
        // loop 挂载中同样不可删除。
        if self
            .loop_mounts
            .attachments()
            .iter()
            .any(|a| a.image == path.to_string_lossy())
        {
            return Err((
                ErrorCode::ImageInUse,
                format!("image {name} is locally loop-mounted; unmount it before deleting"),
            ));
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {
                // 偏移缓存随镜像一起清理，否则同名镜像重建时会读到旧偏移。
                Offsets::remove(&self.dirs, name).ok();
                Ok(serde_json::json!({}))
            }
            Err(err) => {
                let code = fs_error_code(&gadgetdisk_core::CoreError::Io(std::io::Error::new(
                    err.kind(),
                    err.to_string(),
                )));
                Err((code, format!("cannot delete {}: {err}", path.display())))
            }
        }
    }

    fn upload_begin(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            dest_name: String,
            /// 客户端声明的大小。**只用于进度与空间预检**——provider 可能给出
            /// 0 或与实际不符的值，因此它不构成写入长度的权威。
            #[serde(default)]
            size_bytes: u64,
        }

        let req: Body = parse_body(body)?;

        // 目标已是导出镜像时拒绝（与 create/delete 同一条数据安全底线）。
        if let Some(dest) = self.dirs.image_path(&req.dest_name) {
            self.ensure_not_exported(&dest.to_string_lossy())?;
        }

        let upload_id = self
            .uploads
            .begin(&self.dirs, &self.jobs, &req.dest_name, req.size_bytes)
            .map_err(job_error)?;

        Ok(serde_json::json!({ "upload_id": upload_id }))
    }

    fn upload_chunk(
        &mut self,
        upload_id: &str,
        offset: u64,
        body: impl std::io::Read,
    ) -> Result<serde_json::Value, (ErrorCode, String)> {
        let written = self
            .uploads
            .chunk(&self.dirs, &self.jobs, upload_id, offset, body)
            .map_err(job_error)?;
        Ok(serde_json::json!({ "bytes_done": written }))
    }

    fn upload_commit(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            upload_id: String,
        }

        let req: Body = parse_body(body)?;
        let dest_name = self
            .uploads
            .commit(&self.dirs, &self.jobs, &req.upload_id)
            .map_err(job_error)?;

        // 与既有导入一致：**带 `job_id`**，让 WebUI 用同一套「有没有 job_id」
        // 分流逻辑（有则轮询、无则按终态收尾）。
        let status = self.jobs.status(&req.upload_id);
        Ok(serde_json::json!({
            "job_id": req.upload_id,
            "path": self
                .dirs
                .image_path(&dest_name)
                .map(|p| p.to_string_lossy().to_string()),
            "state": status.as_ref().map(|s| s.state).unwrap_or(JobState::Done),
            "bytes_done": status.as_ref().map(|s| s.bytes_done).unwrap_or(0),
        }))
    }

    fn upload_abort(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            upload_id: String,
        }

        let req: Body = parse_body(body)?;
        self.uploads
            .abort(&self.dirs, &self.jobs, &req.upload_id)
            .map_err(job_error)?;
        Ok(serde_json::json!({}))
    }

    fn loop_attach(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            image: String,
            #[serde(default)]
            read_only: bool,
            #[serde(default)]
            partition_index: Option<u32>,
        }

        let req: Body = parse_body(body)?;
        let path = PathBuf::from(&req.image);

        // 数据安全底线：正被导出为 USB 设备时不得本地挂载。
        self.ensure_not_exported(&req.image)?;

        // 镜像的 SELinux 安全上下文：loop 的底层镜像同样由**内核工作线程**读写，
        // 标签未开放读写权限时内核将拒绝（表现为 `mount` 报 EACCES/EIO，或写入被
        // 静默丢弃）。必须在 `LoopMounts::attach`（其内部 `LOOP_SET_FD`）**之前**
        // 完成——内核在打开后备文件的瞬间即按当时的标签锁定句柄权限。
        // 与 CLI 的 `run_attach_loop`、gadget 导出的 `gdd_op` 复用同一实现。
        let warnings = crate::image_context::check(&self.dirs, &path);
        for warning in &warnings {
            eprintln!("gadgetdisk serve: warning: {warning}");
        }

        match self
            .loop_mounts
            .attach(&path, req.read_only, req.partition_index)
        {
            Ok(attachment) => Ok(serde_json::json!({
                "loop_dev": attachment.loop_dev,
                "loop_part_devs": attachment.loop_part_devs,
                "mountpoint": attachment.mountpoint,
                "warnings": warnings,
            })),
            Err(err) => Err((err.code, err.message)),
        }
    }

    fn loop_detach(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(default)]
            image: Option<String>,
            #[serde(default)]
            loop_dev: Option<String>,
        }

        let req: Body = parse_body(body)?;
        if req.image.is_none() && req.loop_dev.is_none() {
            return Err((
                ErrorCode::InvalidArgument,
                "either image or loop_dev is required".to_string(),
            ));
        }

        let image = req.image.map(PathBuf::from);
        match self
            .loop_mounts
            .detach(image.as_deref(), req.loop_dev.as_deref())
        {
            Ok(released) => Ok(serde_json::json!({ "released": !released.is_empty() })),
            Err(err) => Err((err.code, err.message)),
        }
    }

    fn config_get(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
        let stored = load_identity(&self.dirs);
        // 镜像上下文也在同一个文件里，因此一并回显——诊断「为什么 PC 读不出
        // 内容」时需要看到它。
        let image_context = crate::cli_paths::GadgetConfig::load(&self.dirs);
        // 同时回**内核当前生效值**：用户最常问的是「我设的生效了吗」，
        // 只回文件内容无法回答这个问题。
        let effective = IdentityEditor::open().map(|editor| editor.read());
        Ok(serde_json::json!({
            "config": stored,
            "effective": effective,
            "image_context": image_context.resolved_image_context(),
            "image_context_configured": image_context.image_context,
            "path": crate::cli_paths::gadget_config(&self.dirs).to_string_lossy(),
        }))
    }

    fn config_set(&mut self, body: &[u8]) -> Result<serde_json::Value, (ErrorCode, String)> {
        let identity: gadgetdisk_usb::Identity = parse_body(body)?;
        save_and_apply_identity(&self.dirs, &identity)?;
        Ok(serde_json::json!({
            "config": identity,
            "path": crate::cli_paths::gadget_config(&self.dirs).to_string_lossy(),
        }))
    }

    fn config_security_get(&mut self) -> Result<serde_json::Value, (ErrorCode, String)> {
        let config = crate::cli_paths::GadgetConfig::load(&self.dirs);
        Ok(serde_json::json!({
            "image_context": config.resolved_image_context(),
            "configured": config.image_context,
            "default": crate::selinux::DEFAULT_IMAGE_CONTEXT,
            "path": crate::cli_paths::gadget_config(&self.dirs).to_string_lossy(),
        }))
    }

    fn config_security_set(
        &mut self,
        body: &[u8],
    ) -> Result<serde_json::Value, (ErrorCode, String)> {
        /// 请求体：传递 `image_context`（自定义设置）或 `reset: true`（恢复默认），二者**互斥**。
        ///
        /// 采用显式 `reset` 布尔值而非以空字符串表示清除：避免将用户误提交空输入框
        /// 与主动恢复默认设置混淆，二者具备截然不同的语义。
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(default)]
            image_context: Option<String>,
            #[serde(default)]
            reset: bool,
        }

        let req: Body = parse_body(body)?;

        // 先校验再落盘：非法值一旦写入 `config/gadget.json`，后续**每一次挂载**
        // 均将重新加载并再度失败，直至用户手动修正文件（遵循与
        // `save_and_apply_identity` 相同的架构纪律）。
        let value = match (req.image_context, req.reset) {
            (Some(_), true) => {
                return Err((
                    ErrorCode::InvalidArgument,
                    "image_context and reset are mutually exclusive".to_string(),
                ));
            }
            (Some(raw), false) => Some(
                crate::selinux::validate_context_format(&raw)
                    .map_err(|message| (ErrorCode::InvalidArgument, message))?,
            ),
            (None, true) => None,
            (None, false) => {
                return Err((
                    ErrorCode::InvalidArgument,
                    "either image_context or reset is required".to_string(),
                ));
            }
        };

        // 读—改—写：只动 `image_context`，**保留**同一文件里的 USB 身份。
        crate::cli_paths::GadgetConfig::store_image_context(&self.dirs, value.as_deref()).map_err(
            |err| {
                (
                    ErrorCode::Internal,
                    format!("failed to write the config: {err}"),
                )
            },
        )?;

        Ok(serde_json::json!({
            "image_context": value
                .clone()
                .unwrap_or_else(|| crate::selinux::DEFAULT_IMAGE_CONTEXT.to_string()),
            "configured": value,
            "default": crate::selinux::DEFAULT_IMAGE_CONTEXT,
            "path": crate::cli_paths::gadget_config(&self.dirs).to_string_lossy(),
            // 改标签需要**重新挂载**才生效：内核在打开后备文件时按当时的标签
            // pin 住它（见 docs/android-integration.md）。
            "applies_on": "next-mount",
        }))
    }

    fn gdd_op(&mut self, action: GddAction) -> Result<serde_json::Value, (ErrorCode, String)> {
        let socket_path = self.socket_path.clone();

        let (request, is_mount, is_unmount) = match action {
            GddAction::Mount(message) => (message, true, false),
            GddAction::Unmount(message) => (message, false, true),
            GddAction::Rebind(message) => (message, false, false),
            // 删除槽位不涉及身份，也不改变「导出的镜像集合」之外的东西，
            // 但它会**隐式解绑 UDC**（内核 rmdir 行为），因此仍需按内核真值
            // 更新导出意图。
            GddAction::DeleteSlot(message) => (message, false, false),
        };

        // 挂载前先应用身份：身份归 CLI，但它只在下次 bind 时生效，因此必须在
        // `gdd` 挂载**之前**写好。反过来会让这次挂载仍用旧描述符，用户看到
        // 「设了没效果」。
        if is_mount {
            // 镜像的 SELinux 上下文：内核线程要能读它，否则 PC 侧能认出设备但
            // 读不出内容。与 CLI 的 `run_mount` 共用同一个函数，避免两条通道漂移。
            // 目标值只解析一次，使同一批导出的所有 LUN 用同一个上下文。
            if let Message::MountRequest(ref req) = request {
                let target = crate::image_context::target_for(&self.dirs);
                for device in &req.devices {
                    for warning in crate::image_context::check_in(
                        &self.dirs.images(),
                        Path::new(&device.image_path),
                        &target,
                    ) {
                        eprintln!("gadgetdisk serve: warning: {warning}");
                    }
                }
            }

            let identity = load_identity(&self.dirs);
            if !identity.is_empty()
                && let Some(mut editor) = IdentityEditor::open()
            {
                if let Err(err) = editor.capture_backup_if_absent(&self.dirs) {
                    eprintln!(
                        "gadgetdisk serve: failed to back up the identity (continuing with the mount): {err}"
                    );
                }
                if let Err(err) = editor.apply(&identity) {
                    return Err((
                        ErrorCode::InvalidArgument,
                        format!("cannot apply the USB device identity: {err}"),
                    ));
                }
            }
        }

        let response = gdd_request(&self.dirs, &socket_path, &request)?;

        // 用**内核真值**更新导出意图：写「我们刚造成了什么」，而不是「我们请求
        // 了什么」。两者可能不同（某个 LUN 建失败、内核不支持 inquiry_string、
        // 镜像在写入前被删等）。
        match &response {
            Message::MountResponse(resp) => {
                let intent = gadget_adapter::intent_from_luns(&resp.devices);
                if let Err(err) = gadget_adapter::save_intent(&self.dirs, &intent) {
                    eprintln!(
                        "gadgetdisk serve: warning: failed to write the export intent: {err}"
                    );
                }
            }
            Message::UnmountResponse(resp) => {
                let intent = gadget_adapter::intent_from_luns(&resp.devices);
                if let Err(err) = gadget_adapter::save_intent(&self.dirs, &intent) {
                    eprintln!(
                        "gadgetdisk serve: warning: failed to update the export intent: {err}"
                    );
                }
                // 全部弹出时把 Android 身份还回去——这是「不再当 U 盘」的完整
                // 语义，否则手机会带着我们设的 VID/产品名继续跑 MTP。
                if intent.is_empty()
                    && is_unmount
                    && let Some(mut editor) = IdentityEditor::open()
                {
                    for failure in editor.restore_backup(&self.dirs) {
                        eprintln!("gadgetdisk serve: identity restore incomplete: {failure}");
                    }
                }
            }
            Message::DeleteSlotResponse(resp) => {
                // 删除槽位后剩下的 LUN 才是意图——被删的那个不该在重启时被恢复。
                let intent = gadget_adapter::intent_from_luns(&resp.devices);
                if let Err(err) = gadget_adapter::save_intent(&self.dirs, &intent) {
                    eprintln!(
                        "gadgetdisk serve: warning: failed to update the export intent: {err}"
                    );
                }
            }
            _ => {}
        }

        match response {
            Message::MountResponse(r) => Ok(serde_json::json!({ "devices": r.devices })),
            Message::UnmountResponse(r) => Ok(serde_json::json!({
                "released": r.released,
                "devices": r.devices,
            })),
            Message::RebindResponse(r) => Ok(serde_json::json!({ "udc": r.udc })),
            Message::DeleteSlotResponse(r) => Ok(serde_json::json!({ "devices": r.devices })),
            Message::Error(err) => Err((err.code, err.message)),
            other => Err((
                ErrorCode::Internal,
                format!("gdd returned an unexpected response: {other:?}"),
            )),
        }
    }
}
impl LiveBackend {
    /// 读取分区偏移缓存（缺失返回 `None`）。
    /// 镜像的分区起始偏移。
    ///
    /// 缓存优先（`create` 时写入，最快），**缓存缺失时从分区表推导**。
    /// 后者不可省：用户导入的镜像没有缓存，只读缓存会让列表对它们显示
    /// 「无偏移」，而实际挂载用的是分区表里的真实偏移（见 `LoopMounts::attach`）——
    /// 列表与挂载行为不一致本身就是个 bug。
    fn partition_offset(&self, name: &str) -> Option<u64> {
        let cached = Offsets::get(&self.dirs, name);
        if cached > 0 {
            return Some(cached);
        }

        let path = self.dirs.image_path(name)?;
        let scan = gadgetdisk_core::read_partitions(&path).ok()?;
        scan.default_index()
            .and_then(|index| scan.partitions.iter().find(|p| p.index == index))
            .map(|entry| entry.offset_bytes())
    }

    /// 镜像当前的占用状态（导出 / loop / 空闲）。
    ///
    /// 与 gdd 的内存状态机无关：这里从**内核真值**推导，因此
    /// gdd 未运行也能给出正确答案。
    fn image_state(&self, name: &str) -> ImageState {
        let Some(path) = self.dirs.image_path(name) else {
            return ImageState::None;
        };
        let path_str = path.to_string_lossy().into_owned();

        if self.exported_images().contains(&path_str) {
            return ImageState::Gadget;
        }
        if self
            .loop_mounts
            .attachments()
            .iter()
            .any(|a| a.image == path_str)
        {
            return ImageState::Loop;
        }
        if self.jobs.is_importing(name) {
            return ImageState::Importing;
        }
        ImageState::None
    }
}

/// `CoreError` → 协议错误码。
///
/// 逐项映射：把所有非 `NotFound` 都当权限错误会误导排查方向。
fn fs_error_code(err: &gadgetdisk_core::CoreError) -> ErrorCode {
    use gadgetdisk_core::fsinfo::FsError;
    match gadgetdisk_core::fsinfo::classify(err) {
        FsError::NotFound => ErrorCode::ImageNotFound,
        FsError::PermissionDenied => ErrorCode::PermissionDenied,
        FsError::Invalid => ErrorCode::InvalidArgument,
        FsError::Other => ErrorCode::Internal,
    }
}

/// 某文件系统在未指定类型时使用的默认分区类型。
///
/// 与 [`gadgetdisk_core::create::CreateOptions::effective_partitions`] 的映射
/// 必须一致：否则「显式给分区但不给类型」与「完全不给分区」会得到不同布局。
/// 解析 JSON 请求体。
fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, (ErrorCode, String)> {
    serde_json::from_slice(body).map_err(|err| {
        (
            ErrorCode::InvalidArgument,
            format!("the request body is not valid JSON: {err}"),
        )
    })
}

/// `CoreError` → 协议错误。
fn core_err(err: gadgetdisk_core::CoreError) -> (ErrorCode, String) {
    (crate::job::core_error_code(&err), err.to_string())
}

/// `df` 的应答体（CLI 的 `df` 子命令与 REST 的 `/api/v1/tool/df` 共用）。
///
/// **字段只此一处**：两个入口给出不同形状的 `df` 应答，会让 WebUI 在"回退到 CLI
/// 通道"时拿到缺字段的响应——这类不一致只有真的走回退通道才暴露。
///
/// `min_partition_bytes` 是**按文件系统**给出的分区下限，而不是"镜像下限"：
/// 镜像容量本身没有下限，能否成立取决于每个分区在其文件系统下是否够大。
pub fn df_payload(target: &std::path::Path, available: u64, total: u64) -> serde_json::Value {
    serde_json::json!({
        "path": target.to_string_lossy(),
        "available_bytes": available,
        "total_bytes": total,
        "min_partition_bytes": {
            "fat32": gadgetdisk_core::MIN_FAT32_BYTES,
            "exfat": gadgetdisk_core::MIN_EXFAT_BYTES,
            "ext4": gadgetdisk_core::MIN_EXT4_BYTES,
        },
        "default_image_bytes": gadgetdisk_core::DEFAULT_SIZE_BYTES,
    })
}

/// `JobError` → 协议错误。
fn job_error(err: job::JobError) -> (ErrorCode, String) {
    (crate::job::import_error_code(&err), err.to_string())
}

/// 读取 SELinux 当前模式（`Enforcing` 时为真）。
///
/// 读 `/sys/fs/selinux/enforce` 而不是解析 `getenforce` 输出：
/// 少一次进程创建，且不依赖命令是否存在。
fn selinux_enforcing() -> bool {
    std::fs::read_to_string("/sys/fs/selinux/enforce")
        .map(|t| t.trim() == "1")
        .unwrap_or(false)
}

/// 运行 `serve`，直到空闲退出。
///
/// 返回退出原因（仅用于日志与测试）。
pub fn run(config: ServeConfig) -> std::io::Result<()> {
    let token = rest::random_token()?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.port))?;
    // 关键：只绑 127.0.0.1。绝不能是 0.0.0.0（那会把能力暴露到局域网）。
    let port = listener.local_addr()?.port();

    let api_json = config.api_json_path();
    rest::write_api_info(
        &api_json,
        &ApiInfo {
            port,
            token: token.clone(),
        },
    )?;
    println!(
        "gadgetdisk serve: listening on 127.0.0.1:{port} (api.json={})",
        api_json.display()
    );

    let backend = Arc::new(Mutex::new(
        LiveBackend::new(config.dirs.clone(), config.socket_path())
            // 带上模块目录，使自带的 `bin/mkfs.vfat` 能被探测到。
            .with_module_dir(config.module_dir.clone()),
    ));

    // 一请求一连接：与既有协议一致，简单且无跨请求状态。
    //
    // **轮询间隔必须短**，不能跟着空闲超时走。早期实现用
    // `idle_timeout / 4`（默认 60s / 4 = **15s**）当轮询间隔，于是每个
    // 请求最多要等 15 秒才被 `accept`：实测 GET /api/v1/status 稳定耗时
    // 15000ms，三次连续请求各等 15s。这正是「点了没反应、要等刷新」的
    // 主因——不是任务慢，是**监听循环睡着了**。
    //
    // 代价：每 25ms 一次空转唤醒（约 40 次/秒），开销可忽略。
    listener.set_nonblocking(true)?;
    let mut last_activity = Instant::now();

    let result = loop {
        let idle = last_activity.elapsed() >= config.idle_timeout;
        // 有导入在跑时不得退出：那会中断用户的复制任务。
        let busy = backend
            .lock()
            .map(|b| b.has_running_jobs())
            .unwrap_or(false);
        if idle && !busy {
            println!(
                "gadgetdisk serve: idle for {}s, exiting",
                config.idle_timeout.as_secs()
            );
            break Ok(());
        }

        match listener.accept() {
            Ok((stream, _)) => {
                last_activity = Instant::now();
                let token = token.clone();
                let backend = Arc::clone(&backend);
                // 每个连接一个线程：一请求一连接，无共享可变状态。
                std::thread::spawn(move || {
                    http::serve_connection(stream, |request, body| match backend.lock() {
                        Ok(mut guard) => rest::handle(&request, body, &token, &mut *guard),
                        Err(_) => http::Response::json(
                            500,
                            serde_json::json!({
                                "error": "internal",
                                "message": "backend mutex is poisoned",
                            })
                            .to_string()
                            .into_bytes(),
                        ),
                    });
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(err) => break Err(err),
        }
    };

    // 退出前删除 api.json：留着过期的 token/端口只会让 WebUI 连到空气。
    std::fs::remove_file(&api_json).ok();
    println!("gadgetdisk serve: exited and removed api.json");
    result
}

/// 把一段文本写到 stdout（带换行与 flush）。
pub fn print_line(text: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// `gdd` 是否可连且能应答（真正的健康检查，而非只看 socket 文件是否存在）。
///
/// 只看文件会误判：崩溃留下的陈旧 socket 也在那里，而 `connect` 得到的是
/// `ECONNREFUSED`。必须发一条真实请求才算健康。
pub fn gdd_healthy(socket_path: &Path) -> bool {
    let Ok(mut connection) = Connection::connect(socket_path) else {
        return false;
    };
    if connection.handshake().is_err() {
        return false;
    }
    matches!(
        connection.request(&Message::StatusRequest),
        Ok(Message::StatusResponse(_))
    )
}

/// 确保 `gdd` 在运行；不在则拉起并等它就绪。
///
/// **`gdd` 是按需进程**：只在「有镜像被导出为 USB 设备」期间需要存在，因此
/// 不能假设它已经在跑。
///
/// 用 `setsid` 让它脱离调用方的会话：KernelSU 在脚本/命令返回后会回收其进程组，
/// 普通子进程会收到 `SIGKILL`（已实测 rc=137）；而 `gdd` 必须比调用它的那条
/// 命令活得更久——它要在拔线后继续观察并清理。
///
/// ## 为什么从这里找 `gdd` 而不是从 PATH
///
/// 模块包把两个二进制放在同一个 `bin/` 下（安装后的扁平布局）。用
/// `current_exe()` 的同级目录最可靠：它自动适配「模块目录」「开发者手工拷贝」
/// 「测试临时目录」三种情形，且不依赖 PATH——KernelSU 的 `su` 环境里 PATH
/// 未必包含模块目录。
pub fn ensure_gdd(dirs: &DataDirs, socket_path: &Path) -> Result<(), (ErrorCode, String)> {
    if gdd_healthy(socket_path) {
        return Ok(());
    }

    // 陈旧 socket 会让 bind 失败（误判 `AlreadyRunning`），先清掉。
    // 若其实有活着的 gdd，上面的健康检查已经返回了。
    std::fs::remove_file(socket_path).ok();

    let exe = gdd_binary_path().ok_or((
        ErrorCode::Internal,
        "cannot find the gdd executable (it should sit next to gadgetdisk)".to_string(),
    ))?;

    let mut child = Command::new("setsid")
        .arg(&exe)
        .arg("--data-dir")
        .arg(dirs.root())
        // 日志路径由**调用方**决定（`gdd` 只认 `--data-dir`）。放 `logs/gdd.log`，
        // 与 CLI 自己的 `logs/cli.log` 分开——共用文件会让「谁写的这一行」需要
        // 靠猜，而排查一次挂载恰恰最需要区分这一点。
        .arg("--log-file")
        .arg(crate::cli_paths::gdd_log(dirs))
        // 让 gdd 比调用方活得久，且不继承其标准流
        // （否则调用方会一直等这个管道关闭）。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| {
            (
                ErrorCode::Internal,
                format!("cannot start gdd ({}): {err}", exe.display()),
            )
        })?;

    // 最多等 5 秒：这是交互路径，不能让 WebUI 无限等待。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if gdd_healthy(socket_path) {
            // 不让 `Drop` 收尸：gdd 的寿命与本进程无关。
            std::mem::forget(child);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    Err((
        ErrorCode::Internal,
        "gdd was not ready within 5 seconds".to_string(),
    ))
}

/// `gdd` 可执行文件路径：优先 `--gdd` 覆盖，其次与本进程同目录。
///
/// 环境变量 `GADGETDISK_GDD` 优先，便于测试与手工排查时指向别处。
fn gdd_binary_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GADGETDISK_GDD") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join("gdd");
    sibling.is_file().then_some(sibling)
}

/// 发一条请求给 `gdd`（必要时先拉起它），把 `Error` 应答转成 `Err`。
///
/// 这是 CLI 侧访问 socket 的**唯一**入口，`mount`/`unmount`/`uninstall` 都用它，
/// 因此「确保在跑 → 握手 → 发请求 → 解错误」这套流程只有一份实现。
pub fn gdd_request(
    dirs: &DataDirs,
    socket_path: &Path,
    request: &Message,
) -> Result<Message, (ErrorCode, String)> {
    ensure_gdd(dirs, socket_path)?;

    let mut connection = Connection::connect(socket_path).map_err(|err| {
        (
            ErrorCode::Internal,
            format!("cannot connect to gdd ({}): {err}", socket_path.display()),
        )
    })?;
    connection
        .handshake()
        .map_err(|err| (ErrorCode::Internal, format!("gdd handshake failed: {err}")))?;

    match connection.request(request) {
        Ok(Message::Error(err)) => Err((err.code, err.message)),
        Ok(other) => Ok(other),
        Err(crate::client::ClientError::Server { code, message }) => Err((code, message)),
        Err(err) => Err((ErrorCode::Internal, err.to_string())),
    }
}

/// 读取持久身份配置（`config/gadget.json`）。
///
/// 文件不存在或损坏时返回**空身份**（不报错）：空身份的含义是「用户没有配置」，
/// 此时我们完全不碰 gadget 的 `idVendor`/字符串，让 Android 的原值继续生效。
/// 这正是 `Identity` 的「全字段可选」语义。
pub fn load_identity(dirs: &DataDirs) -> gadgetdisk_usb::Identity {
    std::fs::read_to_string(crate::cli_paths::gadget_config(dirs))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 写入身份配置，并**立即应用**到 configfs（若可用）。
///
/// ## 生效时机：下一次 UDC bind，而不是现在
///
/// 身份属性（`idVendor`/`idProduct`/`strings/*`）写在 configfs 上，但主机只有在
/// **重新枚举**时才会读到它们。本函数**不做**任何 UDC 断开/重绑：
///
/// - 自动重绑会让 USB 链路抖动（主机侧看到设备消失再出现），而用户只是在改一个
///   展示用的产品名；
/// - 真机观察：断开 USB 后 init 会按 `sys.usb.config` **重置 VID/PID**，所以「立刻
///   重绑一次」的收益也会被后续的断开抹掉。
///
/// 因此语义定为：**保存意图，下次连接生效**。持久意图以 `config/gadget.json` 为准，
/// 用户拔插数据线后由 init 的枚举路径读取当前值。WebUI 与 CLI 都会把这一点明确
/// 告诉用户，而不是假装已经生效。
///
/// 需要立刻生效的手动逃生口仍然存在（`mount --rebind` 与 `POST /api/v1/rebind`），
/// 但**没有任何自动调用方**。
pub fn save_and_apply_identity(
    dirs: &DataDirs,
    identity: &gadgetdisk_usb::Identity,
) -> Result<(), (ErrorCode, String)> {
    // 1. **先校验，再落盘。**
    //
    // 顺序在这里很关键。曾实现为「先写文件、再尝试应用」，理由是「即使应用失败
    // 用户的意图也已保存」。但非法输入一旦落盘，之后**每一次挂载**都会重新读到
    // 它并再次失败——一次手误输入会让导出功能永久不可用，直到用户手工删掉配置
    // 文件。AVD 实测踩中：写入一个非法产品名后，后续 `mount` 全部报
    // 「身份无法应用」。
    //
    // 非法输入（超长、序列号非 ASCII、含控制字符、越界）本就是**输入**错误，
    // 不该落盘。
    identity
        .validate()
        .map_err(|err| (ErrorCode::InvalidArgument, err.to_string()))?;

    // 2. 落盘。**必须走 `store_identity`**：它保留同一文件里的 `image_context`。
    //    曾经这里整文件覆盖 `Identity`，把用户的镜像上下文静默清空（AVD 实测）。
    crate::cli_paths::GadgetConfig::store_identity(dirs, identity).map_err(|err| {
        (
            ErrorCode::Internal,
            format!("failed to write the identity config: {err}"),
        )
    })?;

    // 3. 应用到 configfs（读回核实）。**到此为止，不碰 UDC。**
    let Some(mut editor) = IdentityEditor::open() else {
        // 无 configfs：文件已保存，等下次有 configfs 时生效。这不是失败。
        return Ok(());
    };

    if let Err(err) = editor.capture_backup_if_absent(dirs) {
        eprintln!("gadgetdisk: failed to back up the identity (continuing to apply): {err}");
    }
    if let Err(err) = editor.apply(identity) {
        // 不删已落盘的文件：应用失败往往是环境问题（strings 目录建不出来、
        // 内核读回不一致），而文件里存的是**用户明确表达的意图**。删掉它等于
        // 悄悄丢弃用户的输入，比「下次挂载再试并再次报错」更糟。
        // 非法输入已在上面的 `validate` 挡掉，因此不会出现「永远无法应用」的值。
        return Err((
            ErrorCode::InvalidArgument,
            format!("cannot apply the identity: {err}"),
        ));
    }

    Ok(())
}

/// 开机对账的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootOutcome {
    /// 按导出意图重新导出（列出实际恢复的 LUN 序号）。
    Resumed(Vec<u8>),
    /// 有意图，但全部镜像都已不存在。
    IntentDropped(Vec<String>),
    /// 没有意图，且内核里没有我们的残留。
    Nothing,
    /// 没有意图，但清理了内核里的残留。
    Cleaned,
    /// 失败（保留意图，下次开机再试）。
    Failed(String),
}

impl BootOutcome {
    /// 供日志与 stdout 展示的一句话。
    pub fn describe(&self) -> String {
        match self {
            BootOutcome::Resumed(luns) => {
                format!(
                    "restored {} LUN(s) from the export intent: {luns:?}",
                    luns.len()
                )
            }
            BootOutcome::IntentDropped(images) => {
                format!("the images in the export intent no longer exist; dropped them: {images:?}")
            }
            BootOutcome::Nothing => {
                "no export record to restore; kernel state is normal".to_string()
            }
            BootOutcome::Cleaned => {
                "no export intent; cleaned up leftover USB Function export links in the kernel"
                    .to_string()
            }
            BootOutcome::Failed(reason) => {
                format!("reconcile failed (intent kept; will retry on next boot): {reason}")
            }
        }
    }
}

/// 开机对账：把 configfs 收敛到 `run/state.json` 记录的导出意图。
///
/// ## 三种情况（顺序即优先级）
///
/// 1. **有导出意图** → 丢弃镜像已不存在的条目；若还有剩余，经 `gdd` 重新导出
///    同一组 LUN（含各自的模式与 INQUIRY）；意图里的镜像**全部**不存在时清空
///    意图（否则每次开机都白跑一次）。
/// 2. **没有意图、但内核里还有我们残留的 function/链接** → 清理干净。这是
///    「上次挂载中途失败」留下的半配置状态，**这一条是对账修复它的唯一路径**。
/// 3. **都没有** → 无事可做。
///
/// 失败时**保留** `run/state.json`：用户的意图不该被一次失败抹掉。
pub fn boot_reconcile(dirs: &DataDirs) -> BootOutcome {
    let intent = gadget_adapter::load_intent(dirs);

    // ---- 情况 3 的前置判断：完全没有意图 ----
    let Some(intent) = intent.filter(|intent| !intent.is_empty()) else {
        return if has_leftovers() {
            match gdd_request(
                dirs,
                &dirs.socket_path(),
                &Message::UnmountRequest(gadgetdisk_proto::UnmountRequest { lun: None }),
            ) {
                Ok(_) => BootOutcome::Cleaned,
                Err((_code, message)) => BootOutcome::Failed(message),
            }
        } else {
            BootOutcome::Nothing
        };
    };

    // ---- 情况 1：按意图恢复 ----
    let mut devices = Vec::new();
    let mut dropped = Vec::new();
    for lun in &intent.luns {
        if !Path::new(&lun.image_path).is_file() {
            dropped.push(lun.image_path.clone());
            continue;
        }
        devices.push(gadgetdisk_proto::MountDevice {
            lun: Some(lun.index),
            image_path: lun.image_path.clone(),
            mode: gadget_adapter::parse_mode(&lun.mode),
            inquiry_string: lun.inquiry_string.clone(),
        });
    }

    if devices.is_empty() {
        let _ = gadget_adapter::clear_intent(dirs);
        return BootOutcome::IntentDropped(dropped);
    }

    let request = Message::MountRequest(gadgetdisk_proto::MountRequest {
        devices,
        // 重启后 configfs 由 init 重建，我们的链接必然不在 → `gdd` 本来就会
        // 走紧凑段。这里不额外要求重绑。
        rebind: false,
    });
    match gdd_request(dirs, &dirs.socket_path(), &request) {
        Ok(Message::MountResponse(resp)) => {
            // 用内核真值回写意图：被丢弃的条目、以及内核没接受的项都会消失。
            let refreshed = gadget_adapter::intent_from_luns(&resp.devices);
            let _ = gadget_adapter::save_intent(dirs, &refreshed);
            BootOutcome::Resumed(resp.devices.iter().map(|lun| lun.index).collect())
        }
        Ok(other) => BootOutcome::Failed(format!("gdd returned an unexpected response: {other:?}")),
        // 失败**保留**意图（不删文件），让下次开机再试。
        Err((_code, message)) => BootOutcome::Failed(message),
    }
}

/// 内核里是否还有**我们**的 mass_storage 残留。
///
/// 判据是 configfs 真值：我们的 function 存在，或我们的配置链接存在。
/// 不看 `state.json`——那正是「没有意图」这一前提。
fn has_leftovers() -> bool {
    match gadget_adapter::GadgetReader::open() {
        Some(reader) => reader.storage().function_exists() || reader.storage().link_exists(),
        None => false,
    }
}

/// 追加一行到 `logs/service.log`。
///
/// 格式与其余日志一致（ISO8601 时间戳 + 等级 + 消息），由
/// [`gadgetdisk_gdd::logging`] 统一负责——包括超限时轮转一代（保留 `.1`）。
///
/// `service.log` 单独存在（而不是混进 `cli.log`）的理由：`service.sh` 只调用
/// `gadgetdisk boot`，把它的记录单独放一处，用户排查「开机到底做了什么」时
/// 不必在 CLI 的全部日志里翻找。
pub fn append_service_log(dirs: &DataDirs, line: &str) -> std::io::Result<()> {
    let path = crate::cli_paths::service_log(dirs);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 用与 logging 相同的轮转规则与行格式，但直接写这个**独立文件**：`boot` 的
    // 记录不该混进 CLI 的常规日志。
    rotate_log_if_needed(&path);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write as _;
    writeln!(
        file,
        "{} [INFO] {line}",
        gadgetdisk_gdd::logging::timestamp_utc()
    )
}

/// 超过上限时把 `<name>` 轮转为 `<name>.1`（保留一代）。
fn rotate_log_if_needed(path: &Path) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > gadgetdisk_gdd::logging::LOG_MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("log");
    let rotated = path.with_file_name(format!("{name}.1"));
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, &rotated);
}

#[cfg(test)]
mod tests {
    /// **非法身份不得落盘**。
    ///
    /// 回归（AVD 实测）：曾实现为「先写文件、再尝试应用」，理由是「即使应用失败
    /// 用户的意图也已保存」。但非法输入一旦落盘，之后**每一次挂载**都会重新读到
    /// 它并再次失败——一次手误输入会让导出功能永久不可用，直到手工删文件。
    ///
    /// 现在 `save_and_apply_identity` 先 `validate` 再落盘，因此这条测试断言的是
    /// 「文件没有被创建/修改」。
    #[test]
    fn invalid_identity_is_never_persisted() {
        let dir = crate::testutil::temp_dir("serve-identity-poison");
        let dirs = DataDirs::new(&dir);
        dirs.create_all().unwrap();

        // 序列号非 ASCII（真机实测会让电脑连不上设备；制造商/产品名不受此限）。
        let bad = gadgetdisk_usb::Identity {
            serial: Some("序列号".into()),
            ..gadgetdisk_usb::Identity::default()
        };
        let err = save_and_apply_identity(&dirs, &bad).unwrap_err();
        assert_eq!(err.0, ErrorCode::InvalidArgument);
        assert!(
            !crate::cli_paths::gadget_config(&dirs).exists(),
            "非法身份不得落盘：那样会让之后每次挂载都失败"
        );

        // 超长同理（按**字节**算：127 个 ASCII 字符 = 127 字节）。
        let too_long = gadgetdisk_usb::Identity {
            serial: Some("x".repeat(gadgetdisk_usb::STRING_MAX_BYTES + 1)),
            ..gadgetdisk_usb::Identity::default()
        };
        assert!(save_and_apply_identity(&dirs, &too_long).is_err());
        assert!(!crate::cli_paths::gadget_config(&dirs).exists());

        // 控制字符同理。
        let control = gadgetdisk_usb::Identity {
            product: Some("a\nb".into()),
            ..gadgetdisk_usb::Identity::default()
        };
        assert!(save_and_apply_identity(&dirs, &control).is_err());
        assert!(!crate::cli_paths::gadget_config(&dirs).exists());

        crate::testutil::cleanup(&dir);
    }

    /// `df` 的应答体形状：**按文件系统**的分区下限，且没有"镜像下限"字段。
    ///
    /// 这是 REST 与 CLI 两条通道共用的定义，因此两个入口不可能给出不同的形状。
    /// 早先这里是 `min_image_bytes`（一个数，取自 FAT32 下限）——那个字段本身就在
    /// 暗示"镜像有下限"，而这正是把 64 MiB 镜像误报成空间不足的根源。
    #[test]
    fn df_payload_reports_per_filesystem_floors_and_no_image_floor() {
        let payload = df_payload(Path::new("/data"), 123, 456);

        assert_eq!(payload["path"], "/data");
        assert_eq!(payload["available_bytes"], 123);
        assert_eq!(payload["total_bytes"], 456);
        assert_eq!(
            payload["default_image_bytes"],
            gadgetdisk_core::DEFAULT_SIZE_BYTES
        );

        // 三个文件系统各有自己的下限，且与 core 的常量同源。
        let floors = &payload["min_partition_bytes"];
        assert_eq!(floors["fat32"], gadgetdisk_core::MIN_FAT32_BYTES);
        assert_eq!(floors["exfat"], gadgetdisk_core::MIN_EXFAT_BYTES);
        assert_eq!(floors["ext4"], gadgetdisk_core::MIN_EXT4_BYTES);

        // 不能再出现"镜像下限"这种字段。
        assert!(
            payload.get("min_image_bytes").is_none(),
            "不应再有 min_image_bytes：镜像容量本身没有下限"
        );
    }

    /// **保存身份不得断开 USB**。
    ///
    /// 回归：M10 在保存身份后会自动请 `gdd` 重绑 UDC（`RebindRequest`），主机侧
    /// 会看到设备消失再出现——用户只是想改一个展示用的产品名。M11 起身份只写
    /// configfs，生效时机交给用户拔插数据线。
    ///
    /// ## 为什么是源码断言而不是行为断言
    ///
    /// `IdentityEditor` 是**具体类型**（包着 `RealConfigFs`），主机上没有 configfs
    /// 时 `open()` 直接返回 `None`，因此「有没有重绑」在主机上不可观测。而重绑是
    /// 唯一能碰 UDC 的路径，所以「这段代码里不再出现 `RebindRequest`」就是等价且
    /// 可断言的命题。这与 `gadgetdisk-gdd/tests/scope.rs` 用源码扫描钉住边界是
    /// 同一个手法。
    #[test]
    fn saving_identity_never_rebinds() {
        let source = include_str!("serve.rs");

        // 截出 `save_and_apply_identity` 的函数体：从签名到下一个顶层 `pub fn`。
        let start = source
            .find("pub fn save_and_apply_identity")
            .expect("应能找到该函数");
        let rest = &source[start..];
        let end = rest
            .find("\npub fn ")
            .expect("该函数后面应还有其它顶层函数");
        let body = &rest[..end];

        assert!(
            !body.contains("RebindRequest"),
            "保存身份不得触发 UDC 重绑（那会让 USB 链路抖动）——见 M11 的取舍"
        );
        assert!(
            !body.contains("udc_is_bound"),
            "保存身份不该再去判断 UDC 绑定状态：身份与挂载已解耦"
        );
        // 正例：它**必须**真的把身份写下去，否则上面两条会因「函数被掏空」而假通过。
        assert!(
            body.contains("store_identity"),
            "保存身份必须经 store_identity 落盘（否则会清空 image_context）"
        );
    }

    /// 保存身份在无 configfs 的主机环境里也必须成功（只落盘，不应用）。
    ///
    /// 这条覆盖「CLI 在非 Android 环境跑」的降级路径：身份文件已保存，等有
    /// configfs 时生效，不该报错。
    #[test]
    fn saving_identity_without_configfs_still_persists() {
        let dir = crate::testutil::temp_dir("serve-identity-no-configfs");
        let dirs = DataDirs::new(&dir);
        dirs.create_all().unwrap();

        let identity = gadgetdisk_usb::Identity {
            product: Some("磁盘存储".into()),
            ..gadgetdisk_usb::Identity::default()
        };
        save_and_apply_identity(&dirs, &identity).expect("无 configfs 时保存应成功");
        assert_eq!(
            crate::cli_paths::GadgetConfig::load(&dirs)
                .identity
                .product
                .as_deref(),
            Some("磁盘存储"),
            "身份应已落盘"
        );

        crate::testutil::cleanup(&dir);
    }

    /// `GET|POST /api/v1/config/security`：镜像 SELinux 目标上下文的读写。
    ///
    /// 三条必须同时成立的性质：
    /// 1. 未配置时 `GET` 回**内置默认值**（界面要显示「现在生效的是什么」）；
    /// 2. `POST` 落盘后 `GET` 与 `image_context` 一致；
    /// 3. **写入标签不得清空 USB 身份**——两者同在 `config/gadget.json`，
    ///    WebUI 的「保存镜像标签」与「保存身份」是两张卡片，互不干扰。
    #[test]
    fn config_security_read_write_preserves_the_identity() {
        let dir = crate::testutil::temp_dir("serve-config-security");
        let dirs = DataDirs::new(&dir);
        dirs.create_all().unwrap();

        // 先存一份身份：它是「写标签会不会顺手清掉身份」的对照物。
        let identity = gadgetdisk_usb::Identity {
            id_vendor: Some(0x18d1),
            product: Some("GD Storage".into()),
            ..gadgetdisk_usb::Identity::default()
        };
        save_and_apply_identity(&dirs, &identity).expect("保存身份应成功");

        let mut backend = LiveBackend::new(dirs.clone(), dirs.socket_path());

        // 1. 未配置 → 生效值是内置默认，且 `configured` 明确为空。
        let got = backend.config_security_get().unwrap();
        assert_eq!(
            got["image_context"].as_str(),
            Some(crate::selinux::DEFAULT_IMAGE_CONTEXT)
        );
        assert!(got["configured"].is_null(), "未配置时 configured 应为 null");
        assert_eq!(
            got["default"].as_str(),
            Some(crate::selinux::DEFAULT_IMAGE_CONTEXT)
        );

        // 2. 设置一个非默认值。
        let body = serde_json::json!({ "image_context": "u:object_r:vendor_file:s0" }).to_string();
        let saved = backend.config_security_set(body.as_bytes()).unwrap();
        assert_eq!(
            saved["image_context"].as_str(),
            Some("u:object_r:vendor_file:s0")
        );
        assert_eq!(
            saved["applies_on"].as_str(),
            Some("next-mount"),
            "必须说明改动在下次挂载才生效"
        );
        assert_eq!(
            backend.config_security_get().unwrap()["configured"].as_str(),
            Some("u:object_r:vendor_file:s0")
        );

        // 3. 身份必须原样还在。
        let identity_back = crate::cli_paths::GadgetConfig::load(&dirs).identity;
        assert_eq!(identity_back.id_vendor, Some(0x18d1));
        assert_eq!(identity_back.product.as_deref(), Some("GD Storage"));

        // 4. `reset` 清除并回落默认，身份同样不受影响。
        let reset = backend.config_security_set(br#"{"reset":true}"#).unwrap();
        assert_eq!(
            reset["image_context"].as_str(),
            Some(crate::selinux::DEFAULT_IMAGE_CONTEXT)
        );
        let after = crate::cli_paths::GadgetConfig::load(&dirs);
        assert_eq!(after.image_context, None);
        assert_eq!(after.identity.product.as_deref(), Some("GD Storage"));

        crate::testutil::cleanup(&dir);
    }

    /// 非法上下文必须**被拒绝且不落盘**。
    ///
    /// 与 `save_and_apply_identity` 同一条纪律：非法值一旦持久化，之后每一次
    /// 挂载都会重新读到它并再次失败，直到用户手工改文件。
    #[test]
    fn config_security_rejects_invalid_input_without_persisting() {
        let dir = crate::testutil::temp_dir("serve-config-security-bad");
        let dirs = DataDirs::new(&dir);
        dirs.create_all().unwrap();
        let mut backend = LiveBackend::new(dirs.clone(), dirs.socket_path());

        for body in [
            // 缺 `:`。
            br#"{"image_context":"media_rw_data_file"}"#.as_slice(),
            // 空值（不是「清除」——清除必须显式 `reset`）。
            br#"{"image_context":"   "}"#.as_slice(),
            // 含空白：xattr 会原样写入，内核随后拒绝。
            br#"{"image_context":"u:object_r:a b:s0"}"#.as_slice(),
            // 两个字段都给：语义有歧义，拒绝而不是猜。
            br#"{"image_context":"u:object_r:a:s0","reset":true}"#.as_slice(),
            // 都不给：调用方写错了。
            br#"{}"#.as_slice(),
        ] {
            let err = backend
                .config_security_set(body)
                .expect_err("非法请求体必须被拒绝");
            assert_eq!(err.0, ErrorCode::InvalidArgument, "body {body:?}");
        }

        assert_eq!(
            crate::cli_paths::GadgetConfig::load(&dirs).image_context,
            None,
            "非法值不得落盘"
        );

        crate::testutil::cleanup(&dir);
    }

    /// 创建镜像时**必须**把上下文交给 formatter，且警告随应答回报。
    ///
    /// ## 为什么是源码断言
    ///
    /// 真实的 `create_image` 要跑 `mkfs`（主机上没有该工具）且需要 loop 设备；
    /// 主机上无法端到端跑。而这里真正要守住的是两件事：formatter **拿到了**
    /// 上下文（否则真机上 `mkfs` 会因为标签被拒），以及警告**回到了应答里**
    /// （否则用户看不到「标签没改成」这个后果）。两者都是调用点的形状。
    #[test]
    fn create_passes_the_image_context_to_the_formatter() {
        let source = include_str!("serve.rs");

        let start = source
            .find("    fn create(&mut self, body: &[u8])")
            .expect("应能找到 LiveBackend::create");
        let rest = &source[start..];
        let end = rest
            .find("\n    fn delete(")
            .expect("create 后面应是 delete");
        let body = &rest[..end];

        let inject = body
            .find("with_image_context(")
            .expect("创建时必须把镜像目录与目标上下文交给 formatter（真机 loop 格式化受同一限制）");
        let create = body
            .find("create_image(options, &formatter)")
            .expect("create 必须调用 create_image");
        assert!(
            inject < create,
            "上下文必须在 create_image（其内部经 loop 格式化）**之前**注入"
        );
        assert!(
            body.contains("take_warnings()"),
            "formatter 收集到的上下文警告必须取出"
        );
        assert!(
            body.contains("\"warnings\": warnings"),
            "警告必须随应答回报——改标签失败时用户看不到后果"
        );
    }

    use super::*;

    /// 轮询间隔必须远小于空闲超时。
    ///
    /// 回归：早期用 `idle_timeout / 4`（默认 15s）当轮询间隔，导致每个请求
    /// 最多排队 15 秒才被 `accept`——实测 GET /api/v1/status 稳定 15000ms。
    /// 这条测试把「间隔与超时解耦」钉死，避免有人再把它改回去。
    #[test]
    fn accept_poll_interval_is_decoupled_from_idle_timeout() {
        let default_idle =
            ServeConfig::new(DataDirs::new("/tmp/x"), PathBuf::from("/tmp/m")).idle_timeout;
        assert!(
            ACCEPT_POLL_INTERVAL * 100 < default_idle,
            "轮询间隔 {ACCEPT_POLL_INTERVAL:?} 必须远小于空闲超时 {default_idle:?}"
        );
        assert!(
            ACCEPT_POLL_INTERVAL <= Duration::from_millis(100),
            "轮询间隔过大会让请求排队等 accept"
        );
    }

    /// 端到端：一个真实监听 + 真实连接，请求延迟必须远低于空闲超时。
    ///
    /// 这条覆盖真正的失败模式——`accept` 循环睡着时，请求本身完全正常，
    /// 只是要等到下一次轮询才被处理；单测 handler 是测不出来的。
    #[test]
    fn real_request_is_served_promptly() {
        use std::io::{BufRead, BufReader, Write};

        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        // 用一个足够长的空闲超时：若实现又把它当轮询间隔，本测试会明显变慢。
        let idle = Duration::from_secs(60);
        let tick = ACCEPT_POLL_INTERVAL;
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        http::serve_connection(stream, |_req, _body| {
                            http::Response::json(200, br#"{"ok":true}"#.to_vec())
                        });
                        return true;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(tick);
                    }
                    Err(_) => return false,
                }
            }
            false
        });
        // 让 accept 循环先进入轮询状态。
        std::thread::sleep(Duration::from_millis(50));

        let started = Instant::now();
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        // 读超时取得比「旧实现的 15s 轮询」更长：这样回归时失败来自下面那条
        // elapsed 断言（带可读信息），而不是一个光秃秃的 socket 超时。
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        // 显式要求关闭：本测试关心的是「accept 循环醒不醒」，不是连接复用。
        // 不写的话（HTTP/1.1 默认 keep-alive）服务端会保持连接直到空闲超时，
        // 本测试就得白等 10s——那会把一个「响应是否及时」的断言变成一个
        // 「连接何时关闭」的断言。
        stream
            .write_all(
                b"GET /api/v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        // 不直接 unwrap：回归时监听线程会先超时退出，连接随之结束，
        // 这里可能拿到 EOF 或 WouldBlock。先取耗时，好给出可读的失败信息。
        let read_result = reader.read_line(&mut line);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "请求耗时 {elapsed:?}（读取结果 {read_result:?}，响应行 {line:?}），\
             远超轮询间隔 {tick:?}（空闲超时 {idle:?}）——accept 循环又睡着了"
        );
        read_result.unwrap();
        assert!(line.contains("200"), "响应行异常：{line:?}");
        assert!(handle.join().unwrap(), "accept 循环未处理到请求");
    }

    /// 不存在的 socket 必须被判为「不健康」。
    ///
    /// 回归：早期实现只看 socket **文件**是否存在。崩溃留下的陈旧文件也在
    /// 那里，于是健康检查会误判为「gdd 活着」，既不拉起也不报错，
    /// 后续 `connect` 才以 `ECONNREFUSED` 失败。
    #[test]
    fn missing_socket_is_unhealthy() {
        let root = crate::testutil::temp_dir("serve-missing-sock");
        let dirs = DataDirs::new(&root);
        assert!(!gdd_healthy(&dirs.socket_path()));
        crate::testutil::cleanup(&root);
    }

    /// 普通文件冒充 socket 也必须被判为「不健康」，而不是 panic。
    #[test]
    fn regular_file_in_place_of_socket_is_unhealthy() {
        let root = crate::testutil::temp_dir("serve-fake-sock");
        let dirs = DataDirs::new(&root);
        std::fs::create_dir_all(dirs.run()).unwrap();
        std::fs::write(dirs.socket_path(), b"not a socket").unwrap();
        assert!(!gdd_healthy(&dirs.socket_path()));
        crate::testutil::cleanup(&root);
    }

    /// `ensure_gdd` 在无法拉起 `gdd` 时必须返回**明确的错误**，
    /// 而不是假装成功——否则调用方会继续 `connect` 并报一个更难懂的错。
    ///
    /// 这里用一个不存在的数据根，使 gdd 无法 bind。
    #[test]
    fn ensure_daemon_reports_a_clear_error_when_it_cannot_start() {
        let root = crate::testutil::temp_dir("serve-ensure-fail");
        let dirs = DataDirs::new(&root);
        let sock = dirs.socket_path();
        // 让 socket 路径的父目录是一个**普通文件**，gdd 必然无法创建 socket。
        std::fs::create_dir_all(dirs.run()).ok();
        std::fs::write(&sock, b"x").ok();
        // 用一个不可能存在的可执行文件来拉起：current_exe 在测试里是测试二进制，
        // 它会以未知参数退出，因此这里预期超时或错误——关键是**不 panic**。
        let result = ensure_gdd(&dirs, &sock);
        assert!(result.is_err(), "无法启动时必须返回错误");
        crate::testutil::cleanup(&root);
    }

    /// **导出的导入必须立即返回，且复制期间不持有后端锁。**
    ///
    /// 分块上传的完整往返：`begin` → 多个 `chunk` → `commit`。
    ///
    /// 覆盖三件必须同时成立的事：
    /// 1. `begin` 立即返回 `upload_id`，且**目标镜像此时还不存在**（半成品不可见）；
    /// 2. `chunk` 的偏移必须与已写长度一致（顺序追加，不允许空洞）；
    /// 3. `commit` 之后内容与源**逐字节一致**，且 `tmp/` 不留残骸。
    #[test]
    fn chunked_upload_round_trip() {
        let root = crate::testutil::temp_dir("serve-upload-round-trip");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();

        let mut backend = LiveBackend::new(dirs.clone(), dirs.socket_path());

        // 刻意让块边界不对齐，验证拼接。
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
        let begin = serde_json::json!({
            "dest_name": "uploaded.img",
            "size_bytes": payload.len(),
        })
        .to_string();
        let response = backend.upload_begin(begin.as_bytes()).unwrap();
        let upload_id = response
            .get("upload_id")
            .and_then(|v| v.as_str())
            .expect("begin 必须返回 upload_id")
            .to_string();

        // 半成品不可见：commit 之前 `images/` 里不得有目标。
        assert!(
            !dirs.image_path("uploaded.img").unwrap().exists(),
            "commit 前目标镜像不得出现"
        );
        // 上传期间必须登记 running job，否则 `serve` 会空闲退出把上传掐断。
        assert!(
            backend.jobs.is_importing("uploaded.img"),
            "上传期间目标名必须处于 importing"
        );

        let mut offset = 0u64;
        for piece in payload.chunks(333) {
            let out = backend
                .upload_chunk(&upload_id, offset, std::io::Cursor::new(piece.to_vec()))
                .unwrap();
            offset += piece.len() as u64;
            assert_eq!(out.get("bytes_done").and_then(|v| v.as_u64()), Some(offset));
        }

        let commit = serde_json::json!({ "upload_id": upload_id }).to_string();
        let done = backend.upload_commit(commit.as_bytes()).unwrap();
        assert!(
            done.get("job_id").is_some(),
            "commit 应回 job_id 供 UI 分流"
        );

        let got = std::fs::read(dirs.image_path("uploaded.img").unwrap()).unwrap();
        assert_eq!(got, payload, "上传结果必须与源逐字节一致");
        assert!(
            !dirs.tmp().join(format!("{upload_id}.part")).exists(),
            "完成后不得残留暂存文件"
        );

        crate::testutil::cleanup(&root);
    }

    /// 乱序分块必须被拒绝，且**不会在暂存文件里留下空洞**。
    #[test]
    fn out_of_order_chunk_is_rejected() {
        let root = crate::testutil::temp_dir("serve-upload-order");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let mut backend = LiveBackend::new(dirs.clone(), dirs.socket_path());

        let begin = serde_json::json!({ "dest_name": "x.img", "size_bytes": 100 }).to_string();
        let upload_id = backend.upload_begin(begin.as_bytes()).unwrap()["upload_id"]
            .as_str()
            .unwrap()
            .to_string();

        backend
            .upload_chunk(&upload_id, 0, std::io::Cursor::new(b"abcde".to_vec()))
            .unwrap();

        let err = backend
            .upload_chunk(&upload_id, 50, std::io::Cursor::new(b"xyz".to_vec()))
            .expect_err("跳号必须被拒绝");
        assert_eq!(err.0, ErrorCode::InvalidArgument, "乱序是客户端错误（400）");

        // 暂存文件仍只有已顺序写入的 5 字节，没有空洞。
        assert_eq!(
            std::fs::metadata(dirs.tmp().join(format!("{upload_id}.part")))
                .unwrap()
                .len(),
            5
        );

        crate::testutil::cleanup(&root);
    }

    /// `abort` 必须清理暂存，且目标永不出现。
    #[test]
    fn abort_cleans_up_without_creating_destination() {
        let root = crate::testutil::temp_dir("serve-upload-abort");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let mut backend = LiveBackend::new(dirs.clone(), dirs.socket_path());

        let begin = serde_json::json!({ "dest_name": "gone.img", "size_bytes": 10 }).to_string();
        let upload_id = backend.upload_begin(begin.as_bytes()).unwrap()["upload_id"]
            .as_str()
            .unwrap()
            .to_string();
        backend
            .upload_chunk(&upload_id, 0, std::io::Cursor::new(b"xy".to_vec()))
            .unwrap();

        let abort = serde_json::json!({ "upload_id": upload_id }).to_string();
        backend.upload_abort(abort.as_bytes()).unwrap();

        assert!(!dirs.tmp().join(format!("{upload_id}.part")).exists());
        assert!(!dirs.image_path("gone.img").unwrap().exists());
        // 放弃后不得再有 running job，否则 `serve` 永不退出。
        assert_eq!(backend.jobs.running_count(), 0);

        crate::testutil::cleanup(&root);
    }

    /// 同名目标：`begin` 就必须拒绝，而不是传完才失败。
    #[test]
    fn upload_begin_rejects_existing_destination() {
        let root = crate::testutil::temp_dir("serve-upload-dup");
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        let mut backend = LiveBackend::new(dirs.clone(), dirs.socket_path());

        std::fs::write(dirs.image_path("taken.img").unwrap(), b"x").unwrap();
        let begin = serde_json::json!({ "dest_name": "taken.img", "size_bytes": 1 }).to_string();
        let err = backend
            .upload_begin(begin.as_bytes())
            .expect_err("同名必须被拒绝");
        assert_eq!(err.0, ErrorCode::AlreadyExists);

        crate::testutil::cleanup(&root);
    }
}
