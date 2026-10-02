//! 分区表生成：GPT 与 MBR，均支持多分区。
//!
//! 规格与实测依据见 [docs/disk-image-format.md](../../../docs/disk-image-format.md)。
//! 用户的**分区意图**（数量、容量、类型、名称）由 [`crate::partspec`] 表达与校验，
//! 本模块只负责把已校验的意图写成字节。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::layout::{ALIGNMENT_SECTORS, SECTOR_BYTES, SECTOR_BYTES_USIZE};
use crate::{CoreError, Result};

/// MBR 分区类型：FAT32 LBA。
pub const MBR_TYPE_FAT32_LBA: u8 = 0x0C;

/// MBR 分区类型：扩展分区（容纳逻辑分区的容器）。
///
/// 出现在两处：首扇区里指向整个扩展区间，以及每个 EBR 里指向**下一个** EBR。
/// 两处的起始 LBA 语义不同——首扇区是绝对值，EBR 是相对本 EBR 的偏移。
pub const MBR_TYPE_EXTENDED: u8 = 0x05;

/// MBR 分区项在首扇区中的字节偏移。
pub const MBR_PARTITION_ENTRY_OFFSET: usize = 446;

/// MBR 分区项长度。
pub const MBR_PARTITION_ENTRY_LEN: usize = 16;

/// MBR 首扇区中的分区项个数（分区项数组固定 4 项）。
pub const MBR_MAX_ENTRIES: usize = crate::partspec::MBR_MAX_PRIMARY;

/// 引导扇区签名（小端 `0xAA55`，即字节序列 `55 AA`）。
pub const BOOT_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// CHS 起始哨兵：表示该分区使用 LBA 寻址。
pub const CHS_LBA_SENTINEL: [u8; 3] = [0xFE, 0xFF, 0xFF];

/// 已写入的单个分区信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionTable {
    /// 1 起序号（与内核 `loopNpM` 及 [`crate::partitions::PartitionEntry::index`] 对齐）。
    pub index: u32,
    /// 分区起始 LBA（512 字节扇区）。
    pub first_lba: u64,
    /// 分区结束 LBA（含）。
    pub last_lba: u64,
    /// GPT 分区类型（GPT 布局下有值）。
    pub gpt_type: Option<crate::partspec::GptPartitionType>,
    /// MBR 分区类型（MBR 布局下有值）。
    pub mbr_type: Option<crate::partspec::MbrPartitionType>,
    /// 分区名（GPT 写入镜像；MBR 保留用户输入仅供展示）。
    pub name: String,
    /// 该分区在入参 `specs` 里的下标。
    ///
    /// **调用方必须用它配对规格，不能按下标位置配对**：MBR 启用逻辑分区后，
    /// 返回值按**内核序号**排列（主分区在前、逻辑分区在后），而 `specs` 是用户
    /// 填写顺序（逻辑分区可能夹在中间）。按位置 zip 会把逻辑分区的文件系统
    /// 规格套到别的主分区上，格式化随之落到错误的区间。
    pub spec_index: usize,
}

impl PartitionTable {
    /// 分区起始字节偏移。
    pub const fn offset_bytes(&self) -> u64 {
        self.first_lba * SECTOR_BYTES
    }

    /// 分区容量字节数。
    pub const fn size_bytes(&self) -> u64 {
        (self.last_lba - self.first_lba + 1) * SECTOR_BYTES
    }

    /// 分区区间的结束字节偏移（不含）。
    pub const fn end_bytes(&self) -> u64 {
        (self.last_lba + 1) * SECTOR_BYTES
    }
}

/// 在已有文件上创建 GPT 分区表并写入若干分区。
///
/// **强制项**：对齐参数显式传 `Some(`[`ALIGNMENT_SECTORS`]`)`，否则 `gpt` 的默认对齐
/// 会把分区起点放在 LBA 34。
///
/// 容量上界由 `find_free_sectors()` 求得后取 `min(请求值, 上界)`，
/// 不自行硬算，否则会与 GPT 备份头所需空间冲突。
///
/// `sizes` 与 `specs` 必须等长，且容量已由
/// [`crate::partspec::resolve_sizes`] 展开（不含 `0`）。
pub fn write_gpt(
    file: File,
    specs: &[crate::partspec::PartitionSpec],
    sizes: &[u64],
) -> Result<Vec<PartitionTable>> {
    use gpt::GptConfig;
    use gpt::disk::LogicalBlockSize;

    if specs.len() != sizes.len() {
        return Err(CoreError::InvalidArgument(
            "partition spec and size lists have different lengths".into(),
        ));
    }

    let config = GptConfig::new()
        .writable(true)
        .logical_block_size(LogicalBlockSize::Lb512);
    let mut disk = config.create_from_device(file, None)?;

    // 逐个添加。**每次添加后重新查询空闲区间**：`gpt` 的 `add_partition` 会在
    // 内部按对齐规则挑选位置，缓存首次结果会让第二个分区与第一个重叠。
    for (spec, size) in specs.iter().zip(sizes) {
        let guid = gpt_type_for(&spec.effective_gpt_type());
        let requested_lba = size.div_ceil(SECTOR_BYTES);

        // **在所有空闲区间里找一个放得下的**，而不是只看第一个：`find_free_sectors`
        // 会同时返回「分区项数组之后到第一个分区之前」这类碎片区间，只看第一个
        // 会误判为空间不足。
        let free = disk.find_free_sectors();
        let mut best: Option<(u64, u64)> = None;
        let mut best_available: u64 = 0;

        for (start, len) in free {
            let aligned_start = align_lba(start, ALIGNMENT_SECTORS);
            let end_exclusive = start + len;
            let usable = end_exclusive.saturating_sub(aligned_start);
            if usable > best_available {
                best_available = usable;
                best = Some((aligned_start, usable));
            }
            if usable >= requested_lba {
                best = Some((aligned_start, usable));
                break;
            }
        }

        let Some((_aligned_start, max_len)) = best else {
            return Err(CoreError::InvalidArgument(
                "the GPT disk has no free sectors".into(),
            ));
        };

        if max_len == 0 {
            return Err(CoreError::InvalidArgument(format!(
                "not enough free GPT space for partition '{}' (no sectors left after alignment)",
                spec.name
            )));
        }

        if requested_lba > max_len {
            // 不静默裁剪：静默裁剪会让用户拿到一个比预期小的分区。
            return Err(CoreError::NoSpace {
                needed: requested_lba * SECTOR_BYTES,
                available: max_len * SECTOR_BYTES,
            });
        }

        disk.add_partition(
            &spec.name,
            requested_lba * SECTOR_BYTES,
            guid,
            0,
            Some(ALIGNMENT_SECTORS),
        )?;
    }

    // 在任何写入落盘前先把分区表快照取出：`disk.write()` 之后借用关系不方便再读。
    let snapshot: Vec<(u64, u64)> = {
        let mut entries: Vec<(u64, u64)> = disk
            .partitions()
            .values()
            .map(|p| (p.first_lba, p.last_lba))
            .collect();
        // `partitions()` 是 HashMap，顺序不确定；按起点排序后才是磁盘顺序，
        // 也才能与 index（1 起）对应。
        entries.sort_unstable();
        entries
    };

    if snapshot.len() != specs.len() {
        return Err(CoreError::VerifyFailed(format!(
            "GPT partition count mismatch on write: expected {}, got {}",
            specs.len(),
            snapshot.len()
        )));
    }

    disk.write()?;

    Ok(snapshot
        .into_iter()
        .zip(specs)
        .enumerate()
        .map(|(i, ((first_lba, last_lba), spec))| PartitionTable {
            index: u32::try_from(i + 1).unwrap_or(u32::MAX),
            first_lba,
            last_lba,
            gpt_type: Some(spec.effective_gpt_type()),
            mbr_type: None,
            name: spec.name.clone(),
            spec_index: i,
        })
        .collect())
}

/// 把 [`GptPartitionType`] 映射为 `gpt` crate 的类型。
///
/// 预设类型走 crate 的常量（保证 GUID 与其内部表一致）；`Custom` 直接构造
/// ——`gpt::partition_types::Type` 的两个字段都是 `pub`，因此无需绕过 crate API。
fn gpt_type_for(type_id: &crate::partspec::GptPartitionType) -> gpt::partition_types::Type {
    use crate::partspec::GptPartitionType;
    use gpt::partition_types;

    match type_id {
        GptPartitionType::EfiSystem => partition_types::EFI,
        GptPartitionType::MicrosoftBasic => partition_types::BASIC,
        GptPartitionType::MicrosoftReserved => partition_types::MICROSOFT_RESERVED,
        GptPartitionType::WindowsRecovery => partition_types::WINDOWS_RECOVERY,
        GptPartitionType::LinuxFilesystem => partition_types::LINUX_FS,
        GptPartitionType::LinuxSwap => partition_types::LINUX_SWAP,
        GptPartitionType::LinuxLvm => partition_types::LINUX_LVM,
        GptPartitionType::LinuxRaid => partition_types::LINUX_RAID,
        GptPartitionType::BiosBoot => partition_types::BIOS,
        // 自定义 GUID：直接构造。`os` 用 `Custom` 变体如实表达"不在已知表里"。
        GptPartitionType::Custom(guid) => partition_types::Type {
            guid: *guid,
            os: partition_types::OperatingSystem::Custom(guid.to_string()),
        },
    }
}

/// 在已有文件上创建 MBR 分区表并写入若干分区（主分区与逻辑分区）。
///
/// 自行构造首扇区（无成熟 crate）。每个分区项（16 字节）：
/// - `+0`：引导标志 `0x00`（非活动）；
/// - `+1..+4`：CHS 起始哨兵 `FE FF FF`（表示使用 LBA 寻址）；
/// - `+4`：分区类型字节（由 [`crate::partspec::MbrPartitionType::byte`] 给出）；
/// - `+5..+8`：CHS 结束哨兵 `FE FF FF`；
/// - `+8..+12`：起始 LBA（小端 `u32`，1 MiB 对齐）；
/// - `+12..+16`：扇区数（小端 `u32`）；
/// - `510..512`：`55 AA`。
///
/// **MBR 没有分区名字段**：`specs[i].name` 不会被写入，仅随返回值回传给调用方
/// 供 UI 展示（见 [`crate::partspec`] 的模块文档）。
///
/// ## 逻辑分区与扩展分区
///
/// 标为 [`crate::partspec::PartitionKind::Logical`] 的规格会写进 EBR 链，
/// 扩展分区容器由 [`crate::partspec::resolve_mbr_layout`] 自动定位。位置与容量
/// 的**全部计算都在求解器里**，本函数只负责把结果编成字节——这样「会不会重叠」
/// 这类问题能在主机上用纯函数测试穷举，而不必写坏真实镜像。
///
/// 首扇区里扩展分区项的 `starting_lba` 是**绝对** LBA，而 EBR 项里的是**相对
/// 本 EBR** 的偏移。两者混用会得到错得离谱的偏移，且镜像在 Host 上看起来仍然
/// 「合法」——这是本文件最容易出错的地方。
pub fn write_mbr(
    file: &mut File,
    specs: &[crate::partspec::PartitionSpec],
    sizes: &[u64],
) -> Result<Vec<PartitionTable>> {
    use crate::partspec::PartitionKind;

    if specs.len() != sizes.len() {
        return Err(CoreError::InvalidArgument(
            "partition spec and size lists have different lengths".into(),
        ));
    }
    if specs.is_empty() {
        return Err(CoreError::InvalidArgument(
            "MBR requires at least one partition".into(),
        ));
    }
    // 只有扩展容器、没有任何分区：那张表在 Host 上什么都挂不上，是个纯空壳。
    // 允许它只会让用户白费一次创建，故明确拒绝。
    if !specs
        .iter()
        .any(crate::partspec::PartitionSpec::is_partition)
    {
        return Err(CoreError::InvalidArgument(
            "MBR requires at least one primary or logical partition: an extended container is not \
mountable and cannot be the only content"
                .into(),
        ));
    }

    // **不在这里检查 `specs.len() > 4`**：启用逻辑分区后分区总数可以远超 4
    // （3 主 + 最多 64 逻辑）。真正的槽位约束（主分区数 + 是否启用扩展容器
    // ≤ 4）由 `resolve_mbr_layout` 判定，它才知道哪些是逻辑分区。

    let total_sectors = image_bytes_to_sectors(file)?;
    let plan = crate::partspec::resolve_mbr_layout(specs, sizes, total_sectors, ALIGNMENT_SECTORS)?;
    let plan = &plan;

    let mut header = [0u8; SECTOR_BYTES_USIZE];

    // ---- 首扇区：主分区项 + 扩展分区项 ----
    //
    // 槽位顺序即规格顺序（求解器保证主分区先占 0..n_primary，扩展容器紧随其后）。
    // 用 `enumerate()` 而不是手写计数器：后者会被 lint 判为「循环计数器」。
    for (slot, placement) in plan
        .placements
        .iter()
        .filter(|p| p.kind == PartitionKind::Primary)
        .enumerate()
    {
        write_entry(
            &mut header,
            slot,
            placement.type_byte,
            placement.first_lba,
            placement.sectors(),
        );
    }

    if let (Some(slot), Some((first, last))) = (plan.extended_slot, plan.extended_range) {
        // 扩展分区项：**绝对** LBA，覆盖整条 EBR 链。
        write_entry(
            &mut header,
            slot,
            MBR_TYPE_EXTENDED,
            first,
            last - first + 1,
        );
    }

    header[510..512].copy_from_slice(&BOOT_SIGNATURE);

    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;

    // ---- EBR 链 ----
    //
    // 每个 EBR 描述**一个**逻辑分区，并指向下一个 EBR（链尾不指）。
    for ebr in &plan.ebrs {
        let placement = plan
            .placements
            .iter()
            .find(|p| p.spec_index == ebr.spec_index)
            .ok_or_else(|| {
                CoreError::VerifyFailed("EBR has no matching logical partition".into())
            })?;

        let mut sector = [0u8; SECTOR_BYTES_USIZE];

        // 第一个项：本 EBR 描述的逻辑分区。**起始 LBA 相对本 EBR**。
        let relative_start = placement.first_lba.checked_sub(ebr.lba).ok_or_else(|| {
            CoreError::VerifyFailed("logical partition starts before its EBR".into())
        })?;
        write_entry(
            &mut sector,
            0,
            placement.type_byte,
            relative_start,
            placement.sectors(),
        );

        // 第二个项：下一个 EBR。同样相对本 EBR；链尾不写（保持空项）。
        if let Some(next_lba) = ebr.next_lba {
            let relative_next = next_lba.checked_sub(ebr.lba).ok_or_else(|| {
                CoreError::VerifyFailed("EBR chain pointer moved backwards".into())
            })?;
            write_entry(&mut sector, 1, MBR_TYPE_EXTENDED, relative_next, 1);
        }

        sector[510..512].copy_from_slice(&BOOT_SIGNATURE);

        file.seek(SeekFrom::Start(u64::from(ebr.lba) * SECTOR_BYTES))?;
        file.write_all(&sector)?;
    }

    file.sync_all()?;

    // 按内核序号输出，与 `read_partitions` 的顺序一致。
    Ok(plan
        .placements
        .iter()
        .map(|p| PartitionTable {
            index: p.index,
            first_lba: u64::from(p.first_lba),
            last_lba: u64::from(p.last_lba),
            gpt_type: None,
            mbr_type: Some(specs[p.spec_index].effective_mbr_type()),
            name: specs[p.spec_index].name.clone(),
            spec_index: p.spec_index,
        })
        .collect())
}

/// 写入一个分区项（`start` 的语义由调用方决定：首扇区为绝对，EBR 为相对）。
///
/// 独立成函数是为了让「CHS 哨兵 + 引导标志」这两处易错常量只写一次：EBR 与首
/// 扇区的项布局完全相同，分别手写迟早会漂移。
fn write_entry(
    sector: &mut [u8; SECTOR_BYTES_USIZE],
    slot: usize,
    kind: u8,
    start: u32,
    sectors: u32,
) {
    let offset = MBR_PARTITION_ENTRY_OFFSET + slot * MBR_PARTITION_ENTRY_LEN;
    sector[offset] = 0x00; // 引导标志：非活动
    sector[offset + 1..offset + 4].copy_from_slice(&CHS_LBA_SENTINEL);
    sector[offset + 4] = kind;
    sector[offset + 5..offset + 8].copy_from_slice(&CHS_LBA_SENTINEL);
    sector[offset + 8..offset + 12].copy_from_slice(&start.to_le_bytes());
    sector[offset + 12..offset + 16].copy_from_slice(&sectors.to_le_bytes());
}

/// 由文件长度求出总扇区数（MBR 的 LBA 为 32 位，超出即报错）。
fn image_bytes_to_sectors(file: &File) -> Result<u32> {
    let bytes = file.metadata()?.len();
    let sectors = bytes / SECTOR_BYTES;
    let narrowed = u32::try_from(sectors)
        .map_err(|_| CoreError::InvalidArgument("MBR layout is limited to about 2 TiB".into()))?;
    if narrowed <= 1 {
        return Err(CoreError::InvalidArgument(
            "image is too small to hold an MBR partition table".into(),
        ));
    }
    Ok(narrowed)
}

/// 解析已有镜像的 MBR 首扇区（用于自检与布局探测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrHeader {
    /// 第一个非空分区项的分区类型字节。
    pub partition_type: u8,
    /// 第一个非空分区项的起始 LBA。
    pub first_lba: u32,
    /// 第一个非空分区项的扇区数。
    pub sectors: u32,
    /// 首扇区末尾是否为 `55 AA`。
    pub has_boot_signature: bool,
    /// 非空分区项个数。
    ///
    /// 多分区后「哪个是第一个分区」不再等同于「分区项 0」——用户可能不按顺序
    /// 填写，空项也必须跳过。该计数让自检能断言实际写入了几项。
    pub used_entries: u32,
}

/// 读取并解析 MBR 首扇区。
///
/// 按分区项顺序返回**第一个非空项**（起始 LBA 非 0 且扇区数非 0）。
/// 这是「探测布局」路径关心的信息；逐分区读取请用 [`read_mbr_partitions`]。
pub fn read_mbr_header(file: &mut File) -> Result<MbrHeader> {
    let mut header = [0u8; SECTOR_BYTES_USIZE];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;

    let mut first: Option<(u8, u32, u32)> = None;
    let mut used_entries = 0u32;

    for i in 0..MBR_MAX_ENTRIES {
        let offset = MBR_PARTITION_ENTRY_OFFSET + i * MBR_PARTITION_ENTRY_LEN;
        let entry = &header[offset..offset + MBR_PARTITION_ENTRY_LEN];
        let first_lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        let sectors = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]);

        if entry[4] == 0 || sectors == 0 {
            continue;
        }
        used_entries += 1;
        if first.is_none() {
            first = Some((entry[4], first_lba, sectors));
        }
    }

    // 一个非空项都没有时不报错：调用方（`read_partitions`）据此判定「无分区表」。
    let (partition_type, first_lba, sectors) = first.unwrap_or((0, 0, 0));

    Ok(MbrHeader {
        partition_type,
        first_lba,
        sectors,
        has_boot_signature: header[510..512] == BOOT_SIGNATURE,
        used_entries,
    })
}

/// 读取 MBR 中全部非空分区项（按分区项顺序）。
///
/// 返回 `(类型字节, 起始 LBA, 扇区数)` 列表，供 [`crate::partitions`] 渲染分区列表。
pub fn read_mbr_partitions(file: &mut File) -> Result<Vec<(u8, u32, u32)>> {
    let mut header = [0u8; SECTOR_BYTES_USIZE];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;

    let mut out = Vec::new();
    for i in 0..MBR_MAX_ENTRIES {
        let offset = MBR_PARTITION_ENTRY_OFFSET + i * MBR_PARTITION_ENTRY_LEN;
        let entry = &header[offset..offset + MBR_PARTITION_ENTRY_LEN];
        let first_lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        let sectors = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]);
        if entry[4] == 0 || sectors == 0 {
            continue;
        }
        out.push((entry[4], first_lba, sectors));
    }
    Ok(out)
}

/// 向上对齐 LBA 到 `align` 的整数倍。
const fn align_lba(lba: u64, align: u64) -> u64 {
    crate::layout::align_up(lba, align)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ALIGNMENT_BYTES;
    use crate::testutil;
    use std::io::Write;

    #[test]
    fn gpt_partition_starts_at_one_mib() {
        let (path, _) = testutil::temp_image("part-gpt-4g", 4 * 1024 * 1024 * 1024);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // 请求 3 GiB 且上界足够，应精确得到 3 GiB 分区。
        let specs = [spec(3 * 1024 * 1024 * 1024, "MAIN")];
        let tables = write_gpt(file, &specs, &[3 * 1024 * 1024 * 1024]).unwrap();
        let table = &tables[0];

        assert_eq!(table.first_lba, ALIGNMENT_SECTORS);
        assert_eq!(table.offset_bytes(), ALIGNMENT_BYTES);
        assert_eq!(table.size_bytes(), 3 * 1024 * 1024 * 1024);

        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_rejects_oversized_image() {
        // 造一个超过 u32 扇区数上限的文件长度。
        let dir = testutil::temp_dir("part-oversize");
        let path = dir.join("big.img");
        {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(u64::from(u32::MAX) * SECTOR_BYTES + SECTOR_BYTES * 2)
                .unwrap();
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let specs = [crate::partspec::PartitionSpec::fill_remaining("A")];
        let err = write_mbr(&mut file, &specs, &[64 * 1024 * 1024]).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        testutil::cleanup(&path);
    }

    #[test]
    fn partition_table_accessors_are_consistent() {
        let table = PartitionTable {
            index: 1,
            first_lba: 2048,
            last_lba: 4095,
            gpt_type: Some(crate::partspec::GptPartitionType::MicrosoftBasic),
            mbr_type: None,
            name: "MAIN".into(),
            spec_index: 0,
        };
        assert_eq!(table.offset_bytes(), 2048 * 512);
        assert_eq!(table.size_bytes(), 2048 * 512);
        assert_eq!(table.end_bytes(), 4096 * 512);
    }

    #[test]
    fn writing_mbr_does_not_touch_other_sectors() {
        let size = 64 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part", size);
        // 在第二扇区写入哨兵，确认 MBR 只改首扇区。
        file.seek(SeekFrom::Start(512)).unwrap();
        file.write_all(b"KEEPOUT").unwrap();
        let specs = [crate::partspec::PartitionSpec::fill_remaining("MAIN")];
        // 用一段只占部分空间的容量：整盘大小写进去会（正确地）被判为超出
        // 分区表所剩空间，而本测试只关心"第二扇区未被改动"。
        write_mbr(&mut file, &specs, &[16 * 1024 * 1024]).unwrap();
        let mut probe = [0u8; 7];
        file.seek(SeekFrom::Start(512)).unwrap();
        use std::io::Read as _;
        file.read_exact(&mut probe).unwrap();
        assert_eq!(&probe, b"KEEPOUT");
        testutil::cleanup(&path);
    }

    // ------------------------------------------------ 多分区

    fn spec(size: u64, name: &str) -> crate::partspec::PartitionSpec {
        crate::partspec::PartitionSpec {
            size_bytes: size,
            gpt_type: Some(crate::partspec::GptPartitionType::MicrosoftBasic),
            mbr_type: None,
            name: name.into(),
            filesystem: crate::partspec::PartitionFilesystem::Some(
                crate::fs::FilesystemType::Fat32,
            ),
            kind: crate::partspec::PartitionKind::Primary,
        }
    }

    #[test]
    fn gpt_writes_multiple_non_overlapping_aligned_partitions() {
        let size = 512 * 1024 * 1024;
        let (path, _) = testutil::temp_image("part-gpt-multi", size);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        let specs = [spec(64 * 1024 * 1024, "A"), spec(128 * 1024 * 1024, "B")];
        let sizes = [64 * 1024 * 1024u64, 128 * 1024 * 1024];
        let tables = write_gpt(file, &specs, &sizes).unwrap();

        assert_eq!(tables.len(), 2);
        // 序号必须 1 起且连续，才能与 loopNpM 对应。
        assert_eq!(tables[0].index, 1);
        assert_eq!(tables[1].index, 2);
        assert_eq!(tables[0].name, "A");
        assert_eq!(tables[1].name, "B");

        for t in &tables {
            assert_eq!(t.first_lba % ALIGNMENT_SECTORS, 0, "分区必须 1 MiB 对齐");
        }
        // 不重叠：后一个的起点必须晚于前一个的终点。
        assert!(
            tables[1].first_lba > tables[0].last_lba,
            "分区重叠：{} 起点 {} 不大于 {} 终点 {}",
            tables[1].name,
            tables[1].first_lba,
            tables[0].name,
            tables[0].last_lba
        );
        // 容量精确（未超出可用空间，不应被裁剪）。
        assert_eq!(tables[0].size_bytes(), 64 * 1024 * 1024);
        assert_eq!(tables[1].size_bytes(), 128 * 1024 * 1024);

        testutil::cleanup(&path);
    }

    #[test]
    fn gpt_rejects_too_large_partition_without_clamping() {
        let size = 128 * 1024 * 1024;
        let (path, _) = testutil::temp_image("part-gpt-toobig", size);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        // 请求远超容量的分区：必须报 NoSpace，而不是静默裁剪成一个小分区。
        let specs = [spec(16 * 1024 * 1024 * 1024, "HUGE")];
        let err = write_gpt(file, &specs, &[16 * 1024 * 1024 * 1024]).unwrap_err();
        assert!(matches!(err, CoreError::NoSpace { .. }), "得到 {err:?}");

        testutil::cleanup(&path);
    }

    #[test]
    fn gpt_rejects_mismatched_lengths() {
        let (path, _) = testutil::temp_image("part-gpt-mismatch", 128 * 1024 * 1024);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let specs = [spec(64 * 1024 * 1024, "A")];
        let err = write_gpt(file, &specs, &[64 * 1024 * 1024, 64 * 1024 * 1024]).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_writes_multiple_partitions_with_correct_bytes() {
        let size = 512 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-multi", size);

        let specs = [spec(64 * 1024 * 1024, "A"), spec(128 * 1024 * 1024, "B")];
        let sizes = [64 * 1024 * 1024u64, 128 * 1024 * 1024];
        let tables = write_mbr(&mut file, &specs, &sizes).unwrap();

        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].index, 1);
        assert_eq!(tables[1].index, 2);
        assert_eq!(tables[0].first_lba, ALIGNMENT_SECTORS);
        assert_eq!(tables[1].first_lba % ALIGNMENT_SECTORS, 0);
        assert!(tables[1].first_lba > tables[0].last_lba, "分区重叠");

        // 直接核对首扇区字节：两个分区项各自 16 字节。
        let mut raw = [0u8; SECTOR_BYTES_USIZE];
        use std::io::Read as _;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut raw).unwrap();

        for (i, table) in tables.iter().enumerate() {
            let offset = MBR_PARTITION_ENTRY_OFFSET + i * MBR_PARTITION_ENTRY_LEN;
            assert_eq!(raw[offset], 0x00, "分区 {i} 引导标志");
            assert_eq!(&raw[offset + 1..offset + 4], &CHS_LBA_SENTINEL);
            assert_eq!(raw[offset + 4], 0x0C, "分区 {i} 类型");
            assert_eq!(&raw[offset + 5..offset + 8], &CHS_LBA_SENTINEL);
            let start = u32::from_le_bytes(raw[offset + 8..offset + 12].try_into().unwrap());
            let sectors = u32::from_le_bytes(raw[offset + 12..offset + 16].try_into().unwrap());
            assert_eq!(u64::from(start), table.first_lba);
            assert_eq!(u64::from(sectors), table.size_bytes() / SECTOR_BYTES);
        }
        assert_eq!(&raw[510..512], &BOOT_SIGNATURE);

        // 第三个分区项必须仍是空的（只有两个分区）。
        let third = MBR_PARTITION_ENTRY_OFFSET + 2 * MBR_PARTITION_ENTRY_LEN;
        assert_eq!(raw[third + 4], 0, "第三个分区项应为空");

        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_rejects_more_than_four_primary_partitions() {
        let size = 512 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-five", size);
        let specs: Vec<_> = (0..5)
            .map(|i| spec(16 * 1024 * 1024, &format!("P{i}")))
            .collect();
        let sizes = vec![16 * 1024 * 1024u64; 5];
        let err = write_mbr(&mut file, &specs, &sizes).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_rejects_partitions_exceeding_capacity() {
        let size = 128 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-toobig", size);
        let specs = [spec(64 * 1024 * 1024, "A"), spec(256 * 1024 * 1024, "B")];
        let sizes = [64 * 1024 * 1024u64, 256 * 1024 * 1024];
        let err = write_mbr(&mut file, &specs, &sizes).unwrap_err();
        assert!(matches!(err, CoreError::NoSpace { .. }), "得到 {err:?}");
        testutil::cleanup(&path);
    }

    #[test]
    fn read_mbr_partitions_lists_all_non_empty_entries() {
        let size = 256 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-read", size);
        let specs = [spec(64 * 1024 * 1024, "A"), spec(64 * 1024 * 1024, "B")];
        let sizes = [64 * 1024 * 1024u64, 64 * 1024 * 1024];
        let tables = write_mbr(&mut file, &specs, &sizes).unwrap();

        let read = read_mbr_partitions(&mut file).unwrap();
        assert_eq!(read.len(), 2);
        for (i, (type_byte, start, sectors)) in read.iter().enumerate() {
            assert_eq!(*type_byte, 0x0C);
            assert_eq!(u64::from(*start), tables[i].first_lba);
            assert_eq!(u64::from(*sectors), tables[i].size_bytes() / SECTOR_BYTES);
        }

        let header = read_mbr_header(&mut file).unwrap();
        assert_eq!(header.used_entries, 2);
        assert_eq!(header.first_lba, 2048);

        testutil::cleanup(&path);
    }

    #[test]
    fn gpt_writes_custom_type_guid_into_partition_entry() {
        // 自定义类型 GUID 必须**确实写入磁盘**，而不是只回显在响应里。
        // GPT 分区项数组从 LBA2 开始，每项 128 字节，类型 GUID 在项内偏移 0。
        let size = 256 * 1024 * 1024;
        let (path, _) = testutil::temp_image("part-gpt-custom-guid", size);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        let guid = uuid::Uuid::parse_str("12345678-9ABC-DEF0-1234-56789ABCDEF0").unwrap();
        let specs = [crate::partspec::PartitionSpec {
            size_bytes: 64 * 1024 * 1024,
            gpt_type: Some(crate::partspec::GptPartitionType::Custom(guid)),
            mbr_type: None,
            name: "CUSTOM".into(),
            filesystem: crate::partspec::PartitionFilesystem::None,
            kind: crate::partspec::PartitionKind::Primary,
        }];
        let tables = write_gpt(file, &specs, &[64 * 1024 * 1024]).unwrap();
        assert_eq!(tables.len(), 1);

        // 读回第一个分区项的类型 GUID，按磁盘字节序与 uuid 的字节序比对。
        use std::io::Read as _;
        let mut f = std::fs::File::open(&path).unwrap();
        f.seek(SeekFrom::Start(2 * SECTOR_BYTES)).unwrap();
        let mut entry = [0u8; 128];
        f.read_exact(&mut entry).unwrap();

        let written = uuid::Uuid::from_bytes_le(entry[0..16].try_into().unwrap());
        assert_eq!(
            written, guid,
            "自定义类型 GUID 未正确写入分区项（读回 {written}）"
        );

        // 名称也应写入（项内偏移 56，UTF-16LE）。
        let mut name_units = Vec::new();
        for chunk in entry[56..128].as_chunks::<2>().0 {
            let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
            if unit == 0 {
                break;
            }
            name_units.push(unit);
        }
        assert_eq!(String::from_utf16_lossy(&name_units), "CUSTOM");

        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_writes_custom_type_byte() {
        let size = 128 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-custom-byte", size);

        let specs = [crate::partspec::PartitionSpec {
            size_bytes: 64 * 1024 * 1024,
            gpt_type: None,
            mbr_type: Some(crate::partspec::MbrPartitionType::Custom(0x1A)),
            name: "X".into(),
            filesystem: crate::partspec::PartitionFilesystem::None,
            kind: crate::partspec::PartitionKind::Primary,
        }];
        write_mbr(&mut file, &specs, &[64 * 1024 * 1024]).unwrap();

        let mut raw = [0u8; SECTOR_BYTES_USIZE];
        use std::io::Read as _;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut raw).unwrap();
        assert_eq!(raw[MBR_PARTITION_ENTRY_OFFSET + 4], 0x1A);

        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_single_partition_matches_legacy_layout() {
        // 回归：单分区 MBR 的字节布局必须与旧实现一致，否则既有镜像的
        // 解析与挂载路径会出现不可预期的差异。
        let size = 64 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-legacy", size);
        let specs = [spec(size - ALIGNMENT_BYTES, "MAIN")];
        let tables = write_mbr(&mut file, &specs, &[size - ALIGNMENT_BYTES]).unwrap();

        assert_eq!(tables[0].first_lba, ALIGNMENT_SECTORS);
        assert_eq!(tables[0].last_lba, size / SECTOR_BYTES - 1);

        let header = read_mbr_header(&mut file).unwrap();
        assert_eq!(header.partition_type, MBR_TYPE_FAT32_LBA);
        assert_eq!(header.first_lba, 2048);
        assert!(header.has_boot_signature);

        let mut raw = [0u8; SECTOR_BYTES_USIZE];
        use std::io::Read as _;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut raw).unwrap();
        assert_eq!(raw[446], 0x00);
        assert_eq!(&raw[447..450], &CHS_LBA_SENTINEL);
        assert_eq!(raw[450], 0x0C);
        assert_eq!(&raw[451..454], &CHS_LBA_SENTINEL);
        assert_eq!(&raw[510..512], &BOOT_SIGNATURE);

        testutil::cleanup(&path);
    }

    // ------------------------------------------------ 扩展分区与逻辑分区

    use crate::partspec::{PartitionKind, resolve_mbr_layout};

    /// 造一个逻辑分区规格。
    fn logical(size: u64, name: &str) -> crate::partspec::PartitionSpec {
        spec(size, name).with_kind(PartitionKind::Logical)
    }

    /// 读回首扇区。
    fn read_first_sector(file: &mut File) -> [u8; SECTOR_BYTES_USIZE] {
        use std::io::Read as _;
        let mut raw = [0u8; SECTOR_BYTES_USIZE];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut raw).unwrap();
        raw
    }

    /// 读回任意扇区。
    fn read_sector(file: &mut File, lba: u64) -> [u8; SECTOR_BYTES_USIZE] {
        use std::io::Read as _;
        let mut raw = [0u8; SECTOR_BYTES_USIZE];
        file.seek(SeekFrom::Start(lba * SECTOR_BYTES)).unwrap();
        file.read_exact(&mut raw).unwrap();
        raw
    }

    /// 从分区项里取出 `(类型, 起始 LBA, 扇区数)`。
    fn entry_at(sector: &[u8; SECTOR_BYTES_USIZE], slot: usize) -> (u8, u32, u32) {
        let base = MBR_PARTITION_ENTRY_OFFSET + slot * MBR_PARTITION_ENTRY_LEN;
        (
            sector[base + 4],
            u32::from_le_bytes(sector[base + 8..base + 12].try_into().unwrap()),
            u32::from_le_bytes(sector[base + 12..base + 16].try_into().unwrap()),
        )
    }

    #[test]
    fn mbr_writes_extended_container_and_ebr_chain() {
        // **核心往返**：3 主 + 2 逻辑。首扇区里必须有一个 0x05 容器项覆盖整条
        // EBR 链，且每个逻辑分区前面有一个 EBR。
        let size = 512 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-ebr", size);

        let specs = vec![
            spec(16 * 1024 * 1024, "P1"),
            spec(16 * 1024 * 1024, "P2"),
            spec(16 * 1024 * 1024, "P3"),
            logical(32 * 1024 * 1024, "L1"),
            logical(32 * 1024 * 1024, "L2"),
        ];
        let sizes = vec![
            16 * 1024 * 1024u64,
            16 * 1024 * 1024,
            16 * 1024 * 1024,
            32 * 1024 * 1024,
            32 * 1024 * 1024,
        ];
        let tables = write_mbr(&mut file, &specs, &sizes).unwrap();

        // 序号：主分区 1、2、3，逻辑分区 5、6。
        let indices: Vec<u32> = tables.iter().map(|t| t.index).collect();
        assert_eq!(indices, vec![1, 2, 3, 5, 6], "逻辑分区序号必须从 5 起");

        // 首扇区：3 个数据主分区 + 第 4 槽为扩展容器。
        let raw = read_first_sector(&mut file);
        assert_eq!(entry_at(&raw, 0).0, 0x0C);
        assert_eq!(entry_at(&raw, 1).0, 0x0C);
        assert_eq!(entry_at(&raw, 2).0, 0x0C);
        let (ext_type, ext_start, ext_len) = entry_at(&raw, 3);
        assert_eq!(ext_type, MBR_TYPE_EXTENDED, "第 4 槽必须是扩展分区容器");

        // 扩展容器必须覆盖整条链：从首个 EBR 到最后一个逻辑分区结束。
        let plan = resolve_mbr_layout(
            &specs,
            &sizes,
            u32::try_from(size / SECTOR_BYTES).unwrap(),
            ALIGNMENT_SECTORS,
        )
        .unwrap();
        assert_eq!(plan.ebrs.len(), 2);
        assert_eq!(ext_start, plan.ebrs[0].lba, "容器起点必须是首个 EBR");
        let last_logical = plan
            .placements
            .iter()
            .filter(|p| p.kind == PartitionKind::Logical)
            .map(|p| p.last_lba)
            .max()
            .unwrap();
        assert_eq!(
            ext_start + ext_len - 1,
            last_logical,
            "容器必须覆盖到最后一个逻辑分区"
        );

        // 每个 EBR：项 0 是本分区（相对偏移），项 1 是指向下一个 EBR 的链指针。
        for (i, ebr) in plan.ebrs.iter().enumerate() {
            let sector = read_sector(&mut file, u64::from(ebr.lba));
            assert_eq!(&sector[510..512], &BOOT_SIGNATURE, "EBR {i} 缺少 55AA");

            let (kind, relative_start, _sectors) = entry_at(&sector, 0);
            assert_ne!(kind, 0, "EBR {i} 的项 0 必须描述一个逻辑分区");
            assert_eq!(
                u64::from(ebr.lba) + u64::from(relative_start),
                tables
                    .iter()
                    .find(|t| t.index == 5 + u32::try_from(i).unwrap())
                    .unwrap()
                    .first_lba,
                "EBR {i} 的项 0 起始偏移（相对）必须指向该逻辑分区"
            );

            let (link_type, link_rel, link_len) = entry_at(&sector, 1);
            match ebr.next_lba {
                Some(next) => {
                    assert_eq!(link_type, MBR_TYPE_EXTENDED);
                    assert_eq!(u64::from(ebr.lba) + u64::from(link_rel), u64::from(next));
                    assert_eq!(link_len, 1, "链指针项只占一个扇区");
                }
                None => {
                    // 链尾：项 1 必须为空，否则读取侧会继续走。
                    assert_eq!(link_type, 0, "链尾必须留空项");
                    assert_eq!(link_rel, 0);
                }
            }
        }

        testutil::cleanup(&path);
    }

    #[test]
    fn ebr_container_never_overlaps_primary_partitions() {
        let size = 512 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-ebr-overlap", size);

        let specs = vec![
            spec(64 * 1024 * 1024, "P1"),
            logical(64 * 1024 * 1024, "L1"),
            logical(64 * 1024 * 1024, "L2"),
        ];
        let sizes = vec![64 * 1024 * 1024u64; 3];
        let tables = write_mbr(&mut file, &specs, &sizes).unwrap();

        // 所有分区区间两两不相交（按位置排序后判断）。
        let mut ranges: Vec<(u64, u64)> =
            tables.iter().map(|t| (t.first_lba, t.last_lba)).collect();
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            assert!(pair[1].0 > pair[0].1, "分区区间重叠：{pair:?}");
        }
        // 扩展容器不得覆盖任何主分区。
        let raw = read_first_sector(&mut file);
        let (_, ext_start, ext_len) = entry_at(&raw, 1); // 1 主 + 扩展 → 容器在槽 1
        let ext_end = u64::from(ext_start) + u64::from(ext_len) - 1;
        let primary = tables.iter().find(|t| t.index == 1).unwrap();
        assert!(
            primary.last_lba < u64::from(ext_start) || primary.first_lba > ext_end,
            "扩展容器与主分区重叠"
        );

        testutil::cleanup(&path);
    }

    #[test]
    fn mbr_without_logical_writes_no_ebr_and_no_extended_entry() {
        // **回归底线**：没有逻辑分区时不得出现 0x05 项。
        let size = 128 * 1024 * 1024;
        let (path, mut file) = testutil::temp_image("part-mbr-noebr", size);

        let specs = [spec(64 * 1024 * 1024, "A"), spec(32 * 1024 * 1024, "B")];
        let sizes = [64 * 1024 * 1024u64, 32 * 1024 * 1024];
        write_mbr(&mut file, &specs, &sizes).unwrap();

        let raw = read_first_sector(&mut file);
        for slot in 0..MBR_MAX_ENTRIES {
            assert_ne!(
                entry_at(&raw, slot).0,
                MBR_TYPE_EXTENDED,
                "槽 {slot} 不该有扩展分区项"
            );
        }
        // 第三、四槽必须仍是空的。
        assert_eq!(entry_at(&raw, 2).0, 0);
        assert_eq!(entry_at(&raw, 3).0, 0);

        testutil::cleanup(&path);
    }
}
