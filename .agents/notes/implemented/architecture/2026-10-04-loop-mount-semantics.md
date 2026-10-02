# Agent Note: loop 挂载的实测语义与内核接缝

Status: implemented
Category: architecture

## Problem

设备本地 loop 挂载在实机与虚拟机环境下存在若干隐式内核行为约束：后备镜像文件与 loop 块设备的打开标志不一致时会触发 `EACCES`；动态分配的 loop 索引与 `/dev` 块设备次设备号并非简单一一映射；Android 系统缺失 udev/devtmpfs 导致即便内核支持 `max_part > 0`，亦无法自动创建分区设备节点。此外，还需确保本地挂载与 USB 导出在多进程并发下的数据互斥与原子化释放。

## Proposal

### 1. 内核访问抽象与测试替身

| trait | 职责 | 生产实现 | 测试替身 |
|---|---|---|---|
| `LoopControl` | `get_free`/`set_fd`/`set_status64`/`set_capacity`/`set_autoclear`/`clear_fd`/`status` | `RealLoopControl`（loop ioctl） | `MemLoop` |
| `Mounter` | `mount`/`umount`/`sync_all`/`is_mounted` | `RealMounter`（`mount(2)`） | `MemMounter` |

能力探测输入抽象为 `CapabilitySource`，使分支覆盖可在开发宿主环境单测断言。

### 2. 内核关键语义与处理规则

1. **设备次设备号计算**：内核次设备号规则为 `minor = index * (max_part + 1)`。直接按索引号创建设备节点会导致访问错误设备并引发 `LOOP_SET_FD` 报 `ENXIO`。实现优先读取 `/sys/block/loopN/dev` 获取内核真实设备号，并在补充创建节点时校验 `st_rdev`。
2. **分区访问依赖显式 `lo_offset`**：因 Android 缺失分区节点自动生成机制，无法依赖 `losetup -P`，必须依据分区表元数据通过指定偏移进行单分区挂载。
3. **单一挂载路径**：只有 `lo_offset`。曾经存在「partscan 失败则回退」的双路径，已于 2026-10-06 移除（见第 9 条）。
4. **读写模式成对同步**：后备文件打开标志与 loop 设备打开标志必须同时满足读写要求。只读挂载采用 `O_RDONLY`，读写挂载必须均以 `O_RDWR` 打开，任一环节缺省读写权限均会导致内核标记为只读并触发 `EACCES`。
5. **`LO_FLAGS_AUTOCLEAR` 时序约束**：仅在 `mount(2)` 成功之后回读状态并置位。若在 setup 阶段置位，ioctl 返回后由于文件描述符关闭会立即触发内核自动卸载并报 `ENXIO`。
6. **严格释放时序**：严格按 `sync` → `umount` → `LOOP_CLR_FD` 执行。`umount` 失败时严禁执行 `clear_fd`，防止挂载状态下块设备被异常解绑导致文件系统损坏。
7. **能力探测只回报事实**：逐级探测 `/dev/loop-control` 可用性与支持的文件系统（`vfat` 优先、`exfat` 兜底）。**不再探测分区节点存在性**，也不再派生「是否支持分区扫描」的结论（见第 9 条）。`max_part` 仍如实回报，但仅供第 1 条的次设备号推导使用，不参与分支。
8. **`LO_FLAGS_PARTSCAN` 位保留**：`set_status64` 仍携带该标志位参数，且偏移路径必须显式传 `false`——否则内核会在偏移处再次解析分区表，派生出错误的子设备。读回校验（`loopdev` 测试断言该位为 0）同样保留，用于捕捉内核行为变化。
9. **partscan 路径已移除（2026-10-06）**：`LO_FLAGS_PARTSCAN` 让内核按分区表派生 `loopNpM` 子设备，但 Android 无 udev/devtmpfs，内核即使扫描出分区也不在 `/dev` 下建节点，该路径**恒不可用**（实测 `max_part = 7` 而 `loopNpM` 不存在，`mount` 报 `ENOENT`）。此前每次挂载都先白走一轮注定失败的 partscan（多分配一个 loop 设备再清理）；且能力探测只能看到 `max_part > 0`，会向用户报告 `true` 而实际必然回退——属乐观报告。因此删除 `Strategy`/`part_device_exists`/`prefer_partscan` 与协议字段 `partscan_supported`，`lo_offset` 成为唯一路径。决策记录见 [放弃 partscan](../../rejected/architecture/2026-10-06-drop-partscan-mount-path.md)。

### 3. 数据安全与跨进程互斥

**互斥底线**：同一镜像严禁同时作为 gadget LUN 与 loop 后端。

判定完全取自内核实际状态（configfs 的 `lun.N/file` 与 `/sys/block/loopN/loop/backing_file`），不依赖内存状态。跨进程并发由 `run/ops.lock` 上的 `flock(LOCK_EX|LOCK_NB)` 串行化，锁生命周期绑定至打开的文件描述符，进程异常退出由内核自动释放，杜绝死锁。

### 3b. 内存记录必须与内核真值对账（长期存活进程）

**互斥判定**取自内核，但**操作寻址**（`attach` 的挂载点占用检查、`detach` 的目标查找）走进程内的 `mounted` 记录。记录的来源有两条，职责必须分开：

| 方法 | 语义 | 使用场合 |
|---|---|---|
| `sync_from_kernel` | **无条件以内核为准**（清空重建） | 进程启动；需要丢弃全部记录时 |
| `reconcile` | 保留仍成立的记录、补上内核新增的绑定 | 长期存活进程的**每次操作前** |

**真机实测缺陷**：`serve` 仅在启动初始化阶段执行一次 `sync_from_kernel`。若 loop 挂载随后由其他进程（如一次性 CLI 执行 `detach-loop`）释放，`serve` 内部仍残留旧有记录，导致后续 `POST /api/v1/loop/attach` 误报：

```text
{"error":"busy","message":"the mount point is already in use: …/mnt/disk.img"}
```

而此刻内核中 `list-loop` 与 `/proc/self/mounts` 均为空——用户侧观察到“提示挂载点已被占用，但内核无任何活跃挂载”，且必须重启 `serve` 进程方可恢复。

因此 `LoopMounts::attach` 与 `detach` 在委托内核操作前均主动调用 `reconcile` 执行状态对账。

**双重判定规则**：逐条校验内存记录是否仍然成立：
1. 内核仍绑定该 loop 序号 → 记录有效，予以保留；
2. 内核已解绑，但**挂载表中该挂载点依然存在** → 依然保留。此规则为 `detach` 的关键前提：其依挂载点寻址释放目标并执行 `sync` → `umount` → `clear_fd`，而“loop 已被外部解绑但挂载点仍在”正是其必须处理的边缘状态。若移除该规则，用户将彻底无法通过本模块卸载残留挂载点，从而引发更严重的资源泄露。

仅当两项判定均不成立时，该记录方被确认为陈旧失效项并予以丢弃。

**两个方向都有测试**（且都做过负向验证——故意把 `reconcile` 改成 no-op 时失败）：

- `attach::tests::stale_records_do_not_make_attach_report_busy` —— 陈旧记录不得让挂载失败；
- `attach::tests::reconcile_keeps_a_live_mountpoint_even_after_the_loop_is_gone` —— 仅存于挂载表的记录必须保留且仍可卸载；
- `attach::tests::reconcile_adopts_bindings_created_by_another_process` —— 别的进程新建的绑定要被采纳，且**绝不**采纳非本模块的 loop；
- `attach::tests::sync_from_kernel_still_resets_to_the_kernel_truth` —— 两条路径的行为差异必须真实存在，否则「启动时清空重来」的语义会丢失。

对账失败**不阻断**挂载/卸载：读不到内核状态时，后续 ioctl 才是权威的错误来源。

### 4. 隔离清理边界

系统启动与挂载回滚清理仅处理后备文件位于本模块 `images/` 下的设备，严禁越界解绑系统其他业务的 loop 挂载；活跃附件同时记录于 `run/loop-attachments.json` 以便状态审计。

## Alternatives considered

- **仅在 `set_status64` 指定只读而不改打开模式**：否决。内核按底层文件描述符的可写性自动强制标记。
- **配置阶段置位 autoclear 并持久持有 fd**：否决。增加了复杂的生命周期管理成本，挂载后单次追加置位更加简洁。
- **依赖系统 `losetup` 命令行**：否决。Android toybox 实现缺乏 `-P` 且不支持容量更新调用。
- **保留 partscan 作为对照手段**：否决（2026-10-06）。曾考虑将其降级为「可用时的交叉验证」，但 Android 上恒不可用，保留只会继续产生乐观的能力报告与每次挂载的无效开销；若将来需在有 devtmpfs 的内核上恢复，入口见 `attach.rs` 的 git 历史。
- **仅修正 `partscan_supported` 语义而保留路径**：否决。字段与路径同源，单独改字段会留下无人使用的策略代码。
- **依据文件级 flock 保障互斥**：否决。Linux flock 属于建议锁（advisory lock），无法阻止第三方进程或直接系统调用写入，无法提供严格的底层数据安全保障。
- **基于锁文件的跨进程互斥**：否决。进程异常终止易残留失效锁文件，且 PID 复用会造成误判。

## Acceptance criteria

1. `cargo nextest run -p gadgetdisk-loop` 全绿通过，覆盖调用顺序、清理边界、`autoclear` 时序与次设备号推导。
2. 以下两条守卫测试必须通过（防止 partscan 分支被重新引入）：
   - `attach::tests::partitioned_image_allocates_loop_device_once` —— 分区镜像挂载只分配一次 loop 设备；
   - `attach::tests::max_part_never_changes_the_mount_path` —— `max_part` 取 `0`/`7`/`64` 时调用序列完全一致。
3. `loopdev::tests::build_status_sets_offset_and_read_only_flag` 仍断言偏移路径下 `LO_FLAGS_PARTSCAN` 位为 0。
4. `LoopControl` 的 `part_device_exists` 及相关替身方法已不存在；`grep -rn partscan_supported` 无结果。
5. `cargo nextest run -p gadgetdisk-loop -p gadgetdisk-proto -p gadgetdisk-cli -p gadgetdisk-gdd` 全绿。
6. 内存记录与内核真值的对账有**双向**守卫（见 3b 节）：陈旧记录不得误报 `busy`，仅存于挂载表的记录不得被误删；`gadgetdisk-cli` 侧的接线由 `loop_adapter::tests::attach_reconciles_with_the_kernel_before_touching_it` 源码断言钉住。
7. `verify_agent_gates.py` 门禁全绿通过。

## Risks

- **待验证假设**：`lo_offset` 在各类 OEM 定制内核上的细微差异需进一步真机测试。
- **待验证假设**：切换至全局 mount namespace 后在各厂商定制 ROM 下对第三方文件管理器的穿透可见性需进一步评估。
- **已知限制**：内核 `sync(2)` 为系统全局同步，释放单个附件会刷新整机页缓存；鉴于卸载操作低频，开销可接受。
- **已知限制**：移除 partscan 后，分区表解析完全依赖 `gadgetdisk-core::partitions` 的手写实现，失去了「内核解析结果」这一独立对照。**待验证假设**：在有 devtmpfs 的内核上恢复该对照手段的价值，需真机评估（该场景目前无设备可测）。
