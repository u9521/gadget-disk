//! 镜像文件的 SELinux 上下文：挂载前检查并按需修正。
//!
//! ## 问题
//!
//! `mass_storage` 由**内核线程**读取后备镜像（`f_mass_storage.c` 的
//! `file-storage`，用 `kernel_read`），而 SELinux 按**该内核线程的域**判定其读
//! 权限。我们的镜像默认继承 `u:object_r:adb_data_file:s0`（由 root 管理器定义），
//! 而 `kernel` 域对它没有 `read` 权限：
//!
//! ```text
//! avc: denied { read } for comm="file-storage"
//!      scontext=u:r:kernel:s0 tcontext=u:object_r:adb_data_file:s0 tclass=file
//! ```
//!
//! 表现是：设备**枚举成功**，但主机侧**读不出任何内容**
//! （`critical medium error` / `unable to read partition table`）。
//!
//! ## 实测（x86_64 AVD，Android 17）
//!
//! 判据取自**内核真值表** `/sys/fs/selinux/access`（`u:r:kernel:s0` → 目标类型），
//! 并用「guest 写入后镜像的 `md5` 是否变化」做端到端交叉验证：
//!
//! | 上下文 | kernel `read` | kernel `write` | guest 写入是否落到镜像 | 新增 avc |
//! |---|---|---|---|---|
//! | `u:object_r:media_rw_data_file:s0` | ALLOW | **ALLOW** | **YES** | **0** |
//! | `u:object_r:system_file:s0` | ALLOW | **DENY** | **NO**（`md5` 不变） | 10 |
//! | `u:object_r:vendor_file:s0` | DENY | DENY | 不可用 | — |
//! | `u:object_r:adb_data_file:s0`（继承的默认） | DENY | DENY | 不可用 | — |
//!
//! ### 读写双向权限约束
//!
//! 若仅配置只读上下文（如 `system_file`），实际后果是**写入被内核静默丢弃**：
//! `denied { write }` 会引发底层写入失败，而设备侧无明细告警——`lun.N/ro` 仍回显 `0`，
//! guest 挂载为 `rw`，`touch` 成功，`dmesg` 无只读报错。必须比对镜像校验和方能验证落盘。
//!
//! 因此判据必须要求 **read 与 write 权限同时满足**，`media_rw_data_file` 为实测同时满足两者的类型。
//!
//! ### 为什么 `lun.N/ro` 不能当判据
//!
//! `fsg_lun_open`（`storage_common.c`）在 `O_RDWR` 失败时回退到 `O_RDONLY`
//! 并置 `curlun->ro = 1`，但 `fsg_show_ro` 在 LUN 已打开时回显逻辑特殊，实测在写入被拒的情况下仍读到 `0`。
//! 验收必须以写入后校验和为准（见 docs/testing.md）。
//!
//! 另两条实测：上下文跨重启保持（未被 `restorecon` 重置）；用
//! `ksud sepolicy apply 'allow kernel adb_data_file:file { read open }'` 加规则
//! 无效（规则被接受但 AVC 拦截依旧）。因此修正文件自身标签为当前唯一可靠手段。
//!
//! ## 设计要点
//!
//! 1. **只修改本模块镜像目录内的文件**。不在 `images/` 下时仅输出警告、绝不改动，
//!    避免越权破坏宿主其他应用的安全策略。
//! 2. **标签修正失败不阻断挂载**。权限不足或文件系统只读时仅输出警告并说明潜在后果，
//!    交由用户决策是否继续。
//! 3. **目标上下文支持配置**。默认 `media_rw_data_file`，支持在配置文件中自定义覆盖
//!    以适配不同定制 ROM 策略差异。
//! 4. **改标签后需要重新挂载才生效**：内核在 `lun.N/file` 写入的瞬间按**当时**的
//!    标签打开文件并 pin 住，已在导出中的 LUN 不会因为改标签而重新校验权限。
//! 5. **改标签有副作用**：`media_rw_data_file` 的访问规则比 `adb_data_file` 宽，
//!    其他应用对该文件的访问可能受影响。我们的镜像位于 `0700` 的 `/data/adb`
//!    下、只有 root 能到达，因此实际影响面很小——但这是一条**明确的取舍**。

use std::path::{Path, PathBuf};

/// 默认的目标上下文。
///
/// 依据：AVD 实测**内核读与写都允许**、0 条 avc，且 guest 侧的写入真的落到镜像
/// （`md5` 变化）。详见本模块文档的实测表。
///
/// 为什么不是 `system_file`：它只允许内核 `read`，`write` 被拒且**静默丢失**——
/// 设备侧完全看不出异常，只有比对校验和才能发现。M10 曾误选它。
pub const DEFAULT_IMAGE_CONTEXT: &str = "u:object_r:media_rw_data_file:s0";

/// xattr 名。
const SELINUX_XATTR: &str = "security.selinux";

/// 读/写 SELinux 上下文的能力。
///
/// 抽成 trait 是为了让「检查与修正」的**决策逻辑**能在主机上测试——开发机上
/// `security.selinux` 通常不可写（甚至不存在），直接调 `libc` 无法断言四种分支。
pub trait ContextStore {
    /// 读取文件的 SELinux 上下文；读不到返回 `Err`（原因由实现决定）。
    fn get(&self, path: &Path) -> std::io::Result<String>;

    /// 设置文件的 SELinux 上下文。
    fn set(&self, path: &Path, context: &str) -> std::io::Result<()>;
}

/// 真实实现：`lgetxattr`/`lsetxattr`（**l** 前缀 = 不跟随符号链接）。
///
/// 用 xattr 而不是 `chcon` 子进程：少一次 fork/exec，且错误码直接可用（不必
/// 解析子进程的输出）。模块包也少一个对外部命令的依赖。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealContextStore;

impl ContextStore for RealContextStore {
    fn get(&self, path: &Path) -> std::io::Result<String> {
        get_xattr(path, SELINUX_XATTR)
    }

    fn set(&self, path: &Path, context: &str) -> std::io::Result<()> {
        set_xattr(path, SELINUX_XATTR, context)
    }
}

/// 读一个扩展属性。
fn get_xattr(path: &Path, name: &str) -> std::io::Result<String> {
    // 先问长度：`rustix::fs::lgetxattr` 不自动扩容，空 Vec 即等价于「传 null/0」。
    // 两步都不再需要裸 libc 调用，也不需要手工构造 `CString` 或转换 errno。
    let mut probe: Vec<u8> = Vec::new();
    let len = rustix::fs::lgetxattr(path, name, &mut probe).map_err(std::io::Error::from)?;

    let mut buf = vec![0u8; len];
    let read = rustix::fs::lgetxattr(path, name, &mut buf).map_err(std::io::Error::from)?;
    buf.truncate(read);

    // SELinux 的上下文是 ASCII，但用 lossy 以免极端情况下 panic。
    Ok(String::from_utf8_lossy(&buf)
        .trim_end_matches('\0')
        .to_string())
}

/// 写一个扩展属性。
fn set_xattr(path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    let c_path = c_path(path)?;
    let c_name = std::ffi::CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "attribute name contains NUL",
        )
    })?;
    let c_value = std::ffi::CString::new(value).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "attribute value contains NUL",
        )
    })?;

    // SAFETY: 三个指针都指向有效的 NUL 结尾字符串/缓冲区，长度不含结尾 NUL。
    let rc = unsafe {
        libc::lsetxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            c_value.as_ptr().cast::<libc::c_void>(),
            value.len(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// 把路径转成 C 字符串。
fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })
}

/// 一次上下文检查的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextOutcome {
    /// 已经是目标上下文，无需动作。
    AlreadyCorrect {
        /// 当前上下文。
        context: String,
    },
    /// 已改成目标上下文。
    Relabeled {
        /// 改动前的上下文。
        from: String,
        /// 改动后的上下文。
        to: String,
    },
    /// 不在镜像目录里，**未改动**。
    OutsideImagesDir {
        /// 当前上下文。
        context: String,
    },
    /// 在镜像目录里但改不动，**未改动**。
    RelabelFailed {
        /// 当前上下文。
        from: String,
        /// 想改成的目标。
        to: String,
        /// 失败原因。
        reason: String,
    },
    /// 读不到上下文（非 SELinux 设备，或该文件系统不支持 xattr）。
    ///
    /// **不是错误**：没有 SELinux 的环境本来就不需要改。
    Unknown {
        /// 读取失败的原因。
        reason: String,
    },
}

impl ContextOutcome {
    /// 需要向用户显示的警告文本；无需警告时返回 `None`。
    pub fn warning(&self, image: &Path) -> Option<String> {
        match self {
            // 已正确、以及读不到上下文（非 SELinux 环境），都不打扰用户。
            ContextOutcome::AlreadyCorrect { .. } | ContextOutcome::Unknown { .. } => None,
            ContextOutcome::Relabeled { from, to } => Some(format!(
                "changed the security context of {} from {from} to {to} (the kernel needs this to \
                 read the image; otherwise the computer sees the device but reads no contents)",
                image.display()
            )),
            ContextOutcome::OutsideImagesDir { context } => Some(format!(
                "{} is outside the image directory (current context {context}); its security context \
                 was left unchanged. If the kernel therefore cannot read the image, the computer will \
                 read no contents",
                image.display()
            )),
            ContextOutcome::RelabelFailed { from, to, reason } => Some(format!(
                "cannot change the security context of {} from {from} to {to}: {reason}; the kernel may \
                 therefore be unable to read the image (the computer sees the device but reads no \
                 contents)",
                image.display()
            )),
        }
    }
}

/// 检查并（必要时）修正 `image` 的 SELinux 上下文。
///
/// `images_dir` 是**我们自己的**镜像目录：只有位于它下面的文件才允许改标签。
/// `target` 是目标上下文（来自配置，默认 [`DEFAULT_IMAGE_CONTEXT`]）。
pub fn check_and_fix(
    store: &impl ContextStore,
    images_dir: &Path,
    image: &Path,
    target: &str,
) -> ContextOutcome {
    let current = match store.get(image) {
        Ok(value) => value,
        Err(err) => {
            return ContextOutcome::Unknown {
                reason: err.to_string(),
            };
        }
    };

    if current == target {
        return ContextOutcome::AlreadyCorrect { context: current };
    }

    if !is_inside(images_dir, image) {
        return ContextOutcome::OutsideImagesDir { context: current };
    }

    match store.set(image, target) {
        Ok(()) => ContextOutcome::Relabeled {
            from: current,
            to: target.to_string(),
        },
        Err(err) => ContextOutcome::RelabelFailed {
            from: current,
            to: target.to_string(),
            reason: err.to_string(),
        },
    }
}

/// `image` 是否位于 `dir` 之下。
///
/// 用**词法**比较而不是 `canonicalize`：调用方传进来的路径已经是绝对路径
/// （`serve`/CLI 都做过校验），而 `canonicalize` 会在文件不存在时失败——那时
/// 我们更需要「它不在镜像目录里」这个判断，而不是一个 IO 错误。
fn is_inside(dir: &Path, image: &Path) -> bool {
    // 逐组件比较，避免 `/data/adb/gadget-disk/images-evil/x` 被误判为在
    // `/data/adb/gadget-disk/images` 之下（字符串前缀比较的经典陷阱）。
    let mut dir_parts = dir.components();
    let mut image_parts = image.components();
    loop {
        match (dir_parts.next(), image_parts.next()) {
            (Some(expected), Some(actual)) => {
                if expected != actual {
                    return false;
                }
            }
            // 目录组件用完 → 在它之下。
            (None, Some(_)) => return true,
            // 文件路径更短或完全相同。
            _ => return false,
        }
    }
}

/// 解析目标上下文：配置里的值，缺省用 [`DEFAULT_IMAGE_CONTEXT`]。
pub fn resolve_target(configured: Option<&str>) -> String {
    match configured {
        Some(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => DEFAULT_IMAGE_CONTEXT.to_string(),
    }
}

/// 上下文长度的上限（字节）。
///
/// SELinux 上下文没有内核强制的短上限，但**真实标签通常在 100 字节以内**
/// （`u:object_r:<type>:s0[:<categories>]`）。设置宽松上限可防范意外粘贴超长文本等误操作——
/// 避免畸形内容被原样写入 `security.selinux` 扩展属性并在后续挂载中引发连锁异常。
pub const MAX_CONTEXT_BYTES: usize = 256;

/// 校验并规范化用户输入的上下文。
///
/// ## 仅执行格式形态校验，完整语法交由内核判定
///
/// 完整语法由内核判定，我们**不**解析类型名是否真实存在（亦无法在无 SELinux
/// 的宿主上判定）。此处主要拦截显而易见的格式错误：空值、缺少 `:` 分隔符、
/// 包含空白或控制字符（`lsetxattr` 会原样写入，随后内核拒绝导致每次挂载均失败）、
/// 以及超长值。
///
/// 返回**规范化**（`trim` 后）的值，调用方无需重复处理空白。
///
/// ## 为什么先校验再落盘
///
/// 与 `save_and_apply_identity` 保持相同的架构纪律：非法值一旦写入
/// `config/gadget.json`，后续**每一次挂载**均将重新加载并再度失败，
/// 直至用户手动修正文件。因此输入错误绝不应被持久化。
///
/// 错误信息为英文：CLI 与 REST 面向操作者与程序化接口，界面中文文案由前端
/// `webui/pure/task.js` 的 `validateImageContext` 独占（见
/// [语言边界](../../../../docs/README.md#语言边界操作者输出用英文)）。
pub fn validate_context_format(value: &str) -> Result<String, String> {
    let trimmed = value.trim();

    if trimmed.is_empty() {
        return Err(
            "the security context is empty (example: u:object_r:media_rw_data_file:s0)".to_string(),
        );
    }
    if trimmed.len() > MAX_CONTEXT_BYTES {
        return Err(format!(
            "the security context is {} bytes, exceeding the limit of {MAX_CONTEXT_BYTES} bytes",
            trimmed.len()
        ));
    }
    if !trimmed.contains(':') {
        return Err(format!(
            "invalid SELinux context format (example: u:object_r:media_rw_data_file:s0), got: {trimmed:?}"
        ));
    }
    // 空白与控制字符会让 `lsetxattr` 写入一个内核无法解析的值；换行尤其隐蔽：
    // WebUI 的输入框能粘进多行文本，而 xattr 会原样保存。
    if let Some(bad) = trimmed
        .chars()
        .find(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "the security context contains whitespace or a control character {bad:?}: {trimmed:?}"
        ));
    }
    Ok(trimmed.to_string())
}

/// 镜像目录的绝对路径（供调用方构造参数）。
pub fn images_dir(root: &Path) -> PathBuf {
    root.join("images")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 可脚本化的上下文存储。
    #[derive(Default)]
    struct FakeStore {
        contexts: Mutex<HashMap<PathBuf, String>>,
        /// 设置时是否报错。
        fail_set: bool,
        /// 读取时是否报错（模拟非 SELinux 环境）。
        fail_get: bool,
    }

    impl FakeStore {
        fn with(path: &str, context: &str) -> Self {
            let store = Self::default();
            store
                .contexts
                .lock()
                .unwrap()
                .insert(PathBuf::from(path), context.to_string());
            store
        }

        fn failing_set() -> Self {
            Self {
                fail_set: true,
                ..Self::default()
            }
        }

        fn failing_get() -> Self {
            Self {
                fail_get: true,
                ..Self::default()
            }
        }

        fn context_of(&self, path: &str) -> Option<String> {
            self.contexts.lock().unwrap().get(Path::new(path)).cloned()
        }
    }

    impl ContextStore for FakeStore {
        fn get(&self, path: &Path) -> std::io::Result<String> {
            if self.fail_get {
                return Err(std::io::Error::other("no xattr support"));
            }
            self.contexts
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "无此属性"))
        }

        fn set(&self, path: &Path, context: &str) -> std::io::Result<()> {
            if self.fail_set {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Operation not permitted",
                ));
            }
            self.contexts
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), context.to_string());
            Ok(())
        }
    }

    const IMAGES: &str = "/data/adb/gadget-disk/images";
    const IMAGE: &str = "/data/adb/gadget-disk/images/a.img";
    const TARGET: &str = DEFAULT_IMAGE_CONTEXT;

    #[test]
    fn already_correct_context_needs_no_warning() {
        let store = FakeStore::with(IMAGE, TARGET);
        let outcome = check_and_fix(&store, Path::new(IMAGES), Path::new(IMAGE), TARGET);

        assert_eq!(
            outcome,
            ContextOutcome::AlreadyCorrect {
                context: TARGET.to_string()
            }
        );
        assert!(outcome.warning(Path::new(IMAGE)).is_none(), "不该打扰用户");
    }

    #[test]
    fn wrong_context_inside_images_dir_is_fixed_with_a_warning() {
        let store = FakeStore::with(IMAGE, "u:object_r:adb_data_file:s0");
        let outcome = check_and_fix(&store, Path::new(IMAGES), Path::new(IMAGE), TARGET);

        match &outcome {
            ContextOutcome::Relabeled { from, to } => {
                assert_eq!(from, "u:object_r:adb_data_file:s0");
                assert_eq!(to, TARGET);
            }
            other => panic!("期望 Relabeled，得到 {other:?}"),
        }
        // 真的改了。
        assert_eq!(store.context_of(IMAGE).as_deref(), Some(TARGET));
        // 警告要说明**为什么**（否则用户不理解为何动了他的文件标签）。
        let warning = outcome.warning(Path::new(IMAGE)).expect("应有警告");
        assert!(warning.contains("security context"), "得到 {warning}");
        assert!(warning.contains("reads no contents"), "得到 {warning}");
    }

    /// **关键边界**：不在镜像目录里的文件只警告、**绝不改**。
    ///
    /// 用户的文件可能被别的策略依赖，越权改别人的标签是危险的副作用。
    #[test]
    fn files_outside_the_images_dir_are_never_relabeled() {
        let outside = "/data/local/tmp/out.img";
        let store = FakeStore::with(outside, "u:object_r:shell_data_file:s0");
        let outcome = check_and_fix(&store, Path::new(IMAGES), Path::new(outside), TARGET);

        match &outcome {
            ContextOutcome::OutsideImagesDir { context } => {
                assert_eq!(context, "u:object_r:shell_data_file:s0");
            }
            other => panic!("期望 OutsideImagesDir，得到 {other:?}"),
        }
        // 上下文**未变**。
        assert_eq!(
            store.context_of(outside).as_deref(),
            Some("u:object_r:shell_data_file:s0"),
            "目录外的文件不得被改标签"
        );
        let warning = outcome.warning(Path::new(outside)).expect("应有警告");
        assert!(warning.contains("left unchanged"), "得到 {warning}");
    }

    /// 目录**外**但恰好已是对的上下文 → 无需警告（我们没做任何事）。
    #[test]
    fn outside_dir_with_correct_context_is_silent() {
        let outside = "/data/local/tmp/out.img";
        let store = FakeStore::with(outside, TARGET);
        let outcome = check_and_fix(&store, Path::new(IMAGES), Path::new(outside), TARGET);
        assert!(matches!(outcome, ContextOutcome::AlreadyCorrect { .. }));
        assert!(outcome.warning(Path::new(outside)).is_none());
    }

    #[test]
    fn relabel_failure_warns_but_does_not_pretend_success() {
        let store = FakeStore::failing_set();
        store
            .contexts
            .lock()
            .unwrap()
            .insert(PathBuf::from(IMAGE), "u:object_r:adb_data_file:s0".into());

        let outcome = check_and_fix(&store, Path::new(IMAGES), Path::new(IMAGE), TARGET);

        match &outcome {
            ContextOutcome::RelabelFailed { from, to, reason } => {
                assert_eq!(from, "u:object_r:adb_data_file:s0");
                assert_eq!(to, TARGET);
                assert!(reason.contains("permitted"), "得到 {reason}");
            }
            other => panic!("期望 RelabelFailed，得到 {other:?}"),
        }
        // 未变。
        assert_eq!(
            store.context_of(IMAGE).as_deref(),
            Some("u:object_r:adb_data_file:s0")
        );
        // 警告必须点明后果，而不是只说「失败了」。
        let warning = outcome.warning(Path::new(IMAGE)).expect("应有警告");
        assert!(warning.contains("reads no contents"), "得到 {warning}");
    }

    /// 读不到上下文（非 SELinux 设备）→ 静默通过，**不是**错误。
    #[test]
    fn unreadable_context_is_silent() {
        let store = FakeStore::failing_get();
        let outcome = check_and_fix(&store, Path::new(IMAGES), Path::new(IMAGE), TARGET);

        assert!(matches!(outcome, ContextOutcome::Unknown { .. }));
        assert!(
            outcome.warning(Path::new(IMAGE)).is_none(),
            "没有 SELinux 的环境不该被打扰"
        );
    }

    /// 目录前缀比较必须按**路径组件**，不能被字符串前缀骗到。
    #[test]
    fn is_inside_compares_path_components_not_string_prefix() {
        let dir = Path::new("/data/adb/gadget-disk/images");
        assert!(is_inside(
            dir,
            Path::new("/data/adb/gadget-disk/images/a.img")
        ));
        assert!(is_inside(
            dir,
            Path::new("/data/adb/gadget-disk/images/sub/a.img")
        ));
        // 经典陷阱：`images-evil` 以 `images` 开头，但它不在 images/ 下。
        assert!(!is_inside(
            dir,
            Path::new("/data/adb/gadget-disk/images-evil/a.img")
        ));
        // 目录本身不算「在其下」（它是目录，不是目录里的文件）。
        assert!(!is_inside(dir, dir));
        // 更短的路径。
        assert!(!is_inside(dir, Path::new("/data/adb/gadget-disk")));
        assert!(!is_inside(dir, Path::new("/data/local/tmp/out.img")));
    }

    #[test]
    fn resolve_target_falls_back_to_the_default() {
        assert_eq!(resolve_target(None), DEFAULT_IMAGE_CONTEXT);
        assert_eq!(resolve_target(Some("")), DEFAULT_IMAGE_CONTEXT);
        assert_eq!(resolve_target(Some("   ")), DEFAULT_IMAGE_CONTEXT);
        assert_eq!(
            resolve_target(Some(" u:object_r:vendor_file:s0 ")),
            "u:object_r:vendor_file:s0"
        );
    }

    #[test]
    fn validate_context_format_accepts_and_trims_a_well_formed_context() {
        assert_eq!(
            validate_context_format("u:object_r:media_rw_data_file:s0").unwrap(),
            "u:object_r:media_rw_data_file:s0"
        );
        // 两侧空白由**规范化**吃掉：否则会被原样写进 xattr。
        assert_eq!(
            validate_context_format("  u:object_r:vendor_file:s0\n").unwrap(),
            "u:object_r:vendor_file:s0"
        );
        // 带 MLS 级别的上下文同样合法（真机上常见 `:s0:c0.c1023`）。
        assert_eq!(
            validate_context_format("u:object_r:media_rw_data_file:s0:c0.c1023").unwrap(),
            "u:object_r:media_rw_data_file:s0:c0.c1023"
        );
    }

    /// 非法输入必须**被拒绝**，而不是落盘后每次挂载都再失败一次。
    #[test]
    fn validate_context_format_rejects_obviously_wrong_input() {
        // 空 / 纯空白。
        assert!(validate_context_format("").is_err());
        assert!(validate_context_format("   ").is_err());
        assert!(validate_context_format("\n").is_err());
        // 缺 `:`：用户把它当成类型名写。
        assert!(validate_context_format("media_rw_data_file").is_err());
        // 内部空白（含制表符与换行）：xattr 会原样写入，内核随后拒绝。
        assert!(validate_context_format("u:object_r:media rw:s0").is_err());
        assert!(validate_context_format("u:object_r:a\tb:s0").is_err());
        assert!(validate_context_format("a:b\nc:d").is_err());
        // 超长。
        let long = format!("u:object_r:{}:s0", "x".repeat(MAX_CONTEXT_BYTES));
        assert!(validate_context_format(&long).is_err());
    }

    /// 错误信息必须**点名格式**（用户要照着重写），且是英文。
    #[test]
    fn validate_context_format_error_is_actionable_and_english() {
        let err = validate_context_format("nope").unwrap_err();
        assert!(err.contains("u:object_r:"), "得到 {err}");
        assert!(
            !err.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "面向 CLI/REST 的错误必须是英文：{err}"
        );
    }

    /// **默认上下文必须允许内核写入**，不能是只读的 `system_file`。
    ///
    /// 回归（AVD 实测，见模块文档的表）：M10 选了 `system_file`，内核 `read` 允许
    /// 但 `write` 被拒，导致 guest 的写入被**静默丢弃**——`lun.N/ro` 仍回显 `0`、
    /// guest 挂载为 `rw`、`dmesg` 无 `read-only`，只有镜像 `md5` 能揭穿。
    ///
    /// 这条测试把默认值钉死，避免有人凭「能读就行」的印象改回去。
    #[test]
    fn default_context_must_allow_kernel_write() {
        assert_eq!(
            DEFAULT_IMAGE_CONTEXT, "u:object_r:media_rw_data_file:s0",
            "默认上下文必须同时允许内核 read 与 write；system_file 只允许 read，\
             会让 guest 写入静默丢失（见模块文档的实测表与 docs/testing.md）"
        );
        // 反过来也钉住：不要退回只读的那个类型。
        assert_ne!(
            DEFAULT_IMAGE_CONTEXT, "u:object_r:system_file:s0",
            "system_file 只允许内核 read，写入会被静默丢弃"
        );
    }

    #[test]
    fn images_dir_is_under_the_data_root() {
        assert_eq!(
            images_dir(Path::new("/data/adb/gadget-disk")),
            PathBuf::from("/data/adb/gadget-disk/images")
        );
    }
}
