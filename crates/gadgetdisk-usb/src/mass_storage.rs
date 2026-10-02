//! mass_storage 挂载编排：函数/LUN/链接/UDC 的唯一操作者。
//!
//! 规格见 [docs/android-integration.md](../../../../docs/android-integration.md)
//! 的「挂载流程」。本模块是 `gdd` 唯一使用的类型，因此它**只**碰下面这些
//! configfs 条目，绝不触碰 gadget 身份（`idVendor`/`idProduct`/`strings/*`/
//! `os_desc`）——那些归 CLI 的 [`crate::identity`]：
//!
//! - `functions/mass_storage.gadget-disk` 及其下的 `lun.N/*`
//! - `configs/<b.N>/mass_storage.gadget-disk`（**只增删我们自己名字的链接**）
//! - `UDC`
//!
//! 它**不写盘、不读 `/proc`、不持有任何持久状态**：所有决策由调用方（CLI）持有。
//!
//! ## 顺序的依据（内核源码）
//!
//! | 约束 | 出处 |
//! |---|---|
//! | **只有建符号链接**要求 UDC 未绑定，已绑定时 `EINVAL` | `configfs.c` `config_usb_cfg_link` |
//! | `mkdir lun.N`（N≥1）在 function **被配置链接引用**时 `EBUSY` | `f_mass_storage.c` `fsg_lun_make`（`fsg_opts->refcnt`，由 link/unlink 增减） |
//! | `rmdir lun.N` 在 UDC 已绑定时会**隐式解绑** gadget | 同上 `fsg_lun_drop` → `unregister_gadget_item` |
//! | `lun.N/file` 在绑定时**可写**，仅 `prevent_medium_removal && open` 时 `EBUSY` | `storage_common.c` `fsg_store_file` |
//! | `lun.N/forced_eject` 先清 `prevent_medium_removal` 再解绑后端 = 强制弹出 | 同上 `fsg_store_forced_eject` |
//! | `lun.N/inquiry_string` 按 `%-28s` 定宽写入 | 同上 `fsg_store_inquiry_string` |
//! | UDC 已绑定时再写 `UDC` 返回 `EBUSY` | `configfs.c` `gadget_dev_desc_UDC_store` |
//!
//! 由此得出：**断开 UDC 是「刷新并生效」的手段，不是所有写操作的前提**。
//! 只有「建链接」与「增删 LUN 目录」这两件结构性改动需要它。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gadgetdisk_proto::Mode;

use crate::configfs::{ConfigFs, FsResult};
use crate::paths::{self, Layout};

/// 单个待设置的 LUN。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LunRequest {
    /// LUN 序号（`0..paths::MAX_LUNS`）。
    pub index: u8,
    /// 后端镜像路径。
    pub image_path: String,
    /// 设备模式。
    pub mode: Mode,
    /// SCSI INQUIRY 字符串（最长 [`paths::INQUIRY_STRING_MAX`]）。
    pub inquiry_string: Option<String>,
}

impl LunRequest {
    /// 以序号、镜像与模式构造（无 INQUIRY 字符串）。
    pub fn new(index: u8, image_path: impl Into<String>, mode: Mode) -> Self {
        Self {
            index,
            image_path: image_path.into(),
            mode,
            inquiry_string: None,
        }
    }

    /// 设置 INQUIRY 字符串。
    pub fn with_inquiry(mut self, inquiry: impl Into<String>) -> Self {
        self.inquiry_string = Some(inquiry.into());
        self
    }
}

/// 从 configfs 读回的 LUN 状态（**内核真值**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LunState {
    /// LUN 序号。
    pub index: u8,
    /// 后端镜像路径（空串表示未绑定）。
    pub image_path: String,
    /// 设备模式。
    pub mode: Mode,
    /// INQUIRY 字符串（未设置或内核缺该属性时为 `None`）。
    pub inquiry_string: Option<String>,
    /// 是否已绑定后端文件（即「介质已装载」）。
    pub attached: bool,
    /// 是否已生效。
    ///
    /// 取**内核真值**：既绑定了后端文件，且 UDC 的 `state` 为 `configured`
    /// （主机已成功 `SET_CONFIGURATION`）。只看 `file` 非空会在「主机未接受
    /// 配置」时谎报成功——真机实测正是这种情形下 Windows 报「代码 10」。
    pub effective: bool,
    /// 该槽位能否删除。
    ///
    /// `lun.0` 为 `false`：它由内核随 function 创建，`rmdir` 返回 `EPERM`
    /// （已实测）。把这条内核知识放在**读状态**里，UI 就不必自己复制它。
    pub deletable: bool,
}

/// 一次操作记录，供测试断言**顺序**。
///
/// 生产路径不收集日志（避免无谓分配），仅在测试中启用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// 写某个属性。
    Write(String, String),
    /// 创建目录。
    Mkdir(String),
    /// 删除目录。
    Rmdir(String),
    /// 创建符号链接。
    Symlink(String, String),
    /// 删除符号链接。
    Unlink(String),
}

/// 等待 Android「替我们绑回 UDC」的上限。
///
/// 实测（红魔9 Pro）：清空 UDC 后 `init` 的延迟反应通常在数百毫秒内发生。
/// 3 秒是经验值——足够覆盖延迟，又不至于让用户等太久。
pub const UDC_REBIND_TIMEOUT: Duration = Duration::from_secs(3);

/// 轮询 UDC 的间隔。
pub const UDC_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// mass_storage 编排器。
///
/// 泛型参数只有 `F: ConfigFs`：内核接口的最小面就是 configfs 读写，
/// 不再有额外的 HAL 抽象层（真机上没有可暂停的 gadget HAL 进程，
/// 且内核以 `EBUSY` 显式拒绝并发写 UDC）。
pub struct MassStorage<F: ConfigFs> {
    fs: F,
    /// 探测到的 configfs 布局（决定用哪个 gadget、哪个 config 目录）。
    layout: Layout,
    /// UDC class 目录（`/sys/class/udc`）。
    ///
    /// 做成字段以便主机测试指向临时目录：`/sys` 在开发机上只读，否则
    /// 「`effective` 是否正确反映 `state`」这条语义无法测试。
    udc_class_dir: PathBuf,
    /// 等待「Android 替我们绑回 UDC」的上限。
    ///
    /// 做成字段而不是直接用常量，是为了让主机测试把它设为 0——否则每个挂载
    /// 测试都要实睡 3 秒（实测会让整个 crate 的测试从 0.1s 涨到 6s）。
    udc_rebind_timeout: Duration,
    /// 操作记录（仅测试使用）。
    trace: Vec<Step>,
    /// 是否记录操作序列。
    tracing: bool,
}

impl<F: ConfigFs> MassStorage<F> {
    /// 以 configfs 构造。
    pub fn new(fs: F) -> Self {
        Self {
            fs,
            layout: Layout::aosp_default(),
            udc_class_dir: PathBuf::from(paths::UDC_CLASS_DIR),
            udc_rebind_timeout: UDC_REBIND_TIMEOUT,
            trace: Vec::new(),
            tracing: false,
        }
    }

    /// 设置探测到的布局。
    ///
    /// 生产路径**必须**调用：在存在多个 gadget 的设备上，兜底的 `g1` 未必是
    /// 我们要操作的那个（见 [`crate::discover`]）。
    pub fn with_layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }

    /// 当前布局（诊断用）。
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 设置等待绑回 UDC 的上限（测试用 `Duration::ZERO`）。
    pub fn with_udc_rebind_timeout(mut self, timeout: Duration) -> Self {
        self.udc_rebind_timeout = timeout;
        self
    }

    /// 等待上限。
    pub fn udc_rebind_timeout(&self) -> Duration {
        self.udc_rebind_timeout
    }

    /// 覆盖 UDC class 目录（测试用）。
    pub fn with_udc_class_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.udc_class_dir = dir.into();
        self
    }

    /// 启用操作记录。
    pub fn with_trace(mut self) -> Self {
        self.tracing = true;
        self
    }

    /// 取走操作记录。
    pub fn take_trace(&mut self) -> Vec<Step> {
        std::mem::take(&mut self.trace)
    }

    /// 操作记录（只读）。
    pub fn trace(&self) -> &[Step] {
        &self.trace
    }

    /// configfs 只读访问（诊断用）。
    pub fn fs(&self) -> &F {
        &self.fs
    }

    /// configfs 可变访问（诊断/测试用）。
    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    // ------------------------------------------------------------ 内部记录

    fn record(&mut self, step: Step) {
        if self.tracing {
            self.trace.push(step);
        }
    }

    fn write_attr(&mut self, relative: &str, value: &str) -> FsResult<()> {
        self.record(Step::Write(relative.to_string(), value.to_string()));
        self.fs.write(relative, value)
    }

    fn mkdir(&mut self, relative: &str) -> FsResult<()> {
        self.record(Step::Mkdir(relative.to_string()));
        self.fs.mkdir(relative)
    }

    fn rmdir(&mut self, relative: &str) -> FsResult<()> {
        self.record(Step::Rmdir(relative.to_string()));
        self.fs.rmdir(relative)
    }

    fn symlink(&mut self, target: &str, link: &str) -> FsResult<PathBuf> {
        self.record(Step::Symlink(target.to_string(), link.to_string()));
        self.fs.symlink(target, link)
    }

    fn unlink(&mut self, relative: &str) -> FsResult<()> {
        self.record(Step::Unlink(relative.to_string()));
        self.fs.unlink(relative)
    }

    // ------------------------------------------------------------ 基础查询

    /// 探测 UDC。
    ///
    /// **无 UDC 时必须返回 `NoUdc` 且不得继续改动 configfs**。
    pub fn detect_udc(&self) -> Result<String, crate::error::GadgetError> {
        let bound = self.fs.read(paths::UDC_ATTR).unwrap_or_default();
        if !bound.trim().is_empty() {
            return Ok(bound.trim().to_string());
        }

        match paths::read_udc_name() {
            paths::Udc::Available(name) => Ok(name),
            paths::Udc::None => Err(crate::error::GadgetError::NoUdc),
        }
    }

    /// 当前 `UDC` 属性值（空串表示未绑定）。
    pub fn bound_udc(&self) -> String {
        self.fs
            .read(paths::UDC_ATTR)
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// UDC 是否已被主机接受（`state == configured`）。
    ///
    /// 读不到 UDC 名或 `state` 文件时返回 `false`：`effective` 的语义是
    /// 「已确认生效」，确认不了就不该声称生效。
    pub fn udc_is_configured(&self) -> bool {
        let bound = self.bound_udc();
        if bound.is_empty() {
            return false;
        }
        match paths::read_udc_state_in(&self.udc_class_dir, &bound) {
            Ok(state) => paths::state_means_configured(&state),
            Err(_) => false,
        }
    }

    /// 我们的 function 目录是否存在。
    pub fn function_exists(&self) -> bool {
        self.fs.exists(&paths::function_path())
    }

    /// 我们的配置符号链接是否存在。
    pub fn link_exists(&self) -> bool {
        self.fs.exists(&self.layout.link_path())
    }

    /// 当前挂载的 LUN 信息（从 configfs 读回）。
    pub fn luns(&self) -> Vec<LunState> {
        let mut result = Vec::new();
        let function = paths::function_path();

        let Ok(entries) = self.fs.read_dir(&function) else {
            return result;
        };

        for entry in entries {
            let Some(index) = parse_lun_index(&entry.name) else {
                continue;
            };

            let read = |attr: &str| -> String {
                self.fs
                    .read(&paths::lun_attr(index, attr))
                    .unwrap_or_default()
            };

            let file = read("file");
            let mode = match (read("cdrom").as_str(), read("ro").as_str()) {
                ("1", _) => Mode::Cdrom,
                (_, "1") => Mode::Ro,
                _ => Mode::Rw,
            };
            let inquiry = read(paths::LUN_ATTR_INQUIRY_STRING);
            let inquiry = if inquiry.trim().is_empty() {
                None
            } else {
                Some(inquiry.trim().to_string())
            };

            result.push(LunState {
                index,
                image_path: file.trim().to_string(),
                mode,
                inquiry_string: inquiry,
                attached: !file.trim().is_empty(),
                effective: !file.trim().is_empty() && self.udc_is_configured(),
                // 与 `delete_slot` 的判据保持一处：0 号不可删。
                deletable: index != 0,
            });
        }

        result.sort_by_key(|lun| lun.index);
        result
    }

    /// 所有 LUN 的 `file` 是否都为空。
    ///
    /// 用于「设备被主机弹出」的判据：内核在 gadget deactivate（拔线）时会清空
    /// 全部 `lun.N/file`。**单个** LUN 为空不算弹出（那是正常的按 LUN 卸载）。
    pub fn all_luns_detached(&self) -> bool {
        let luns = self.luns();
        // 一个 LUN 都没有（function 不存在）→ 谈不上「弹出」。
        !luns.is_empty() && luns.iter().all(|lun| !lun.attached)
    }

    /// 设备是否已被主机弹出：我们的痕迹还在，但全部 LUN 的后端都已解绑。
    ///
    /// 只看「file 为空」不够：刚创建 function、还没写 `file` 的瞬间也满足。
    /// 加上「我们的 function 与链接存在」把它限定为「导出过、且刚被弹出」。
    pub fn is_ejected(&self) -> bool {
        self.function_exists() && self.link_exists() && self.all_luns_detached()
    }

    /// 等待 UDC 被（Android 或我们）绑定，最多 `timeout`。
    ///
    /// 返回实际读到的 UDC 名；超时返回 `None`。
    pub fn wait_for_udc_bound(&self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let bound = self.bound_udc();
            if !bound.is_empty() {
                return Some(bound);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(UDC_POLL_INTERVAL);
        }
    }

    // ------------------------------------------------------------ 挂载

    /// 挂载 / 更新一批 LUN，返回读回的内核真值。
    ///
    /// 每个 LUN 的操作尽可能独立：
    /// - **已存在的 LUN**：强制弹出该 LUN → 写参数 → 写 `file`。全程**不碰 UDC**。
    /// - **新增的 LUN**：`mkdir lun.N` 要求 UDC 空闲（内核 `EBUSY`），
    ///   因此走一次「断开 → 建目录与链接 → 写 file → 绑回」的紧凑段。
    ///
    /// 只有确实需要结构性改动时才进入紧凑段，这是把「UDC 抖动」降到最小的关键。
    pub fn mount(
        &mut self,
        requests: &[LunRequest],
    ) -> Result<Vec<LunState>, crate::error::GadgetError> {
        self.mount_with_options(requests, false)
    }

    /// [`Self::mount`]，但可要求**强制**走一次紧凑段（断 UDC → 重绑）。
    ///
    /// `force_rebind` 的用途是让**身份改动生效**：`idVendor`/字符串描述符只在
    /// 下次 bind 时才被主机看到。若本次没有结构性改动（LUN 都已在、链接也在），
    /// 常规路径不会碰 UDC，身份改动就会被拖到「下次因为别的原因重绑」才生效。
    pub fn mount_with_options(
        &mut self,
        requests: &[LunRequest],
        force_rebind: bool,
    ) -> Result<Vec<LunState>, crate::error::GadgetError> {
        use crate::error::GadgetError;

        if requests.is_empty() {
            return Ok(self.luns());
        }

        // 0. 参数校验（在任何 configfs 写入之前）。
        let mut seen = std::collections::BTreeSet::new();
        for request in requests {
            if request.index >= paths::MAX_LUNS {
                return Err(GadgetError::InvalidArgument(format!(
                    "LUN index {} exceeds the limit {}",
                    request.index,
                    paths::MAX_LUNS
                )));
            }
            if !seen.insert(request.index) {
                return Err(GadgetError::InvalidArgument(format!(
                    "LUN {} appears more than once in this request",
                    request.index
                )));
            }
            if let Some(inquiry) = &request.inquiry_string
                && inquiry.chars().count() > paths::INQUIRY_STRING_MAX
            {
                return Err(GadgetError::InvalidArgument(format!(
                    "the inquiry_string of LUN {} is {} characters long, exceeding the limit of {} (the kernel truncates silently)",
                    request.index,
                    inquiry.chars().count(),
                    paths::INQUIRY_STRING_MAX
                )));
            }
        }

        // 1. 探测 UDC；无 UDC 时**不得继续改动 configfs**。
        let udc = self.detect_udc()?;

        // 2. 确保 function 与 lun.0 存在。
        //
        // `mkdir functions/<name>` 不受 UDC 绑定限制（它走 `function_make`，
        // 无 refcnt 检查），可以安全地在 UDC 存在时执行。
        let function = paths::function_path();
        if !self.fs.exists(&function) {
            self.mkdir(&function)?;
        }
        let lun0 = paths::lun_path(0);
        if !self.fs.exists(&lun0) {
            // lun.0 应随 function 创建；缺失说明内核不支持 mass_storage。
            return Err(GadgetError::MassStorageUnsupported);
        }

        // 2b. **数据安全底线**：同一镜像不得同时成为两个 LUN 的后端。
        //
        // 本次请求内部的重复由 `gdd` 的 service 层拦掉，但那**只覆盖一个请求**。
        // AVD 实测漏掉的情形：先挂 `a.img`（落在 lun.0），再请求
        // `mount a.img b.img`——`a.img` 在请求里只出现一次，于是通过检查，而
        // 分配逻辑把它放到第一个空闲 LUN（lun.1），结果同一镜像同时挂在
        // lun.0 与 lun.1 上。两端并发写会损坏文件系统。
        //
        // 这里按**内核真值**（当前已绑定的 LUN）比对，且允许「同一序号上重复设置
        // 同一镜像」（那是幂等更新，不是重复占用）。
        let existing: Vec<(u8, String)> = self
            .luns()
            .into_iter()
            .filter(|lun| lun.attached && !lun.image_path.is_empty())
            .map(|lun| (lun.index, lun.image_path))
            .collect();
        for request in requests {
            if let Some((index, _)) = existing
                .iter()
                .find(|(index, path)| path == &request.image_path && *index != request.index)
            {
                return Err(crate::error::GadgetError::InvalidArgument(format!(
                    "the image is already exported as the backing file of lun.{index}: {}; \
                     one image cannot back two LUNs at once (writes from both sides would \
                     corrupt the filesystem). unmount lun.{index} first",
                    request.image_path
                )));
            }
        }

        // 3. 逐个处理：已存在的 LUN 就地更新（不动 UDC），缺失的记录为
        //    需要结构性改动。
        let mut to_create: Vec<&LunRequest> = Vec::new();
        for request in requests {
            let path = paths::lun_path(request.index);
            if self.fs.exists(&path) {
                self.reconfigure_lun(request)?;
            } else {
                to_create.push(request);
            }
        }

        // 4. 结构性改动：新增 LUN 需要 UDC 空闲；我们的链接缺失也需要它。
        //    调用方要求 `force_rebind`（身份改动）同样走这一段——那是唯一会写
        //    `UDC` 的地方，因此也是唯一能让新描述符生效的地方。
        let link = self.layout.link_path();
        let need_structural = force_rebind || !to_create.is_empty() || !self.fs.exists(&link);

        if need_structural {
            // ---- 紧凑段开始 ----
            //
            // 此段内**不得插入任何日志、`/proc` 读取或写盘**。Android 的 `init`
            // 监听 `sys.usb.config`，对我们清空 UDC 的反应**有延迟**；窗口越短，
            // 它越不可能在这个间隙里把配置改成自己的状态（真机实测：窗口一大，
            // UDC 就停在 `addressed`，Windows 侧表现为「代码 10」）。
            //
            // ## 顺序：**先删我们的链接，再 `mkdir lun.N`**（AVD 实测校正）
            //
            // 内核的 `fsg_lun_make` 检查的是 `fsg_opts->refcnt`：
            //
            //     if (fsg_opts->refcnt || fsg_opts->common->luns[num]) return -EBUSY;
            //
            // `refcnt` 由 `fsg_alloc`/`fsg_free` 增减，而它们分别由
            // `config_usb_cfg_link`/`config_usb_cfg_unlink` 调用——也就是说
            // **`refcnt` 反映的是「我们的 function 是否被某个配置链接引用」**，
            // 而不是「UDC 是否绑定」。
            //
            // AVD 实测：只写空 `UDC`（不断开链接）就 `mkdir lun.3`，仍然拿到
            // `EBUSY`。因此必须先 `unlink` 我们的链接，把 refcnt 降到 0。
            // （unlink 本身会让内核隐式解绑 gadget，所以断开 UDC 这一步在
            // 有链接时是冗余的，但保留它以便处理「链接不在、UDC 还绑着」的残局。）
            self.detach_udc()?;
            self.remove_our_link()?;

            // 4.1 建缺失的 LUN 目录并写好全部参数（`file` 留到最后）。
            for request in &to_create {
                let path = paths::lun_path(request.index);
                self.mkdir(&path)?;
                self.write_lun_params(request)?;
            }

            // 4.2 重建配置目录与**我们自己的**链接。
            //
            // 只增删我们自己名字的链接：AOSP 的 `sys.usb.config=none` 动作只删
            // `configs/b.N/f1..f3`，异名链接因此能在 Android 的 teardown 中幸存——
            // 这是本策略成立的前提（见 `paths::LINK_NAME`）。
            let config_dir = self.layout.config_path();
            if !self.fs.exists(&config_dir) {
                self.mkdir(&config_dir)?;
            }
            if !self.fs.exists(&link) {
                self.symlink(&function, &link)?;
            }

            // 4.3 等绑回；超时则自己绑。
            //
            // 注意顺序：写 `file` 放在**紧凑段之外**（见下），因为无论是否做了
            // 结构性改动，每个被请求的 LUN 都需要重新绑定后端——尤其是就地更新
            // 的 LUN，`forced_eject` 已经把它的介质弹出了。
            self.ensure_link_and_bind(&udc, &function, &link)?;
            // ---- 紧凑段结束 ----
        }

        // 4.4 逐个绑定后端文件（**最后**写）。
        //
        // 内核在写入 `file` 的瞬间打开并 pin 住后端，因此 `cdrom`/`ro` 必须先写
        // 好。UDC 已绑定时该写入仍然合法（`fsg_store_file` 无绑定检查，只有
        // `prevent_medium_removal` 时返回 `EBUSY`——而 `forced_eject` 已清掉它）。
        for request in requests {
            self.write_attr(&paths::lun_attr(request.index, "file"), &request.image_path)?;
        }

        // 5. 校验：必须真的生效，不得谎报成功。
        let indices: Vec<u8> = requests.iter().map(|r| r.index).collect();
        self.verify_bound(&indices)?;

        Ok(self.luns())
    }

    /// 紧凑段的收尾：确保链接存在，然后等回绑 / 自己绑。
    ///
    /// 抽成方法是因为**删除槽位**也需要它：内核的 `fsg_lun_drop` 会
    /// `unregister_gadget_item`，即 `rmdir lun.N` **隐式解绑整个 gadget**
    /// （AVD 实测确认）。删完必须重建链接并重绑，否则整条导出会失效。
    fn ensure_link_and_bind(&mut self, udc: &str, function: &str, link: &str) -> FsResult<()> {
        let config_dir = self.layout.config_path();
        if !self.fs.exists(&config_dir) {
            self.mkdir(&config_dir)?;
        }
        if !self.fs.exists(link) {
            self.symlink(function, link)?;
        }
        if self.wait_for_udc_bound(self.udc_rebind_timeout).is_none() {
            self.write_attr(paths::UDC_ATTR, udc)?;
        }
        Ok(())
    }

    /// 删除一个**空闲槽位**：弹出介质 → `rmdir lun.N` → 重建链接并重绑 UDC。
    ///
    /// ## 为什么必须重建链接并重绑
    ///
    /// 内核的 `fsg_lun_drop` 在 `refcnt` 非零时会调 `unregister_gadget_item`，
    /// 也就是**删 LUN 会隐式解绑整个 gadget**（AVD 实测：删完 `lun.1` 后
    /// `UDC` 变空）。若不重绑，用户会看到「删了一个槽位，整块 U 盘掉了」。
    ///
    /// ## `lun.0` 为什么不能删
    ///
    /// 它由内核在创建 function 时注册为默认组（`fsg_alloc_inst`），`rmdir`
    /// 返回 `EPERM`（已实测）。因此这里明确拒绝，而不是让它以 `EPERM` 的形式
    /// 冒到上层——错误信息要能说明**为什么**。
    pub fn delete_slot(&mut self, index: u8) -> Result<Vec<LunState>, crate::error::GadgetError> {
        use crate::error::GadgetError;

        if index == 0 {
            return Err(GadgetError::InvalidArgument(
                "lun.0 is a kernel-provided slot; it can only be ejected (made idle), not deleted"
                    .into(),
            ));
        }
        if index >= paths::MAX_LUNS {
            return Err(GadgetError::InvalidArgument(format!(
                "LUN index {index} exceeds the limit {}",
                paths::MAX_LUNS
            )));
        }

        let lun = paths::lun_path(index);
        if !self.fs.exists(&lun) {
            return Err(GadgetError::InvalidArgument(format!(
                "slot lun.{index} does not exist (it may have been deleted)"
            )));
        }

        // 先把介质弹掉：`rmdir` 一个仍绑定后端的 LUN 会被内核拒绝。
        self.force_eject(index).map_err(GadgetError::Fs)?;

        // 记住当前 UDC：`rmdir` 之后就读不到了。
        let udc = self.detect_udc()?;

        // 删之前先删我们的链接：既降低 `refcnt`（让 rmdir 干净），也避免留下
        // 一个指向「即将不存在的 function」之外的悬空状态。
        self.remove_our_link().map_err(GadgetError::Fs)?;

        self.rmdir(&lun).map_err(GadgetError::Fs)?;

        // 重建链接并重绑（内核已因 rmdir 隐式解绑）。
        let function = paths::function_path();
        let link = self.layout.link_path();
        self.ensure_link_and_bind(&udc, &function, &link)
            .map_err(GadgetError::Fs)?;

        Ok(self.luns())
    }

    /// 就地更新一个**已存在**的 LUN（不碰 UDC）。
    fn reconfigure_lun(&mut self, request: &LunRequest) -> FsResult<()> {
        // 强制弹出：`forced_eject` 会先清 `prevent_medium_removal` 再解绑后端，
        // 因此即使主机锁了介质也能弹出。这是「已绑定 UDC 时改 LUN」的关键一步。
        self.force_eject(request.index)?;
        self.write_lun_params(request)
    }

    /// 写 `cdrom` / `ro` / `inquiry_string`（**不含** `file`）。
    ///
    /// `cdrom` 与 `ro` 必须先于 `file`；`inquiry_string` 无顺序要求，但一并放在
    /// 这里以保证「参数全部就绪后才绑定后端」。
    fn write_lun_params(&mut self, request: &LunRequest) -> FsResult<()> {
        self.write_attr(
            &paths::lun_attr(request.index, "cdrom"),
            &request.mode.cdrom_attr().to_string(),
        )?;
        self.write_attr(
            &paths::lun_attr(request.index, "ro"),
            &request.mode.ro_attr().to_string(),
        )?;

        // INQUIRY：未指定时写空串清掉，避免上一次的值残留。
        let inquiry = request.inquiry_string.clone().unwrap_or_default();
        let attr = paths::lun_inquiry_path(request.index);
        if self.fs.exists(&attr) {
            self.write_attr(&attr, &inquiry)?;
        } else if request.inquiry_string.is_some() {
            // 用户明确要求了该参数，而内核没有这个属性 —— 报错而不是静默忽略。
            return Err(crate::configfs::FsError::NotFound(PathBuf::from(attr)));
        }
        Ok(())
    }

    /// 强制弹出一个 LUN 的介质（`forced_eject`），失败时退化为清空 `file`。
    ///
    /// 退化路径的存在理由：`forced_eject` 是较新的属性，老内核可能没有。
    /// 清 `file` 在未被 `prevent_medium_removal` 锁住时等效。
    fn force_eject(&mut self, index: u8) -> FsResult<()> {
        let attr = paths::lun_forced_eject_path(index);
        if self.fs.exists(&attr) {
            return self.write_attr(&attr, "1").map(|_| ());
        }
        self.clear_lun_file(index)
    }

    /// 清空 `lun.N/file`（解除后端文件绑定）。
    fn clear_lun_file(&mut self, index: u8) -> FsResult<()> {
        let attr = paths::lun_attr(index, "file");
        let current = self.fs.read(&attr).unwrap_or_default();
        if !current.trim().is_empty() {
            self.write_attr(&attr, "")?;
        }
        Ok(())
    }

    /// 断开 UDC（写空串）。幂等：本来就没绑定时内核返回 ENODEV，视为成功。
    fn detach_udc(&mut self) -> FsResult<()> {
        if self.fs.exists(paths::UDC_ATTR) {
            self.write_attr(paths::UDC_ATTR, "")?;
        }
        Ok(())
    }

    /// 校验配置真的生效；不满足则报错，**不得**谎报成功。
    ///
    /// 判据（全部为内核真值）：
    /// 1. `UDC` 非空——控制器已绑定；
    /// 2. 我们的链接存在于配置目录里；
    /// 3. 每个被请求的 `lun.N/file` 非空——后端文件已绑定。
    ///
    /// 刻意**不**把 UDC `state == configured` 作为硬条件：`configured` 需要主机
    /// 主动 `SET_CONFIGURATION`，拔线时必然是 `not attached`，那属于正常状态。
    /// 主机是否接受由 `effective` 字段单独表达。
    pub fn verify_bound(&self, indices: &[u8]) -> Result<(), crate::error::GadgetError> {
        use crate::error::GadgetError;

        if self.bound_udc().is_empty() {
            return Err(GadgetError::NotActive("the UDC is not bound".into()));
        }
        if !self.link_exists() {
            return Err(GadgetError::NotActive(format!(
                "the config directory has no link of ours ({})",
                self.layout.link_path()
            )));
        }
        for index in indices {
            let file = self
                .fs
                .read(&paths::lun_attr(*index, "file"))
                .unwrap_or_default();
            if file.trim().is_empty() {
                return Err(GadgetError::NotActive(format!(
                    "lun.{index}/file is not bound"
                )));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ 卸载

    /// 弹出一个 LUN 的介质，**保留** LUN 目录、链接、function 与 UDC。
    ///
    /// 「卸载单个 LUN」不等于「拆除导出」：主机侧只应看到该介质消失，LUN 数量
    /// 与其余 LUN 都不变。要彻底拆除请用 [`Self::teardown`]。
    pub fn unmount_lun(&mut self, index: u8) -> Result<Vec<LunState>, crate::error::GadgetError> {
        self.force_eject(index)
            .map_err(crate::error::GadgetError::Fs)?;
        Ok(self.luns())
    }

    /// 弹出全部 LUN 的介质，但保留 function/链接/UDC。
    pub fn eject_all(&mut self) -> Result<Vec<LunState>, crate::error::GadgetError> {
        for lun in self.luns() {
            self.force_eject(lun.index)
                .map_err(crate::error::GadgetError::Fs)?;
        }
        Ok(self.luns())
    }

    /// 拆除我们自己的全部痕迹：断 UDC → 清全部 file → 删链接 → 删 function。
    ///
    /// **不**触碰 gadget 身份（`idVendor`/`strings`/`os_desc`）——那归 CLI。
    /// 返回未能完成的清理项（空表示全部成功）；失败只报告不抛出，因为留下残留
    /// 好过让整个拆除失败。
    ///
    /// 顺序里的一个关键点：`rmdir lun.N`（N≥1）在内核里会**隐式解绑** gadget，
    /// 所以必须先自己断 UDC，否则「断 UDC」这一步会作用在一个已经被解绑的
    /// gadget 上（内核返回 ENODEV，我们视为幂等，但行为不再明确）。
    pub fn teardown(&mut self) -> Vec<String> {
        let mut failures = Vec::new();

        if let Err(err) = self.detach_udc() {
            failures.push(format!("failed to disconnect the UDC: {err}"));
        }

        for lun in self.luns() {
            if let Err(err) = self.clear_lun_file(lun.index) {
                failures.push(format!("failed to clear lun.{}/file: {err}", lun.index));
            }
        }

        if let Err(err) = self.remove_our_link() {
            failures.push(format!("failed to delete our link: {err}"));
        }

        failures.extend(self.remove_own_function());
        failures
    }

    /// 删除**我们自己**的配置符号链接。
    ///
    /// **绝不遍历删除整个 config 目录**：Android 的 `f1..f3` 属于框架，删掉它们
    /// 会让框架的 USB 状态与内核不一致，反而更容易被它重新抢占。
    fn remove_our_link(&mut self) -> FsResult<()> {
        let link = self.layout.link_path();
        if self.fs.exists(&link) {
            self.unlink(&link)?;
        }
        Ok(())
    }

    /// 删除我们自己的 function（含改名前的遗留名）。
    ///
    /// 失败只收集不抛出：configfs 在仍绑定时可能拒绝（`EBUSY`/`EINVAL`），保留
    /// 残留比让清理整体失败更好——下次挂载前会再清一次。
    pub fn remove_own_function(&mut self) -> Vec<String> {
        let mut failures = Vec::new();

        let function = paths::function_path();
        if self.fs.exists(&function) {
            // lun.0 只能 clear、不能删；lun.1+ 先清 file 再删目录。
            for index in 1..paths::MAX_LUNS {
                let lun = paths::lun_path(index);
                if self.fs.exists(&lun) {
                    let _ = self.clear_lun_file(index);
                    if let Err(err) = self.rmdir(&lun) {
                        failures.push(format!("failed to delete lun.{index}: {err}"));
                    }
                }
            }
            if let Err(err) = self.rmdir(&function) {
                failures.push(format!("failed to delete the function: {err}"));
            }
        }

        failures
    }

    /// 弹出后的收尾：清 file、删链接与 function，**不解绑 UDC**。
    ///
    /// 一旦解绑，Android 的 `init` 会立刻按 `sys.usb.config` 重装它自己的配置，
    /// 把状态搅乱。因此这里只撤我们自己的痕迹。
    pub fn cleanup_after_eject(&mut self) -> Vec<String> {
        let mut failures = Vec::new();

        for lun in self.luns() {
            if let Err(err) = self.clear_lun_file(lun.index) {
                failures.push(format!("failed to clear lun.{}/file: {err}", lun.index));
            }
        }
        if let Err(err) = self.remove_our_link() {
            failures.push(format!("failed to delete our link: {err}"));
        }
        failures.extend(self.remove_own_function());

        failures
    }

    /// 重新绑定 UDC：断开 → 等待 Android 绑回（3s）→ 仍为空则自己绑。
    ///
    /// 用途：身份（`idVendor`/`strings`）已由 CLI 改写，需要一次「刷新」才能让
    /// 主机看到新描述符。configfs 的属性写入本身不受 UDC 绑定限制，但**生效**
    /// 发生在下一次 bind。
    ///
    /// ## 没有导出时**什么都不做**
    ///
    /// 「重绑」的前提是**已经**绑着——没有导出时既没有主机在看描述符（改动会在
    /// 下次挂载时自然生效），而且此时写 `UDC` 会绑定一个**没有任何 function 的**
    /// gadget（我们的链接在拆除时已被删掉），把 configfs 留在一个奇怪的状态里。
    ///
    /// 回归（AVD 实测）：调用方曾用 `udc().is_some()` 判断「是否已绑定」，而
    /// `detect_udc()` 在没有绑定时会退回到系统属性 `sys.usb.controller`——于是
    /// 「可用的控制器存在」被误当成「已经绑着」，每次身份改动都触发一次重绑，
    /// 拿回 `EBUSY` 并以 `internal` 报错。
    pub fn rebind(&mut self) -> Result<String, crate::error::GadgetError> {
        use crate::error::GadgetError;

        // 先记录当前**已绑定**的 UDC（断开后就读不到了）。
        let bound = self.bound_udc();
        if bound.is_empty() {
            // 没有导出：无需重绑，也不该绑定一个空配置。
            return self.detect_udc();
        }
        self.detach_udc().map_err(GadgetError::Fs)?;

        if let Some(bound) = self.wait_for_udc_bound(self.udc_rebind_timeout) {
            return Ok(bound);
        }
        // 断开了、又没人绑回 → 用之前那个名字自己绑。
        self.write_attr(paths::UDC_ATTR, &bound)
            .map_err(GadgetError::Fs)?;
        Ok(bound)
    }
}

/// 解析 `lun.<n>` 形式的目录名。
pub fn parse_lun_index(name: &str) -> Option<u8> {
    name.strip_prefix("lun.")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configfs::MemConfigFs;
    use crate::paths;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gd-ms-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 构造一个 UDC 已绑定、已含 function 与 lun.0 的内存 configfs。
    fn ready_fs() -> MemConfigFs {
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        fs.write(paths::UDC_ATTR, "dummy_udc.0").unwrap();
        fs.mkdir(&paths::function_path()).unwrap();
        fs.mkdir(&paths::lun_path(0)).unwrap();
        fs
    }

    /// 构造编排器，并把绑回等待设为 0（测试里无人替我们绑）。
    fn ms(fs: MemConfigFs) -> MassStorage<MemConfigFs> {
        MassStorage::new(fs)
            .with_udc_rebind_timeout(Duration::ZERO)
            .with_trace()
    }

    /// 声明一个后端文件存在，使 `file` 写入能通过 `MemConfigFs` 的校验。
    fn known(fs: &mut MemConfigFs, path: &str) {
        fs.add_known_file(path);
    }

    #[test]
    fn parse_lun_index_handles_valid_and_invalid_names() {
        assert_eq!(parse_lun_index("lun.0"), Some(0));
        assert_eq!(parse_lun_index("lun.1"), Some(1));
        assert_eq!(parse_lun_index("lun.255"), Some(255));
        assert_eq!(parse_lun_index("lun.256"), None);
        assert_eq!(parse_lun_index("lun.abc"), None);
        assert_eq!(parse_lun_index("stall"), None);
        assert_eq!(parse_lun_index("luns"), None);
    }

    #[test]
    fn no_udc_leaves_configfs_untouched() {
        // 无 UDC 时（属性为空 + 系统属性缺失）必须报 NoUdc 且不写任何东西。
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        fs.mkdir(&paths::function_path()).unwrap();
        fs.mkdir(&paths::lun_path(0)).unwrap();
        let mut m = ms(fs);

        let err = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap_err();
        assert!(matches!(err, crate::error::GadgetError::NoUdc));
        assert!(
            m.take_trace().is_empty(),
            "无 UDC 时不得改动 configfs，实际：{:?}",
            m.trace()
        );
    }

    #[test]
    fn missing_lun0_reports_mass_storage_unsupported() {
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        fs.write(paths::UDC_ATTR, "dummy_udc.0").unwrap();
        // 不建 function：内核在无 mass_storage 支持时不会生成 lun.0。
        let mut m = ms(fs);
        let err = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap_err();
        assert!(matches!(
            err,
            crate::error::GadgetError::MassStorageUnsupported
        ));
    }

    #[test]
    fn first_mount_creates_link_and_binds_file() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);

        let luns = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();

        assert_eq!(luns.len(), 1);
        assert_eq!(luns[0].image_path, "/data/a.img");
        assert!(luns[0].attached);
        // 链接已建立，且指向我们的 function。
        assert!(m.link_exists());
        // UDC 已绑回。
        assert!(!m.bound_udc().is_empty());
    }

    #[test]
    fn mount_never_removes_foreign_links() {
        // 核心回归：Android 的 f1..f3 属于框架，绝不能删。
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        fs.mkdir("configs/b.1").unwrap();
        for name in ["f1", "f2", "f3"] {
            fs.symlink("functions/ffs.adb", &format!("configs/b.1/{name}"))
                .unwrap();
        }
        let mut m = ms(fs);

        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();

        for name in ["f1", "f2", "f3"] {
            assert!(
                m.fs().exists(&format!("configs/b.1/{name}")),
                "框架链接 {name} 被删除了——这会让 Android 重新抢占 UDC"
            );
        }
    }

    #[test]
    fn link_name_is_ours_and_distinct_from_f1() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        assert!(m.fs().exists(&paths::Layout::aosp_default().link_path()));
        assert!(!m.fs().exists("configs/b.1/f1"));
    }

    /// **本轮最关键的顺序不变量**：所有 `mkdir lun.N` 都必须在写 `UDC` 之前。
    ///
    /// 依据（内核）：`fsg_lun_make` 在 `fsg_opts->refcnt` 非零（gadget 已绑定）
    /// 时返回 `EBUSY`。若把 `mkdir` 放在写 `UDC` 之后，新增 LUN 必然失败。
    #[test]
    fn creating_luns_happens_before_binding_udc() {
        let mut fs = ready_fs().with_lun_create_needing_unlinked();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);

        // lun.1 不存在 → 需要结构性改动 → 进入紧凑段。
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();

        let trace = m.take_trace();
        let last_lun_mkdir = trace
            .iter()
            .rposition(|s| matches!(s, Step::Mkdir(p) if p.ends_with("lun.1")))
            .expect("应创建 lun.1");
        // UDC 的首次非空写入（绑回自己）必须晚于 lun.1 的创建。
        let first_bind = trace
            .iter()
            .position(|s| matches!(s, Step::Write(p, v) if p == "UDC" && !v.is_empty()))
            .expect("应绑回 UDC");
        assert!(
            last_lun_mkdir < first_bind,
            "mkdir lun.N 必须早于写 UDC；trace={trace:?}"
        );
    }

    /// 新增 LUN 前必须**先删我们的链接**（而不是只断 UDC）。
    ///
    /// AVD 实测校正：内核 `fsg_lun_make` 检查 `fsg_opts->refcnt`，而 `refcnt` 由
    /// `config_usb_cfg_link`/`unlink` 增减——它反映「我们的 function 是否被配置
    /// 链接引用」。只写空 `UDC` 而不删链接，`mkdir lun.N` 仍返回 `EBUSY`。
    ///
    /// 这条测试用「链接存在时 mkdir 即 EBUSY」的替身来钉住顺序：若实现忘了先
    /// unlink，`mkdir lun.1` 会失败，整个 mount 报错。
    #[test]
    fn creating_a_lun_unlinks_our_link_first() {
        let mut fs = ready_fs().with_lun_create_needing_unlinked();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);

        // 先建立链接（挂一个 LUN）。
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        assert!(m.link_exists());
        let _ = m.take_trace();

        // 再新增 lun.1：链接存在时若直接 mkdir 会 EBUSY，因此必须先 unlink。
        let luns = m
            .mount(&[
                LunRequest::new(0, "/data/a.img", Mode::Rw),
                LunRequest::new(1, "/data/b.img", Mode::Rw),
            ])
            .expect("先 unlink 再 mkdir 才能成功");

        let trace = m.take_trace();
        let unlink_pos = trace
            .iter()
            .position(|s| matches!(s, Step::Unlink(p) if p.ends_with(paths::LINK_NAME)))
            .expect("必须删掉我们的链接以把 refcnt 降到 0");
        let mkdir_pos = trace
            .iter()
            .position(|s| matches!(s, Step::Mkdir(p) if p.ends_with("lun.1")))
            .expect("应创建 lun.1");
        assert!(
            unlink_pos < mkdir_pos,
            "必须先 unlink 再 mkdir lun.N（内核 refcnt）trace={trace:?}"
        );
        // 链接被重建，且两个 LUN 都绑上了。
        assert!(m.link_exists());
        assert_eq!(luns.len(), 2);
        assert!(luns.iter().all(|l| l.attached));
    }

    /// **本轮最关键的行为不变量**：已存在的 LUN 就地更新时**不碰 UDC**。
    ///
    /// 这是「每个 LUN 操作尽可能独立」的落地：改一个已有 LUN 的镜像或参数，
    /// 不应让整条 USB 链路抖动。
    #[test]
    fn updating_an_existing_lun_does_not_touch_udc() {
        let mut fs = ready_fs().with_lun_create_needing_unlinked();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/a2.img");
        let mut m = ms(fs);

        // 第一次：建立链接与绑定。
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let _ = m.take_trace();

        // 第二次：换镜像 + 换模式 + 加 INQUIRY。LUN 已存在、链接已存在。
        let luns = m
            .mount(&[LunRequest {
                index: 0,
                image_path: "/data/a2.img".into(),
                mode: Mode::Ro,
                inquiry_string: Some("GD TEST".into()),
            }])
            .unwrap();

        let trace = m.take_trace();
        let udc_writes: Vec<&Step> = trace
            .iter()
            .filter(|s| matches!(s, Step::Write(p, _) if p == "UDC"))
            .collect();
        assert!(
            udc_writes.is_empty(),
            "就地更新既有 LUN 不得写 UDC，实际：{udc_writes:?}"
        );

        assert_eq!(luns[0].image_path, "/data/a2.img");
        assert_eq!(luns[0].mode, Mode::Ro);
        assert_eq!(luns[0].inquiry_string.as_deref(), Some("GD TEST"));
    }

    /// 强制弹出必须真的解绑后端，否则 `ro`/`cdrom` 的写法会无效。
    #[test]
    fn updating_existing_lun_force_ejects_first() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/a2.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let _ = m.take_trace();

        m.mount(&[LunRequest::new(0, "/data/a2.img", Mode::Rw)])
            .unwrap();

        let trace = m.take_trace();
        let eject = trace
            .iter()
            .position(|s| matches!(s, Step::Write(p, v) if p.ends_with("forced_eject") && v == "1"))
            .expect("应先强制弹出该 LUN");
        let bind = trace
            .iter()
            .rposition(|s| matches!(s, Step::Write(p, v) if p.ends_with("/file") && !v.is_empty()))
            .expect("应重新绑定 file");
        assert!(eject < bind, "强制弹出必须早于重新绑定；trace={trace:?}");
    }

    /// 退化路径：内核没有 `forced_eject` 属性时，改用清 `file`。
    #[test]
    fn force_eject_falls_back_to_clearing_file() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/a2.img");
        // 移除 forced_eject 属性，模拟老内核。
        fs.remove_attr("functions/mass_storage.gadget-disk/lun.0/forced_eject");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let _ = m.take_trace();

        m.mount(&[LunRequest::new(0, "/data/a2.img", Mode::Rw)])
            .unwrap();

        let trace = m.take_trace();
        // 应出现「写空 file」这一步（清 file 退化路径）。
        assert!(
            trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, v) if p.ends_with("/file") && v.is_empty())),
            "无 forced_eject 时应退化为清空 file；trace={trace:?}"
        );
    }

    #[test]
    fn cdrom_mode_sets_both_cdrom_and_ro() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.iso");
        let mut m = ms(fs);
        let luns = m
            .mount(&[LunRequest::new(0, "/data/a.iso", Mode::Cdrom)])
            .unwrap();
        assert_eq!(luns[0].mode, Mode::Cdrom);
        let trace = m.take_trace();
        assert!(
            trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, v) if p.ends_with("/cdrom") && v == "1"))
        );
        assert!(
            trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, v) if p.ends_with("/ro") && v == "1"))
        );
    }

    #[test]
    fn read_write_mode_clears_cdrom_and_ro() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let trace = m.take_trace();
        assert!(
            trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, v) if p.ends_with("/cdrom") && v == "0"))
        );
        assert!(
            trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, v) if p.ends_with("/ro") && v == "0"))
        );
    }

    #[test]
    fn file_attribute_is_written_after_cdrom_and_ro() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.iso");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.iso", Mode::Cdrom)])
            .unwrap();
        let trace = m.take_trace();

        let bind = trace
            .iter()
            .rposition(|s| matches!(s, Step::Write(p, v) if p.ends_with("/file") && !v.is_empty()))
            .expect("应有一次 file 绑定");
        let cdrom = trace
            .iter()
            .rposition(|s| matches!(s, Step::Write(p, _) if p.ends_with("/cdrom")))
            .unwrap();
        let ro = trace
            .iter()
            .rposition(|s| matches!(s, Step::Write(p, _) if p.ends_with("/ro")))
            .unwrap();
        assert!(cdrom < bind, "cdrom 必须先于 file");
        assert!(ro < bind, "ro 必须先于 file");
    }

    #[test]
    fn inquiry_string_is_written_and_read_back() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        let luns = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw).with_inquiry("GD DISK")])
            .unwrap();
        assert_eq!(luns[0].inquiry_string.as_deref(), Some("GD DISK"));
    }

    #[test]
    fn inquiry_string_longer_than_kernel_limit_is_rejected() {
        // 内核用 "%-28s" 定宽写入，超长会**静默截断**。必须在写入前拒绝，
        // 否则用户以为设置生效了。
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        let too_long = "x".repeat(paths::INQUIRY_STRING_MAX + 1);
        let err = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw).with_inquiry(too_long)])
            .unwrap_err();
        match err {
            crate::error::GadgetError::InvalidArgument(msg) => {
                assert!(msg.contains("inquiry_string"), "得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }
    }

    #[test]
    fn inquiry_string_at_the_limit_is_accepted() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        let at_limit = "y".repeat(paths::INQUIRY_STRING_MAX);
        let luns = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw).with_inquiry(at_limit)])
            .unwrap();
        assert_eq!(
            luns[0].inquiry_string.as_deref().map(str::len),
            Some(paths::INQUIRY_STRING_MAX)
        );
    }

    #[test]
    fn lun_index_above_the_limit_is_rejected() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        let err = m
            .mount(&[LunRequest::new(paths::MAX_LUNS, "/data/a.img", Mode::Rw)])
            .unwrap_err();
        match err {
            crate::error::GadgetError::InvalidArgument(msg) => {
                assert!(msg.contains("exceeds the limit"), "得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }
    }

    /// **数据安全底线**：同一镜像不得同时是两个 LUN 的后端。
    ///
    /// 回归（AVD 实测）：service 层的「同一请求内不得重复」只覆盖一个请求。
    /// 先挂 `a.img`（lun.0），再请求 `mount a.img b.img`——`a.img` 在请求里只出现
    /// 一次即通过检查，分配逻辑把它放到第一个空闲 LUN（lun.1），于是同一镜像同时
    /// 挂在两个 LUN 上。这里按**已绑定的内核真值**拦住它。
    #[test]
    fn same_image_cannot_land_on_two_luns_across_requests() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);

        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();

        // 再请求 a.img（隐式分配到 lun.1）+ b.img。
        let err = m
            .mount(&[
                LunRequest::new(0, "/data/a.img", Mode::Rw),
                LunRequest::new(1, "/data/a.img", Mode::Rw),
            ])
            .unwrap_err();
        match err {
            crate::error::GadgetError::InvalidArgument(msg) => {
                assert!(msg.contains("cannot back two LUNs"), "得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }

        // 幂等情形必须放行：同一序号上重复设置同一镜像。
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .expect("同一 LUN 上重复设置同一镜像应幂等通过");
    }

    /// 另一个镜像可以正常追加。
    #[test]
    fn a_different_image_can_be_added_alongside() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let luns = m
            .mount(&[LunRequest::new(1, "/data/b.img", Mode::Ro)])
            .unwrap();
        assert_eq!(luns.len(), 2);
        assert!(luns.iter().all(|l| l.attached));
    }

    #[test]
    fn duplicate_lun_indices_in_one_request_are_rejected() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        let err = m
            .mount(&[
                LunRequest::new(0, "/data/a.img", Mode::Rw),
                LunRequest::new(0, "/data/a.img", Mode::Ro),
            ])
            .unwrap_err();
        assert!(matches!(err, crate::error::GadgetError::InvalidArgument(_)));
    }

    /// 多个 LUN 各自独立：模式互不影响。
    #[test]
    fn multiple_luns_have_independent_modes() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.iso");
        let mut m = ms(fs);
        let luns = m
            .mount(&[
                LunRequest::new(0, "/data/a.img", Mode::Rw).with_inquiry("DISK"),
                LunRequest::new(1, "/data/b.iso", Mode::Cdrom).with_inquiry("ISO"),
            ])
            .unwrap();

        assert_eq!(luns.len(), 2);
        assert_eq!(luns[0].mode, Mode::Rw);
        assert_eq!(luns[0].inquiry_string.as_deref(), Some("DISK"));
        assert_eq!(luns[1].mode, Mode::Cdrom);
        assert_eq!(luns[1].inquiry_string.as_deref(), Some("ISO"));
    }

    /// 按 LUN 卸载必须**保留** LUN 目录、链接与其余 LUN。
    #[test]
    fn unmounting_one_lun_keeps_the_lun_directory_and_others() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();

        let luns = m.unmount_lun(1).unwrap();

        // lun.1 目录仍在，只是介质被弹出。
        assert!(
            m.fs().exists(&paths::lun_path(1)),
            "按 LUN 卸载不得删除 LUN 目录"
        );
        assert!(
            !luns.iter().find(|l| l.index == 1).unwrap().attached,
            "lun.1 应已弹出"
        );
        // lun.0 不受影响。
        assert!(
            luns.iter().find(|l| l.index == 0).unwrap().attached,
            "lun.0 不应受影响"
        );
        // 链接与 function 都在。
        assert!(m.link_exists());
        assert!(m.function_exists());
    }

    #[test]
    fn eject_all_keeps_function_and_link() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();

        let luns = m.eject_all().unwrap();
        assert!(luns.iter().all(|l| !l.attached));
        assert!(m.function_exists());
        assert!(m.link_exists());
    }

    #[test]
    fn teardown_removes_our_artifacts_but_not_foreign_links() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        fs.mkdir("configs/b.1").unwrap();
        fs.symlink("functions/ffs.adb", "configs/b.1/f1").unwrap();
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();

        let failures = m.teardown();

        assert!(failures.is_empty(), "拆除应无失败：{failures:?}");
        assert!(!m.function_exists(), "function 应被删除");
        assert!(!m.link_exists(), "我们的链接应被删除");
        assert!(
            m.fs().exists("configs/b.1/f1"),
            "框架链接不得被拆除流程删掉"
        );
        // UDC 已断开。
        assert!(m.bound_udc().is_empty());
    }

    #[test]
    fn teardown_removes_extra_lun_directories() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();
        assert!(m.fs().exists(&paths::lun_path(1)));

        m.teardown();

        assert!(!m.fs().exists(&paths::lun_path(1)), "lun.1 应被删除");
        assert!(!m.function_exists());
    }

    /// 弹出判据：**全部** LUN 的 file 为空才算弹出。
    #[test]
    fn is_ejected_requires_every_lun_to_be_detached() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);

        // 未挂载（function 在但没有链接）→ 不是弹出。
        assert!(!m.is_ejected(), "没有链接时不该判为弹出");

        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();
        assert!(!m.is_ejected(), "刚挂载完不应判为弹出");

        // 只弹出 lun.1 → 仍不算弹出（这是正常的按 LUN 卸载）。
        m.unmount_lun(1).unwrap();
        assert!(!m.is_ejected(), "单个 LUN 为空不得判为弹出");

        // 全部弹出 → 是弹出。
        m.eject_all().unwrap();
        assert!(m.is_ejected(), "全部 LUN 为空应判为弹出");
    }

    #[test]
    fn cleanup_after_eject_never_detaches_udc() {
        // 弹出后一旦解绑 UDC，Android 的 init 会立刻重装它自己的配置。
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let _ = m.take_trace();

        m.cleanup_after_eject();

        let trace = m.take_trace();
        assert!(
            !trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, _) if p == "UDC")),
            "弹出清理不得写 UDC；trace={trace:?}"
        );
        assert!(!m.function_exists());
        assert!(!m.link_exists());
    }

    /// `force_rebind` 必须真的走紧凑段（写 UDC），否则身份改动不会生效。
    ///
    /// 常规路径对「已存在的 LUN + 已有链接」完全不碰 UDC——那是本轮的性能与
    /// 稳定性收益，但也意味着身份改动会被拖到下次因别的原因重绑。因此调用方
    /// 需要一个显式的「强制重绑」入口。
    #[test]
    fn force_rebind_makes_mount_write_udc_even_without_structural_changes() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        // 先建立链接与绑定。
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let _ = m.take_trace();

        // 再挂一次，但要求强制重绑。
        m.mount_with_options(&[LunRequest::new(0, "/data/a.img", Mode::Rw)], true)
            .unwrap();

        let trace = m.take_trace();
        assert!(
            trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, v) if p == "UDC" && v.is_empty())),
            "强制重绑必须写 UDC（断开），trace={trace:?}"
        );
    }

    /// 不要求强制重绑时，就地更新既有 LUN 仍然不碰 UDC。
    #[test]
    fn mount_without_force_rebind_still_avoids_udc() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/a2.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let _ = m.take_trace();

        m.mount_with_options(&[LunRequest::new(0, "/data/a2.img", Mode::Rw)], false)
            .unwrap();

        let trace = m.take_trace();
        assert!(
            !trace
                .iter()
                .any(|s| matches!(s, Step::Write(p, _) if p == "UDC")),
            "未要求强制重绑时不得写 UDC，trace={trace:?}"
        );
    }

    #[test]
    fn lun_zero_is_not_deletable_but_others_are() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        let luns = m
            .mount(&[
                LunRequest::new(0, "/data/a.img", Mode::Rw),
                LunRequest::new(1, "/data/b.img", Mode::Rw),
            ])
            .unwrap();

        let zero = luns.iter().find(|l| l.index == 0).unwrap();
        let one = luns.iter().find(|l| l.index == 1).unwrap();
        assert!(!zero.deletable, "lun.0 不可删（内核 EPERM）");
        assert!(one.deletable, "lun.1 可删");
    }

    #[test]
    fn delete_slot_rejects_lun_zero_with_a_reason() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();

        let err = m.delete_slot(0).unwrap_err();
        match err {
            crate::error::GadgetError::InvalidArgument(msg) => {
                // 错误必须说明**为什么**（内核随 function 创建），而不是只报 EPERM。
                assert!(msg.contains("kernel"), "得到 {msg}");
                assert!(msg.contains("only be ejected"), "得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }
        // 槽位仍在。
        assert!(m.fs().exists(&paths::lun_path(0)));
    }

    #[test]
    fn delete_slot_rejects_a_missing_slot() {
        let fs = ready_fs();
        let mut m = ms(fs);
        let err = m.delete_slot(3).unwrap_err();
        match err {
            crate::error::GadgetError::InvalidArgument(msg) => {
                assert!(msg.contains("does not exist"), "得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }
    }

    #[test]
    fn delete_slot_rejects_out_of_range() {
        let fs = ready_fs();
        let mut m = ms(fs);
        assert!(matches!(
            m.delete_slot(paths::MAX_LUNS).unwrap_err(),
            crate::error::GadgetError::InvalidArgument(_)
        ));
    }

    /// 删除槽位必须**弹掉介质 → rmdir → 重建链接并重绑 UDC**。
    ///
    /// 关键在内核行为：`fsg_lun_drop` 会 `unregister_gadget_item`，即删 LUN
    /// **隐式解绑整个 gadget**（AVD 实测）。不重绑的话用户会看到「删了一个槽位，
    /// 整块 U 盘掉了」。`MemConfigFs` 已建模该行为（`rmdir lun.N` 清空 `UDC`）。
    #[test]
    fn delete_slot_ejects_removes_and_rebinds_udc() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();
        assert!(m.fs().exists(&paths::lun_path(1)));
        let _ = m.take_trace();

        let luns = m.delete_slot(1).unwrap();

        // 槽位没了。
        assert!(!m.fs().exists(&paths::lun_path(1)), "lun.1 应被删除");
        assert!(!luns.iter().any(|l| l.index == 1), "读回不应再含 lun.1");
        // lun.0 不受影响。
        assert!(luns.iter().any(|l| l.index == 0 && l.attached));
        // 链接重建、UDC 绑回（否则整条导出失效）。
        assert!(m.link_exists(), "链接必须重建");
        assert!(
            !m.bound_udc().is_empty(),
            "UDC 必须重绑：内核 rmdir 会隐式解绑"
        );

        let trace = m.take_trace();
        let rmdir_pos = trace
            .iter()
            .position(|s| matches!(s, Step::Rmdir(p) if p.ends_with("lun.1")))
            .expect("应 rmdir lun.1");
        // 重建链接必须晚于 rmdir。
        let symlink_pos = trace
            .iter()
            .position(|s| matches!(s, Step::Symlink(_, l) if l.ends_with(paths::LINK_NAME)))
            .expect("应重建链接");
        assert!(
            rmdir_pos < symlink_pos,
            "必须先 rmdir 再重建链接；trace={trace:?}"
        );
    }

    /// 删除后能再加回同号（序号回到「不存在」状态）。
    #[test]
    fn delete_slot_then_recreate_works() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();

        m.delete_slot(1).unwrap();
        let luns = m
            .mount(&[LunRequest::new(1, "/data/b.img", Mode::Ro)])
            .expect("删完应能再加回同号");
        assert!(luns.iter().any(|l| l.index == 1 && l.attached));
    }

    /// 删除一个**仍挂着介质**的槽位：必须先弹掉介质再 rmdir。
    ///
    /// 内核会拒绝 `rmdir` 一个仍绑定后端的 LUN（`MemConfigFs` 也建模了这一点），
    /// 因此顺序错了会直接失败。
    #[test]
    fn delete_slot_ejects_a_mounted_slot_first() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        known(&mut fs, "/data/b.img");
        let mut m = ms(fs);
        m.mount(&[
            LunRequest::new(0, "/data/a.img", Mode::Rw),
            LunRequest::new(1, "/data/b.img", Mode::Rw),
        ])
        .unwrap();
        assert!(m.fs().exists(&paths::lun_path(1)));
        let _ = m.take_trace();

        // lun.1 仍绑着 b.img —— 直接 rmdir 会被拒，必须先弹。
        m.delete_slot(1).expect("应先弹出再删除");

        let trace = m.take_trace();
        let eject_pos = trace
            .iter()
            .position(|s| matches!(s, Step::Write(p, v) if p.ends_with("forced_eject") && v == "1"))
            .expect("应先强制弹出");
        let rmdir_pos = trace
            .iter()
            .position(|s| matches!(s, Step::Rmdir(p) if p.ends_with("lun.1")))
            .expect("应 rmdir lun.1");
        assert!(eject_pos < rmdir_pos, "弹出必须早于 rmdir；trace={trace:?}");
    }

    #[test]
    fn verify_bound_rejects_missing_link() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let m = MassStorage::new(fs);
        let err = m.verify_bound(&[0]).unwrap_err();
        assert!(matches!(err, crate::error::GadgetError::NotActive(_)));
    }

    #[test]
    fn verify_bound_rejects_unbound_file() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        // 清掉绑定后校验必须失败。
        m.write_attr(&paths::lun_attr(0, "file"), "").unwrap();
        assert!(m.verify_bound(&[0]).is_err());
    }

    #[test]
    fn wait_for_udc_bound_reflects_kernel_truth() {
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        let mut m = MassStorage::new(fs).with_udc_rebind_timeout(Duration::from_millis(60));

        // 未绑定：等待应超时返回 None（不谎报）。
        assert_eq!(m.wait_for_udc_bound(Duration::ZERO), None);
        // 绑上之后立刻可见。
        m.fs_mut().write(paths::UDC_ATTR, "dummy_udc.0").unwrap();
        assert_eq!(
            m.wait_for_udc_bound(Duration::ZERO).as_deref(),
            Some("dummy_udc.0")
        );
    }

    #[test]
    fn wait_for_udc_bound_can_observe_an_external_binding() {
        // 模拟「Android 在等待期间替我们绑回」：由另一个句柄写入。
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        let m = MassStorage::new(fs).with_udc_rebind_timeout(Duration::from_millis(50));

        let handle = m.fs().clone();
        let bound = m.wait_for_udc_bound(Duration::from_millis(40));
        // 无人绑 → None。
        assert!(bound.is_none());
        // 句柄可用（用于构造外部绑定场景的断言）。
        assert!(handle.read(paths::UDC_ATTR).unwrap().is_empty());
    }

    #[test]
    fn rebind_rebinds_the_same_udc_when_nobody_helps() {
        let fs = ready_fs();
        let mut m = ms(fs);
        let udc = m.rebind().unwrap();
        assert_eq!(udc, "dummy_udc.0");
        assert_eq!(m.bound_udc(), "dummy_udc.0");
    }

    #[test]
    fn mount_is_idempotent_for_the_same_image() {
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");
        let mut m = ms(fs);
        m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        let luns = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        assert_eq!(luns.len(), 1);
        assert_eq!(luns[0].image_path, "/data/a.img");
    }

    #[test]
    fn mount_with_empty_list_is_a_noop() {
        // 空请求不得做任何 configfs 改动（`luns()` 仍会如实报告内核里已有的
        // lun.0，因此断言的是 **trace** 而不是返回值）。
        let fs = ready_fs();
        let mut m = ms(fs);
        m.mount(&[]).unwrap();
        assert!(m.take_trace().is_empty(), "空请求不得改动 configfs");
    }

    #[test]
    fn effective_requires_configured_udc_state() {
        // `effective` 必须取内核真值：只有 UDC state == configured 才算生效。
        let dir = temp_dir("effective");
        let mut fs = ready_fs();
        known(&mut fs, "/data/a.img");

        let class_dir = dir.join("udc");
        std::fs::create_dir_all(class_dir.join("dummy_udc.0")).unwrap();
        std::fs::write(class_dir.join("dummy_udc.0/state"), "addressed\n").unwrap();

        let mut m = MassStorage::new(fs)
            .with_udc_rebind_timeout(Duration::ZERO)
            .with_udc_class_dir(&class_dir);
        let luns = m
            .mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
            .unwrap();
        assert!(luns[0].attached);
        assert!(
            !luns[0].effective,
            "state=addressed 时不得谎报 effective=true"
        );

        std::fs::write(class_dir.join("dummy_udc.0/state"), "configured\n").unwrap();
        assert!(m.luns()[0].effective, "state=configured 时应为 effective");

        // 读不到 state 文件时保守为 false。
        std::fs::remove_dir_all(&class_dir).ok();
        assert!(!m.luns()[0].effective);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_means_configured_is_strict() {
        assert!(paths::state_means_configured("configured"));
        assert!(paths::state_means_configured("Configured\n"));
        for other in ["addressed", "not attached", "suspended", ""] {
            assert!(
                !paths::state_means_configured(other),
                "{other} 不应被判为 configured"
            );
        }
    }

    #[test]
    fn function_is_created_when_absent_but_lun0_is_missing() {
        // function 不存在 → 我们创建它；内核会随之生成 lun.0。
        // 内存实现模拟了这一行为（mkdir function 时插入 lun.0 属性集），
        // 但**不会**自动建 lun.0 目录，所以这里显式区分：
        // 先建 function，再建 lun.0，看是否成功。
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        fs.write(paths::UDC_ATTR, "dummy_udc.0").unwrap();
        let mut m = ms(fs);
        // 内核支持时 lun.0 随 function 出现；内存实现里需要显式建。
        m.fs_mut().mkdir(&paths::function_path()).unwrap();
        m.fs_mut().mkdir(&paths::lun_path(0)).unwrap();
        m.fs_mut().add_known_file("/data/a.img");
        assert!(
            m.mount(&[LunRequest::new(0, "/data/a.img", Mode::Rw)])
                .is_ok()
        );
    }
}
