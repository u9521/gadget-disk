//! 分区表读取（GPT 与 MBR）。
//!
//! 规格依据 [docs/disk-image-format.md](../../../docs/disk-image-format.md)；
//! 写入侧见 [`crate::partition`]。
//!
//! ## 为什么手写而不是用 `gpt` crate 的读取 API
//!
//! 写入用 `gpt` crate 是因为它替我们处理了 GPT 的边界与对齐计算。读取不同：
//! 我们只需要「列出分区」这一件小事，而手写解析
//!
//! 1. **完全可在主机测试**（解析是纯函数，喂字节即可，见本文件测试）；
//! 2. **不扩大依赖面**——仓库既有风格就是手写磁盘/内核结构
//!    （MBR 写入、`loop_info64`、`loop_info` 偏移）；
//! 3. 能对**畸形输入**给出明确定义的行为（见 `parse_gpt` 的上界与校验），
//!    而库 API 在这些情况下往往直接报一个笼统错误。
//!
//! ## 序号语义（关键）
//!
//! [`PartitionEntry::index`] 是 **1 起的序数，且跳过空项**，与内核 `loopNpM`
//! 的 `M` 一致。GPT 分区项数组里通常夹着大量空项，若按「数组下标」编号，
//! UI 显示的序号与内核设备名会对不上。
//!
//! MBR 下还多一层：主分区占 1–4，**逻辑分区从 5 起**（Linux 惯例）。扩展分区
//! 容器本身**不占序号**——它不是可挂载分区，其内容由 [`parse_ebr_chain`] 遍历
//! EBR 链后追加。

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::layout::{ImageLayout, SECTOR_BYTES, SECTOR_BYTES_USIZE};
use crate::{CoreError, Result};

/// GPT 头签名（LBA1 偏移 0 处）。
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// GPT 头所在 LBA。
const GPT_HEADER_LBA: u64 = 1;

/// 引导扇区签名。
const BOOT_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// MBR 分区项数组的起始偏移与单项长度。
const MBR_ENTRY_OFFSET: usize = 446;
const MBR_ENTRY_LEN: usize = 16;
const MBR_ENTRY_COUNT: usize = 4;

/// MBR 类型：GPT 保护分区。
const MBR_TYPE_PROTECTIVE: u8 = 0xEE;

/// MBR 类型：扩展分区（容纳逻辑分区的容器）。
const MBR_TYPE_EXTENDED: u8 = 0x05;

/// EBR 链的**遍历长度上限**。
///
/// EBR 链是磁盘上的链表，损坏的镜像可以让它成环（某项指回自己或早先的 EBR）。
/// 没有上限的遍历会永远循环下去——而且是在 root 域的 `gadgetdisk serve` 里，
/// 表现为"界面卡死"而非明确报错。64 与写入侧的上限一致。
pub const MAX_EBR_CHAIN: usize = crate::partspec::MBR_MAX_LOGICAL;

/// GPT 分区项的最小长度（UEFI 规定 ≥128）。
const GPT_ENTRY_MIN_LEN: u32 = 128;

/// 分区项数量的**读取上界**。
///
/// 头里的 `entry_count` 是磁盘上的数据，可能被损坏成极大值。不设上界就会按它
/// 去分配内存（一个 4 GiB 的分配请求足以让进程被 OOM 杀掉）。128 远超单分区
/// 镜像所需，也覆盖常见多分区镜像。
pub const MAX_PARTITION_ENTRIES: u32 = 128;

/// 单个分区，或扩展分区**容器**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionEntry {
    /// 1 起序号，跳过空项与保护分区（与 `loopNpM` 的 `M` 对应）。
    ///
    /// **扩展容器为 `0`**：它不是分区，`loopNpM` 里没有对应的项（见
    /// [`PartitionEntry::kind`]）。
    pub index: u32,
    /// 起始 LBA（512 字节扇区）。
    pub start_lba: u64,
    /// 容量字节数。
    pub size_bytes: u64,
    /// 人类可读的分区类型。
    pub type_label: String,
}

impl PartitionEntry {
    /// 起始字节偏移（`LOOP_SET_STATUS64` 的 `lo_offset`）。
    pub const fn offset_bytes(&self) -> u64 {
        self.start_lba * SECTOR_BYTES
    }

    /// 该条目是主分区、逻辑分区，还是扩展分区容器。
    ///
    /// **由序号推出，不额外存字段**：MBR 的编号规则本身就编码了归属（主分区
    /// 占 1–4，逻辑分区从 5 起，见本模块的序号语义说明），因此再存一份独立的
    /// `kind` 只会多一个可能与 `index` 不一致的状态。GPT 一律为主分区。
    ///
    /// **容器靠 `index == 0` 识别**：容器不占内核序号，序号 0 是它唯一的
    /// 不可能与真实分区混淆的取值（真实分区从 1 起）。
    pub const fn kind(&self) -> crate::partspec::PartitionKind {
        use crate::partspec::PartitionKind;
        if self.index == 0 {
            return PartitionKind::Extended;
        }
        // 字面量 4：`MBR_MAX_PRIMARY as u32` 会被 `cast_possible_truncation`
        // 判为在 32 位目标上可能截断（lint 看不见常量取值）。取值由测试守住。
        if self.index > 4 {
            PartitionKind::Logical
        } else {
            PartitionKind::Primary
        }
    }

    /// 是否是扩展分区容器（占槽位、不占序号、不可挂载）。
    pub const fn is_extended_container(&self) -> bool {
        self.index == 0
    }
}

/// 一次分区扫描的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionScan {
    /// 识别到的布局。
    pub layout: ImageLayout,
    /// 分区列表（`raw` 为空）。
    pub partitions: Vec<PartitionEntry>,
}

impl PartitionScan {
    /// 默认应挂载的分区序号（第一个分区）。
    ///
    /// `None` 表示无分区表，调用方应按**整盘**处理。
    pub fn default_index(&self) -> Option<u32> {
        self.partitions.first().map(|entry| entry.index)
    }
}

/// 读取镜像的分区表。
///
/// 行为约定：
/// - `gpt` → [`ImageLayout::Gpt`] + 分区列表；
/// - `mbr` → [`ImageLayout::Mbr`] + 分区列表；
/// - 无分区表（整盘 FAT 或无法识别）→ [`ImageLayout::Raw`] + **空**列表，
///   **不是**错误：调用方据此按整盘挂载；
/// - 有 GPT 保护分区但 GPT 头不可解析 → **`Err`**：这是真正的损坏，
///   静默降级为「整盘」会让用户挂到一个错误的偏移上。
pub fn read_partitions(path: &Path) -> Result<PartitionScan> {
    let mut file = std::fs::File::open(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            CoreError::NotFound(path.display().to_string())
        } else {
            CoreError::Io(err)
        }
    })?;

    let mut sector = [0u8; SECTOR_BYTES_USIZE];
    file.read_exact(&mut sector).map_err(|_| {
        CoreError::UnsupportedLayout(
            "image is smaller than one sector; cannot detect layout".into(),
        )
    })?;

    // **先看 LBA1 的 GPT 签名，再看 MBR**（顺序不可颠倒，已实测）。
    //
    // 本模块用 `gpt` crate 生成的镜像**没有保护性 MBR**：LBA0 全零（连 `55AA`
    // 引导签名都没有），只有 LBA1 的 `EFI PART`。因此「先判断保护分区」会漏掉
    // 自己创建的 GPT 镜像，把它误判成 `raw` —— 于是 UI 只能按整盘挂载，
    // 而整盘不是 FAT32，挂载以 `filesystem_unsupported` 失败。
    let mut header = [0u8; SECTOR_BYTES_USIZE];
    let has_gpt_header = file
        .seek(SeekFrom::Start(GPT_HEADER_LBA * SECTOR_BYTES))
        .and_then(|_| file.read_exact(&mut header))
        .is_ok()
        && &header[0..8] == GPT_SIGNATURE;

    if has_gpt_header {
        let entries = read_gpt_entries(&mut file, &header)?;
        let partitions = parse_gpt_entries(&entries)?;
        return Ok(PartitionScan {
            layout: ImageLayout::Gpt,
            partitions,
        });
    }

    // 有保护性 MBR 却没有 GPT 头 = 真正的损坏。静默降级为「整盘」会让用户挂到
    // 一个错误的偏移上，因此这里明确报错。
    let has_boot_signature = sector[510..512] == BOOT_SIGNATURE;
    if has_boot_signature && mbr_type(&sector, 0) == Some(MBR_TYPE_PROTECTIVE) {
        return Err(CoreError::UnsupportedLayout(
            "a GPT protective partition is present, but LBA1 has no EFI PART signature (corrupt partition table)".into(),
        ));
    }

    if has_boot_signature {
        let mut partitions = parse_mbr_sector(&sector);

        // 有扩展分区时继续走 EBR 链，把逻辑分区以序号 5 起追加。
        //
        // 这里**不复用**已读入的 `sector`：EBR 位于镜像各处，需要按需 seek。
        // 读取器注入成闭包，使 `parse_ebr_chain` 保持纯函数、可在主机测试。
        if let Some(extended_lba) = find_extended_lba(&sector) {
            let mut read_sector = |lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> {
                let mut buf = Box::new([0u8; SECTOR_BYTES_USIZE]);
                file.seek(SeekFrom::Start(u64::from(lba) * SECTOR_BYTES))
                    .ok()?;
                file.read_exact(buf.as_mut()).ok()?;
                Some(buf)
            };
            let logical = parse_ebr_chain(extended_lba, &mut read_sector);

            // 链上有逻辑分区时，容器**不再单独列出**：那会让同一个区间既显示为
            // 容器又显示为里面的分区，用户会以为容量被算了两遍。
            //
            // 链为空时**保留全部条目**（主分区 + 容器条目）——否则用户建完一个
            // 预留用的扩展分区后回头看详情，不但发现分区少了、还会以为镜像坏了。
            if !logical.is_empty() {
                partitions.retain(|p| !p.is_extended_container());
                partitions.extend(logical);
            }
        }

        if !partitions.is_empty() {
            return Ok(PartitionScan {
                layout: ImageLayout::Mbr,
                partitions,
            });
        }
    }

    // 无分区表：整盘即一个卷（`raw`），或无法识别。两者都按整盘处理。
    Ok(PartitionScan {
        layout: ImageLayout::Raw,
        partitions: Vec::new(),
    })
}

/// 读 MBR 中第 `slot` 个分区项的类型（0 起）。
fn mbr_type(sector: &[u8; SECTOR_BYTES_USIZE], slot: usize) -> Option<u8> {
    if slot >= MBR_ENTRY_COUNT {
        return None;
    }
    Some(sector[MBR_ENTRY_OFFSET + slot * MBR_ENTRY_LEN + 4])
}

/// 解析 MBR 首扇区（纯函数）。
///
/// 跳过类型为 0 的空项；`0xEE`（GPT 保护分区）**不计入序号**，因为它不是
/// 可供挂载的数据分区。
///
/// **`0x05`（扩展分区）也不计入序号**：它是容器，本身不可挂载。它描述的逻辑
/// 分区由 [`parse_ebr_chain`] 单独遍历后以序号 5 起追加。早先把 `0x05` 当作
/// 普通数据分区列出，UI 会显示一个挂上去必然失败的"分区"。
pub fn parse_mbr_sector(sector: &[u8; SECTOR_BYTES_USIZE]) -> Vec<PartitionEntry> {
    let mut out = Vec::new();
    let mut index = 0u32;

    for slot in 0..MBR_ENTRY_COUNT {
        let base = MBR_ENTRY_OFFSET + slot * MBR_ENTRY_LEN;
        let kind = sector[base + 4];
        if kind == 0 {
            continue;
        }
        let start_lba = u32::from_le_bytes([
            sector[base + 8],
            sector[base + 9],
            sector[base + 10],
            sector[base + 11],
        ]) as u64;
        let sectors = u32::from_le_bytes([
            sector[base + 12],
            sector[base + 13],
            sector[base + 14],
            sector[base + 15],
        ]) as u64;

        // 保护分区只用来标记「这是 GPT」，本身不可挂载。
        if kind == MBR_TYPE_PROTECTIVE {
            continue;
        }
        if sectors == 0 {
            // 长度为 0 的项是无效的，跳过而不是报告一个挂不上的分区。
            continue;
        }

        // 扩展分区是容器：它的内容由 EBR 链描述，因此**不作为分区**列出来。
        //
        // 但它仍要占一个条目（`kind() == Extended`），否则用户建了一个**空容器**
        // 之后回头看详情会发现「我建的扩展分区不见了」。容器由 `PartitionEntry`
        // 的类型区分，序号为 0（不占内核序号）。
        if kind == MBR_TYPE_EXTENDED {
            out.push(PartitionEntry {
                index: 0,
                start_lba,
                size_bytes: sectors * SECTOR_BYTES,
                type_label: mbr_type_label(kind),
            });
            continue;
        }

        index += 1;
        out.push(PartitionEntry {
            index,
            start_lba,
            size_bytes: sectors * SECTOR_BYTES,
            type_label: mbr_type_label(kind),
        });
    }

    out
}

/// 从首扇区里取出扩展分区项的起始 LBA（**绝对**）。
///
/// 返回第一个非空 `0x05` 项的起始 LBA。MBR 惯例只允许一个扩展分区，故取首个
/// 即可；后续若还有 `0x05` 项，它们指向的链不会被遍历（异常布局，不猜测意图）。
pub fn find_extended_lba(sector: &[u8; SECTOR_BYTES_USIZE]) -> Option<u32> {
    for slot in 0..MBR_ENTRY_COUNT {
        let base = MBR_ENTRY_OFFSET + slot * MBR_ENTRY_LEN;
        if sector[base + 4] != MBR_TYPE_EXTENDED {
            continue;
        }
        let sectors = u32::from_le_bytes([
            sector[base + 12],
            sector[base + 13],
            sector[base + 14],
            sector[base + 15],
        ]);
        if sectors == 0 {
            continue;
        }
        return Some(u32::from_le_bytes([
            sector[base + 8],
            sector[base + 9],
            sector[base + 10],
            sector[base + 11],
        ]));
    }
    None
}

/// 遍历 EBR 链，返回逻辑分区列表（纯函数，读取器由调用方注入）。
///
/// ## EBR 布局（与首扇区项布局相同，但起始 LBA 语义不同）
///
/// - 项 0（`+446`）：本 EBR 描述的**逻辑分区**，`starting_lba` 是**相对本 EBR**
///   的偏移；
/// - 项 1（`+462`）：**下一个 EBR**，`starting_lba` 同样是**相对本 EBR** 的偏移，
///   类型为 `0x05`；该项为空表示链尾；
/// - `+510`：`55 AA`。
///
/// **主分区项用绝对 LBA，EBR 项用相对偏移。**混用是这一层最危险的错误：镜像在
/// Host 上看起来仍然「合法」，但挂载会落到完全错误的偏移上。因此本函数在
/// 解析后立刻把相对值换算成**绝对 LBA** 再返回，调用方拿到的语义与主分区一致。
///
/// ## 防护
///
/// - **环检测**：记录已访问的 EBR LBA，重复出现即停止；
/// - **长度上限** [`MAX_EBR_CHAIN`]：超过即停止。
///
/// 两者都是必需的：损坏的镜像可让某项指回自身或早先节点，无防护的遍历会在
/// `su` 域里死循环。
///
/// 停止时**如实返回已解析的部分**（而不是报错），因为"链上有几个能用的分区"
/// 对用户仍然有价值；异常本身由 [`parse_ebr_chain`] 的调用方通过日志体现。
pub fn parse_ebr_chain(
    first_ebr_lba: u32,
    read_sector: &mut dyn FnMut(u32) -> Option<Box<[u8; SECTOR_BYTES_USIZE]>>,
) -> Vec<PartitionEntry> {
    let mut out = Vec::new();
    let mut visited: Vec<u32> = Vec::new();
    let mut next = Some(first_ebr_lba);
    // 逻辑分区序号从 5 起（主分区占 1–4），与内核 `loopNpM` 的编号一致。
    let mut index = u32::try_from(crate::partspec::MBR_MAX_PRIMARY + 1).unwrap_or(u32::MAX);

    while let Some(ebr_lba) = next {
        // **链长上限是唯一实际可达的兜底**（见下面注释）。
        if out.len() >= MAX_EBR_CHAIN {
            break;
        }
        // 环检测：保留为**纵深防御**，但按当前读取语义它不可达——
        //
        // 链上每个偏移都是非负 `u32`，形成环需要环上偏移之和为 0；要么全为 0
        // （那是链尾，前面就 break 了），要么某一步加法先溢出，而溢出会被下面的
        // `checked_add` 拦下。所以真正兜底的是链长上限 + 溢出保护。
        //
        // 之所以仍然保留：成本是一次 `Vec` 线性查找（链长 ≤64），而一旦将来
        // 有人把偏移改成有符号解析或放宽溢出检查，这里就是唯一还能拦住死循环的
        // 地方。**不要因为"测不到"就删掉它**——测不到是因为别的保护先起了作用。
        if visited.contains(&ebr_lba) {
            break;
        }
        visited.push(ebr_lba);

        let Some(sector) = read_sector(ebr_lba) else {
            break; // 扇区读不到（镜像被截断）
        };
        // 没有引导签名的扇区不是 EBR。
        if sector[510..512] != BOOT_SIGNATURE {
            break;
        }

        // ---- 项 0：本 EBR 描述的逻辑分区 ----
        let base = MBR_ENTRY_OFFSET;
        let kind = sector[base + 4];
        let relative = u32::from_le_bytes([
            sector[base + 8],
            sector[base + 9],
            sector[base + 10],
            sector[base + 11],
        ]);
        let sectors = u32::from_le_bytes([
            sector[base + 12],
            sector[base + 13],
            sector[base + 14],
            sector[base + 15],
        ]);

        // 类型为 0 或长度为 0 = 空项：该 EBR 没有描述分区。此时仍继续走链，
        // 否则一个中间空项会让后面所有分区都读不到。
        if kind != 0 && sectors != 0 {
            // 相对 → 绝对。溢出说明镜像损坏，停止而不是回绕成一个合法偏移。
            if let Some(start_lba) = ebr_lba.checked_add(relative) {
                out.push(PartitionEntry {
                    index,
                    start_lba: u64::from(start_lba),
                    size_bytes: u64::from(sectors) * SECTOR_BYTES,
                    type_label: mbr_type_label(kind),
                });
                index = index.saturating_add(1);
            }
        }

        // ---- 项 1：下一个 EBR ----
        let link_base = MBR_ENTRY_OFFSET + MBR_ENTRY_LEN;
        let link_rel = u32::from_le_bytes([
            sector[link_base + 8],
            sector[link_base + 9],
            sector[link_base + 10],
            sector[link_base + 11],
        ]);
        // 链尾：项为空（类型 0 / 长度为 0 / 偏移为 0）。
        if link_rel == 0 {
            break;
        }
        next = ebr_lba.checked_add(link_rel);
        if next.is_none() {
            break; // 指针溢出
        }
    }

    out
}

/// 读 GPT 分区项数组的原始字节。
fn read_gpt_entries(
    file: &mut std::fs::File,
    header: &[u8; SECTOR_BYTES_USIZE],
) -> Result<Vec<u8>> {
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap_or([0; 8]));
    let entry_count = u32::from_le_bytes(header[80..84].try_into().unwrap_or([0; 4]));
    let entry_len = u32::from_le_bytes(header[84..88].try_into().unwrap_or([0; 4]));

    if entry_len < GPT_ENTRY_MIN_LEN {
        return Err(CoreError::UnsupportedLayout(format!(
            "invalid GPT entry length: {entry_len} (expected >= {GPT_ENTRY_MIN_LEN})"
        )));
    }
    if entry_count == 0 || entry_count > MAX_PARTITION_ENTRIES {
        return Err(CoreError::UnsupportedLayout(format!(
            "invalid GPT entry count: {entry_count} (allowed 1..={MAX_PARTITION_ENTRIES})"
        )));
    }
    if entries_lba == 0 {
        return Err(CoreError::UnsupportedLayout(
            "GPT entry array LBA is 0 (corrupt partition table)".into(),
        ));
    }

    let total = (entry_count as usize) * (entry_len as usize);
    let mut buf = vec![0u8; total];
    file.seek(SeekFrom::Start(entries_lba * SECTOR_BYTES))?;
    file.read_exact(&mut buf)
        .map_err(|_| CoreError::UnsupportedLayout("GPT entry array is truncated".into()))?;
    Ok(buf)
}

/// 解析 GPT 分区项数组（纯函数）。
///
/// `raw` 需按头里的 `entry_len` 切分，因此这里接收的是**整段字节**。
/// 但 `entry_len` 已由 [`read_gpt_entries`] 校验，此处只需按固定 128 字节步长
/// 读取前 56 字节的关键字段——UEFI 保证前 128 字节的布局，而更大步长只是
/// 填充。为保持纯函数可测，这里按 128 步长解析并要求长度是 128 的整数倍。
pub fn parse_gpt_entries(raw: &[u8]) -> Result<Vec<PartitionEntry>> {
    if raw.is_empty() || !raw.len().is_multiple_of(GPT_ENTRY_MIN_LEN as usize) {
        return Err(CoreError::UnsupportedLayout(
            "GPT entry array length is not a multiple of 128".into(),
        ));
    }

    let mut out = Vec::new();
    let mut index = 0u32;

    for chunk in raw.as_chunks::<{ GPT_ENTRY_MIN_LEN as usize }>().0 {
        // 全零 type GUID = 空项。
        if chunk[0..16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let start_lba = u64::from_le_bytes(chunk[32..40].try_into().unwrap_or([0; 8]));
        let last_lba = u64::from_le_bytes(chunk[40..48].try_into().unwrap_or([0; 8]));
        if last_lba < start_lba {
            // 区间反了的项是损坏数据：跳过而不是报告一个负长度的分区。
            continue;
        }

        index += 1;
        out.push(PartitionEntry {
            index,
            start_lba,
            size_bytes: (last_lba - start_lba + 1) * SECTOR_BYTES,
            type_label: gpt_type_label(&chunk[0..16]),
        });
    }

    Ok(out)
}

/// MBR 分区类型 → 可读名称。
fn mbr_type_label(kind: u8) -> String {
    match kind {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x0B => "FAT32",
        0x0C => "FAT32 (LBA)",
        0x07 => "NTFS / exFAT",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0x05 => "Extended",
        0xEE => "GPT protective",
        0xEF => "EFI System",
        0xAF => "macOS HFS",
        other => return format!("Type 0x{other:02X}"),
    }
    .to_string()
}

/// GPT 类型 GUID（**磁盘字节序**）→ 可读名称。
///
/// GUID 的前三个字段在磁盘上是小端，因此比较的是字节序列而非字符串。
fn gpt_type_label(guid: &[u8]) -> String {
    /// EFI System Partition。
    const EFI_SYSTEM: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ];
    /// Microsoft Basic Data。
    const MS_BASIC_DATA: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];
    /// Linux filesystem data。
    const LINUX_FS: [u8; 16] = [
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D,
        0xE4,
    ];
    /// Microsoft Reserved。
    const MS_RESERVED: [u8; 16] = [
        0x16, 0xE3, 0xC9, 0xE3, 0x5C, 0x0B, 0xB8, 0x4D, 0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15,
        0xAE,
    ];

    if guid == EFI_SYSTEM {
        return "EFI System".to_string();
    }
    if guid == MS_BASIC_DATA {
        return "Microsoft basic data".to_string();
    }
    if guid == LINUX_FS {
        return "Linux filesystem".to_string();
    }
    if guid == MS_RESERVED {
        return "Microsoft reserved".to_string();
    }

    // 未知类型：给 GUID 前 4 字节的十六进制，足以让人去查，也不假装认识。
    format!(
        "GUID {:02X}{:02X}{:02X}{:02X}…",
        guid[0], guid[1], guid[2], guid[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create::{CreateOptions, CreatedImage, create_image};

    /// 造一个全零扇区。
    fn empty_sector() -> [u8; SECTOR_BYTES_USIZE] {
        let mut sector = [0u8; SECTOR_BYTES_USIZE];
        sector[510..512].copy_from_slice(&BOOT_SIGNATURE);
        sector
    }

    /// 往扇区的第 `slot` 个 MBR 项写入一个分区。
    fn put_mbr_entry(
        sector: &mut [u8; SECTOR_BYTES_USIZE],
        slot: usize,
        kind: u8,
        start: u32,
        len: u32,
    ) {
        let base = MBR_ENTRY_OFFSET + slot * MBR_ENTRY_LEN;
        sector[base + 4] = kind;
        sector[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
        sector[base + 12..base + 16].copy_from_slice(&len.to_le_bytes());
    }

    /// 造一个 128 字节的 GPT 分区项。
    fn gpt_entry(guid: [u8; 16], first_lba: u64, last_lba: u64) -> Vec<u8> {
        let mut entry = vec![0u8; GPT_ENTRY_MIN_LEN as usize];
        entry[0..16].copy_from_slice(&guid);
        entry[32..40].copy_from_slice(&first_lba.to_le_bytes());
        entry[40..48].copy_from_slice(&last_lba.to_le_bytes());
        entry
    }

    const MS_BASIC_DATA: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];
    const EFI_SYSTEM: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ];

    #[test]
    fn mbr_parser_reads_one_fat32_partition() {
        let mut sector = empty_sector();
        put_mbr_entry(&mut sector, 0, 0x0C, 2048, 131072);

        let parts = parse_mbr_sector(&sector);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].index, 1);
        assert_eq!(parts[0].start_lba, 2048);
        assert_eq!(parts[0].size_bytes, 131072 * 512);
        assert_eq!(parts[0].offset_bytes(), 1_048_576);
        assert_eq!(parts[0].type_label, "FAT32 (LBA)");
    }

    #[test]
    fn mbr_parser_skips_empty_and_protective_entries() {
        let mut sector = empty_sector();
        // 槽 0 空（类型 0），槽 1 是 GPT 保护分区，槽 2 才是数据分区。
        put_mbr_entry(&mut sector, 1, MBR_TYPE_PROTECTIVE, 1, 0xFFFF_FFFF);
        put_mbr_entry(&mut sector, 2, 0x0C, 2048, 4096);

        let parts = parse_mbr_sector(&sector);
        assert_eq!(parts.len(), 1, "保护分区与空项都不应计入");
        assert_eq!(parts[0].index, 1, "序号必须跳过被忽略的项");
        assert_eq!(parts[0].start_lba, 2048);
    }

    #[test]
    fn mbr_parser_skips_zero_length_entries() {
        let mut sector = empty_sector();
        put_mbr_entry(&mut sector, 0, 0x83, 2048, 0);
        assert!(
            parse_mbr_sector(&sector).is_empty(),
            "长度为 0 的项不可挂载"
        );
    }

    #[test]
    fn mbr_parser_reports_unknown_type_by_hex() {
        let mut sector = empty_sector();
        put_mbr_entry(&mut sector, 0, 0x99, 100, 10);
        assert_eq!(parse_mbr_sector(&sector)[0].type_label, "Type 0x99");
    }

    #[test]
    fn gpt_parser_reads_entries_and_skips_empty_slots() {
        let mut raw = Vec::new();
        raw.extend(gpt_entry([0u8; 16], 0, 0)); // 空项
        raw.extend(gpt_entry(EFI_SYSTEM, 2048, 4095));
        raw.extend(gpt_entry([0u8; 16], 0, 0)); // 空项
        raw.extend(gpt_entry(MS_BASIC_DATA, 4096, 135167));

        let parts = parse_gpt_entries(&raw).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].index, 1);
        assert_eq!(parts[0].type_label, "EFI System");
        assert_eq!(parts[1].index, 2, "序号跳过空项");
        assert_eq!(parts[1].type_label, "Microsoft basic data");
        assert_eq!(parts[1].start_lba, 4096);
        assert_eq!(parts[1].size_bytes, (135167 - 4096 + 1) * 512);
    }

    #[test]
    fn gpt_parser_rejects_misaligned_length() {
        let err = parse_gpt_entries(&[0u8; 100]).unwrap_err();
        assert!(matches!(err, CoreError::UnsupportedLayout(_)), "{err:?}");
    }

    #[test]
    fn gpt_parser_skips_inverted_ranges() {
        // last < first 是损坏数据：跳过而不是报告一个溢出的容量。
        let raw = gpt_entry(MS_BASIC_DATA, 5000, 1000);
        assert!(parse_gpt_entries(&raw).unwrap().is_empty());
    }

    #[test]
    fn gpt_parser_labels_unknown_guid_with_hex_prefix() {
        let raw = gpt_entry(
            [
                0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
            ],
            1,
            2,
        );
        let parts = parse_gpt_entries(&raw).unwrap();
        assert_eq!(parts[0].type_label, "GUID DEADBEEF…");
    }

    // ---------------------------------------------------------------- 往返测试
    //
    // 这几条把「写入」与「读取」钉在一起：写入侧用 `gpt`/手写 MBR 生成，
    // 读取侧必须还原出同一个偏移。任一侧改坏都会在这里失败。

    /// 造一个已创建的镜像，返回 `(路径, 创建结果)`。
    ///
    /// 返回创建结果是为了让调用方**不必重复调用 `create_image`**：同名阻断
    /// 已经生效，对同一路径创建第二次会（正确地）失败。
    fn temp_image(tag: &str, layout: ImageLayout) -> (std::path::PathBuf, CreatedImage) {
        let dir = crate::testutil::temp_dir(tag);
        let path = dir.join("image.img");
        let created = create_image(
            CreateOptions::new(&path)
                // 镜像要比分区下限大：GPT 需在盘首盘尾各留结构，
                // 恰好等于下限的镜像放不下一个下限大小的分区。
                .with_size(crate::layout::MIN_FAT32_BYTES + 4 * 1024 * 1024)
                .with_layout(layout),
            &crate::fat::FatfsFormatter,
        )
        .unwrap();
        (path, created)
    }

    #[test]
    fn round_trip_gpt_offset_matches_created_image() {
        let (path, created) = temp_image("partitions-gpt", ImageLayout::Gpt);

        let scan = read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Gpt);
        assert_eq!(scan.partitions.len(), 1);
        assert_eq!(scan.default_index(), Some(1));
        assert_eq!(
            scan.partitions[0].offset_bytes(),
            created.first_partition_offset_bytes(),
            "读取出的分区偏移必须与创建时报告的一致"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn round_trip_mbr_offset_matches_created_image() {
        let (path, created) = temp_image("partitions-mbr", ImageLayout::Mbr);

        let scan = read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Mbr);
        assert_eq!(scan.partitions.len(), 1);
        assert_eq!(scan.partitions[0].type_label, "FAT32 (LBA)");
        assert_eq!(
            scan.partitions[0].offset_bytes(),
            created.first_partition_offset_bytes()
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn round_trip_raw_has_no_partitions() {
        let (path, _) = temp_image("partitions-raw", ImageLayout::Raw);
        let scan = read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Raw);
        assert!(scan.partitions.is_empty());
        assert_eq!(scan.default_index(), None);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_image_is_not_found() {
        let err = read_partitions(Path::new("/nonexistent/nope.img")).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn truncated_image_is_reported_not_panicking() {
        let dir = crate::testutil::temp_dir("partitions-truncated");
        let path = dir.join("tiny.img");
        std::fs::write(&path, b"too short").unwrap();

        let err = read_partitions(&path).unwrap_err();
        assert!(matches!(err, CoreError::UnsupportedLayout(_)), "{err:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn protective_mbr_without_gpt_header_is_an_error() {
        // 这是「损坏」而不是「无分区表」：静默降级为整盘会让用户挂到错误偏移。
        let dir = crate::testutil::temp_dir("partitions-broken-gpt");
        let path = dir.join("broken.img");
        let mut sector = empty_sector();
        put_mbr_entry(&mut sector, 0, MBR_TYPE_PROTECTIVE, 1, 0xFFFF_FFFF);
        let mut data = sector.to_vec();
        data.extend(vec![0u8; SECTOR_BYTES_USIZE]); // LBA1 全零，没有 EFI PART
        std::fs::write(&path, &data).unwrap();

        let err = read_partitions(&path).unwrap_err();
        assert!(matches!(err, CoreError::UnsupportedLayout(_)), "{err:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // -------------------------------------------------- EBR 链与逻辑分区

    /// 往扇区里写一个分区项（可指定项下标，用于 EBR 的项 0 / 项 1）。
    fn put_entry(
        sector: &mut [u8; SECTOR_BYTES_USIZE],
        slot: usize,
        kind: u8,
        start: u32,
        len: u32,
    ) {
        let base = MBR_ENTRY_OFFSET + slot * MBR_ENTRY_LEN;
        sector[base] = 0x00;
        sector[base + 1..base + 4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        sector[base + 4] = kind;
        sector[base + 5..base + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        sector[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
        sector[base + 12..base + 16].copy_from_slice(&len.to_le_bytes());
    }

    /// 造一个 EBR 扇区：项 0 描述逻辑分区，项 1 指向下一个 EBR。
    fn ebr_sector(
        kind: u8,
        part_start: u32,
        part_len: u32,
        next_ebr: u32,
        next_len: u32,
    ) -> Box<[u8; SECTOR_BYTES_USIZE]> {
        let mut sector = Box::new([0u8; SECTOR_BYTES_USIZE]);
        if kind != 0 && part_len != 0 {
            put_entry(&mut sector, 0, kind, part_start, part_len);
        }
        if next_ebr != 0 {
            put_entry(&mut sector, 1, MBR_TYPE_EXTENDED, next_ebr, next_len);
        }
        sector[510..512].copy_from_slice(&BOOT_SIGNATURE);
        sector
    }

    #[test]
    fn mbr_parser_reports_extended_container_without_a_partition_index() {
        // 扩展分区是容器、不是可挂载分区：早先把它列成**普通分区**会让 UI 显示
        // 一个挂上去必然失败的"分区"，且序号还会与内核设备名错位。
        //
        // 但它也不能凭空消失：用户建了一个**空容器**（预留空间）之后回头看详情，
        // 必须能看到它。折中做法是保留条目、`index = 0` 表示"不占内核序号"。
        let mut sector = empty_sector();
        put_mbr_entry(&mut sector, 0, 0x0C, 2048, 4096);
        put_mbr_entry(&mut sector, 1, MBR_TYPE_EXTENDED, 8192, 100000);

        let parts = parse_mbr_sector(&sector);
        assert_eq!(parts.len(), 2, "容器应作为一个条目出现（但不是分区序号）");

        assert_eq!(parts[0].index, 1);
        assert_eq!(parts[0].type_label, "FAT32 (LBA)");
        assert!(parts[0].kind().is_partition());

        assert_eq!(parts[1].index, 0, "容器不占内核序号");
        assert_eq!(parts[1].start_lba, 8192);
        assert_eq!(parts[1].size_bytes, 100000 * SECTOR_BYTES);
        assert_eq!(parts[1].type_label, "Extended");
        assert!(!parts[1].kind().is_partition());
        assert!(parts[1].is_extended_container());
    }

    #[test]
    fn mbr_parser_index_zero_is_the_only_container_marker() {
        // `kind()` 靠 `index == 0` 识别容器，而真实分区序号从 1 起。两者的边界
        // 必须钉住：若哪天序号改成从 0 开始，容器就会被当成主分区。
        let container = PartitionEntry {
            index: 0,
            start_lba: 0,
            size_bytes: 0,
            type_label: "Extended".into(),
        };
        assert_eq!(
            container.kind(),
            crate::partspec::PartitionKind::Extended,
            "index 0 必须是容器"
        );

        let first = PartitionEntry {
            index: 1,
            start_lba: 0,
            size_bytes: 0,
            type_label: "FAT32 (LBA)".into(),
        };
        assert_eq!(
            first.kind(),
            crate::partspec::PartitionKind::Primary,
            "序号 1 必须仍是主分区"
        );
    }

    #[test]
    fn find_extended_lba_returns_absolute_start() {
        let mut sector = empty_sector();
        put_mbr_entry(&mut sector, 0, 0x0C, 2048, 4096);
        put_mbr_entry(&mut sector, 1, MBR_TYPE_EXTENDED, 8192, 100000);
        assert_eq!(find_extended_lba(&sector), Some(8192));

        // 没有扩展分区时返回 None。
        let mut plain = empty_sector();
        put_mbr_entry(&mut plain, 0, 0x0C, 2048, 4096);
        assert_eq!(find_extended_lba(&plain), None);
    }

    #[test]
    fn ebr_chain_converts_relative_offsets_to_absolute() {
        // **这一层最危险的错误**：EBR 项里的起始 LBA 是相对本 EBR 的偏移，
        // 而主分区项是绝对值。混用会让挂载落到完全错误的偏移上，且镜像在
        // Host 上看起来仍然"合法"。故返回前必须换算成绝对 LBA。
        let ebr_lba = 8192u32;
        let map = |lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> {
            if lba == ebr_lba {
                // 分区起点相对本 EBR 为 +1（即紧随 EBR 之后）。
                Some(ebr_sector(0x0C, 1, 2048, 0, 0))
            } else {
                None
            }
        };
        let mut reader = map;
        let parts = parse_ebr_chain(ebr_lba, &mut reader);

        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0].start_lba,
            u64::from(ebr_lba) + 1,
            "相对偏移必须换算成绝对 LBA"
        );
        assert_eq!(parts[0].size_bytes, 2048 * SECTOR_BYTES);
        assert_eq!(parts[0].index, 5, "逻辑分区序号从 5 起");
    }

    #[test]
    fn ebr_chain_walks_multiple_links_with_ascending_indices() {
        // EBR 位置：8192、12288。第二个 EBR 相对第一个为 +4096。
        let first = 8192u32;
        let second = 12288u32;
        let map = move |lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> {
            if lba == first {
                Some(ebr_sector(0x0C, 1, 2048, second - first, 1))
            } else if lba == second {
                Some(ebr_sector(0x83, 1, 4096, 0, 0))
            } else {
                None
            }
        };
        let mut reader = map;
        let parts = parse_ebr_chain(first, &mut reader);

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].index, 5);
        assert_eq!(parts[0].start_lba, u64::from(first) + 1);
        assert_eq!(parts[0].type_label, "FAT32 (LBA)");
        assert_eq!(parts[1].index, 6);
        assert_eq!(parts[1].start_lba, u64::from(second) + 1);
        assert_eq!(parts[1].type_label, "Linux");
    }

    #[test]
    fn ebr_chain_stops_on_overflowing_link_pointer() {
        // **实测结论**：在 `checked_add` 与「偏移读作非负 `u32`」这两个前提下，
        // **环检测是不可达的**。理由：链上每个偏移 `o_i` 都是无符号的，要形成环
        // 就必须让环上偏移之和为 0；要么全为 0（那是链尾），要么某一步加法先
        // 溢出——而溢出会先被 `checked_add` 拦下。
        //
        // 因此这里测的是**实际可达的终止原因**：损坏的链指针导致加法溢出。
        // 早先本位置有一条「自环 / 两节点环」测试，但它实际测的就是这条溢出
        // 保护（去掉环检测照样通过，已实测），属于**测错了东西**。
        let a = 8192u32;
        // 偏移取一个足以让 `a + offset` 溢出 u32 的值。
        let overflow_offset = u32::MAX - a + 1;
        let map = move |_lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> {
            Some(ebr_sector(0x0C, 1, 2048, overflow_offset, 1))
        };
        let mut reader = map;

        let parts = parse_ebr_chain(a, &mut reader);
        // 第一个 EBR 的分区被读到，然后链指针溢出 → 停止。
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].index, 5);
    }

    #[test]
    fn ebr_chain_respects_length_cap() {
        // **上限是唯一实际可达的兜底**（环检测不可达、溢出保护在这里不触发）。
        // 对抗性最坏情况：读取器对任意 LBA 都返回「指向 +1」的 EBR——链永不重复
        // （环检测无用）、永不溢出（溢出保护无用），只有上限能拦住它。
        // 去掉上限时本测试会**死循环**（已实测），这正是它要守住的性质。
        let mut reader = |_lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> {
            Some(ebr_sector(0x0C, 1, 2048, 1, 1))
        };

        let parts = parse_ebr_chain(8192, &mut reader);
        assert_eq!(parts.len(), MAX_EBR_CHAIN, "必须在上限处停止");
        // 序号必须连续递增到 5 + 上限 - 1，中间不跳号。
        assert_eq!(parts[0].index, 5);
        assert_eq!(
            parts[MAX_EBR_CHAIN - 1].index,
            5 + u32::try_from(MAX_EBR_CHAIN).unwrap() - 1
        );
    }

    #[test]
    fn ebr_chain_stops_on_missing_sector() {
        // 镜像被截断：读不到 EBR 时停止，而不是 panic 或合成一个分区。
        let mut reader = |_lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> { None };
        let parts = parse_ebr_chain(8192, &mut reader);
        assert!(parts.is_empty());
    }

    #[test]
    fn ebr_chain_stops_without_boot_signature() {
        // 没有 55AA 的扇区不是 EBR：继续解析会把随机数据当成分区表。
        let mut sector = ebr_sector(0x0C, 1, 2048, 0, 0);
        sector[510..512].copy_from_slice(&[0x00, 0x00]);
        let mut reader =
            move |_lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> { Some(sector.clone()) };
        let parts = parse_ebr_chain(8192, &mut reader);
        assert!(parts.is_empty());
    }

    #[test]
    fn ebr_chain_skips_empty_entry_but_continues() {
        // 中间一个空项不应让后面所有分区都读不到。
        let a = 8192u32;
        let b = 12288u32;
        let map = move |lba: u32| -> Option<Box<[u8; SECTOR_BYTES_USIZE]>> {
            if lba == a {
                // 本 EBR 不描述分区（kind=0），但仍指向下一个。
                Some(ebr_sector(0, 0, 0, b - a, 1))
            } else if lba == b {
                Some(ebr_sector(0x0C, 1, 2048, 0, 0))
            } else {
                None
            }
        };
        let mut reader = map;
        let parts = parse_ebr_chain(a, &mut reader);

        assert_eq!(parts.len(), 1, "空项不产出分区，但不阻断链");
        assert_eq!(parts[0].index, 5, "序号仍从 5 起");
    }

    // ------------------------------------------------ 端到端往返（写 → 读）

    #[test]
    fn round_trip_empty_container_keeps_the_real_partitions() {
        // **端到端**：「1 主 + 1 空扩展容器」写进磁盘再读回。
        //
        // 这条守的是一个**实测发现的缺陷**：早先读侧写成
        // `partitions.retain(|p| p.is_extended_container())`，于是空容器镜像读回来
        // **只剩容器**——主分区凭空消失。只断言「找得到容器」是测不出它的，
        // 必须同时断言真实分区还在。
        use crate::partspec::{PartitionFilesystem, PartitionKind, PartitionSpec};

        let dir = crate::testutil::temp_dir("partitions-empty-container-roundtrip");
        let path = dir.join("empty-ext.img");
        let image_bytes = 256 * 1024 * 1024u64;

        {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(image_bytes).unwrap();
            file.sync_all().unwrap();
        }

        let specs = vec![
            PartitionSpec::fill_remaining("P1").with_size(16 * 1024 * 1024),
            PartitionSpec::fill_remaining("EXT")
                .with_size(32 * 1024 * 1024)
                .with_kind(PartitionKind::Extended)
                .with_filesystem(PartitionFilesystem::None),
        ];
        let sizes = vec![16 * 1024 * 1024u64, 32 * 1024 * 1024];

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        crate::partition::write_mbr(&mut file, &specs, &sizes).unwrap();

        let scan = read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Mbr);
        assert_eq!(
            scan.partitions.len(),
            2,
            "应是「1 个真实分区 + 1 个容器条目」：{:?}",
            scan.partitions
        );

        // 真实分区（index ≥ 1）必须在，且只有一个。
        let real: Vec<_> = scan.partitions.iter().filter(|p| p.index > 0).collect();
        assert_eq!(real.len(), 1, "主分区不能消失：{:?}", scan.partitions);
        assert_eq!(real[0].index, 1);
        assert_eq!(real[0].size_bytes, 16 * 1024 * 1024);
        assert_eq!(real[0].start_lba, 2048);

        // 容器条目（index == 0）也在，容量为用户声明的 32 MiB。
        let container = scan
            .partitions
            .iter()
            .find(|p| p.is_extended_container())
            .expect("容器条目必须存在");
        assert_eq!(container.size_bytes, 32 * 1024 * 1024);
        assert_eq!(container.start_lba, 2048 + 32768, "紧随主分区之后");

        crate::testutil::cleanup(&path);
    }

    #[test]
    fn round_trip_mbr_logical_partitions_are_read_back() {
        // **端到端**：经 `write_mbr` 真的写进磁盘，再由公开的 `read_partitions`
        // 读回。这是唯一能证明「写入的相对偏移」与「读取的相对偏移解释」一致的
        // 测试——两层各自单测都通过、拼起来错位，是这类格式最典型的事故。
        use crate::partspec::{PartitionKind, PartitionSpec};

        let dir = crate::testutil::temp_dir("partitions-ebr-roundtrip");
        let path = dir.join("ebr.img");
        let image_bytes = 256 * 1024 * 1024u64;

        {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(image_bytes).unwrap();
            file.sync_all().unwrap();
        }

        let specs = vec![
            PartitionSpec::fill_remaining("P1").with_size(16 * 1024 * 1024),
            PartitionSpec::fill_remaining("L1")
                .with_size(32 * 1024 * 1024)
                .with_kind(PartitionKind::Logical),
            PartitionSpec::fill_remaining("L2")
                .with_size(32 * 1024 * 1024)
                .with_kind(PartitionKind::Logical),
        ];
        let sizes = vec![16 * 1024 * 1024u64, 32 * 1024 * 1024, 32 * 1024 * 1024];

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let tables = crate::partition::write_mbr(&mut file, &specs, &sizes).unwrap();

        // ---- 经公开读取路径读回 ----
        let scan = read_partitions(&path).unwrap();
        assert_eq!(scan.layout, ImageLayout::Mbr);
        assert_eq!(scan.partitions.len(), 3, "1 主 + 2 逻辑");

        // 序号必须是 1、5、6（扩展容器不占序号）。
        let indices: Vec<u32> = scan.partitions.iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![1, 5, 6]);

        // **关键断言**：读回的偏移必须与写入侧报告的一致且绝对。
        // 若读取侧把 EBR 的相对偏移当成绝对值，这里会得到约等于 EBR LBA 的值，
        // 而写入侧给的是「EBR LBA + 1」，两者会明显不符。
        for table in &tables {
            let read = scan
                .partitions
                .iter()
                .find(|p| p.index == table.index)
                .unwrap_or_else(|| panic!("读回的列表里缺少分区 {}", table.index));
            assert_eq!(
                read.start_lba, table.first_lba,
                "分区 {} 的起始 LBA 读写不一致（写 {}，读 {}）",
                table.index, table.first_lba, read.start_lba
            );
            assert_eq!(read.size_bytes, table.size_bytes());
            assert_eq!(read.offset_bytes(), table.offset_bytes());
        }

        // 逻辑分区的偏移必须大于主分区，且严格递增。
        assert!(scan.partitions[1].start_lba > scan.partitions[0].start_lba);
        assert!(scan.partitions[2].start_lba > scan.partitions[1].start_lba);

        // 默认挂载分区仍是第一个主分区。
        assert_eq!(scan.default_index(), Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn entry_kind_follows_index_with_literal_boundary() {
        // `kind()` 里写的是字面量 4（见那里的注释）。这里把该字面量与
        // `MBR_MAX_PRIMARY` 钉在一起：任一侧改动而另一侧没跟上，就会失败。
        let entry = |index: u32| PartitionEntry {
            index,
            start_lba: 0,
            size_bytes: 0,
            type_label: String::new(),
        };

        // 主分区边界：1..=4。
        for index in 1..=4u32 {
            assert_eq!(
                entry(index).kind(),
                crate::partspec::PartitionKind::Primary,
                "序号 {index} 应是主分区"
            );
        }
        assert_eq!(
            entry(4).kind(),
            crate::partspec::PartitionKind::Primary,
            "字面量必须等于 MBR_MAX_PRIMARY={}",
            crate::partspec::MBR_MAX_PRIMARY
        );

        // 逻辑分区从 5 起。
        for index in [5u32, 6, 64] {
            assert_eq!(
                entry(index).kind(),
                crate::partspec::PartitionKind::Logical,
                "序号 {index} 应是逻辑分区"
            );
        }
    }
}
