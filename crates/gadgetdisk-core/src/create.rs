//! 镜像创建编排与创建后自检。
//!
//! 自检清单来自 [docs/disk-image-format.md](../../../docs/disk-image-format.md)：
//! 1. `raw`：偏移 510 处为 `55AA`；
//! 2. `gpt`：每个分区起点 1 MiB 对齐，且未越过可用上界；
//! 3. `mbr`：分区项类型字节正确，末两字节为 `55AA`；
//! 4. 分区区间内偏移 `+510` 处为 `55AA`（FAT32）；
//! 5. 文件系统类型与请求一致。
//!
//! 任一步失败都要删除半成品，避免它被当成完整镜像使用。
//!
//! ## 格式化由调用方注入
//!
//! 本模块**不直接调用外部 `mkfs`**：`gadgetdisk-core` 的边界是「不接触平台接口」，
//! 而 `mkfs` 是外部进程。执行器由 [`Formatter`] 抽象，CLI 层提供真实实现
//! （见 [`crate::fs`] 的模块文档）。

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::fat::{self, BootSector};
use crate::fs::{CreatedPartition, FilesystemType, FormatPlan, Formatter};
use crate::layout::{self, ALIGNMENT_BYTES, ALIGNMENT_SECTORS, ImageLayout, SECTOR_BYTES};
use crate::partition::{self, PartitionTable};
use crate::partspec::{self, PartitionSpec};
use crate::{CoreError, Result};

/// 创建镜像的完整结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedImage {
    /// 最终镜像路径。
    pub path: PathBuf,
    /// 镜像总容量字节数。
    pub size_bytes: u64,
    /// 所用布局。
    pub layout: ImageLayout,
    /// 全部分区（`raw` 布局为单个覆盖整盘的分区）。
    pub partitions: Vec<CreatedPartition>,
    /// 分区表信息（`raw` 为空）。
    pub partition_tables: Vec<PartitionTable>,
}

impl CreatedImage {
    /// 第一个分区的起始字节偏移（供 loop 挂载的快速路径使用）。
    ///
    /// 保留该方法是为了让既有调用方（`run/offsets.json` 的写入）不必立刻改动；
    /// 多分区场景应改用 [`CreatedImage::partitions`]。
    pub fn first_partition_offset_bytes(&self) -> u64 {
        self.partitions.first().map_or(0, |p| p.offset_bytes)
    }
}

/// 镜像创建的可调参数。
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// 目标路径。
    pub path: PathBuf,
    /// 请求容量（会被向上对齐到 1 MiB，并受可用空间限制）。
    pub size_bytes: u64,
    /// 磁盘布局。
    pub layout: ImageLayout,
    /// 分区列表；为空表示单分区占满剩余空间（MVP 行为）。
    pub partitions: Vec<PartitionSpec>,
    /// 卷标（≤11 字节，超出截断）。
    pub label: String,
    /// 文件系统。
    pub filesystem: FilesystemType,
}

impl CreateOptions {
    /// 以默认布局（GPT）与默认容量构造选项。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            size_bytes: layout::DEFAULT_SIZE_BYTES,
            layout: ImageLayout::default(),
            partitions: Vec::new(),
            label: String::from_utf8_lossy(&crate::layout::VOLUME_LABEL)
                .trim_end()
                .to_string(),
            filesystem: FilesystemType::default(),
        }
    }

    /// 指定请求容量（字节）。内部会自动向上规整至 1 MiB 边界，且必须满足 FAT32 的最小簇数下限。
    pub fn with_size(mut self, size_bytes: u64) -> Self {
        self.size_bytes = size_bytes;
        self
    }

    /// 指定目标磁盘布局（Raw / GPT / MBR）。
    pub fn with_layout(mut self, layout: ImageLayout) -> Self {
        self.layout = layout;
        self
    }

    /// 指定分区列表。
    ///
    /// 未指定时按单分区处理（见 [`CreateOptions::effective_partitions`]），
    /// 以保持既有调用方的行为不变。
    pub fn with_partitions(mut self, partitions: Vec<PartitionSpec>) -> Self {
        self.partitions = partitions;
        self
    }

    /// 指定卷标。
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// 指定文件系统。
    pub fn with_filesystem(mut self, filesystem: FilesystemType) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// 实际生效的分区列表。
    ///
    /// 用户没给分区时，**默认单分区占满剩余空间**——这正是 MVP 的行为，
    /// 因此不指定 `partitions` 的老调用方（含既有 REST 请求体）语义不变。
    ///
    /// 默认分区同时带上 GPT 与 MBR 两套类型（由文件系统推出），这样它在两种
    /// 布局下都合理——用户先选 GPT 再改成 MBR 时不会得到错误的类型字节。
    pub fn effective_partitions(&self) -> Vec<PartitionSpec> {
        if self.partitions.is_empty() {
            let (gpt, mbr) = default_types_for(self.filesystem);
            vec![
                PartitionSpec::fill_remaining(DEFAULT_PARTITION_NAME)
                    .with_gpt_type(gpt)
                    .with_mbr_type(mbr)
                    .with_filesystem(crate::partspec::PartitionFilesystem::Some(self.filesystem)),
            ]
        } else {
            // 用户给了分区：只补**缺省**的类型，不覆盖显式指定。
            //
            // 类型默认值取决于该分区最终用哪个文件系统，故先解析文件系统意图
            // （`Inherit` → 全局默认；`None` → 不格式化；`Some` → 用指定的）。
            self.partitions
                .iter()
                .map(|spec| {
                    let resolved = spec.filesystem.resolve(self.filesystem);
                    let (gpt, mbr) = default_types_for(resolved.unwrap_or(self.filesystem));
                    let mut out = spec.clone();
                    if out.gpt_type.is_none() {
                        out.gpt_type = Some(gpt);
                    }
                    if out.mbr_type.is_none() {
                        out.mbr_type = Some(mbr);
                    }
                    out
                })
                .collect()
        }
    }
}

/// 文件系统 → 该布局下的默认分区类型。
///
/// **GPT 与 MBR 分属两套类型空间**，因此必须分别给出：同一个「FAT32」在 GPT 里是
/// Microsoft Basic Data 的 GUID，在 MBR 里是类型字节 `0x0C`，二者没有对应关系。
pub fn default_types_for(
    filesystem: FilesystemType,
) -> (
    crate::partspec::GptPartitionType,
    crate::partspec::MbrPartitionType,
) {
    use crate::partspec::{GptPartitionType, MbrPartitionType};
    match filesystem {
        FilesystemType::Fat32 => (GptPartitionType::MicrosoftBasic, MbrPartitionType::Fat32Lba),
        FilesystemType::ExFat => (
            GptPartitionType::MicrosoftBasic,
            MbrPartitionType::NtfsExfat,
        ),
        FilesystemType::Ext4 => (GptPartitionType::LinuxFilesystem, MbrPartitionType::Linux),
    }
}

/// 未指定分区名时使用的默认名（与 MVP 的 GPT 分区名一致）。
pub const DEFAULT_PARTITION_NAME: &str = "MAIN";

/// 预检目标位置可用空间。
///
/// 依赖 `statvfs`；`std::fs` 未提供该接口，故用 `libc`。
/// 为避免 `gadgetdisk-core` 引入平台依赖，这里通过 [`avail_bytes`] 抽象。
pub fn check_available_space(path: &Path, needed: u64) -> Result<()> {
    let available = avail_bytes(path)?;
    if available < needed {
        return Err(CoreError::NoSpace { needed, available });
    }
    Ok(())
}

#[cfg(unix)]
fn avail_bytes(path: &Path) -> Result<u64> {
    // 若路径尚不存在，则检查其父目录。
    let probe: &Path = if path.exists() {
        path
    } else {
        path.parent().unwrap_or(Path::new("."))
    };

    // `rustix::fs::statvfs` 返回 Result、字段是 u64，不需要裸 libc 调用或手工零初始化；
    // 路径直接接受 `&Path`，也不需要自己构造 `CString`。
    let stat = rustix::fs::statvfs(probe).map_err(std::io::Error::from)?;

    // f_bavail 是非特权用户可用块数；f_frsize 是基本块大小。
    Ok(stat.f_bavail * stat.f_frsize)
}

#[cfg(not(unix))]
fn avail_bytes(_path: &Path) -> Result<u64> {
    // 非 Unix 平台不提供 statvfs；交由后续写入失败（ENOSPC）兜底。
    Ok(u64::MAX)
}

/// 创建镜像：预分配 → 写分区表 → 格式化 FAT32 → 自检。
///
/// 失败时删除半成品。成功返回完整结果供 gdd 回填 `partition_offset_bytes`。
///
/// `formatter` 负责实际格式化（外部 `mkfs` 或内置 `fatfs`）；本函数只做编排与自检。
pub fn create_image(options: CreateOptions, formatter: &dyn Formatter) -> Result<CreatedImage> {
    // 目标已存在时**拒绝**：静默覆盖会让用户丢掉同名镜像里的全部数据。
    ensure_target_free(&options.path)?;

    let check = layout::check_size(options.size_bytes)?;

    // 预检可用空间：稀疏文件在写入时才占空间，故必须在创建前拦截。
    check_available_space(&options.path, check.image_bytes)?;

    if let Some(parent) = options.path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let result = build(&options, check.image_bytes, formatter);
    if result.is_err() {
        // 半成品必须删除，避免被后续 ListImages 当作完整镜像。
        std::fs::remove_file(&options.path).ok();
    }
    result
}

/// 断言目标路径当前不存在。
///
/// 独立成函数是为了让「同名阻断」这条**行为约定**可以被单元测试直接覆盖，
/// 而不必每次都造一个完整镜像再观察副作用。
pub fn ensure_target_free(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(CoreError::AlreadyExists(path.display().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // 其他错误（如父目录无权限）如实上报，不要当成"不存在"而放行。
        Err(err) => Err(CoreError::Io(err)),
    }
}

fn build(
    options: &CreateOptions,
    image_bytes: u64,
    formatter: &dyn Formatter,
) -> Result<CreatedImage> {
    let specs = options.effective_partitions();
    partspec::validate(&specs, options.layout)?;

    // 可用区间：`raw` 是整盘；有分区表时要扣除**表结构占用的头部**与
    // **末尾的备份结构**。
    //
    // GPT 在盘首写主头 + 分区项数组、盘尾写备份头 + 备份分区项数组，第一个
    // 分区从 1 MiB 开始（[`ALIGNMENT_BYTES`]）。因此"分区可用空间"的保守上界是
    // `image_bytes - ALIGNMENT_BYTES`（头部）再减去尾部 1 MiB 的备份。
    // 精确上界由 `partition::write_gpt` 内部用 `find_free_sectors()` 求得并校验，
    // 这里只需保证不会**高估**——高估会让 `resolve_sizes` 放行一个写不进去的请求。
    //
    // MBR 只在盘首占一个扇区，尾部没有备份结构，故只扣头部对齐。
    // **每个逻辑分区还要额外扣一个 EBR 扇区**（EBR 位于其逻辑分区之前），
    // 以及每个 EBR 前的对齐余量。不扣就是高估，同样会放行写不进去的请求。
    //
    // **空扩展容器不扣这两项**：它不产生任何 EBR。容器自身的容量由
    // `resolve_sizes` 走常规记账（见那里的说明），因此这里不能重复扣减。
    let available = match options.layout {
        ImageLayout::Raw => image_bytes,
        ImageLayout::Gpt => image_bytes.saturating_sub(ALIGNMENT_BYTES * 2),
        ImageLayout::Mbr => {
            let logicals = specs.iter().filter(|s| s.is_logical()).count();
            let ebr_bytes = (logicals as u64)
                .saturating_mul(SECTOR_BYTES)
                // 每个 EBR 之后（逻辑分区之前）以及 EBR 自身所在处都要对齐，
                // 最坏情况每个逻辑分区多消耗一个完整对齐单位。
                .saturating_add((logicals as u64).saturating_mul(ALIGNMENT_BYTES));
            image_bytes
                .saturating_sub(ALIGNMENT_BYTES)
                .saturating_sub(ebr_bytes)
        }
    };
    // 下限按**每个分区各自的文件系统**判定，因此要把全局默认值一并传下去：
    // 分区规格里的 `Inherit` 需要它才能解析出实际要用的文件系统。
    let sizes = partspec::resolve_sizes(&specs, available, options.filesystem)?;

    // 截断文件以建立稀疏预分配空间，并强制同步文件长度元数据；
    // 必须在此落盘，后续 GPT 分区表需要 Seek 至末尾扇区写入 Backup Header。
    // 稀疏文件仅在后续写入扇区时才实际分配物理块，真实空间可用性由前置 check_available_space 校验保证。
    {
        let file = std::fs::File::create(&options.path)?;
        file.set_len(image_bytes)?;
        file.sync_all()?;
    }

    // 2. 写分区表。
    let tables = match options.layout {
        ImageLayout::Raw => {
            // 无分区表：唯一的"分区"覆盖整盘，好让下游统一按分区列表处理。
            Vec::new()
        }
        ImageLayout::Gpt => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&options.path)?;
            let tables = partition::write_gpt(file, &specs, &sizes)?;
            for table in &tables {
                verify_gpt_entry(table, image_bytes)?;
            }
            tables
        }
        ImageLayout::Mbr => {
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&options.path)?;
            let tables = partition::write_mbr(&mut file, &specs, &sizes)?;
            verify_mbr(&mut file, &tables)?;
            tables
        }
    };

    // 3. 建立"区间 → 规格"的映射，然后逐个格式化。
    //
    // `raw` 没有分区表，只有一个覆盖整盘的区间。
    //
    // **必须按 `spec_index` 配对，不能按位置 zip**：MBR 启用逻辑分区后，`tables`
    // 按内核序号排列（主分区在前、逻辑分区在后），而 `specs` 是用户填写顺序
    // （逻辑分区可能夹在中间）。按位置配对会把逻辑分区的文件系统规格套到别的
    // 主分区上，格式化随之落到错误的区间——表现为引导扇区内容错乱。
    let ranges: Vec<(u64, u64, &PartitionSpec, u32)> = if tables.is_empty() {
        vec![(0, image_bytes, &specs[0], 1)]
    } else {
        tables
            .iter()
            .map(|t| {
                let spec = specs.get(t.spec_index).ok_or_else(|| {
                    CoreError::VerifyFailed(format!(
                        "partition entry {} references nonexistent spec index {}",
                        t.index, t.spec_index
                    ))
                })?;
                Ok((t.offset_bytes(), t.end_bytes(), spec, t.index))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let mut created = Vec::with_capacity(ranges.len());
    for (start, end, spec, index) in ranges.iter() {
        // **每个分区用自己的文件系统**；`None` 表示只写分区表、不格式化。
        let volume = match spec.filesystem.resolve(options.filesystem) {
            None => None,
            Some(filesystem) => {
                let plan = FormatPlan::new(filesystem, *start, *end, options.label.clone());
                let volume = formatter.format(&options.path, &plan)?;
                // 自检：读回引导扇区/超级块，确认格式化确实落在期望的区间。
                //
                // **必须传真实区间上界**，不能传 `u64::MAX`：`fscommon::StreamSlice`
                // 按 `size = end - start` 计算读写窗口，传 `u64::MAX` 会得到一个
                // 远超镜像实际长度的窗口，于是可能读到刚写进去的那一份之外的
                // 字节（甚至越过文件末尾），校验结果不再可信。
                verify_formatted(&options.path, *start, *end, filesystem)?;
                Some(volume)
            }
        };

        created.push(CreatedPartition {
            // 序号取分区表给出的**内核序号**（MBR 逻辑分区从 5 起），
            // 而不是枚举下标——后者会与 `loopNpM` 对不上。
            index: *index,
            name: spec.name.clone(),
            gpt_type: Some(spec.effective_gpt_type()),
            mbr_type: Some(spec.effective_mbr_type()),
            offset_bytes: *start,
            size_bytes: *end - *start,
            filesystem: spec.filesystem.resolve(options.filesystem),
            volume,
        });
    }

    Ok(CreatedImage {
        path: options.path.clone(),
        size_bytes: image_bytes,
        layout: options.layout,
        partitions: created,
        partition_tables: tables,
    })
}

/// 校验清单 2：分区起点 1 MiB 对齐，且未越过可用上界。
fn verify_gpt_entry(table: &PartitionTable, image_bytes: u64) -> Result<()> {
    if !table.first_lba.is_multiple_of(ALIGNMENT_SECTORS) {
        return Err(CoreError::VerifyFailed(format!(
            "GPT partition {} starts at sector {}, not aligned to {ALIGNMENT_SECTORS}",
            table.index, table.first_lba
        )));
    }
    if table.offset_bytes() != ALIGNMENT_BYTES && table.index == 1 {
        // 第一个分区必须正好落在 1 MiB；后续分区按对齐规则递推即可。
        return Err(CoreError::VerifyFailed(format!(
            "GPT first partition offset is {} bytes, expected {ALIGNMENT_BYTES}",
            table.offset_bytes()
        )));
    }
    // 末尾至少要留一个扇区给备份 GPT 头。
    if (table.last_lba + 1) * SECTOR_BYTES >= image_bytes {
        return Err(CoreError::VerifyFailed(format!(
            "GPT partition {} crosses the image's usable upper bound",
            table.index
        )));
    }
    Ok(())
}

/// 校验清单 3：分区类型字节与请求一致，且有 `55AA` 签名。
///
/// ## 逻辑分区与 `used_entries`
///
/// `read_mbr_header` 的 `used_entries` 只数**首扇区**里的非空项，而 `tables`
/// 现在含逻辑分区（它们住在 EBR 里，不在首扇区）。因此这里必须按
/// 「主分区数 + 是否启用扩展容器」比较，而不是 `tables.len()`——后者在有逻辑
/// 分区时会永远不等。
fn verify_mbr(file: &mut File, tables: &[PartitionTable]) -> Result<()> {
    // 序号分界：主分区 ≤4，逻辑分区 ≥5。
    //
    // **必须写字面量 4**：`MBR_MAX_PRIMARY as u32` 会被 `cast_possible_truncation`
    // 判为在 32 位目标上可能截断（lint 看不见常量取值），而 `u32::try_from`
    // 尚未 const 稳定。`layout.rs` 的 `SECTOR_BYTES_USIZE` 出于同一原因写字面量，
    // 两者取值必须一致。
    const FIRST_LOGICAL: u32 = 4;

    let header = partition::read_mbr_header(file)?;
    if !header.has_boot_signature {
        return Err(CoreError::VerifyFailed(
            "MBR is missing the 55AA signature".into(),
        ));
    }

    // `PartitionTable::index` 已由写入侧按内核惯例编号：主分区 ≤4，逻辑分区 ≥5。
    // 因此"是不是逻辑分区"可以直接从序号判断，不必回看规格。
    let primaries: Vec<&PartitionTable> =
        tables.iter().filter(|t| t.index <= FIRST_LOGICAL).collect();
    let has_logical = tables.iter().any(|t| t.index > FIRST_LOGICAL);
    // 扩展容器的存在性**不能**只看有没有逻辑分区：用户也可以建一个**空容器**
    // （预留空间）。空容器时首扇区里同样有一个 `0x05` 项，只是没有 EBR 链。
    //
    // 因此判定方式是「表里有逻辑分区，或实际项数比主分区数多 1」。
    let has_extended_by_count = header.used_entries as usize > primaries.len();
    let has_extended = has_logical || has_extended_by_count;

    // 一致性：有逻辑分区却没有多余的项，说明扩展容器项缺失（链没有容器可依附）。
    if has_logical && !has_extended_by_count {
        return Err(CoreError::VerifyFailed(
            "MBR has logical partitions but the first sector has no extended container entry"
                .into(),
        ));
    }
    // 项数必须恰好是「主分区数 + 是否有多余的那一项」，多出两项以上即为异常。
    if header.used_entries as usize != primaries.len() + usize::from(has_extended_by_count) {
        return Err(CoreError::VerifyFailed(format!(
            "MBR partition entry count mismatch: expected {}, got {}",
            primaries.len() + usize::from(has_extended_by_count),
            header.used_entries
        )));
    }
    if u64::from(header.first_lba) != ALIGNMENT_SECTORS {
        return Err(CoreError::VerifyFailed(format!(
            "MBR first partition starts at sector {}, expected {ALIGNMENT_SECTORS}",
            header.first_lba
        )));
    }

    // 逐个核对首扇区里的项。扩展容器项位于主分区之后，其类型字节由写入侧
    // 生成（`0x05`），不是用户选择的类型。
    let read = partition::read_mbr_partitions(file)?;
    let expected_extended_slot = primaries.len();

    for (slot, (type_byte, start, sectors)) in read.iter().enumerate() {
        if has_extended && slot == expected_extended_slot {
            if *type_byte != partition::MBR_TYPE_EXTENDED {
                return Err(CoreError::VerifyFailed(format!(
                    "MBR extended entry has type {type_byte:#04X}, expected {:#04X}",
                    partition::MBR_TYPE_EXTENDED
                )));
            }
            // 容器的位置与长度由求解器保证（有逻辑分区时覆盖整条链；空容器时
            // 覆盖用户声明的容量），此处只要求它是一个非空项——容量是否为 0
            // 会让整个项被读成空项，容器随之消失。
            if *sectors == 0 {
                return Err(CoreError::VerifyFailed(
                    "MBR extended entry has 0 sectors (it would read as an empty entry)".into(),
                ));
            }
            continue;
        }

        let Some(table) = primaries.get(slot) else {
            return Err(CoreError::VerifyFailed(format!(
                "MBR first sector has an unexpected extra partition entry (slot {slot})"
            )));
        };
        let expected = table
            .mbr_type
            .expect("MBR 布局的写入结果必定带 mbr_type")
            .byte();
        if *type_byte != expected {
            return Err(CoreError::VerifyFailed(format!(
                "MBR partition {} has type {type_byte:#04X}, expected {expected:#04X}",
                table.index
            )));
        }
        if u64::from(*start) != table.first_lba {
            return Err(CoreError::VerifyFailed(format!(
                "MBR partition {} starts at LBA {start}, expected {}",
                table.index, table.first_lba
            )));
        }
        if u64::from(*sectors) != table.size_bytes() / SECTOR_BYTES {
            return Err(CoreError::VerifyFailed(format!(
                "MBR partition {} sector count mismatch",
                table.index
            )));
        }
    }

    Ok(())
}

/// 按文件系统类型做创建后自检（校验清单 4 与 5）。
///
/// **只校验能廉价且无依赖校验的部分**：
/// - FAT32：读引导扇区签名并打开卷确认类型（`fatfs` 已在进程内）；
/// - ext4：核对 superblock 魔数 `53EF`（偏移 `+1024+56`，字节序小端）；
/// - exFAT：核对引导扇区 `EXFAT` 标识。
///
/// 不做「重新挂载」这类重量级校验：那属于 loop 挂载路径的职责。
fn verify_formatted(path: &Path, start: u64, end: u64, filesystem: FilesystemType) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    match filesystem {
        FilesystemType::Fat32 => {
            let mut file = std::fs::OpenOptions::new().read(true).open(path)?;
            let boot: BootSector = fat::check_boot_sector(&mut file, start)?;
            if !boot.has_signature {
                return Err(CoreError::VerifyFailed(format!(
                    "the FAT boot sector of the partition at offset {start} is missing 55AA"
                )));
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            // 用真实区间（而非 `u64::MAX`）限定窗口，见调用点的说明。
            fat::verify_fat32(file, start, end)?;
            Ok(())
        }
        FilesystemType::Ext4 => {
            // ext4 superblock 魔数在分区起点 +1024 +56 处，字节序列为 [0x53, 0xEF]（小端 u16 数值为 0xEF53）。
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(start + 1024 + 56))?;
            let mut magic = [0u8; 2];
            file.read_exact(&mut magic)?;
            if u16::from_le_bytes(magic) != 0xEF53 {
                return Err(CoreError::VerifyFailed(format!(
                    "the partition at offset {start} is missing the ext superblock magic 53EF"
                )));
            }
            Ok(())
        }
        FilesystemType::ExFat => {
            // exFAT 引导扇区的 OEM 区（偏移 +3）为 "EXFAT"。
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(start + 3))?;
            let mut oem = [0u8; 5];
            file.read_exact(&mut oem)?;
            if &oem != b"EXFAT" {
                return Err(CoreError::VerifyFailed(format!(
                    "the partition at offset {start} is missing the exFAT signature"
                )));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FormattedVolume, Formatter};
    use crate::testutil;

    /// 测试用格式化器：**始终使用内置 fatfs**，因此不依赖设备上是否装了 mkfs。
    ///
    /// 这正是 `Formatter` 抽象的目的之一：核心编排逻辑可在主机上完整测试。
    fn fatfs() -> fat::FatfsFormatter {
        fat::FatfsFormatter
    }

    /// 记录调用参数的格式化器，用于断言编排层传给格式化器的区间是否正确。
    #[derive(Default)]
    struct RecordingFormatter {
        calls: std::cell::RefCell<Vec<(u64, u64, String)>>,
    }

    impl Formatter for RecordingFormatter {
        fn format(&self, image_path: &Path, plan: &FormatPlan) -> Result<FormattedVolume> {
            self.calls
                .borrow_mut()
                .push((plan.start_bytes, plan.end_bytes, plan.label.clone()));
            fat::FatfsFormatter.format(image_path, plan)
        }
    }

    fn created_bytes(path: &Path, offset: u64, len: usize) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn creates_raw_image_with_whole_volume_fat32() {
        let dir = testutil::temp_dir("create-raw");
        let path = dir.join("raw.img");
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(64 * 1024 * 1024)
                .with_layout(ImageLayout::Raw),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partitions.len(), 1);
        assert_eq!(image.partitions[0].offset_bytes, 0);
        assert_eq!(image.partitions[0].size_bytes, image.size_bytes);
        assert!(image.partition_tables.is_empty());
        assert_eq!(
            image.partitions[0].volume.as_ref().unwrap().label,
            "GADGETDISK"
        );

        // 校验清单 1：偏移 510 处为 55AA。
        assert_eq!(created_bytes(&path, 510, 2), vec![0x55, 0xAA]);

        testutil::cleanup(&path);
    }

    #[test]
    fn creates_gpt_image_aligned_to_one_mib() {
        let dir = testutil::temp_dir("create-gpt");
        let path = dir.join("gpt.img");
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(128 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partitions.len(), 1);
        assert_eq!(image.partition_tables[0].first_lba, ALIGNMENT_SECTORS);
        assert_eq!(image.partitions[0].offset_bytes, ALIGNMENT_BYTES);

        // MBR 保护分区签名为 0（gpt crate 不写 55AA），GPT 头签名为 "EFI PART"。
        assert_eq!(&created_bytes(&path, 512, 8), b"EFI PART");
        // 校验清单 4：分区区间内 +510 处为 55AA。
        assert_eq!(
            created_bytes(&path, ALIGNMENT_BYTES + 510, 2),
            vec![0x55, 0xAA]
        );
        // 校验清单 5：FAT32 类型字符串。
        assert_eq!(&created_bytes(&path, ALIGNMENT_BYTES + 82, 8), b"FAT32   ");

        testutil::cleanup(&path);
    }

    #[test]
    fn creates_mbr_image_with_type_0c() {
        let dir = testutil::temp_dir("create-mbr");
        let path = dir.join("mbr.img");
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(128 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partition_tables[0].first_lba, ALIGNMENT_SECTORS);
        assert_eq!(image.partitions[0].offset_bytes, ALIGNMENT_BYTES);

        // 校验清单 3。
        let entry = created_bytes(&path, 446, 16);
        assert_eq!(entry[4], 0x0C);
        assert_eq!(created_bytes(&path, 510, 2), vec![0x55, 0xAA]);
        // 校验清单 4。
        assert_eq!(
            created_bytes(&path, ALIGNMENT_BYTES + 510, 2),
            vec![0x55, 0xAA]
        );
        // 校验清单 5。
        assert_eq!(&created_bytes(&path, ALIGNMENT_BYTES + 82, 8), b"FAT32   ");

        testutil::cleanup(&path);
    }

    #[test]
    fn rejects_zero_size_and_leaves_no_file() {
        // 容量 0 建不出任何可用镜像，属于参数错误（而不是"容量太小"）。
        let dir = testutil::temp_dir("create-zero");
        let path = dir.join("zero.img");
        let err = create_image(CreateOptions::new(&path).with_size(0), &fatfs()).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)), "得到 {err:?}");
        assert!(!path.exists(), "半成品必须不残留");

        testutil::cleanup(&path);
    }

    #[test]
    fn rejects_partition_below_filesystem_floor_and_leaves_no_file() {
        // 镜像足够大，但分区拿不到它那个文件系统的下限——报的是**分区**太小，
        // 并带上行号与文件系统（早先这里借用 no_space，前端显示成"空间不足"）。
        let dir = testutil::temp_dir("create-partition-small");
        let path = dir.join("small-part.img");
        let specs = vec![
            PartitionSpec::fill_remaining("A")
                .with_size(16 * 1024 * 1024)
                .with_filesystem(crate::partspec::PartitionFilesystem::Some(
                    crate::FilesystemType::Fat32,
                )),
        ];
        let err = create_image(
            CreateOptions::new(&path)
                .with_size(64 * 1024 * 1024)
                .with_layout(ImageLayout::Raw)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap_err();
        match err {
            CoreError::SizeBelowMinimum {
                row,
                filesystem,
                minimum,
                ..
            } => {
                assert_eq!(row, 1);
                assert_eq!(filesystem, "fat32");
                assert_eq!(minimum, crate::layout::MIN_FAT32_BYTES);
            }
            other => panic!("应报 SizeBelowMinimum，得到 {other:?}"),
        }
        assert!(!path.exists(), "半成品必须不残留");

        testutil::cleanup(&path);
    }

    #[test]
    fn default_layout_is_gpt() {
        assert_eq!(ImageLayout::default(), ImageLayout::Gpt);
        let options = CreateOptions::new("/tmp/x.img");
        assert_eq!(options.layout, ImageLayout::Gpt);
        assert_eq!(options.size_bytes, layout::DEFAULT_SIZE_BYTES);
        assert!(options.partitions.is_empty(), "默认不显式指定分区");
        assert_eq!(options.filesystem, FilesystemType::Fat32);
    }

    #[test]
    fn all_layouts_produce_verifiable_fat32() {
        for layout in [ImageLayout::Raw, ImageLayout::Gpt, ImageLayout::Mbr] {
            let dir = testutil::temp_dir(&format!("create-all-{}", layout.as_str()));
            let path = dir.join("img");
            let image = create_image(
                CreateOptions::new(&path)
                    .with_size(128 * 1024 * 1024)
                    .with_layout(layout),
                &fatfs(),
            )
            .unwrap();

            assert_eq!(
                image.partitions[0].offset_bytes,
                layout.partition_offset_bytes()
            );
            assert_eq!(image.partitions[0].filesystem, Some(FilesystemType::Fat32));

            testutil::cleanup(&path);
        }
    }

    // ------------------------------------------------ 同名阻断（行为反转）

    #[test]
    fn refuses_to_overwrite_existing_file() {
        // **语义反转**：旧实现会静默覆盖同名镜像，这会让用户丢掉镜像里的
        // 全部数据。现在必须拒绝。
        let dir = testutil::temp_dir("create-refuse");
        let path = dir.join("exists.img");

        // 先造一个内容可辨识的文件。
        let original = b"ORIGINAL CONTENT - MUST SURVIVE";
        std::fs::write(&path, original).unwrap();

        let err = create_image(
            CreateOptions::new(&path)
                .with_size(64 * 1024 * 1024)
                .with_layout(ImageLayout::Raw),
            &fatfs(),
        )
        .unwrap_err();

        assert!(
            matches!(err, CoreError::AlreadyExists(_)),
            "必须报 AlreadyExists，得到 {err:?}"
        );

        // 关键：原文件必须**逐字节未被修改**——拒绝创建不能有副作用。
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, original, "被拒绝的创建不得改动已存在的文件");

        testutil::cleanup(&path);
    }

    #[test]
    fn refuses_even_for_zero_length_existing_file() {
        // 空文件同样算"已存在"：用户可能正在用别的工具往里写。
        let dir = testutil::temp_dir("create-refuse-empty");
        let path = dir.join("empty.img");
        std::fs::write(&path, b"").unwrap();

        let err = create_image(CreateOptions::new(&path), &fatfs()).unwrap_err();
        assert!(matches!(err, CoreError::AlreadyExists(_)));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        testutil::cleanup(&path);
    }

    #[test]
    fn ensure_target_free_accepts_missing_path() {
        let dir = testutil::temp_dir("ensure-free");
        let path = dir.join("nope.img");
        assert!(ensure_target_free(&path).is_ok());

        std::fs::write(&path, b"x").unwrap();
        let err = ensure_target_free(&path).unwrap_err();
        assert!(matches!(err, CoreError::AlreadyExists(_)));

        testutil::cleanup(&path);
    }

    #[test]
    fn failure_after_creation_still_removes_half_product() {
        // 分区校验失败时（MBR 放不下两个大分区）不得留下半成品。
        let dir = testutil::temp_dir("create-half");
        let path = dir.join("half.img");

        let specs = vec![
            PartitionSpec::fill_remaining("A").with_size(200 * 1024 * 1024),
            PartitionSpec::fill_remaining("B").with_size(200 * 1024 * 1024),
        ];
        let err = create_image(
            CreateOptions::new(&path)
                .with_size(64 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap_err();

        assert!(matches!(err, CoreError::NoSpace { .. }), "得到 {err:?}");
        assert!(!path.exists(), "失败后不得残留半成品");

        testutil::cleanup(&path);
    }

    // ------------------------------------------------ 多分区

    #[test]
    fn creates_multiple_gpt_partitions_each_formatted() {
        let dir = testutil::temp_dir("create-gpt-multi");
        let path = dir.join("multi.img");

        let specs = vec![
            PartitionSpec::fill_remaining("BOOT").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("DATA"),
        ];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partitions.len(), 2);
        assert_eq!(image.partitions[0].index, 1);
        assert_eq!(image.partitions[1].index, 2);
        assert_eq!(image.partitions[0].name, "BOOT");
        assert_eq!(image.partitions[1].name, "DATA");
        assert_eq!(image.partitions[0].size_bytes, 64 * 1024 * 1024);
        // 第二个分区吃掉"占满剩余"——即镜像容量扣除分区表头尾与第一个分区。
        // 断言关系而非魔数：GPT 需在盘首盘尾各留结构，精确值由 write_gpt 决定。
        assert!(
            image.partitions[1].size_bytes >= 180 * 1024 * 1024,
            "剩余空间应约 187 MiB，得到 {}",
            image.partitions[1].size_bytes
        );
        assert!(
            image.partitions[1].offset_bytes + image.partitions[1].size_bytes <= image.size_bytes,
            "第二个分区不得越过镜像末尾"
        );

        // 两个分区的 FAT32 引导扇区都必须就位且签名正确。
        for p in &image.partitions {
            assert_eq!(
                created_bytes(&path, p.offset_bytes + 510, 2),
                vec![0x55, 0xAA],
                "分区 {} 的 FAT 引导扇区签名",
                p.index
            );
            assert_eq!(
                &created_bytes(&path, p.offset_bytes + 82, 8),
                b"FAT32   ",
                "分区 {} 的 FS 类型字符串",
                p.index
            );
        }

        testutil::cleanup(&path);
    }

    #[test]
    fn creates_multiple_mbr_partitions() {
        let dir = testutil::temp_dir("create-mbr-multi");
        let path = dir.join("multi.img");

        let specs = vec![
            PartitionSpec::fill_remaining("A").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("B").with_size(64 * 1024 * 1024),
        ];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partitions.len(), 2);
        // 两个分区项的类型字节都应为 0x0C。
        let e0 = created_bytes(&path, 446, 16);
        let e1 = created_bytes(&path, 446 + 16, 16);
        assert_eq!(e0[4], 0x0C);
        assert_eq!(e1[4], 0x0C);
        // 第二项必须非空（起始 LBA 非 0）。
        assert_ne!(e1[8..12], [0u8; 4]);

        testutil::cleanup(&path);
    }

    #[test]
    fn formatter_receives_correct_ranges_for_multiple_partitions() {
        let dir = testutil::temp_dir("create-ranges");
        let path = dir.join("ranges.img");

        let recorder = RecordingFormatter::default();
        let specs = vec![
            PartitionSpec::fill_remaining("A").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("B"),
        ];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt)
                .with_partitions(specs),
            &recorder,
        )
        .unwrap();

        let calls = recorder.calls.borrow();
        assert_eq!(calls.len(), 2, "每个分区都应触发一次格式化");

        // 传给格式化器的区间必须与最终报告的分区区间一致。
        for (call, partition) in calls.iter().zip(&image.partitions) {
            assert_eq!(call.0, partition.offset_bytes);
            assert_eq!(call.1, partition.offset_bytes + partition.size_bytes);
        }
        // 第一个分区必须从 1 MiB 开始（不能是 0，那是分区表所在处）。
        assert_eq!(calls[0].0, ALIGNMENT_BYTES);

        testutil::cleanup(&path);
    }

    #[test]
    fn explicit_no_format_survives_global_default() {
        // **回归**：请求里显式写 `filesystem: none` 的分区**不得**被套上全局默认。
        // 三态意图（Inherit/None/Some）正是为了区分这两者——早先用
        // `Option<FilesystemType>` 时两者都是 `None`，于是"不格式化"变成了 FAT32，
        // 而且该分区还真的被格式化了（实测在 AVD 上复现）。
        let dir = testutil::temp_dir("create-no-format");
        let path = dir.join("mixed.img");

        let specs = vec![
            PartitionSpec::fill_remaining("A")
                .with_size(64 * 1024 * 1024)
                .with_filesystem(crate::partspec::PartitionFilesystem::Some(
                    FilesystemType::Fat32,
                )),
            // 显式声明"不格式化"。
            PartitionSpec::fill_remaining("RAW")
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
        ];

        let recorder = RecordingFormatter::default();
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt)
                .with_partitions(specs),
            &recorder,
        )
        .unwrap();

        assert_eq!(image.partitions.len(), 2);
        // 只有第一个分区被格式化：格式化器只应被调用一次。
        assert_eq!(
            recorder.calls.borrow().len(),
            1,
            "未格式化的分区不应触发格式化调用"
        );

        assert_eq!(image.partitions[0].filesystem, Some(FilesystemType::Fat32));
        assert!(image.partitions[0].volume.is_some());

        assert_eq!(
            image.partitions[1].filesystem, None,
            "显式 none 的分区必须保持未格式化"
        );
        assert!(image.partitions[1].volume.is_none());

        testutil::cleanup(&path);
    }

    #[test]
    fn unspecified_filesystem_inherits_global_default() {
        // 对照组：**没有**指定文件系统的分区应继承全局默认。
        let dir = testutil::temp_dir("create-inherit");
        let path = dir.join("inherit.img");

        let specs = vec![PartitionSpec::fill_remaining("A").with_size(64 * 1024 * 1024)];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt)
                .with_filesystem(FilesystemType::Fat32)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partitions[0].filesystem, Some(FilesystemType::Fat32));
        assert!(image.partitions[0].volume.is_some());

        testutil::cleanup(&path);
    }

    #[test]
    fn custom_label_reaches_formatter() {
        let dir = testutil::temp_dir("create-label");
        let path = dir.join("label.img");
        let recorder = RecordingFormatter::default();

        create_image(
            CreateOptions::new(&path)
                .with_size(64 * 1024 * 1024)
                .with_layout(ImageLayout::Raw)
                .with_label("MYDISK"),
            &recorder,
        )
        .unwrap();

        let calls = recorder.calls.borrow();
        assert_eq!(calls[0].2, "MYDISK");

        testutil::cleanup(&path);
    }

    #[test]
    fn custom_label_is_written_to_volume() {
        // docs/roadmap.md 已知缺陷 #1：`--label` 曾被静默忽略。这里守住修复。
        let dir = testutil::temp_dir("create-label-real");
        let path = dir.join("label.img");

        create_image(
            CreateOptions::new(&path)
                .with_size(64 * 1024 * 1024)
                .with_layout(ImageLayout::Raw)
                .with_label("HELLO"),
            &fatfs(),
        )
        .unwrap();

        use std::io::Read as _;
        let mut file = std::fs::File::open(&path).unwrap();
        let mut buf = [0u8; 512];
        file.read_exact(&mut buf).unwrap();
        // FAT32 卷标在引导扇区偏移 71 处（BPB 的 BS_VolLab），11 字节。
        assert_eq!(&buf[71..82], b"HELLO      ");

        testutil::cleanup(&path);
    }

    #[test]
    fn rejects_raw_layout_with_multiple_partitions() {
        let dir = testutil::temp_dir("create-raw-multi");
        let path = dir.join("raw.img");

        let specs = vec![
            PartitionSpec::fill_remaining("A").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("B"),
        ];
        let err = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Raw)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap_err();

        assert!(matches!(err, CoreError::InvalidArgument(_)));
        assert!(!path.exists());

        testutil::cleanup(&path);
    }

    #[test]
    fn rejects_too_many_mbr_partitions() {
        let dir = testutil::temp_dir("create-mbr-toomany");
        let path = dir.join("many.img");

        let specs: Vec<_> = (0..5)
            .map(|i| PartitionSpec::fill_remaining(format!("P{i}")).with_size(16 * 1024 * 1024))
            .collect();
        let err = create_image(
            CreateOptions::new(&path)
                .with_size(512 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap_err();

        assert!(matches!(err, CoreError::InvalidArgument(_)), "得到 {err:?}");

        testutil::cleanup(&path);
    }

    #[test]
    fn default_single_partition_preserves_legacy_shape() {
        // 回归：不传 partitions 时，结果必须与 MVP 的单分区行为一致，
        // 否则旧 REST 调用方会拿到不同布局。
        let dir = testutil::temp_dir("create-legacy");
        let path = dir.join("legacy.img");
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(128 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt),
            &fatfs(),
        )
        .unwrap();

        assert_eq!(image.partitions.len(), 1);
        assert_eq!(image.partitions[0].name, DEFAULT_PARTITION_NAME);
        assert_eq!(
            image.partitions[0].gpt_type,
            Some(crate::partspec::GptPartitionType::MicrosoftBasic)
        );
        assert_eq!(
            image.partitions[0].mbr_type,
            Some(crate::partspec::MbrPartitionType::Fat32Lba)
        );
        assert_eq!(image.partitions[0].offset_bytes, ALIGNMENT_BYTES);
        assert_eq!(
            image.first_partition_offset_bytes(),
            ALIGNMENT_BYTES,
            "兼容访问器必须给出 1 MiB"
        );

        testutil::cleanup(&path);
    }

    #[test]
    fn ext4_partition_gets_linux_type_and_mkfs_path() {
        // ext4 需要真实 mkfs，主机上没有，故用记录型格式化器验证编排：
        // 关键是要断言 ext4 确实走了 mkfs 路径（而不是被 FAT32 实现吞掉）。
        let dir = testutil::temp_dir("create-ext4");
        let path = dir.join("ext4.img");

        struct FailUnlessMkfs;
        impl Formatter for FailUnlessMkfs {
            fn format(&self, _p: &Path, plan: &FormatPlan) -> Result<FormattedVolume> {
                assert_eq!(plan.filesystem, FilesystemType::Ext4);
                assert!(
                    plan.filesystem.uses_external_mkfs(),
                    "ext4 必须声明为使用外部 mkfs"
                );
                // 真实 mkfs 会写入超级块魔数；这里模拟它，好让创建后自检
                // （verify_formatted）能走到"通过"分支。若不写，自检必须失败
                // ——那正是另一条测试要覆盖的场景。
                use std::io::{Seek, SeekFrom, Write};
                let mut f = std::fs::OpenOptions::new().write(true).open(_p).unwrap();
                f.seek(SeekFrom::Start(plan.start_bytes + 1024 + 56))
                    .unwrap();
                f.write_all(&0xEF53u16.to_le_bytes()).unwrap();
                f.sync_all().unwrap();
                Ok(FormattedVolume {
                    filesystem: FilesystemType::Ext4,
                    label: plan.label.clone(),
                    tool: Some("/system/bin/mkfs.ext4".into()),
                })
            }
        }

        let image = create_image(
            CreateOptions::new(&path)
                .with_size(128 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt)
                .with_filesystem(FilesystemType::Ext4),
            &FailUnlessMkfs,
        )
        .unwrap();

        // 默认分区类型应随文件系统变为 Linux。
        assert_eq!(
            image.partitions[0].gpt_type,
            Some(crate::partspec::GptPartitionType::LinuxFilesystem)
        );

        testutil::cleanup(&path);
    }

    // ------------------------------------------------ 扩展分区与逻辑分区

    #[test]
    fn creates_mbr_image_with_logical_partitions_formatted() {
        // **端到端**：3 主 + 2 逻辑，每个分区都真的格式化。
        // 逻辑分区必须拿到正确区间并被格式化——`ranges` 由 `tables` 驱动，
        // 而 `tables` 现在含逻辑分区（序号 5、6），这正是要守住的衔接点。
        let dir = testutil::temp_dir("create-mbr-logical");
        let path = dir.join("logical.img");

        let specs = vec![
            PartitionSpec::fill_remaining("P1").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("L1")
                .with_size(64 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical),
            PartitionSpec::fill_remaining("L2")
                .with_size(64 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical),
        ];
        // 容量要给足：每个分区都必须 ≥33 MiB（FAT32 下限），否则 `fatfs` 会静默降级为
        // FAT16，而校验层（正确地）只接受 FAT32。
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(512 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        // 序号：主分区 1，逻辑分区 5、6。
        let indices: Vec<u32> = image.partitions.iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![1, 5, 6]);

        // 每个分区（含逻辑分区）都必须有 FAT32 引导扇区就位。
        for p in &image.partitions {
            assert_eq!(
                created_bytes(&path, p.offset_bytes + 510, 2),
                vec![0x55, 0xAA],
                "分区 {} 的 FAT 引导扇区签名",
                p.index
            );
            assert_eq!(
                &created_bytes(&path, p.offset_bytes + 82, 8),
                b"FAT32   ",
                "分区 {} 的 FS 类型字符串",
                p.index
            );
            assert!(p.volume.is_some(), "分区 {} 应已格式化", p.index);
        }

        // 逻辑分区的区间不得与主分区或彼此重叠，且都在镜像内。
        let mut ranges: Vec<(u64, u64)> = image
            .partitions
            .iter()
            .map(|p| (p.offset_bytes, p.offset_bytes + p.size_bytes))
            .collect();
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            assert!(pair[1].0 > pair[0].1, "分区区间重叠：{pair:?}");
        }
        for p in &image.partitions {
            assert!(
                p.offset_bytes + p.size_bytes <= image.size_bytes,
                "分区 {} 越过镜像末尾",
                p.index
            );
        }

        testutil::cleanup(&path);
    }

    #[test]
    fn logical_partitions_get_their_own_filesystem_spec_not_a_neighbours() {
        // **回归**：`ranges` 必须按 `PartitionTable::spec_index` 配对规格，
        // 不能按位置 zip。
        //
        // 这条测试用**可区分**的规格：主分区不格式化、逻辑分区格式化。若按位置
        // zip，`tables` 的顺序（主分区在前、逻辑分区在后）与 `specs` 的用户顺序
        // 一旦不同，逻辑分区的「格式化」意图就会被套到主分区上。
        //
        // 早先的同类测试没能守住这一点：它的分区规格**完全相同**（同类型、同容量），
        // 互换规格没有任何可观测差异——注入验证时把代码改回按位置 zip，测试照样通过。
        let dir = testutil::temp_dir("create-mbr-logical-spec-index");
        let path = dir.join("pairing.img");

        // **用户顺序刻意与内核序号顺序不一致**：逻辑分区写在前面、主分区写在
        // 后面。这样才能区分「按 spec_index 配对」与「按位置 zip」——若两者顺序
        // 恰好相同，两种实现给出同一结果，测试就守不住任何东西。
        //
        // 内核序号会把主分区排在前面（1），逻辑分区排在后面（5），因此
        // `tables` 的顺序与 `specs` 的顺序在这里正好相反。
        let specs = vec![
            // specs[0]：逻辑分区，要格式化。
            PartitionSpec::fill_remaining("FMTL")
                .with_size(64 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical)
                .with_filesystem(crate::partspec::PartitionFilesystem::Some(
                    FilesystemType::Fat32,
                )),
            // specs[1]：主分区，不格式化。
            PartitionSpec::fill_remaining("RAWP")
                .with_size(64 * 1024 * 1024)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
        ];

        let recorder = RecordingFormatter::default();
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(512 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &recorder,
        )
        .unwrap();

        // 序号 1（主）、5（逻辑），名字与所属规格必须对应。
        assert_eq!(
            image.partitions.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 5]
        );
        assert_eq!(image.partitions[0].name, "RAWP");
        assert_eq!(image.partitions[1].name, "FMTL");

        // **关键断言**：只有逻辑分区（序号 5）被格式化。
        assert_eq!(
            image.partitions[0].filesystem, None,
            "主分区显式声明了不格式化，不得被套上别的分区的文件系统"
        );
        assert!(image.partitions[0].volume.is_none());
        assert_eq!(image.partitions[1].filesystem, Some(FilesystemType::Fat32));
        assert!(image.partitions[1].volume.is_some());

        // 格式化调用只应发生一次，且落在逻辑分区的区间上。
        let calls = recorder.calls.borrow();
        assert_eq!(calls.len(), 1, "只有逻辑分区应触发格式化");
        assert_eq!(
            calls[0].0, image.partitions[1].offset_bytes,
            "格式化区间必须落在逻辑分区上，而不是主分区"
        );

        testutil::cleanup(&path);
    }

    #[test]
    fn created_logical_partitions_round_trip_through_read_partitions() {
        // 闭环：创建 → 重新读表。逻辑分区的偏移必须与创建时报告的一致，
        // 否则 loop 挂载会落到错误区间。
        let dir = testutil::temp_dir("create-mbr-logical-rt");
        let path = dir.join("rt.img");

        let specs = vec![
            PartitionSpec::fill_remaining("P1").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("L1")
                .with_size(64 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical),
        ];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        let scan = crate::read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Mbr);
        assert_eq!(scan.partitions.len(), 2);
        assert_eq!(
            scan.partitions.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 5]
        );

        for (read, created) in scan.partitions.iter().zip(&image.partitions) {
            assert_eq!(
                read.offset_bytes(),
                created.offset_bytes,
                "分区 {} 偏移读写不一致",
                read.index
            );
            assert_eq!(read.size_bytes, created.size_bytes);
        }

        testutil::cleanup(&path);
    }

    #[test]
    fn creates_mbr_image_with_an_empty_extended_container() {
        // **端到端（1 主 + 1 空扩展容器）**：容器由用户显式声明、里面没有逻辑
        // 分区。这条路径此前不存在——容器只在「有逻辑分区」时才产生。
        //
        // 要守住的三件事：首扇区确实写出 `0x05` 项；**不产生任何 EBR**；
        // 读回来时容器仍然可见（否则用户会以为它消失了）。
        let dir = testutil::temp_dir("create-mbr-empty-extended");
        let path = dir.join("empty-ext.img");

        let specs = vec![
            PartitionSpec::fill_remaining("P1").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("EXT")
                .with_size(32 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Extended)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
        ];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        // 容器不是分区：只有 1 个分区，序号为 1。
        let indices: Vec<u32> = image.partitions.iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![1], "容器不得进入分区列表");

        // 首扇区里必须有 `0x05` 项，且第 2 个槽位就是它。
        let mut file = std::fs::File::open(&path).unwrap();
        let header = partition::read_mbr_header(&mut file).unwrap();
        assert_eq!(header.used_entries, 2, "主分区项 + 扩展容器项");

        let entries = partition::read_mbr_partitions(&mut file).unwrap();
        assert_eq!(
            entries[1].0,
            partition::MBR_TYPE_EXTENDED,
            "槽 1 必须是 0x05"
        );
        // 容器区间：从主分区之后的对齐边界起，长度等于声明的 32 MiB。
        assert_eq!(u64::from(entries[1].2) * SECTOR_BYTES, 32 * 1024 * 1024);

        // **容器起点处不得有 EBR**：空容器不该写任何 EBR 扇区。
        // EBR 的第一个分区项类型字节若为 0x83/0x0C 之类，说明写错了。
        let container_start = u64::from(entries[1].1);
        let at_container = created_bytes(&path, container_start * SECTOR_BYTES + 450, 1);
        assert_eq!(
            at_container,
            vec![0x00],
            "空容器起点不应有 EBR 的引导标志/分区项"
        );

        // 读回来时容器可见，且被标为容器（不占序号）。
        let scan = crate::partitions::read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Mbr);

        // **主分区必须还在**：早先这里写成 `retain(|p| p.is_extended_container())`，
        // 于是空容器镜像读回来**只剩容器**、主分区凭空消失。只断言"找得到容器"
        // 测不出这个 bug——必须同时断言另一侧也还在。
        let real: Vec<_> = scan.partitions.iter().filter(|p| p.index > 0).collect();
        assert_eq!(real.len(), 1, "主分区必须仍在列表里：{:?}", scan.partitions);
        assert_eq!(real[0].index, 1);
        assert_eq!(real[0].size_bytes, 64 * 1024 * 1024);

        let container = scan
            .partitions
            .iter()
            .find(|p| p.is_extended_container())
            .expect("空扩展容器必须能读回来");
        assert_eq!(container.index, 0, "容器不占内核序号");
        assert_eq!(container.start_lba, container_start);
        assert_eq!(container.size_bytes, 32 * 1024 * 1024);

        testutil::cleanup(&path);
    }

    #[test]
    fn empty_extended_container_is_not_formatted() {
        // 容器没有数据区：即便用户（或旧客户端）给它填了文件系统，也不能去
        // 格式化那块区间——那里放的是 EBR 链的位置。
        let dir = testutil::temp_dir("create-mbr-extended-no-fmt");
        let path = dir.join("ext-nofmt.img");

        let specs = vec![
            PartitionSpec::fill_remaining("P1").with_size(64 * 1024 * 1024),
            PartitionSpec::fill_remaining("EXT")
                .with_size(32 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Extended)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
        ];
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap();

        // 只有主分区被格式化；容器不在 `partitions` 里。
        assert_eq!(image.partitions.len(), 1);
        assert!(image.partitions[0].volume.is_some());

        testutil::cleanup(&path);
    }

    #[test]
    fn rejects_logical_partitions_when_four_primaries_present() {
        // 4 主已占满首扇区槽位，扩展容器无处安放：必须在写任何字节前拒绝，
        // 且不留下半成品。
        let dir = testutil::temp_dir("create-mbr-4p-logical");
        let path = dir.join("nope.img");

        let mut specs: Vec<PartitionSpec> = (0..4)
            .map(|i| PartitionSpec::fill_remaining(format!("P{i}")).with_size(16 * 1024 * 1024))
            .collect();
        specs.push(
            PartitionSpec::fill_remaining("L1")
                .with_size(16 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical),
        );

        let err = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap_err();

        assert!(matches!(err, CoreError::InvalidArgument(_)), "得到 {err:?}");
        assert!(!path.exists(), "失败后不得残留半成品");

        testutil::cleanup(&path);
    }

    #[test]
    fn rejects_logical_partitions_on_gpt() {
        let dir = testutil::temp_dir("create-gpt-logical");
        let path = dir.join("nope.img");

        let specs = vec![
            PartitionSpec::fill_remaining("A").with_size(16 * 1024 * 1024),
            PartitionSpec::fill_remaining("L1")
                .with_size(16 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical),
        ];

        let err = create_image(
            CreateOptions::new(&path)
                .with_size(256 * 1024 * 1024)
                .with_layout(ImageLayout::Gpt)
                .with_partitions(specs),
            &fatfs(),
        )
        .unwrap_err();

        assert!(matches!(err, CoreError::InvalidArgument(_)), "得到 {err:?}");

        testutil::cleanup(&path);
    }

    #[test]
    fn verify_mbr_index_boundary_matches_partition_kind_boundary() {
        // `verify_mbr` 用字面量 4 判断"主分区还是逻辑分区"（见那里的注释），
        // `PartitionEntry::kind` 也用字面量 4。两者必须与 `MBR_MAX_PRIMARY`
        // 一致，否则同一个镜像在写入自检与读取展示上会得出不同结论。
        assert_eq!(
            u32::try_from(crate::partspec::MBR_MAX_PRIMARY).unwrap(),
            4,
            "字面量 4 必须等于 MBR_MAX_PRIMARY"
        );

        // 端到端确认：3 主 + 1 逻辑的镜像，序号 1..=3 为主、5 为逻辑。
        let dir = testutil::temp_dir("create-verify-mbr-boundary");
        let path = dir.join("b.img");
        let specs = vec![
            PartitionSpec::fill_remaining("P1")
                .with_size(64 * 1024 * 1024)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
            PartitionSpec::fill_remaining("P2")
                .with_size(64 * 1024 * 1024)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
            PartitionSpec::fill_remaining("P3")
                .with_size(64 * 1024 * 1024)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
            PartitionSpec::fill_remaining("L1")
                .with_size(64 * 1024 * 1024)
                .with_kind(crate::partspec::PartitionKind::Logical)
                .with_filesystem(crate::partspec::PartitionFilesystem::None),
        ];
        // 未格式化的分区仍需满足结构校验；用记录型格式化器避免依赖 mkfs。
        let image = create_image(
            CreateOptions::new(&path)
                .with_size(512 * 1024 * 1024)
                .with_layout(ImageLayout::Mbr)
                .with_partitions(specs),
            &RecordingFormatter::default(),
        )
        .unwrap();

        let kinds: Vec<crate::partspec::PartitionKind> =
            image.partitions.iter().map(|p| p.kind()).collect();
        use crate::partspec::PartitionKind::{Logical, Primary};
        assert_eq!(kinds, vec![Primary, Primary, Primary, Logical]);
        assert_eq!(
            image.partitions.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );

        testutil::cleanup(&path);
    }
}
