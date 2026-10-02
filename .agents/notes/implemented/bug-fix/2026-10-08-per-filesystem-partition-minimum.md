# Agent Note: 分区容量下限按文件系统判定

Status: implemented

## Problem

用户报告：**分区容量小于 64 MiB 时界面提示"空间不足"**，但磁盘明明有几十 GiB 空余。

根因不在"空间"，而在**下限被套错了层级**。`partspec::resolve_sizes` 在展开
「占满剩余空间」的那一行时，无条件用 FAT32 的下限（当时 `MIN_FAT32_BYTES = 64 MiB`）
做判断，并用 `CoreError::NoSpace` 报出：

```rust
let floor = specs.iter().find(|s| s.size_bytes == 0)
    .map_or(MIN_FAT32_BYTES, |s| if s.is_extended() { MIN_EXTENDED_BYTES } else { MIN_FAT32_BYTES });
if remaining < floor { return Err(CoreError::NoSpace { needed: floor, available: remaining }); }
```

三个后果，每一个都能单独造成困扰：

1. **`no_space` 的语义是"磁盘放不下"**，映射到 HTTP `507`，前端文案是「存储空间不足」。
   用户于是去删文件、清缓存，而真正的问题是"这一行的文件系统装不进这么小的区间"。
2. **报错不说是哪个分区**。多分区镜像里用户无从得知该改哪一行。
3. **下限与文件系统无关**。分区显式选择 `none`（不格式化）或 `ext4` 时，
   判定仍然按 FAT32 的 64 MiB 走。

实测复现（宿主 + AVD，均为修复前）：

| 命令 | 结果 |
|---|---|
| `create --size 67108864 --layout gpt --partition 0////none` | `no_space`（need 67108864, available 65011712） |
| `create --size 67108864 --layout raw --partition 0////none` | 成功（raw 不扣表结构，`remaining` 恰好等于 64 MiB） |
| `create --size 4294967296 --layout gpt --partition 33554432////none` | **成功**——显式容量那一行不走下限分支 |
| `--filesystem none` / `exfat` / `ext4` 三种 | 报错文本完全相同 → 证明下限忽略文件系统意图 |

判据总结：**填"0 占满剩余"会报 `no_space`，填具体容量则正常**——这与用户看到的
"小于 64 MiB 就报空间不足"完全吻合。

同时，镜像级的"最小容量"本身是伪概念：有分区表的布局要在盘首（GPT 还含盘尾）
留表结构，所以 `image_bytes` 与"分区可用区间"天然不等。用镜像容量当分区下限的
代理，必然在边界上错一次。

### FAT32 的真实硬边界（本次实测）

`fatfs` 在簇数不足时会**静默**产出 FAT16，即使显式指定 `FatType::Fat32`。
边界不是文档早先写的"约 48 MiB"：

| 区间字节数 | 结果 |
|---|---|
| `34077184`（32.50 MiB） | 成功：簇大小 512、总簇数 **65525**，通过后置校验 |
| `34077183` | 失败：`Cannot select FAT type - unfortunate disk size` |
| 32 MiB | `verify_fat32` 判定为 `Fat16` |

即硬边界就是 FAT32 规范要求的 65525 簇。**33 MiB** 是在其上留余量的取值。

## Proposal

### 1. 下限成为"每个分区 × 其文件系统"的函数

`FilesystemType::minimum_bytes()`：

| 文件系统 | 下限 | 依据 |
|---|---|---|
| FAT32 | **33 MiB** | 实测 34077184 字节 = 65525 簇；33 MiB 留余量 |
| exFAT | **1 MiB** | **待验证假设**（宿主无 `mkfs.exfat`，未实测）；仅用于提前拦截 |
| ext4 | **2 MiB** | 实测 1 MiB 报 `Filesystem too small for a journal`，2 MiB 可用 |
| `none`（不格式化） | 1 MiB | 没有文件系统，只需放得下一个对齐后的分区项 |

`resolve_sizes` 因此多收一个 `default_filesystem` 参数（解析 `PartitionFilesystem::Inherit`
需要全局默认值），逐行判定：显式容量的行逐行过，`remaining` 分给"占满剩余"的那一行
时也按**该行**的文件系统过。

### 2. 删除镜像级最小容量

`MIN_IMAGE_BYTES` 删除，`check_size` 只做 1 MiB 对齐 + 拒绝 `requested == 0`
（零长度文件建不出可用镜像，属参数错误）。`--size 1` 会被对齐到 1 MiB 并继续走
分区判定——是否成立由"每个分区够不够大"回答。

### 3. `SizeBelowMinimum` 携带定位信息

```rust
SizeBelowMinimum { row: usize, filesystem: &'static str, requested: u64, minimum: u64 }
```

`#[error]` 文案：
`partition {row} ({filesystem}) is {requested} bytes, which is below the {filesystem} minimum of {minimum} bytes`。

**沿用 `size_below_minimum` 这个稳定错误码**（HTTP 400 / CLI Usage 退出码）：语义从
"镜像太小"修正为"某分区太小"，协议表与前端文案表都不必新增条目。`no_space` 则回到它
本来的含义——**分区总和超过镜像可容纳的区间**。

### 4. REST 与前端同步

- `GET /api/v1/tool/df` 的 `min_image_bytes` 改为 `min_partition_bytes`
  （`{"fat32":…,"exfat":…,"ext4":…}`；前端未消费旧字段）。
- `webui/pure/bytes.js`：`MIN_IMAGE_BYTES` → `FILESYSTEM_MIN_BYTES` + `minPartitionBytes()`；
  `validateSize` 去掉镜像下限；`fat32Note` → `sizeNote`（它现在只解释稀疏文件）。
- `webui/pure/partitions.js` 的 `validatePartitions` 增加逐行下限判定，错误文案带
  **行号 + 文件系统 + 下限 + 实际值 + 可用上限**，并在"占满剩余"那一行按
  `imageBytes - fixedTotal` 预判（提前复现后端的判定）。
- `webui/pure/describe.js` 的 `size_below_minimum` 文案不再写死数字
  （否则会在三种文件系统之间说谎），细节由后端 `message` 经 `detail` 展示。

### 5. 同步修正 FAT32 的旧数字

`mkfs.vfat` 的失败提示 `about 48 MiB` → `about 33 MiB`；`lib.rs` / `fat.rs` 的
"<48 MiB 静默降级"注释 → 65525 簇（约 32.5 MiB）。`MIN_FAT32_CLUSTERS = 65525`
**不动**——它是规范值，正是后置校验赖以发现降级的依据。

## Alternatives considered

**A. 保留统一的 64 MiB 下限，只改错误码与文案。**
最小改动，但错误仍在：`none`/ext4/exFAT 分区明明能建，却要被 FAT32 的门槛拦住。
用户报告的场景是"分区**小于** 64 MiB"——他们要的就是小分区，改文案不能让它可用。

**B. 保留镜像级下限，但改成"下限 = 各分区下限之和"。**
等于把同一个判断算两遍（分区已逐个判过），而且在 `raw` 布局下分区区间等于镜像容量、
在有分区表的布局下又不等——两套口径很难保持一致，容易出现"镜像判定通过、分区判定
仍失败"的错位。

**C. 小分区继续报 `no_space`，只把 message 写清楚。**
HTTP 状态码仍是 `507`（服务端存储不足），而实际是请求参数不合法（`400`）。
让客户端按 507 做重试/降级逻辑会得到错误的行为。

**D. 把 FAT32 下限降到规范最小值 34077184 字节。**
可以，但没有任何余量：分区表/对齐的任何一处调整都会让它掉到边界之下，且
"恰好 65525 簇"在部分宿主工具上已经处于灰色地带。33 MiB 多出的 0.5 MiB 换来确定性。

**E. exFAT 不设下限，交给设备端 `mkfs.exfat` 报错。**
宿主没有该工具可实测，因此"猜一个 1 MiB"与"完全不设"都有道理。选择设一个保守值：
它只用于**提前拦截**明显过小的分区，真实可行性仍以 `mkfs.exfat` 返回为准
（已记为待验证假设 #28）。

## Acceptance criteria

- `cargo nextest run -p gadgetdisk-core`：按文件系统下限的正反用例齐备——
  FAT32 33 MiB 边界（含 `row` / `filesystem` / `minimum` 断言）、ext4 2 MiB、
  exFAT 1 MiB、`none` 无下限、`Inherit` 取全局默认、行号指向真正的违规行、
  `--size 0` → `InvalidArgument`、`resolve_sizes_rejects_auto_smaller_than_its_filesystem_floor`。
- 回归用例 `image_smaller_than_a_fat32_partition_reports_the_partition_not_no_space`：
  实测的 65011712 字节可用区间上，占满剩余的 FAT32 分区**放行**；不格式化的更放行；
  只有总和真的超限才 `no_space`。
- `layout.rs` 的 `fat32_floor_admits_the_measured_cluster_boundary` 钉住
  `MIN_FAT32_BYTES >= 34077184`：下限低于实测硬边界会让"通过校验的镜像"在格式化阶段
  被自检拒绝。
- WebUI `node --test tests/`：`FILESYSTEM_MIN_BYTES` 三个数值、`validateSize` 不再有
  镜像下限、`validatePartitions` 对三种文件系统与"占满剩余"行都给带行号的报错。
- `docs/disk-image-format.md` 的容量约束表、`docs/requirements.md`、`docs/webui.md`、
  `docs/protocol.md` 与代码一致（单一事实源）。
- AVD：64 MiB GPT 镜像 + 一个占满剩余的 FAT32 分区**创建成功**（修复前误报 `no_space`）；
  33 MiB 的 FAT32 分区镜像可创建、`total_clusters >= 65525`，Host 端可挂载读取。

## Risks

- **33–48 MiB 的裸 FAT32 在 Linux 上可能被判成 `vfat`。** `MKFS.FAT` 判"无分区表"
  依据 `st_size <= FAT32_MAX_SIZE (0xFFFFFFFF)`，内核 `fstype` / `libblkid` 依据簇数
  ≥65525——两者判据不同。仅影响 `raw` 布局的类型显示与个别工具的自动挂载决策，
  不影响读写。Windows/macOS 行为是**待验证假设**（路线图 #27）。
- **exFAT 的 1 MiB 下限是猜测**（路线图 #28）。若真机 `mkfs.exfat` 要求更大，
  症状是"界面放行、格式化失败"并带回 `mkfs` 的原文——比静默产出错误的文件系统好，
  但仍是错误的用户体验。
- **降低下限会让更多"极小镜像"进入真实路径**：33 MiB 的 FAT32 在部分宿主工具上
  处于灰色地带。缓解手段是后置簇数校验（`MIN_FAT32_CLUSTERS` 未改），
  它保证不会静默降级成 FAT16。
- **`--size 1` 之类的请求不再被容量层拒绝**，会走到分区判定才失败。错误信息因此
  更精确（说是哪个分区），但如果用户填的是负数/非数字，报错来自容量层而非分区层——
  两条路径的文案需要各自清晰（已分别覆盖测试）。
