//! **CLI 拥有**的运行期与配置文件的路径。
//!
//! ## 为什么与 `gadgetdisk_gdd::paths` 分开
//!
//! 两个 crate 共用同一份**目录布局**（它们必须看到同一个数据根与同一个 socket
//! —— 否则会各自连到自己以为的那一套），但**文件**的归属完全不同：
//!
//! | 文件 | 归属 | 为什么 |
//! |---|---|---|
//! | `run/gdd.sock`、`run/ops.lock` | 共用（`gdd::paths`） | 通信与串行化需要双方一致 |
//! | `run/state.json` | **CLI** | 导出意图：重启后该恢复成什么样 |
//! | `run/gadget-backup.json` | **CLI** | Android 原始身份，身份是 CLI 改的 |
//! | `run/offsets.json` | **CLI** | loop 分区偏移缓存（loop 归 CLI） |
//! | `run/loop-attachments.json` | **CLI** | 活跃 loop 附件（loop 归 CLI） |
//! | `logs/service.log` | **CLI** | 开机对账日志（对账由 CLI 做） |
//! | `logs/cli.log` | **CLI** | CLI 自身日志 |
//! | `logs/gdd.log` | **gdd** | `gdd` 日志（路径由 CLI 经 `--log-file` 传入） |
//! | `logs/serve.log` | **CLI** | `serve` 的标准流（由 WebUI 拉起时重定向） |
//! | `config/gadget.json` | **CLI** | 用户设置的身份 |
//!
//! `gdd` 是**无状态**的：它不认识下面任何一个文件。这条边界不由约定保证，
//! 而由 `crates/gadgetdisk-gdd/tests/scope.rs` 的源码扫描强制——该测试会失败
//! 如果 `gdd` 的代码里出现这些文件名。

use std::path::PathBuf;

use gadgetdisk_gdd::paths::DataDirs;

pub const STATE_JSON_NAME: &str = "state.json";
pub const OFFSETS_JSON_NAME: &str = "offsets.json";
pub const LOOP_REGISTRY_NAME: &str = "loop-attachments.json";
pub const SERVICE_LOG_NAME: &str = "service.log";
pub const CLI_LOG_NAME: &str = "cli.log";
pub const GDD_LOG_NAME: &str = "gdd.log";
pub const SERVE_LOG_NAME: &str = "serve.log";

/// 单个日志文件的大小上限；超过即轮转一代（`<name>.1`）。
pub const LOG_MAX_BYTES: u64 = 256 * 1024;

pub const IDENTITY_BACKUP_NAME: &str = gadgetdisk_usb::identity::BACKUP_FILE_NAME;
pub const GADGET_CONFIG_NAME: &str = "gadget.json";

/// 持久化导出意图路径（`run/state.json`），供开机对账恢复 USB LUN 状态。
pub fn state_json(dirs: &DataDirs) -> PathBuf {
    dirs.run().join(STATE_JSON_NAME)
}

/// Android 原始 gadget 身份备份文件路径（`run/gadget-backup.json`）。
pub fn identity_backup(dirs: &DataDirs) -> PathBuf {
    dirs.run().join(IDENTITY_BACKUP_NAME)
}

/// 磁盘分区起始偏移缓存文件路径（`run/offsets.json`），加速本地 loop 挂载探测。
pub fn offsets_json(dirs: &DataDirs) -> PathBuf {
    dirs.run().join(OFFSETS_JSON_NAME)
}

/// 活跃 loop 设备挂载登记文件路径（`run/loop-attachments.json`），用于跨进程生命周期清理。
pub fn loop_registry(dirs: &DataDirs) -> PathBuf {
    dirs.run().join(LOOP_REGISTRY_NAME)
}

/// 开机对账服务执行日志路径（`logs/service.log`）。
pub fn service_log(dirs: &DataDirs) -> PathBuf {
    dirs.logs().join(SERVICE_LOG_NAME)
}

/// CLI 自身执行与警告日志路径（`logs/cli.log`）。
pub fn cli_log(dirs: &DataDirs) -> PathBuf {
    dirs.logs().join(CLI_LOG_NAME)
}

/// `gdd` 守护进程诊断日志路径（`logs/gdd.log`）。
pub fn gdd_log(dirs: &DataDirs) -> PathBuf {
    dirs.logs().join(GDD_LOG_NAME)
}

/// REST `serve` 后端进程标准流重定向日志路径（`logs/serve.log`）。
pub fn serve_log(dirs: &DataDirs) -> PathBuf {
    dirs.logs().join(SERVE_LOG_NAME)
}

/// `config/gadget.json`：用户设置的 USB 设备身份。
pub fn gadget_config(dirs: &DataDirs) -> PathBuf {
    dirs.config().join(GADGET_CONFIG_NAME)
}

/// 分区偏移缓存表（`run/offsets.json`）。
///
/// 规格：[docs/ondevice-loop-mount.md](../../../docs/ondevice-loop-mount.md) 要求
/// 偏移**持久化**以省去每次重新解析分区表。但它**不是**真相来源：分区表才是，
/// 缓存只在分区表读不出来时兜底（早期实现只读缓存，导致导入的镜像偏移恒为 0、
/// 根本挂不上——见 docs/roadmap.md 已知缺陷 #4）。
///
/// 用单一 JSON 而不是「每个镜像一个文件」，与 `run/` 下其余状态一致。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Offsets {
    /// schema 版本。
    #[serde(default = "default_offsets_version")]
    pub version: u32,
    /// 镜像名 → 分区起始偏移（字节）。
    #[serde(default)]
    pub offsets: std::collections::BTreeMap<String, u64>,
}

fn default_offsets_version() -> u32 {
    1
}

impl Offsets {
    /// 从 `run/offsets.json` 读取；不存在或损坏时返回空表。
    ///
    /// 损坏按「没有缓存」处理：最坏结果是重新解析分区表，而不是拒绝服务。
    pub fn load(dirs: &DataDirs) -> Self {
        std::fs::read_to_string(offsets_json(dirs))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 查询某个镜像的偏移（`0` 表示没有记录）。
    pub fn get(dirs: &DataDirs, name: &str) -> u64 {
        Self::load(dirs).offsets.get(name).copied().unwrap_or(0)
    }

    /// 记录一个偏移并原子写回。
    pub fn set(dirs: &DataDirs, name: &str, offset: u64) -> std::io::Result<()> {
        let mut table = Self::load(dirs);
        table.version = default_offsets_version();
        table.offsets.insert(name.to_string(), offset);
        table.store(dirs)
    }

    /// 删除一个记录并原子写回。
    pub fn remove(dirs: &DataDirs, name: &str) -> std::io::Result<()> {
        let mut table = Self::load(dirs);
        table.offsets.remove(name);
        table.store(dirs)
    }

    /// 原子写回；表为空时删除文件（不留空壳）。
    pub fn store(&self, dirs: &DataDirs) -> std::io::Result<()> {
        if self.offsets.is_empty() {
            return gadgetdisk_usb::jsonfile::remove_if_exists(&offsets_json(dirs));
        }
        let text = serde_json::to_string_pretty(self)?;
        gadgetdisk_usb::jsonfile::write_json_atomic(&offsets_json(dirs), text.as_bytes())
    }
}

/// `config/gadget.json` 的内容：**持久配置**（用户可见、可手改）。
///
/// ## 为什么身份与镜像上下文在同一个文件
///
/// 两者都是「用户对这个模块的持久设置」，放在一起让用户只需看/改一个文件。
/// 用 `#[serde(flatten)]` 保持 JSON **扁平**，因此旧版本写的文件（只有身份字段）
/// 可以直接读——新字段缺省即 `None`。
///
/// 语义上它们是两件事（USB 设备身份 vs 镜像文件的 SELinux 标签），将来若其中
/// 一个膨胀到需要独立演进，就把它拆出去：`flatten` 让拆分不影响磁盘格式。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GadgetConfig {
    /// USB 设备身份（`idVendor`/`idProduct`/字符串描述符）。
    ///
    /// 扁平展开，因此 JSON 里直接是 `{"id_vendor":…,"product":…}`。
    #[serde(flatten)]
    pub identity: gadgetdisk_usb::Identity,
    /// 镜像文件的 SELinux 目标上下文。
    ///
    /// 缺省（`None`）时用 `selinux::DEFAULT_IMAGE_CONTEXT`。做成可配置是因为
    /// AVD 与真机的 policy 未必一致——`system_file` 在 AVD 上实测有效，真机上
    /// 若无效，用户可以换一个值而不必等我们发版。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_context: Option<String>,
}

impl GadgetConfig {
    /// 从 `config/gadget.json` 读取；不存在或损坏时返回默认值。
    ///
    /// 损坏按「没有配置」处理：最坏结果是身份不生效（Android 的原值继续用），
    /// 而不是拒绝服务。
    pub fn load(dirs: &DataDirs) -> Self {
        std::fs::read_to_string(gadget_config(dirs))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 原子写回。
    pub fn store(&self, dirs: &DataDirs) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        gadgetdisk_usb::jsonfile::write_json_atomic(&gadget_config(dirs), text.as_bytes())
    }

    /// 只更新身份部分，**保留** `image_context`，然后原子写回。
    ///
    /// ## 为什么必须走这个函数，而不是自己拼 `GadgetConfig`
    ///
    /// 回归（AVD 实测）：保存身份曾实现为「把 `Identity` 序列化后整文件覆盖」，
    /// 于是用户设好的 `image_context` **被静默清空**、回落到默认值——
    /// 表现是「保存一次产品名之后，PC 又读不出镜像内容了」，而用户根本没碰过
    /// 安全上下文设置。
    ///
    /// 两者在磁盘上是同一个文件，因此任何「只改身份」的调用方都必须经过这里
    /// 的**读—改—写**，不能整文件覆盖。把这件事收在一处，是为了让下一个
    /// 新增的调用方没有机会再犯同样的错。
    pub fn store_identity(
        dirs: &DataDirs,
        identity: &gadgetdisk_usb::Identity,
    ) -> std::io::Result<()> {
        let mut config = Self::load(dirs);
        config.identity = identity.clone();
        config.store(dirs)
    }

    /// 只更新**镜像 SELinux 目标上下文**，保留身份部分，然后原子写回。
    ///
    /// 与 [`Self::store_identity`] 形成对称保护：USB 设备身份与安全上下文在底层持久化于
    /// 同一配置文件，任何单项修改的调用方均须遵循读—改—写机制。直接整文件覆盖
    /// 将导致另一项配置被静默清除——正如 `store_identity` 中记录的 AVD 实测缺陷
    /// “保存身份时意外清空安全上下文”，反向整文件覆盖同样会清除用户已配置的 VID 与产品名称。
    ///
    /// `None` 表示**清除**设置，回落到 [`crate::selinux::DEFAULT_IMAGE_CONTEXT`]。
    /// 落盘时该字段被跳过（`skip_serializing_if`），因此“清除”不会在文件里留下 `null` 键值。
    ///
    /// 值的格式规范由 [`crate::selinux::validate_context_format`] 在调用方校验；
    /// 本函数仅执行原子读—改—写，不重复校验。
    pub fn store_image_context(dirs: &DataDirs, value: Option<&str>) -> std::io::Result<()> {
        let mut config = Self::load(dirs);
        config.image_context = value.map(str::to_string);
        config.store(dirs)
    }

    /// 目标镜像上下文（解析缺省值）。
    pub fn resolved_image_context(&self) -> String {
        crate::selinux::resolve_target(self.image_context.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dirs(tag: &str) -> (DataDirs, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "gd-clipaths-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&root).ok();
        let dirs = DataDirs::new(&root);
        dirs.create_all().unwrap();
        (dirs, root)
    }

    #[test]
    fn paths_live_where_documented() {
        let dirs = DataDirs::new("/tmp/gd");
        assert_eq!(state_json(&dirs), PathBuf::from("/tmp/gd/run/state.json"));
        assert_eq!(
            identity_backup(&dirs),
            PathBuf::from("/tmp/gd/run/gadget-backup.json")
        );
        assert_eq!(
            offsets_json(&dirs),
            PathBuf::from("/tmp/gd/run/offsets.json")
        );
        assert_eq!(
            loop_registry(&dirs),
            PathBuf::from("/tmp/gd/run/loop-attachments.json")
        );
        assert_eq!(
            gadget_config(&dirs),
            PathBuf::from("/tmp/gd/config/gadget.json")
        );
        assert_eq!(
            service_log(&dirs),
            PathBuf::from("/tmp/gd/logs/service.log")
        );
        assert_eq!(cli_log(&dirs), PathBuf::from("/tmp/gd/logs/cli.log"));
        assert_eq!(gdd_log(&dirs), PathBuf::from("/tmp/gd/logs/gdd.log"));
        assert_eq!(serve_log(&dirs), PathBuf::from("/tmp/gd/logs/serve.log"));
    }

    /// 旧版本写的配置文件（只有身份字段、没有 `image_context`）必须仍能读。
    ///
    /// 这是 `#[serde(flatten)]` + `default` 的意义所在：用户升级模块后，
    /// 他原来的 `config/gadget.json` 不该因为多了个字段就读不出来。
    #[test]
    fn gadget_config_reads_files_written_by_older_versions() {
        let (dirs, root) = temp_dirs("cfg-compat");
        // 旧格式：扁平的、只有身份字段。
        std::fs::write(
            gadget_config(&dirs),
            br#"{"id_vendor":6353,"id_product":20199,"product":"GD Storage"}"#,
        )
        .unwrap();

        let config = GadgetConfig::load(&dirs);
        assert_eq!(config.identity.id_vendor, Some(6353));
        assert_eq!(config.identity.id_product, Some(20199));
        assert_eq!(config.identity.product.as_deref(), Some("GD Storage"));
        assert_eq!(config.image_context, None, "新字段缺省应为 None");
        assert_eq!(
            config.resolved_image_context(),
            crate::selinux::DEFAULT_IMAGE_CONTEXT
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gadget_config_round_trips_both_sections_flattened() {
        let (dirs, root) = temp_dirs("cfg-round");

        let config = GadgetConfig {
            identity: gadgetdisk_usb::Identity {
                id_vendor: Some(0x18d1),
                product: Some("GD".into()),
                ..gadgetdisk_usb::Identity::default()
            },
            image_context: Some("u:object_r:vendor_file:s0".into()),
        };
        config.store(&dirs).unwrap();

        // 落盘后必须是**扁平**的（身份字段在顶层）。
        let text = std::fs::read_to_string(gadget_config(&dirs)).unwrap();
        assert!(text.contains(r#""id_vendor""#), "得到 {text}");
        assert!(
            !text.contains(r#""identity""#),
            "不该有嵌套的 identity：{text}"
        );

        let back = GadgetConfig::load(&dirs);
        assert_eq!(back, config);
        assert_eq!(back.resolved_image_context(), "u:object_r:vendor_file:s0");

        std::fs::remove_dir_all(&root).ok();
    }

    /// **保存身份不得清空 `image_context`**。
    ///
    /// 回归（AVD 实测）：曾把 `Identity` 直接序列化后整文件覆盖，于是用户设好的
    /// 镜像上下文被静默清空、回落到默认值——表现是「只改了个产品名，PC 又读不出
    /// 镜像内容了」。两者在磁盘上是同一个文件，所以「只改身份」必须走读—改—写。
    #[test]
    fn store_identity_preserves_image_context() {
        let (dirs, root) = temp_dirs("cfg-store-identity");

        // 先设好一个非默认的上下文。
        let config = GadgetConfig {
            identity: gadgetdisk_usb::Identity {
                id_vendor: Some(0x18d1),
                ..gadgetdisk_usb::Identity::default()
            },
            image_context: Some("u:object_r:media_rw_data_file:s0".into()),
        };
        config.store(&dirs).unwrap();

        // 再只改身份（这正是 WebUI「保存身份」与 `config set` 走的路）。
        let new_identity = gadgetdisk_usb::Identity {
            product: Some("磁盘存储".into()),
            ..gadgetdisk_usb::Identity::default()
        };
        GadgetConfig::store_identity(&dirs, &new_identity).unwrap();

        let back = GadgetConfig::load(&dirs);
        assert_eq!(
            back.image_context.as_deref(),
            Some("u:object_r:media_rw_data_file:s0"),
            "保存身份不得清空 image_context"
        );
        assert_eq!(
            back.resolved_image_context(),
            "u:object_r:media_rw_data_file:s0"
        );
        // 身份本身要真的换了（否则这条测试可能因为「什么都没写」而假通过）。
        assert_eq!(back.identity.product.as_deref(), Some("磁盘存储"));
        // 只改身份不该顺手改掉没提到的字段。
        assert_eq!(
            back.identity.id_vendor, None,
            "身份是整体替换，按调用方给的值"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gadget_config_missing_or_corrupt_is_default() {
        let (dirs, root) = temp_dirs("cfg-missing");
        // 不存在。
        assert_eq!(GadgetConfig::load(&dirs), GadgetConfig::default());
        // 损坏。
        std::fs::write(gadget_config(&dirs), b"{not json").unwrap();
        assert_eq!(GadgetConfig::load(&dirs), GadgetConfig::default());
        std::fs::remove_dir_all(&root).ok();
    }

    /// **保存镜像上下文不得清空身份** —— `store_identity_preserves_image_context`
    /// 的反方向。
    ///
    /// 两者同在 `config/gadget.json`：WebUI 的「保存镜像标签」若整文件覆盖，
    /// 会把用户设好的 VID/产品名一起抹掉，而用户根本没碰身份那张卡片。
    #[test]
    fn store_image_context_preserves_identity() {
        let (dirs, root) = temp_dirs("cfg-store-context");

        let config = GadgetConfig {
            identity: gadgetdisk_usb::Identity {
                id_vendor: Some(0x18d1),
                id_product: Some(0x4ee7),
                product: Some("GD Storage".into()),
                ..gadgetdisk_usb::Identity::default()
            },
            image_context: None,
        };
        config.store(&dirs).unwrap();

        GadgetConfig::store_image_context(&dirs, Some("u:object_r:vendor_file:s0")).unwrap();

        let back = GadgetConfig::load(&dirs);
        assert_eq!(
            back.image_context.as_deref(),
            Some("u:object_r:vendor_file:s0")
        );
        assert_eq!(back.resolved_image_context(), "u:object_r:vendor_file:s0");
        // 身份必须原样保留（否则这条测试会因「文件里什么都没了」而假通过）。
        assert_eq!(back.identity.id_vendor, Some(0x18d1));
        assert_eq!(back.identity.id_product, Some(0x4ee7));
        assert_eq!(back.identity.product.as_deref(), Some("GD Storage"));

        // 清除（`None`）回落到默认，且**不写 null 字段**。
        GadgetConfig::store_image_context(&dirs, None).unwrap();
        let cleared = GadgetConfig::load(&dirs);
        assert_eq!(cleared.image_context, None);
        assert_eq!(
            cleared.resolved_image_context(),
            crate::selinux::DEFAULT_IMAGE_CONTEXT
        );
        assert_eq!(
            cleared.identity.id_vendor,
            Some(0x18d1),
            "清除标签同样不得动身份"
        );
        let text = std::fs::read_to_string(gadget_config(&dirs)).unwrap();
        assert!(
            !text.contains("image_context"),
            "清除后不该留下一个 null 字段：{text}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn offsets_round_trip_and_default_to_zero() {
        let (dirs, root) = temp_dirs("offsets");

        // 没有记录 → 0（"没有缓存"，而不是"偏移是 0 字节"）。
        assert_eq!(Offsets::get(&dirs, "a.img"), 0);

        Offsets::set(&dirs, "a.img", 1_048_576).unwrap();
        assert_eq!(Offsets::get(&dirs, "a.img"), 1_048_576);
        assert_eq!(Offsets::get(&dirs, "b.img"), 0);

        // 多个镜像互不影响。
        Offsets::set(&dirs, "b.img", 2_097_152).unwrap();
        assert_eq!(Offsets::get(&dirs, "a.img"), 1_048_576);
        assert_eq!(Offsets::get(&dirs, "b.img"), 2_097_152);

        // 删除单项后其余保留。
        Offsets::remove(&dirs, "a.img").unwrap();
        assert_eq!(Offsets::get(&dirs, "a.img"), 0);
        assert_eq!(Offsets::get(&dirs, "b.img"), 2_097_152);

        // 损坏文件按"没有缓存"处理，不 panic。
        std::fs::write(offsets_json(&dirs), b"{not json").unwrap();
        assert_eq!(Offsets::get(&dirs, "b.img"), 0);

        // 删空后文件消失，不留空壳。
        Offsets::remove(&dirs, "b.img").unwrap();
        assert!(!offsets_json(&dirs).exists());

        std::fs::remove_dir_all(&root).ok();
    }
}
