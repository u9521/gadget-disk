# Agent Note: GPT 与 MBR 分区类型拆成两套独立类型

Status: implemented

## Problem

分区类型原用一个枚举同时携带两套语义：

```rust
pub enum PartitionType {
    Fat32Lba,        // GPT: BASIC Data  /  MBR: 0x0C
    Linux,           // GPT: LINUX_FS    /  MBR: 0x83
    EfiSystem,       // GPT: EFI         /  MBR: 0xEF
    MicrosoftBasic,  // GPT: BASIC Data  /  MBR: 0x07
}
```

三个具体问题：

1. **`Fat32Lba` 与 `MicrosoftBasic` 的 GPT GUID 是同一个**（都是 BASIC Data），
   在 GPT 布局下二者完全无法区分，枚举里却并列存在；
2. **只能表达 4 种类型**，而 GPT 的类型空间有几十种常用值；
3. 需要 `supports_mbr()` 这类「按布局过滤」的补丁——那正是两种空间被强行合并的症状。

同时用户要求支持**直接输入类型 GUID**，而该枚举结构上无法承载任意 GUID。

值得注意的是，**读取侧本来就是分开的**：`partitions.rs` 有独立的 `mbr_type_label`
（`0x0C` → "FAT32 (LBA)"）与 `gpt_type_label`（按 GUID 字节比对）。也就是说写入侧
与读取侧的模型一直不一致。

## Proposal

### 1. 拆成两个独立类型

- `GptPartitionType`：9 个预设（EFI / Microsoft Basic / Microsoft Reserved /
  Windows Recovery / Linux filesystem / Linux swap / Linux LVM / Linux RAID /
  BIOS boot）+ `Custom(uuid::Uuid)`；
- `MbrPartitionType`：9 个预设（`0x0C`/`0x0B`/`0x0E`/`0x07`/`0x83`/`0x82`/`0x8E`/`0xEF`/`0x05`）
  + `Custom(u8)`。

`PartitionSpec` 同时持有 `gpt_type` 与 `mbr_type`（各自 `Option`）：**哪个生效由布局
决定，无关的那个被忽略**——用户可能先按 GPT 填好再改成 MBR，不应因此报错。

### 2. 线格式名一律带前缀

`gpt:microsoft_basic` / `gpt:<GUID>` / `mbr:fat32_lba` / `mbr:0x1A`。
**不接受不带前缀的名字**，那正是过去混用的来源。

### 3. 自定义类型 GUID（本次实现）

`gpt::partition_types::Type` 的两个字段都是 `pub`，因此可直接构造
`Type { guid, os: OperatingSystem::Custom(..) }` 传给 `add_partition`，
**不需要**绕过 crate 的 API。新增 `uuid` 直接依赖（锁文件本就解析为 1.27.0，
满足 `gpt` 的 `^1.3.4`，依赖图不变）。

### 4. 自定义**分区** GUID 明确不做

`gpt` crate **没有**设置分区 GUID 的公开 API：`add_partition`（`lib.rs:554`）与
`add_partition_at`（`lib.rs:633`）都硬编码 `Uuid::new_v4()`，且 `partitions()` 只
返回不可变引用。要支持只能字节级补写或自写 GPT 写入器，成本与风险都不成比例。
该限制已写入 `docs/disk-image-format.md`，避免用户以为填了会生效。

### 5. MBR 类型字节 `0x00` 被拒绝

`MbrPartitionType::Empty` 在**实际分区**上会被 `validate` 拒绝：那会让分区项被判为
空项而「消失」，用户填的容量白填。

## Alternatives considered

**保留单枚举，增加 `CustomGpt(Uuid)` / `CustomMbr(u8)` 变体** — 改动更小，但
「这个变体在当前布局下是否有意义」仍需在使用点逐个判断，等于把补丁从
`supports_mbr()` 扩散到更多地方。

**直接用 GUID 字符串当类型，不设预设** — 用户要手写 32 位十六进制才能建一个普通
数据分区，体验过差。预设表的价值是「常见类型有名字」。

**字节级补写实现自定义分区 GUID** — 分区项在磁盘上的位置是确定的（LBA2 + i*128，
GUID 在项内偏移 +16），技术上可行；但按用户决定本次不做。

## Acceptance criteria

- `cargo nextest run -p gadgetdisk-core -E 'test(gpt_type_wire_names_round_trip)'`
  等：两套类型的线格式名往返一致。
- `gpt_guid_matches_crate_constants`：9 个预设 GUID 与 `gpt` crate 的常量**逐字一致**
  （防止同一类型在写入与读取两处被判成不同东西）。
- `gpt_writes_custom_type_guid_into_partition_entry`：**字节级**断言自定义 GUID 确实
  写入了分区项（LBA2 起、项内偏移 0），并核对分区名（项内偏移 56，UTF-16LE）。
- `mbr_writes_custom_type_byte`：字节级断言自定义类型字节落在分区项 `+4`。
- `validate_rejects_empty_mbr_type_on_mbr`：`0x00` 在实际分区上被拒。
- `wire_names_require_prefix`：`microsoft_basic`、`mbr:fat32_lba`（当 GPT 类型解析时）
  等一律拒绝。

## Risks

- **破坏性 API 变更**：`PartitionType` 消失，`PartitionTable.type_id` 拆为
  `gpt_type`/`mbr_type`。该类型无仓库外调用方，线格式也在同一次改动中切到前缀式，
  风险可控。
- **UI 必须按布局切换可选项**，否则用户会在 GPT 下看到 MBR 类型（或反之）。
  已由 `partitionTypePresets(layout)` 与 `webui/tests/logic.test.mjs` 覆盖。
- **待验证假设**：预设 GUID 与 MBR 字节取值来自 UEFI 规范与 Linux 惯例，已在
  AVD 上验证能被内核正确识别（`partitions.rs` 的读取路径回读一致）；但
  `gpt:bios_boot`、`gpt:linux_raid` 等冷门类型**未在真机 Host 上验证**其可识别性。
