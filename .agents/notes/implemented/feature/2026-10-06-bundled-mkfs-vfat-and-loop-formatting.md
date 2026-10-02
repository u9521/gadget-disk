# Agent Note: FAT32 抽成自带二进制 mkfs.vfat，格式化统一经 loop

Status: implemented

## Problem

**Android 设备上没有可用的 FAT32 `mkfs`**，因此 FAT32 若不自带实现就完全不可用。
AVD 实测：无 `mkfs.vfat`、无 dosfstools、toybox 也没有 `mkfs`（全盘 `find` 无结果）。
exFAT 与 ext4 则各有系统工具，但它们的**偏移能力不一致**：`mkfs.exfat` 没有 offset
选项，`mke2fs` 有 `-E offset=`。

由此产生两个必须解决的问题：

1. **FAT32 必须随模块分发**（一个独立二进制），不能依赖设备；
2. **每分区可选文件系统**（含「不格式化」）要求格式化路径能按分区分别处理，而三条
   工具三种偏移策略会让「偏移」这件事散落在三处，新增文件系统时要重新研究它的能力。

## Proposal

### 1. FAT32 做成独立二进制 `mkfs.vfat`

新增 crate `gadgetdisk-mkfsvfat`（lib + bin），命令行对齐 dosfstools 的 `mkfs.fat(8)`：
位置参数 `DEVICE [BLOCK-COUNT]`（**KiB**）、`-F`/`-n`/`-i`/`-S`/`-s`/`-R`/`--offset`、
`-v`/`--help`/`--invariant`。

**`-F` 只接受 32**，传 12/16 明确报错：`fatfs` 在簇数不足时会静默降级成
FAT12/FAT16（实测），而本模块只承诺 FAT32。格式化后**必须**重新打开卷自检
（`verify_path`），因为静默降级发生在 `fatfs` 内部、不会报错。

Cargo **不允许二进制名含 `.`**，故 crate 产物叫 `mkfsvfat`，构建脚本按
`BIN_RENAMES` 重命名为 `mkfs.vfat`。

### 2. 统一经 loop 格式化

```
losetup -f
losetup -o <offset> --sizelimit <length> <dev> <image>
<mkfs> <dev>
losetup -d <dev>
```

各工具的偏移能力并不一致（`mkfs.exfat` 没有；`mke2fs` 有 `-E offset`；
`mkfs.fat` 有 `--offset`）。统一经 loop 后，偏移只在**一个**地方表达，各 `mkfs`
只做最简单的「对块设备格式化」，新增文件系统时不必再研究它的偏移能力。

`FormatPlan::needs_loop` 字段**删除**——全走 loop 后它没有意义，留着会误导。

### 3. 每分区文件系统的**三态**意图

```rust
pub enum PartitionFilesystem { Inherit, None, Some(FilesystemType) }
```

用三态而非 `Option<FilesystemType>`：后者无法区分「未指定（继承全局默认）」与
「显式不格式化」——两者都是 `None`。

## Alternatives considered

**FAT32 继续用进程内 `fatfs` 格式化** — 不满足「FAT32 必须随模块分发、且与其它
文件系统同一条路径」的约束：格式化路径会保留一个「内置特例」（不经外部进程、不经
loop），偏移策略仍是两套。

**ext4 继续用 `-E offset`（不经 loop）** — 少一次 loop 往返，但保留两套偏移策略，
与「偏移只在一处表达」的目标相悖。

**不自带 `mkfs.vfat`，直接依赖系统 dosfstools** — 设备上根本没有（AVD 实测：
无 dosfstools、toybox 亦无 `mkfs`），会导致 FAT32 创建在 Android 上完全不可用。

**让 `mkfs.vfat` 只接受 loop 设备、不实现 `--offset`** — 会与 dosfstools 的命令行
不兼容，用户无法用它直接格式化映像文件；实测 dosfstools 本身就有 `--offset`。

## Acceptance criteria

- `cargo nextest run -p gadgetdisk-mkfsvfat`：28 项，含 CLI 解析（`-F 16` 必须报错、
  `-n` 超长报错、`BLOCK-COUNT` 按 KiB 解释、`--offset` 换算为字节、
  `-V`/`--version` 回显注入的版本号）；
- `too_small_volume_is_detected_by_verify_not_silently_fat16`：小于下限的卷被自检拒绝，
  而不是留下一个 FAT16 卷；
- `formats_a_partition_range_within_an_image`：区间外（含偏移 0，即分区表处）
  **逐字节未被修改**；
- `explicit_no_format_survives_global_default`：显式 `none` 的分区保持未格式化，
  且**不触发格式化调用**；
- AVD 实测（x86_64 / Android 17.1）：混合文件系统镜像（FAT32 + ext4 + 不格式化 + FAT32）
  逐个分区的引导扇区/超级块魔数正确；自定义 GPT 类型 GUID 读回一致；
  `losetup -a` 无残留。

## Risks

- **统一经 loop 的代价**：每分区多一次 `losetup`/`losetup -d`（实测约几十毫秒），
  且**必须串行**——loop 是有限资源，不能并行占多个。GPT 上限 128 分区时总耗时显著，
  当前 `create` 无阶段进度（见 `docs/webui.md` 的既有说明）。
- **`--sizelimit` 不可省**：loop 默认把区间延伸到文件末尾，不限制会让 `mkfs` 看到
  一个比实际分区更大的设备并写出越界的元数据。已写入代码注释与文档。
- **两个实测踩出来的实现坑**（均已修复并有注释）：
  1. 对**块设备** `stat(2)` 的 `st_size` 恒为 0，必须用 `ioctl(BLKGETSIZE64)`；
  2. Android 的 `target_os` 是 `"android"` 而非 `"linux"`，只写 `cfg(linux)` 会让
     块设备分支在目标平台上被整个编译掉；
  3. `ioctl` 的请求参数类型随平台而异（glibc `u64` / bionic `i32`）。
- **待验证假设**：自带 `mkfs.vfat` 在 arm64 真机上的可执行性与 SELinux 上下文
  尚未验证（AVD 上正常）。已登记 `docs/roadmap.md` #16、#17。
- **自带二进制可被用户直接调用**（含 `--offset`），与经 loop 的路径应产生一致结果；
  当前没有覆盖「两条路径等价」的测试。
