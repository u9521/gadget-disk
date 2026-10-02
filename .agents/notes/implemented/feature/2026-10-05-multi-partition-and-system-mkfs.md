# Agent Note: 多分区创建与系统 mkfs 格式化

Status: implemented

## Problem

格式化必须支持**多分区、每分区独立文件系统**，而这与 `gadgetdisk-core` 的既定边界
直接冲突：core 不接触 configfs、loop 或 Android 特有接口，完全可在主机测试
（见 `crates/gadgetdisk-core/src/lib.rs`），而 `mkfs` 是**外部进程**，必然涉及
`fork/exec`、`PATH` 与设备上装了什么。

**设备上到底有什么，必须实测**（AVD，x86_64 / Android 17.1，`u:r:su:s0`）：

| 文件系统 | 系统工具 | 分区内偏移方式 |
|---|---|---|
| FAT32 | **不存在**（无 `mkfs.vfat`、无 dosfstools、toybox 亦无 `mkfs`；全盘 `find` 无结果） | — |
| exFAT | `/system/bin/mkfs.exfat`（exfatprogs 1.3.2） | **无 offset 选项** |
| ext4 | `/system/bin/mkfs.ext4` → `mke2fs`（e2fsprogs 1.47.2） | `-E offset=<字节>` |

这张表推翻了「直接用系统 `mkfs` 就行」的假设：FAT32 在设备上**根本无工具可用**，
必须自带实现（见
[自带 mkfs.vfat 与统一 loop 格式化](2026-10-06-bundled-mkfs-vfat-and-loop-formatting.md)）；
而两个可用工具的偏移能力又不一致，偏移策略不能散落在调用点。

## Proposal

### 1. 用 trait 把「如何格式化」与平台隔开

`gadgetdisk-core` 定义 `Formatter` trait 与纯逻辑（`fs.rs`：类型名与别名解析、
`FormatPlan`、`needs_loop` 判定），真实 `fork/exec` 放在 CLI 层
（`crates/gadgetdisk-cli/src/mkfs.rs`）。这样 core 的「不接触平台接口」边界保持不变，
而编排逻辑仍可在主机完整测试（用记录型/失败型替身）。

### 2. 三种文件系统的格式化策略

> **本节早期结论已被 [自带 mkfs.vfat 与统一 loop 格式化](2026-10-06-bundled-mkfs-vfat-and-loop-formatting.md)
> 取代。** 当时按各工具的偏移能力分三条路径（FAT32 进程内 `fatfs`、ext4 用
> `-E offset`、exFAT 经 loop）；**当前事实是三种一律经 loop 设备**
> （`losetup -o <offset> --sizelimit <len>`），`FormatPlan::needs_loop` 已删除。

### 3. 多分区模型与布局能力差异

新增 `partspec`（`PartitionSpec` / `PartitionType` + 校验与容量展开），写入器
`write_gpt` / `write_mbr` 改为接受分区列表并返回 `Vec<PartitionTable>`。

布局能力差异**显式建模**而非靠约定：

- MBR **主分区**上限 4（`MBR_MAX_PRIMARY`，首扇区固定 4 项），GPT 128（与读取侧
  `MAX_PARTITION_ENTRIES` 一致——写入侧超过读取侧上界会导致自己创建的镜像读不全）。
  扩展分区、逻辑分区与预留用的空容器在后续改动中补齐，见
  [MBR 扩展分区、逻辑分区与显式扩展容器](2026-10-07-mbr-extended-partitions.md)：
  MBR 现在可以是「4 主」或「3 主 + 最多 64 逻辑」，也可以只放一个尚未使用的空容器
  （预留空间）；
- **MBR 没有分区名字段**：`spec.name` 不写入镜像，`partspec::validate` 只在 GPT 下
  校验名称长度，UI 禁用名称输入并注明原因。

### 4. 三个「不静默」决定

- **不静默裁剪**：请求分区超出可用区间时返回 `no_space`，而不是裁到放得下。
  裁剪会让用户以为拿到的是自己填的容量。
- **不静默改名/覆盖**：同名镜像拒绝创建（另一个 Note 详述）。
- **不静默丢分区名**：MBR 下 UI 明确禁用并说明。

## Alternatives considered

**只支持单分区，把分区元数据暴露出来** — `partspec` 的校验逻辑本来就与分区数
无关，改动量省不下多少，却让用户无法为不同用途分开放置。

**把 `Formatter` 的实现也放进 core（直接 `Command::new`）** — 会破坏 core 的
「完全可在主机测试、不接触平台接口」边界。该边界由既有架构文档与测试共同维护，
破坏它需要更强的理由。

**为 ext4/exFAT 引入纯 Rust 写入库** — `docs/requirements.md` 把它列为非目标的
理由是「缺乏成熟可靠且无额外依赖的纯 Rust 写入库」；系统 `mkfs` 实测可用后已无必要。

**exFAT 也尝试不带 loop 直接格式化** — 实测其帮助输出中没有任何 offset 选项，
对分区镜像直接调用会格式化错误的位置（或整盘）。故必须经 loop。

**让 GPT 也走 `find_free_sectors()` 自动布局而不预扣尾部** — 保留原做法（由
`find_free_sectors()` 求上界），但**可用空间预检**改为扣除盘首盘尾，避免放行一个
写不进去的请求（见 Risks）。

## Acceptance criteria

- `cargo nextest run --workspace` 全绿；WebUI `node --test tests/` 全绿。
  （具体数量随改动演进，以当时的实际输出为准。）
- 多分区 GPT/MBR 的字节级校验：各分区 1 MiB 对齐、互不重叠、类型字节/GUID 正确、
  分区项数与请求一致。（MBR 启用逻辑分区后「分区项数」按「主分区数 + 是否启用扩展
  容器」计，见
  [MBR 扩展分区、逻辑分区与显式扩展容器](2026-10-07-mbr-extended-partitions.md)。）
- MBR **第 5 个主分区**被拒（`invalid_argument`）；`raw` 多分区被拒。
- 超出容量的分区请求返回 `no_space` 且**不被裁剪**。
- `create::tests::refuses_to_overwrite_existing_file` 断言原文件**逐字节未变**。
- ext4 与 exFAT 的偏移策略有测试固化（当时的 `only_exfat_needs_loop_for_offset`；
  **该测试已随统一 loop 改造被 `plan_carries_range_and_filesystem` 取代**）。

## Risks

- **待验证假设**：以上 mkfs 结论来自 **AVD（模拟器）**，尚未在 arm64 真机验证。
  真机 ROM 的 `mkfs` 集合可能不同（可能多、也可能少）。实现按「探测 + FAT32 内置
  回退」处理，探测不到 FAT32 之外的 `mkfs` 时**报错并说明**，不会静默做出错误的
  文件系统。已登记于 `docs/roadmap.md` 待验证假设 #13、#15。
- **可用空间预检是保守上界**：GPT 按「镜像容量 − 2 MiB」估算（盘首主头+分区项数组、
  盘尾备份），实际精确上界由 `write_gpt` 内部的 `find_free_sectors()` 决定。保守
  意味着可能拒绝一个理论上刚好放得下的请求，但不会放行一个写不进去的请求。
- **GPT 多分区的空闲区间选择**：`find_free_sectors()` 会返回碎片区间（如分区项数组
  之后到第一个分区之前）。若只看第一个区间会误判空间不足，故实现遍历所有区间择一
  放得下者。该问题由 `creates_multiple_gpt_partitions_each_formatted` 回归覆盖。
- **`partitions()` 顺序不确定**：`gpt` crate 返回 `HashMap`，写入后必须按
  `first_lba` 排序再编号，否则分区序号与内核 `loopNpM` 对不上。
