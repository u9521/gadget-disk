# 本地 loop 挂载（镜像编辑）

镜像从 gadget 卸载后，可用内核 loop 设备 + 原生文件系统驱动在设备本地挂载，从而编辑内容。本文件定义挂载路径、能力探测与释放顺序。设计理由（含为何不自实现编辑器）见 [loop Note](../.agents/notes/implemented/architecture/2026-10-04-loop-mount-semantics.md)。

## 方案定位

由**内核**完成文件系统读写，因此不需要自实现 FAT32 编辑器。`gadgetdisk-core` 只负责**镜像创建**，不承担任何文件编辑 API。

实现方式：**由 `gadgetdisk serve`（或一次性 `attach-loop`）** 用 Rust 直接发 ioctl（`LOOP_CTL_GET_FREE` → `LOOP_SET_FD` → `LOOP_SET_STATUS64` → `LOOP_SET_CAPACITY`）后调用 `mount(2)`，**不依赖 busybox 存在**。这与 Android `vold` 自身的 `Loop::create()` 使用完全相同的内核接口。

> **架构解耦**：loop 挂载由 `gadgetdisk serve` 或一次性 CLI 直接调用内核接口执行，不经由 `gdd`。
> `gdd` 仅在 USB 导出期间按需存在，仅持有只读的 loop 视图（`LoopOps::attachments`）用于互斥判定。详见
> [按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

> **实测修正（x86_64 AVD，Android 17）**：后备文件与 loop 设备**都必须以 O_RDWR 打开**，
> 否则内核会把 loop 设备标记为只读，读写挂载将以 `EACCES` 失败；
> `LO_FLAGS_AUTOCLEAR` 只能在挂载**成功之后**置位，否则 ioctl 一返回设备即被解绑。
> 详见 [loop 实测语义 Note](../.agents/notes/implemented/architecture/2026-10-04-loop-mount-semantics.md)。

## 挂载路径

**只有一条路径**：`lo_offset` 分区偏移。

```
losetup -f --show -o <partition_offset_bytes> <image.img>  →  /dev/loopN
mount -t vfat /dev/loopN <mnt>
```

通过 `LOOP_SET_STATUS64` 的 `lo_offset` 跳过分区表，**不依赖 `max_part`，也不依赖 `/dev` 下有分区子设备节点**。

**分区偏移解析优先级**：

1. 调用方显式指定的分区序号（解析其 `start_lba`，序号越界时立即报错并列出可用分区）；
2. 分区表中首个有效分区（动态解析 GPT/MBR，保证导入镜像无需缓存即可挂载）；
3. `run/offsets.json` 缓存记录；
4. 兜底偏移 `0`（对应 `raw` 镜像）。

分区表读取由 `gadgetdisk-core::partitions` 提供（手写 GPT/MBR 解析，纯函数、可在主机离线测试）。**GPT 的识别以 LBA1 的 `EFI PART` 签名为准，而不是保护性 MBR**：由 `gpt` crate 生成的镜像 LBA0 全零，先判断保护分区会漏掉内部创建的镜像。

**失败即明确报错**：`loop_unsupported` / `filesystem_unsupported`，并引导用户改走 USB 编辑路径。**不回退到自实现编辑器**（非目标）。

### 已移除：整盘 partscan（保留实测依据）

曾有一条依赖 `LO_FLAGS_PARTSCAN` 的路径，让**内核**按分区表自动派生分区子设备
（`/dev/block/loopNpM`），从而无需自行解析 GPT/MBR。**该路径已于 2026-10-06 移除**，
理由是它在 Android 上恒不可用——下表数据保留，作为「为何此路不通」的证据：

| 检查项 | 实测值（x86_64 AVD，Android 17） |
|---|---|
| `max_part` | `7`（内核开启分区扫描支持） |
| `losetup -P` | toybox **不支持**（`Unknown option 'P'`） |
| `/dev/block/loopNpM` | **不存在**，`mount` 报 `ENOENT` |

原因是 Android 没有 udev/devtmpfs 把内核扫描出的分区落到 `/dev` 下；没有设备节点就无从挂载。
此前代码以「分区子设备是否真的存在」为判据回退，导致**每次挂载均会先行执行一次必然失败的分区扫描尝试**
（多分配一个 loop 设备再清理），才落到偏移路径。此外，此前能力探测仅依赖 `max_part > 0` 作为判据，
向前端虚报支持分区扫描，而实际调用必然触发降级。

> **不要仅凭 `max_part > 0` 就断定分区扫描可用。** 该参数现在只用于推导 loop 设备
> 次设备号（见「设备节点」一节），与挂载路径无关。

决策记录见 [放弃 partscan Note](../.agents/notes/rejected/architecture/2026-10-06-drop-partscan-mount-path.md)。

### `raw` 布局

无分区表，直接对整盘 `losetup` 后 `mount`（即偏移 `0`）。

## 能力探测阶梯

首次使用时探测并缓存（结果经 `CapabilitiesResponse` 暴露）：

| 顺序 | 探测项 | 用途 |
|---|---|---|
| 1 | `/dev/loop-control` 是否可打开 | 决定 loop 是否可用；不可用即引导 USB 编辑路径 |
| 2 | `/proc/filesystems` 是否含 `vfat` / `exfat` | 决定能否挂载 |
| 3 | 是否存在 `mass_storage` gadget function | 与 USB 侧能力区分 |

**探测只回报原始事实，不派生挂载结论。** 曾经的 `partscan_supported` 字段即由
`loop_control && max_part > 0` 派生，会在恒回退的平台上报告 `true`——已随该路径一并删除。

`/sys/module/loop/parameters/max_part` 仍会被读取并如实回报（诊断用），但**不参与分支**：
它用于推导 loop 设备次设备号以补建设备节点。

## 设备节点

**`max_part` 的真实用途**：loop 驱动的次设备号**不是**序号本身，而是
`minor = index * (max_part + 1)`（本机 `max_part = 7` 时 `loop1 = 7:8`、`loop52 = 7:416`）。

Android 只预建有限数量的 loop 节点，而 `LOOP_CTL_GET_FREE` 会返回更大的序号，
此时 `open` 得到 `ENOENT`——内核侧设备存在，只是用户空间没有节点。代码因此：

1. 优先读 `/sys/block/loopN/dev` 取内核给出的权威 major:minor；
2. 读不到才退回上述公式；
3. 按推导结果补建 `/dev/block/loopN`（权限 `0600`）。

按 `minor = index` 建节点会指向**另一个**设备：`open` 成功，但 `LOOP_SET_FD` 以 `ENXIO`
失败，报错信息无法反映真正的故障根因。

> **`LO_FLAGS_PARTSCAN` 位仍保留在 `set_status64` 中，且偏移路径必须显式置 `false`**：
> 否则内核会在偏移处再解析一次分区表，派生出错误的子设备。

## 挂载点

- 统一位于 `/data/adb/gadget-disk/mnt/<name>`。
- 默认以**加入 init 全局 mount namespace** 的方式挂载（等价 KernelSU `su -M` / `switch_mnt_ns(1)`），以最大化第三方可见性。
- 提供「私有 namespace」配置项作为备选。
- 可见性限制与 vold 评估见 [挂载可见性与 vold](mount-visibility-and-vold.md)。

## 强制约束

**互斥**：同一镜像**绝不可**同时作为 gadget LUN 与 loop 附件。两者同时写入会损坏文件系统。

判据取**内核真值**，两侧各自校验：

| 方向 | 校验者 | 读什么 |
|---|---|---|
| 把已被 loop 挂载的镜像导出为 USB 设备 | `gdd.mount` | `/sys/block/loopN/loop/backing_file` |
| 把正被导出的镜像挂到本地 | `serve` / `attach-loop` | configfs `functions/mass_storage.gadget-disk/lun.0/file` |

**互斥机制与并发控制**：跨进程状态排他性严格依赖内核真值与文件锁。进程间通过 `run/ops.lock` 上的 `flock(LOCK_EX|LOCK_NB)` 串行化状态操作，进程退出时内核自动释放锁，避免死锁或状态残留。详见
[按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

**释放顺序**：重新挂到 gadget 之前必须依次完成并校验：

1. `sync`（刷出页缓存）
2. `umount <mnt>`
3. `losetup -d /dev/loopN`
4. 校验 `/dev/loopN` 已释放、挂载点已不在 `/proc/mounts` 中

未完成释放就挂 gadget，会因文件被占用或状态陈旧而失败。

**只读选项**：提供 `read_only` 开关（对应 `losetup -r` 与只读挂载），用于纯检视场景以降低误改风险。

**挂载前的镜像上下文修正**：后备镜像由**内核工作线程**读写，SELinux 按该线程所属安全域
判定访问权限；安全标签未开放权限时挂载将直接失败或写入被静默丢弃。因此本地挂载与格式化操作均在触碰内核
**之前**检查并（必要时）修正安全标签。规则、默认值与测试依据见
[Android 集成](android-integration.md#镜像文件的-selinux-上下文内核读镜像的前提)
（遵循单一事实源，此处不展开）。在 loop 路径侧包含以下特有约束：

- 修正操作必须在 `LOOP_SET_FD`（loop 挂载）与调用 `losetup`（格式化）**之前**完成——
  内核在打开后备文件的瞬间即按当时标签锁定文件访问权限，后续修改对当前挂载无效；
- 仅修改本模块 `images/` 目录下的镜像文件；目录外部文件仅输出告警，**严禁修改**；
- 修正失败**不阻断**挂载与格式化，但会随 REST 应答中的 `warnings` 字段返回（界面呈现中文结论与英文明细）；`--mode ro` 模式同样应用该标签——不区分 ro/rw 方可保障“先只读检视、后续改读写”无缝衔接且无需二次修改标签（取舍见 [Android 集成](android-integration.md)）。

## 启动清理

设备重启或进程被杀会残留 loop 设备与陈旧挂载点。`LoopMounts::new` 在**使用前**执行：

1. 读 sysfs 判断哪些 loop 设备指向**本模块的 `images/` 目录**；
2. 卸载 `/data/adb/gadget-disk/mnt/` 下的遗留挂载；
3. 释放属于本模块的 loop 设备；
4. 保留非本模块的系统或其他业务 loop 设备（严禁误清理外部资源）。

## 内存记录与内核真值的对账

**互斥判定**取自内核真值，但**操作寻址**（挂载点占用检查、按镜像卸载）依赖进程内的
挂载记录。长期常驻服务进程（`serve`）仅在初始化时同步一次，若 loop 挂载随后由**其他进程**
（如一次性 CLI `detach-loop`）释放，服务进程将持有陈旧记录并误报
`busy: the mount point is already in use`——而内核中并无任何活跃挂载（真机实测缺陷）。

因此 `attach` 与 `detach` 在委托内核操作**之前**均主动与内核真值进行状态对账：

| 记录形态 | 处置 |
|---|---|
| 内核仍绑定该序号 | 记录有效，保留 |
| 内核已解绑，但挂载表里该挂载点仍在 | **保留**（作为后续 `detach` 正常寻址与清理的前提） |
| 两者均不成立 | 确认为陈旧失效记录，予以丢弃 |

对账同时**采纳**其他进程新建的、属于本模块的绑定；非本模块的 loop 设备一律不纳入记录。
对账失败仅输出告警，不阻断正常操作流程——当无法读取内核状态时，后续底层 ioctl 调用将作为权威错误来源。

设计理由与双向守卫测试见
[loop 挂载语义 Note](../.agents/notes/implemented/architecture/2026-10-04-loop-mount-semantics.md)。

## 验收

- 只有一条挂载路径（`lo_offset`）；分区镜像挂载**只分配一次** loop 设备，
  不得出现「先分配再清理」。
- `max_part` 的取值**不改变**挂载调用序列（有测试守卫，防止重新引入分支）。
- `raw` 与 `gpt`/`mbr` 均能正确挂载（后者使用持久化偏移）。
- 同一镜像不会同时出现在 gadget LUN 与 loop 附件中（有拒绝路径测试）。
- 重新挂 gadget 前强制完成 `sync` → `umount` → `losetup -d` 并校验。
- 能力全不可用时给出可操作提示并引导 USB 编辑路径。
- loop 与 mount 的调用序列在 `gadgetdisk-loop` 内有主机侧测试覆盖（trait 替身断言顺序与参数）。
- 启动清理不触碰非本模块的 loop 设备。
- 内存记录与内核真值的对账是**双向**的：陈旧记录不得导致挂载误报 `busy`，
  仅存于挂载表的记录亦不得被误删（确保仍可被 `detach` 正常卸载清理）。

## 实测结论

以下已在 x86_64 AVD（Android 17，KernelSU 3.3.0，`u:r:ksu:s0`）上验证，
完整记录见 [loop 实测语义 Note](../.agents/notes/implemented/architecture/2026-10-04-loop-mount-semantics.md)：

- `max_part = 7`，但 `losetup -P` 不受支持、`loopNpM` 不生成 → **partscan 不可用
  （该路径已因此移除，实测数据保留供追溯）**；
- `vfat` 与 `exfat` 均在 `/proc/filesystems` 中（另有 7 个块设备文件系统）；
- `lo_offset` 支持良好，是**唯一**的挂载路径；
- 只读挂载生效：`vfat ro,`、`/sys/block/loopN/ro = 1`、写入被拒；
- 释放顺序（`sync` → `umount` → `LOOP_CLR_FD` → 校验）已按序验证；
- 启动清理后无遗留 loop 设备。

## 待验证假设

- 全局 namespace 挂载的实际穿透效果（切换至 init 全局 mount namespace 后的第三方应用感知度）；
- 「挂载后删除镜像」时启动清理能否仅靠 `mnt/` 扫描兜底。
