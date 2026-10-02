//! gadget **身份**：`idVendor` / `idProduct` / 字符串描述符 / `os_desc`。
//!
//! ## 归属：这一半属于 CLI，不属于 `gdd`
//!
//! `gdd` 只做 mass_storage 挂载（[`crate::mass_storage`]）。身份与它无关，
//! 因此本模块**不被 `gdd` 使用**，而由 CLI 直接写 configfs。
//!
//! 这样切分的好处是：身份改动与 LUN 改动各自独立，且 `gdd` 的权限面更小——
//! 它连 `idVendor` 都不认识。
//!
//! ## 为什么身份写入不需要 UDC 空闲
//!
//! 依据（源码，`drivers/usb/gadget/configfs.c`）：
//!
//! - `idVendor`/`idProduct` 由 `GI_DEVICE_DESC_SIMPLE_RW` 宏生成，
//!   只写 `cdev.desc`，**没有**任何绑定检查；
//! - `strings/<lang>/<name>` 走 `usb_string_copy`，同样无绑定检查；
//! - `os_desc/use` 的 `os_desc_use_store` 只写 `gi->use_os_desc`。
//!
//! 但这些写入**只在下次 bind 时生效**。因此 CLI 在 UDC 已绑定时改身份后，
//! 需要请 `gdd` 走一次 `RebindRequest` 让改动落地——不能依赖隐式时序。
//!
//! ## 为什么必须读回核实
//!
//! configfs 的 `write` 返回成功不等于值被接受（例如超长字符串会被
//! `usb_string_copy` 以 `-EOVERFLOW` 拒绝，而某些内核路径会吞掉错误）。
//! 用户明确设置了身份却静默沿用 Android 的，属于谎报成功，因此
//! [`Identity::apply`] **逐项读回**并在不一致时报错。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::configfs::ConfigFs;
use crate::error::{GadgetError, GadgetResult};
use crate::paths::{self, BACKUP_ATTRS, STRING_ATTRS};

/// `os_desc/use` 的相对路径。
///
/// 该属性控制是否向主机播报**微软 OS 描述符**（`qw_sign = MSFT100`）。
/// Android 为 MTP 把它置 `1`；但我们导出的是 mass_storage，保留 `1` 会让
/// Windows 去取 MS OS 描述符并按兼容 ID 匹配驱动——这是「代码 10」的
/// 候选成因之一（见 [真机 UDC 抢占 Note](../../../.agents/notes/implemented/architecture/2026-10-04-real-device-udc-contention.md)）。
pub const OS_DESC_USE_ATTR: &str = "os_desc/use";

/// 用户可设置的 USB 设备身份。
///
/// 全部字段可选：**未设置的字段不会被写入**，从而保留 Android 的原始值。
/// 这是有意的——用户只想改产品名时，不该顺带改掉 VID。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// `idVendor`（`0x0000..=0xffff`）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id_vendor: Option<u16>,
    /// `idProduct`（`0x0000..=0xffff`）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id_product: Option<u16>,
    /// `strings/<lang>/manufacturer`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub manufacturer: Option<String>,
    /// `strings/<lang>/product`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub product: Option<String>,
    /// `strings/<lang>/serialnumber`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub serial: Option<String>,
}

impl Identity {
    /// 是否什么都没设置（此时不该写任何 configfs 条目）。
    pub fn is_empty(&self) -> bool {
        self.id_vendor.is_none()
            && self.id_product.is_none()
            && self.manufacturer.is_none()
            && self.product.is_none()
            && self.serial.is_none()
    }

    /// 校验取值范围与字符串约束，返回第一条错误。
    ///
    /// ## 依据（源码 + 实测）
    ///
    /// - `idVendor`/`idProduct` 是 `u16`（`kstrtou16`），故天然限定 `0..=65535`；
    /// - `usb_string_copy` 在 `strlen > USB_MAX_STRING_LEN` 时返回 `-EOVERFLOW`
    ///   （[`paths::STRING_MAX_BYTES`]），且 `strlen < 1` 返回 `-EINVAL`。
    ///   **`strlen` 数的是字节**：AVD 实测 42 个汉字（126 字节）通过、43 个
    ///   （129 字节）被拒，因此上限按 UTF-8 字节算，不能按字符数。
    ///
    /// ## 为什么三个字段的规则不一样
    ///
    /// `manufacturer`/`product`/`serialnumber` 在 configfs 里是**同一套**属性，
    /// 但主机对它们的用法不同：
    ///
    /// - `manufacturer`/`product` 只是给人看的展示文本，内核经
    ///   `utf8s_to_utf16s`（`fs/nls/nls_base.c`）正确转成 UTF-16LE 并处理代理对，
    ///   因此**允许中文**。M10 曾对全部字段拒绝非 ASCII，那让中文产品名根本设不进去。
    /// - `serial` 被主机用于**设备识别与驱动匹配**。真机观察：写入非 ASCII 序列号后
    ///   PC **连不上**设备。成因尚未查明（见 roadmap 的待验证假设），但现象明确，
    ///   因此序列号**保留可打印 ASCII 限制**——这是唯一的字段级例外。
    ///
    /// ## 为什么连控制字符也要拒
    ///
    /// `usb_string_copy` 会剥掉**尾部**的一个 `\n`，而内嵌 `\0` 会让 C 串在那里
    /// 截断。两者都会造成「写入值 ≠ 读回值」，从而触发 [`Identity::apply`] 的读回
    /// 核实报错。与其让用户看到「身份无法应用」，不如在输入处就明确拒绝。
    pub fn validate(&self) -> GadgetResult<()> {
        for (name, value) in [
            ("manufacturer", &self.manufacturer),
            ("product", &self.product),
            ("serial", &self.serial),
        ] {
            let Some(text) = value else { continue };
            if text.is_empty() {
                return Err(GadgetError::InvalidArgument(format!(
                    "{name} must not be an empty string (the kernel's usb_string_copy returns EINVAL)"
                )));
            }
            // `String::len()` 是 UTF-8 **字节**数，正是内核 `strlen` 的判据。
            if text.len() > paths::STRING_MAX_BYTES {
                return Err(GadgetError::InvalidArgument(format!(
                    "{name} length is {} bytes, exceeding the kernel limit of {} bytes (a CJK character usually takes 3 bytes)",
                    text.len(),
                    paths::STRING_MAX_BYTES
                )));
            }
            if let Some(bad) = text.chars().find(|c| c.is_control()) {
                return Err(GadgetError::InvalidArgument(format!(
                    "{name} contains the control character {bad:?}; the kernel strips trailing \
                     newlines and truncates at NUL, so the written and read-back values disagree"
                )));
            }
            // 唯一的字段级例外：序列号必须是可打印 ASCII（非 ASCII 会导致宿主机枚举失败）。
            // 制造商与产品名不受此限，可以写中文。
            if name == "serial"
                && let Some(bad) = text.chars().find(|c| !c.is_ascii_graphic() && *c != ' ')
            {
                return Err(GadgetError::InvalidArgument(format!(
                    "{name} contains the non-ASCII character {bad:?}; the serial identifies the \
                         device to the host and supports printable ASCII only (manufacturer and \
                         product may use CJK)"
                )));
            }
        }
        Ok(())
    }

    /// 从 configfs 读当前身份（供设置页预填 / 备份用）。
    ///
    /// 读不到的项留 `None`（如实反映「内核里没有这个概念」，而不是编造）。
    pub fn read_current(fs: &impl ConfigFs) -> Self {
        let read_attr =
            |name: &str| -> Option<String> { fs.read(name).ok().map(|v| v.trim().to_string()) };
        let read_num = |name: &str| -> Option<u16> {
            let text = read_attr(name)?;
            parse_u16(&text)
        };

        Self {
            id_vendor: read_num("idVendor"),
            id_product: read_num("idProduct"),
            manufacturer: read_string(fs, "manufacturer"),
            product: read_string(fs, "product"),
            serial: read_string(fs, "serialnumber"),
        }
    }

    /// 把身份写入 configfs，并**逐项读回核实**。
    ///
    /// 只写 [`Identity`] 里 `Some` 的字段——未设置的一律不碰。
    ///
    /// 返回实际写入的条目名列表（供日志/诊断）。任何一项「写了但读回不一致」
    /// 都返回 [`GadgetError::Config`]，**不**静默继续。
    pub fn apply(&self, fs: &mut impl ConfigFs) -> GadgetResult<Vec<String>> {
        // 空身份 = 用户没有配置（`config/gadget.json` 不存在）→ **完全不碰** configfs，
        // 让 Android 的原始身份继续生效。这不是优化，而是语义：我们没有意见。
        if self.is_empty() {
            return Ok(Vec::new());
        }

        self.validate()?;

        let mut written = Vec::new();
        let lang = current_lang(fs).unwrap_or_else(|| paths::STRING_LANG.to_string());

        if let Some(vid) = self.id_vendor {
            fs.write("idVendor", &format!("0x{vid:04x}"))?;
            written.push("idVendor".to_string());
        }
        if let Some(pid) = self.id_product {
            fs.write("idProduct", &format!("0x{pid:04x}"))?;
            written.push("idProduct".to_string());
        }

        // 字符串描述符：语言目录可能不存在（Android 重启后 configfs 由 init
        // 重建，未必已建 strings/0x409），按需补建。
        let has_strings =
            self.manufacturer.is_some() || self.product.is_some() || self.serial.is_some();
        if has_strings {
            // 语言目录可能不存在（configfs 是内存文件系统，重启后由 init 按当前
            // USB 配置重建，未必包含我们的语言目录）。按需补建；父目录 `strings`
            // 是 gadget 的默认组，正常情况下一定在，但缺失时一并补上以免
            // `mkdir` 因子目录不存在而失败。
            if !fs.exists("strings") {
                fs.mkdir("strings")?;
            }
            let lang_dir = format!("strings/{lang}");
            if !fs.exists(&lang_dir) {
                fs.mkdir(&lang_dir)?;
            }
            for (name, value) in [
                ("manufacturer", &self.manufacturer),
                ("product", &self.product),
                ("serialnumber", &self.serial),
            ] {
                let Some(text) = value else { continue };
                let relative = format!("{lang_dir}/{name}");
                fs.write(&relative, text)?;
                written.push(relative);
            }
        }

        // 关掉微软 OS 描述符：我们导出的是 mass_storage，留着 `use=1` 会让
        // Windows 按 MTP 的兼容 ID 匹配驱动。原值由调用方在改身份前备份。
        if fs.exists(OS_DESC_USE_ATTR) {
            fs.write(OS_DESC_USE_ATTR, "0")?;
            written.push(OS_DESC_USE_ATTR.to_string());
        }

        // 读回核实。**必须做**：configfs 接受写入不等于值生效。
        let observed = Self::read_current(fs);
        let mut mismatches = Vec::new();
        if let Some(expected) = self.id_vendor
            && observed.id_vendor != Some(expected)
        {
            mismatches.push(format!(
                "idVendor expected {expected:#06x}, read back {:?}",
                observed.id_vendor
            ));
        }
        if let Some(expected) = self.id_product
            && observed.id_product != Some(expected)
        {
            mismatches.push(format!(
                "idProduct expected {expected:#06x}, read back {:?}",
                observed.id_product
            ));
        }
        for (name, expected, actual) in [
            ("manufacturer", &self.manufacturer, &observed.manufacturer),
            ("product", &self.product, &observed.product),
            ("serialnumber", &self.serial, &observed.serial),
        ] {
            if let Some(expected) = expected
                && actual.as_deref() != Some(expected.as_str())
            {
                mismatches.push(format!(
                    "{name} expected {expected:?}, read back {actual:?}"
                ));
            }
        }

        if !mismatches.is_empty() {
            return Err(GadgetError::Config(mismatches.join("; ")));
        }
        Ok(written)
    }
}

/// 读取一个字符串描述符（自动探测 `strings/` 下的语言目录）。
fn read_string(fs: &impl ConfigFs, name: &str) -> Option<String> {
    let lang = current_lang(fs)?;
    let text = fs.read(&format!("strings/{lang}/{name}")).ok()?;
    let text = text.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// 当前 `strings/` 下的语言目录名（形如 `0x409`）。
///
/// 取第一个 `0x` 开头的目录；找不到返回 `None`，由调用方退化到
/// [`paths::STRING_LANG`]。
pub fn current_lang(fs: &impl ConfigFs) -> Option<String> {
    fs.read_dir("strings")
        .ok()?
        .into_iter()
        .find(|entry| entry.is_dir && entry.name.starts_with("0x"))
        .map(|entry| entry.name)
}

/// 解析 `0x1234` / `1234` / `4660` 形式的 `u16`。
fn parse_u16(text: &str) -> Option<u16> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).ok();
    }
    text.parse::<u16>().ok()
}

/// Android 原始身份的备份（JSON，落盘在 `run/gadget-backup.json`）。
///
/// ## 为什么备份是必要的
///
/// 用户设置的 `idVendor`/字符串会**覆盖** Android 为 MTP 准备的身份。卸载模块
/// 或拆除导出时必须还回去，否则手机的 USB 功能会带着我们的身份继续跑。
///
/// ## 为什么是 JSON 而不是「每属性一个文件」
///
/// 备份只由 Rust 读写，不经过任何 shell。因此单一 JSON 更简单，也更不容易出现
/// 「部分写入」的中间态（配合原子改名写入）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityBackup {
    /// schema 版本，便于将来迁移。
    #[serde(default = "default_version")]
    pub version: u32,
    /// gadget 顶层属性（属性名 → 原值）。
    #[serde(default)]
    pub attrs: BTreeMap<String, String>,
    /// 字符串描述符（描述符名 → 原值）。
    #[serde(default)]
    pub strings: BTreeMap<String, String>,
    /// 配置符号链接映射（链接名 → 相对 gadget 根的 function 路径）。
    #[serde(default)]
    pub configs: BTreeMap<String, String>,
    /// 备份时的 UDC 名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udc: Option<String>,
    /// `strings/` 下的语言目录名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strings_lang: Option<String>,
    /// `os_desc/use` 的原始值（`Some` 表示备份时该属性可读）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_desc_use: Option<String>,
}

fn default_version() -> u32 {
    1
}

/// 备份文件名（位于 `run/`）。
pub const BACKUP_FILE_NAME: &str = "gadget-backup.json";

impl IdentityBackup {
    /// 从 configfs 读取当前状态形成备份（尽力而为：读不到的项跳过）。
    ///
    /// `config_dir` 是**探测到的**配置目录（如 `configs/b.1`），不是兜底常量：
    /// 备份错目录会让还原指向不存在的链接。
    pub fn capture(fs: &impl ConfigFs, config_dir: &str) -> Self {
        let mut backup = Self {
            version: default_version(),
            ..Self::default()
        };

        for attr in BACKUP_ATTRS {
            if let Ok(value) = fs.read(attr) {
                backup
                    .attrs
                    .insert((*attr).to_string(), value.trim().to_string());
            }
        }

        for name in STRING_ATTRS {
            let relative = paths::string_attr(name);
            if let Ok(value) = fs.read(&relative) {
                backup
                    .strings
                    .insert((*name).to_string(), value.trim().to_string());
            }
        }

        backup.strings_lang = current_lang(fs);

        // 记录配置符号链接（链接名 → 目标）。
        //
        // **必须排除我们自己的链接**：备份记录的是「Android 的原始状态」，而
        // 我们的链接是导出期间才出现的。若把它记进备份，还原时就会尝试重建一个
        // 指向我们已经删掉的 function 的链接（实测报 ENOENT），并把「我们造成
        // 的状态」当成「原始状态」永久固化下来。
        if let Ok(entries) = fs.read_dir(config_dir) {
            for entry in entries {
                if !entry.is_symlink {
                    continue;
                }
                if entry.name == paths::LINK_NAME {
                    continue;
                }
                let link_relative = format!("{config_dir}/{}", entry.name);
                // 真实 configfs 里链接目标形如
                // `../../../../usb_gadget/g1/functions/mass_storage.gadget-disk`。
                // 必须用 readlink 取真实目标：曾错误地退化为「用链接名当目标」，
                // 导致还原时 `symlink("msd", ...)` 失败（实测 No such file）。
                let target = fs
                    .read_link(&link_relative)
                    .map(|target| normalize_symlink_target(&target))
                    .unwrap_or_else(|_| format!("functions/{}", entry.name));
                // 指向我们 function 的链接不属于 Android 的原始状态：
                // 它是本模块自己建立的，还原时由本模块重建。
                if target.contains(paths::FUNCTION_NAME) {
                    continue;
                }
                backup.configs.insert(entry.name, target);
            }
        }

        if let Ok(value) = fs.read(OS_DESC_USE_ATTR) {
            backup.os_desc_use = Some(value.trim().to_string());
        }
        if let Ok(udc) = fs.read(paths::UDC_ATTR) {
            let udc = udc.trim();
            if !udc.is_empty() {
                backup.udc = Some(udc.to_string());
            }
        }

        backup
    }

    /// 是否什么都没记录（此时不该写文件）。
    pub fn is_empty(&self) -> bool {
        self.attrs.is_empty() && self.strings.is_empty() && self.configs.is_empty()
    }

    /// 原子写入 `dir/<BACKUP_FILE_NAME>`。
    pub fn store(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self)?;
        crate::jsonfile::write_json_atomic(&dir.join(BACKUP_FILE_NAME), text.as_bytes())
    }

    /// 从 `dir` 读取备份；不存在或损坏时返回 `None`。
    ///
    /// 损坏按「没有备份」处理：最坏结果是身份不被还原（Android 下次 USB 重配
    /// 时会自己写回），而不是拒绝服务。
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(BACKUP_FILE_NAME)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 把备份还原到 configfs。
    ///
    /// 还原采用**安全次序**：
    ///
    /// 1. 写空串到 `UDC` 断开；
    /// 2. 删除配置目录下**全部**符号链接（恢复原始状态时，我们不能只删自己的
    ///    ——备份记录的就是当时的全集）；
    /// 3. 写回 `os_desc/use`（必须在重新绑定 UDC **之前**）；
    /// 4. 写回 gadget 属性；
    /// 5. 写回字符串描述符（必要时补建语言目录）；
    /// 6. 重建配置符号链接；
    /// 7. 重新写入 `UDC`。
    ///
    /// 全部写入**尽力而为**：不因单个失败中断（部分还原优于完全不还原）。
    /// 返回实际发生的失败列表。
    pub fn restore(&self, fs: &mut impl ConfigFs, config_dir: &str) -> Vec<String> {
        let mut failures = Vec::new();

        if let Err(err) = fs.write(paths::UDC_ATTR, "") {
            failures.push(format!("failed to disconnect the UDC: {err}"));
        }

        if let Ok(entries) = fs.read_dir(config_dir) {
            for entry in entries {
                if entry.is_symlink
                    && let Err(err) = fs.unlink(&format!("{config_dir}/{}", entry.name))
                {
                    failures.push(format!(
                        "failed to delete the symlink {}: {err}",
                        entry.name
                    ));
                }
            }
        }

        if let Some(value) = &self.os_desc_use
            && fs.exists(OS_DESC_USE_ATTR)
            && let Err(err) = fs.write(OS_DESC_USE_ATTR, value)
        {
            failures.push(format!("failed to restore {OS_DESC_USE_ATTR}: {err}"));
        }

        for attr in BACKUP_ATTRS {
            if let Some(value) = self.attrs.get(*attr)
                && let Err(err) = fs.write(attr, value)
            {
                failures.push(format!("failed to restore {attr}: {err}"));
            }
        }

        if let Some(lang) = &self.strings_lang {
            let lang_dir = format!("strings/{lang}");
            if !fs.exists(&lang_dir)
                && let Err(err) = fs.mkdir(&lang_dir)
            {
                failures.push(format!("failed to recreate {lang_dir}: {err}"));
            }
            for name in STRING_ATTRS {
                if let Some(value) = self.strings.get(*name) {
                    let relative = format!("{lang_dir}/{name}");
                    if let Err(err) = fs.write(&relative, value) {
                        failures.push(format!("failed to restore {relative}: {err}"));
                    }
                }
            }
        }

        if !self.configs.is_empty()
            && !fs.exists(config_dir)
            && let Err(err) = fs.mkdir(config_dir)
        {
            failures.push(format!("failed to recreate {config_dir}: {err}"));
        }
        for (link, func) in &self.configs {
            let link_relative = format!("{config_dir}/{link}");
            if fs.exists(&link_relative) {
                continue;
            }

            // `func` 是**相对 gadget 根**的目标，原样写回。不要无条件再加
            // `functions/` 前缀：那会产生 `functions/functions/...`，内核报
            // ENOENT（实测还原失败的根因）。
            let target = func.clone();

            if let Err(err) = fs.symlink(&target, &link_relative) {
                failures.push(format!("failed to recreate the symlink {link}: {err}"));
            }
        }

        // 7. 重新写入 UDC。
        //
        // **EBUSY 在此处是成功**：它的含义是「gadget 已经绑定了」，而那正是我们
        // 想要的状态（AVD 实测：删链接/删 LUN 会让内核隐式解绑，随后 Android 的
        // `init` 又按 `sys.usb.config` 绑回；此时我们再写一次就会 EBUSY）。
        // 把它当成失败会让还原报告一个并不存在的问题。
        if let Some(udc) = &self.udc
            && let Err(err) = fs.write(paths::UDC_ATTR, udc)
            && !is_busy(&err)
        {
            failures.push(format!("failed to rebind the UDC {udc}: {err}"));
        }

        failures
    }
}

/// 该错误是否为 `EBUSY`（内核表示「已经是这个状态」，通常可当成功）。
fn is_busy(err: &crate::configfs::FsError) -> bool {
    use crate::configfs::FsError;
    match err {
        FsError::Io { source, .. } => source.raw_os_error() == Some(libc::EBUSY),
        _ => false,
    }
}

/// 把 configfs 读到的符号链接目标归一化为**相对 gadget 根**的路径。
fn normalize_symlink_target(target: &str) -> String {
    if let Some(index) = target.find("functions/") {
        return target[index..].to_string();
    }
    target.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configfs::MemConfigFs;

    fn fs_with_android_identity() -> MemConfigFs {
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        for attr in BACKUP_ATTRS {
            fs.add_file(attr);
            fs.write(attr, "0x1234").unwrap();
        }
        for name in STRING_ATTRS {
            fs.add_file(&paths::string_attr(name));
            fs.write(&paths::string_attr(name), "android").unwrap();
        }
        fs.add_file(OS_DESC_USE_ATTR);
        fs.write(OS_DESC_USE_ATTR, "1").unwrap();
        fs
    }

    #[test]
    fn identity_writes_only_the_fields_that_were_set() {
        // 未设置的字段一律不碰：用户只想改产品名时不该顺带改掉 VID。
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            product: Some("GadgetDisk".into()),
            ..Identity::default()
        };

        let written = identity.apply(&mut fs).unwrap();

        // 只写了产品名与 `os_desc/use`（关闭 MS OS 描述符是身份应用的一部分）。
        assert_eq!(
            written,
            vec![
                "strings/0x409/product".to_string(),
                OS_DESC_USE_ATTR.to_string()
            ]
        );
        // idVendor 保持 Android 原值。
        assert_eq!(fs.read("idVendor").unwrap(), "0x1234");
        assert_eq!(
            fs.read(&paths::string_attr("product")).unwrap(),
            "GadgetDisk"
        );
    }

    #[test]
    fn empty_identity_writes_nothing() {
        let mut fs = fs_with_android_identity();
        let written = Identity::default().apply(&mut fs).unwrap();
        assert!(written.is_empty(), "空身份不得写任何条目");
        assert!(Identity::default().is_empty());
        // 未动的条目保持原值。
        assert_eq!(fs.read(OS_DESC_USE_ATTR).unwrap(), "1");
        assert_eq!(fs.read("idVendor").unwrap(), "0x1234");
    }

    #[test]
    fn identity_writes_vid_and_pid_as_hex() {
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            id_vendor: Some(0x18d1),
            id_product: Some(0x4ee7),
            ..Identity::default()
        };
        identity.apply(&mut fs).unwrap();
        assert_eq!(fs.read("idVendor").unwrap(), "0x18d1");
        assert_eq!(fs.read("idProduct").unwrap(), "0x4ee7");
    }

    #[test]
    fn identity_disables_os_desc_use() {
        // 保留 use=1 会让 Windows 按 MTP 的兼容 ID 匹配驱动。
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            product: Some("X".into()),
            ..Identity::default()
        };
        identity.apply(&mut fs).unwrap();
        assert_eq!(fs.read(OS_DESC_USE_ATTR).unwrap(), "0");
    }

    #[test]
    fn identity_creates_missing_language_directory() {
        // configfs 是内存文件系统，重启后 strings/0x409 可能不存在。
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        // 只移除语言目录（真实场景：重启后 init 重建了 gadget，但还没有任何
        // 语言目录）。`strings` 是 gadget 的默认组，永远存在。
        fs.remove_dir_force("strings/0x409");
        let identity = Identity {
            product: Some("P".into()),
            ..Identity::default()
        };
        identity.apply(&mut fs).unwrap();
        assert_eq!(fs.read(&paths::string_attr("product")).unwrap(), "P");
    }

    #[test]
    fn identity_rejects_strings_longer_than_the_kernel_limit() {
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            product: Some("x".repeat(paths::STRING_MAX_BYTES + 1)),
            ..Identity::default()
        };
        let err = identity.apply(&mut fs).unwrap_err();
        match err {
            GadgetError::InvalidArgument(msg) => assert!(msg.contains("bytes"), "得到 {msg}"),
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }
    }

    #[test]
    fn identity_rejects_empty_string() {
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            manufacturer: Some(String::new()),
            ..Identity::default()
        };
        assert!(matches!(
            identity.apply(&mut fs).unwrap_err(),
            GadgetError::InvalidArgument(_)
        ));
    }

    /// 制造商与产品名**允许中文**：内核 `utf8s_to_utf16s` 会正确转成 UTF-16LE
    /// 并处理代理对，因此 M10 的「只接受可打印 ASCII」是过度限制——中文产品名
    /// 根本设不进去。这里钉住「写得进、读得回」。
    #[test]
    fn identity_accepts_utf8_in_manufacturer_and_product() {
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            manufacturer: Some("中文制造商".into()),
            product: Some("GadgetDisk 中文 存储".into()),
            ..Identity::default()
        };
        identity
            .apply(&mut fs)
            .expect("中文制造商/产品名必须被接受");

        assert_eq!(
            fs.read(&paths::string_attr("manufacturer")).unwrap(),
            "中文制造商"
        );
        assert_eq!(
            fs.read(&paths::string_attr("product")).unwrap(),
            "GadgetDisk 中文 存储"
        );
    }

    /// 序列号是**唯一的字段级例外**：真机实测非 ASCII 序列号会让电脑连不上设备，
    /// 因此保留可打印 ASCII 限制。这条测试防止有人「统一规则」把它一起去掉。
    #[test]
    fn identity_rejects_utf8_in_serial() {
        let mut fs = fs_with_android_identity();
        let identity = Identity {
            serial: Some("序列号".into()),
            ..Identity::default()
        };
        match identity.apply(&mut fs).unwrap_err() {
            GadgetError::InvalidArgument(msg) => {
                assert!(
                    msg.contains("ASCII"),
                    "错误消息应说明只接受 ASCII，得到 {msg}"
                );
                assert!(msg.contains("serial"), "应指明是哪个字段，得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }

        // 同一份身份只把序列号换成 ASCII 就必须通过——否则说明误伤了别的字段。
        let ok = Identity {
            serial: Some("ABC123".into()),
            ..Identity::default()
        };
        ok.apply(&mut fs).expect("ASCII 序列号必须被接受");
    }

    /// 长度上限是**字节**不是字符：42 个汉字正好 126 字节（通过），
    /// 43 个是 129 字节（拒绝）。AVD 实测与内核 `strlen` 一致。
    #[test]
    fn identity_length_limit_counts_utf8_bytes_not_characters() {
        let mut fs = fs_with_android_identity();
        let at_limit = "磁".repeat(42);
        assert_eq!(
            at_limit.len(),
            paths::STRING_MAX_BYTES,
            "前提：42 个汉字 = 126 字节"
        );
        Identity {
            product: Some(at_limit),
            ..Identity::default()
        }
        .apply(&mut fs)
        .expect("126 字节必须被接受");

        let over = "磁".repeat(43);
        assert_eq!(over.len(), paths::STRING_MAX_BYTES + 3);
        let err = Identity {
            product: Some(over),
            ..Identity::default()
        }
        .apply(&mut fs)
        .unwrap_err();
        match err {
            GadgetError::InvalidArgument(msg) => {
                assert!(msg.contains("129"), "应报出实际字节数 129，得到 {msg}");
                assert!(msg.contains("bytes"), "应说明单位是字节，得到 {msg}");
            }
            other => panic!("期望 InvalidArgument，得到 {other:?}"),
        }
    }

    /// 控制字符必须被拒：内核 `usb_string_copy` 剥掉**尾部**换行、在 NUL 处截断
    /// C 串，两者都会让「写入值 ≠ 读回值」而触发读回核实失败。早拒比晚报清楚。
    #[test]
    fn identity_rejects_control_characters() {
        for bad in ["a\n", "a\tb", "a\0b", "\r"] {
            let mut fs = fs_with_android_identity();
            let identity = Identity {
                product: Some(bad.to_string()),
                ..Identity::default()
            };
            match identity.apply(&mut fs).unwrap_err() {
                GadgetError::InvalidArgument(msg) => {
                    assert!(
                        msg.contains("control character"),
                        "{bad:?} 应报控制字符，得到 {msg}"
                    )
                }
                other => panic!("{bad:?} 期望 InvalidArgument，得到 {other:?}"),
            }
        }
    }

    #[test]
    fn identity_read_current_parses_hex_and_reads_strings() {
        let fs = fs_with_android_identity();
        let current = Identity::read_current(&fs);
        assert_eq!(current.id_vendor, Some(0x1234));
        assert_eq!(current.id_product, Some(0x1234));
        assert_eq!(current.manufacturer.as_deref(), Some("android"));
    }

    #[test]
    fn parse_u16_accepts_hex_and_decimal() {
        assert_eq!(parse_u16("0x1234"), Some(0x1234));
        assert_eq!(parse_u16("4660"), Some(4660));
        assert_eq!(parse_u16(" 0X00FF "), Some(0xff));
        assert_eq!(parse_u16("0x10000"), None);
        assert_eq!(parse_u16("nope"), None);
    }

    #[test]
    fn backup_capture_records_identity_and_links() {
        let mut fs = fs_with_android_identity();
        fs.mkdir("configs/b.1").unwrap();
        fs.symlink("functions/ffs.adb", "configs/b.1/f1").unwrap();

        let backup = IdentityBackup::capture(&fs, "configs/b.1");

        assert!(!backup.is_empty());
        assert_eq!(
            backup.attrs.get("idVendor").map(String::as_str),
            Some("0x1234")
        );
        assert_eq!(
            backup.strings.get("product").map(String::as_str),
            Some("android")
        );
        assert_eq!(
            backup.configs.get("f1").map(String::as_str),
            Some("functions/ffs.adb")
        );
        assert_eq!(backup.os_desc_use.as_deref(), Some("1"));
        assert_eq!(backup.strings_lang.as_deref(), Some("0x409"));
    }

    #[test]
    fn backup_round_trips_through_json_atomically() {
        let dir = std::env::temp_dir().join(format!("gd-ident-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let mut fs = fs_with_android_identity();
        fs.mkdir("configs/b.1").unwrap();
        let backup = IdentityBackup::capture(&fs, "configs/b.1");
        backup.store(&dir).unwrap();

        let loaded = IdentityBackup::load(&dir).expect("应能读回");
        assert_eq!(loaded, backup);

        // 损坏文件按「没有备份」处理，不 panic。
        std::fs::write(dir.join(BACKUP_FILE_NAME), b"{not json").unwrap();
        assert!(IdentityBackup::load(&dir).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 备份**不得**把我们自己的链接记成 Android 的原始状态。
    ///
    /// 回归（AVD 实测）：导出期间捕获备份时，我们的 `mass_storage.gadget-disk`
    /// 链接还在，于是被记进 `configs`。卸载还原时它会尝试重建一个指向**已被我们
    /// 删除的** function 的链接，报 ENOENT；同时「我们造成的状态」被永久固化成
    /// 「原始状态」。
    #[test]
    fn backup_ignores_our_own_link() {
        let mut fs = fs_with_android_identity();
        fs.mkdir("configs/b.1").unwrap();
        // Android 自己的链接。
        fs.symlink("functions/ffs.adb", "configs/b.1/f1").unwrap();
        // 我们导出期间的链接。
        fs.symlink(
            &format!("functions/{}", paths::FUNCTION_NAME),
            &format!("configs/b.1/{}", paths::LINK_NAME),
        )
        .unwrap();

        let backup = IdentityBackup::capture(&fs, "configs/b.1");

        assert_eq!(
            backup.configs.get("f1").map(String::as_str),
            Some("functions/ffs.adb"),
            "Android 的链接必须被记录"
        );
        assert!(
            !backup.configs.contains_key(paths::LINK_NAME),
            "我们自己的链接不得进备份：{:?}",
            backup.configs
        );
    }

    /// 捕获**必须**在 `apply` 之前，否则会把我们自己的值当成原始值。
    ///
    /// 这条测试锁定的是**调用顺序**这一约定（`capture` 无法自己知道当前值是
    /// Android 的还是我们的）。回归背景：曾有一版实现在「我们的 function 已存在」
    /// 时拒绝捕获，导致「先挂载、再改身份」这条最常见的路径完全没有备份，
    /// 身份再也还原不回去（AVD 实测）。
    #[test]
    fn capture_before_apply_keeps_android_values() {
        let mut fs = fs_with_android_identity();
        // 我们的 function 已经在 configfs 里（先挂载、后改身份的真实顺序）。
        fs.mkdir(&paths::function_path()).unwrap();
        fs.mkdir(&paths::lun_path(0)).unwrap();

        let backup = IdentityBackup::capture(&fs, "configs/b.1");
        let identity = Identity {
            id_vendor: Some(0x18d1),
            product: Some("GD Storage".into()),
            ..Identity::default()
        };
        identity.apply(&mut fs).unwrap();

        // 备份记录的仍是 Android 的值。
        assert_eq!(
            backup.attrs.get("idVendor").map(String::as_str),
            Some("0x1234"),
            "备份必须保留 Android 的 idVendor"
        );
        assert_eq!(
            backup.strings.get("product").map(String::as_str),
            Some("android")
        );

        // 还原后回到 Android 的值，而不是我们自己写进去的。
        backup.restore(&mut fs, "configs/b.1");
        assert_eq!(fs.read("idVendor").unwrap(), "0x1234");
        assert_eq!(fs.read(&paths::string_attr("product")).unwrap(), "android");
    }

    /// 指向我们 function 的**异名**链接也不算 Android 的原始状态。
    #[test]
    fn backup_ignores_links_pointing_at_our_function() {
        let mut fs = fs_with_android_identity();
        fs.mkdir("configs/b.1").unwrap();
        // 手工/早期版本留下的异名链接，但指向我们的 function。
        fs.symlink(
            &format!("functions/{}", paths::FUNCTION_NAME),
            "configs/b.1/msd",
        )
        .unwrap();

        let backup = IdentityBackup::capture(&fs, "configs/b.1");

        assert!(
            backup.configs.is_empty(),
            "指向我们 function 的链接不得进备份：{:?}",
            backup.configs
        );
    }

    /// 还原时写 `UDC` 得到 `EBUSY` 必须当作**成功**。
    ///
    /// 回归（AVD 实测）：删链接/删 LUN 会让内核隐式解绑 UDC，随后 Android 的
    /// `init` 又绑回；此时我们按备份再写一次就会拿到 EBUSY。把它当失败会让还原
    /// 报告一个并不存在的问题（实测输出「重新绑定 UDC 失败：Device or resource busy」）。
    #[test]
    fn restore_treats_busy_udc_as_success() {
        let mut fs = fs_with_android_identity();
        fs.mkdir("configs/b.1").unwrap();
        let backup = IdentityBackup {
            udc: Some("dummy_udc.0".into()),
            ..IdentityBackup::default()
        };
        // 已被绑定 → 再写会 EBUSY。
        fs.write(paths::UDC_ATTR, "dummy_udc.0").unwrap();

        let failures = backup.restore(&mut fs, "configs/b.1");

        assert!(
            failures.is_empty(),
            "EBUSY 表示「已经是这个状态」，不得报为失败：{failures:?}"
        );
    }

    #[test]
    fn backup_load_returns_none_when_absent() {
        let dir = std::env::temp_dir().join(format!("gd-ident-none-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        assert!(IdentityBackup::load(&dir).is_none());
    }

    #[test]
    fn backup_restore_rebuilds_identity_and_links() {
        let dir = std::env::temp_dir().join(format!("gd-ident-restore-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let mut fs = fs_with_android_identity();
        fs.mkdir("configs/b.1").unwrap();
        fs.symlink("functions/ffs.adb", "configs/b.1/f1").unwrap();
        let backup = IdentityBackup::capture(&fs, "configs/b.1");

        // 模拟我们的改动：身份被改成 GadgetDisk、链接被换掉。
        fs.write("idVendor", "0x18d1").unwrap();
        fs.unlink("configs/b.1/f1").unwrap();
        fs.write(OS_DESC_USE_ATTR, "0").unwrap();

        let failures = backup.restore(&mut fs, "configs/b.1");

        assert!(failures.is_empty(), "还原应无失败：{failures:?}");
        assert_eq!(fs.read("idVendor").unwrap(), "0x1234");
        assert_eq!(fs.read(OS_DESC_USE_ATTR).unwrap(), "1");
        assert!(fs.exists("configs/b.1/f1"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_does_not_double_prefix_function_target() {
        // 回归：曾无条件给 target 加 `functions/` 前缀，产生
        // `functions/functions/...` 而 ENOENT。
        let mut fs = MemConfigFs::new();
        fs.add_file(paths::UDC_ATTR);
        fs.mkdir("configs/b.1").unwrap();

        let target = paths::function_path();
        let mut backup = IdentityBackup::default();
        backup.configs.insert("msd".into(), target.clone());

        let failures = backup.restore(&mut fs, "configs/b.1");
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(fs.read_link("configs/b.1/msd").unwrap(), target);
    }

    #[test]
    fn normalize_symlink_target_strips_kernel_prefix() {
        assert_eq!(
            normalize_symlink_target(
                "../../../../usb_gadget/g1/functions/mass_storage.gadget-disk"
            ),
            "functions/mass_storage.gadget-disk"
        );
        assert_eq!(normalize_symlink_target("functions/x"), "functions/x");
    }
}
