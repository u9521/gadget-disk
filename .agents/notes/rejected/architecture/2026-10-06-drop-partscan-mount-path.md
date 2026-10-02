# Agent Note: 放弃 partscan 挂载路径

Status: rejected

## Problem

`gadgetdisk-loop` 曾有两条挂载路径：

1. **partscan**：`LOOP_SET_STATUS64(offset=0, LO_FLAGS_PARTSCAN)` 让**内核**解析分区表并派生出分区子设备 `/dev/block/loopNpM`，直接挂载子设备；
2. **`lo_offset`**：用分区表中解析出的偏移挂载整盘 loop 设备，内核不解析分区表。

partscan 在理论上更优（无需自行解析 GPT/MBR，避免偏移算错），因此被实现为**优先路径**，失败才回退到 `lo_offset`。但该设计在目标平台上产生了两个具体问题：

**问题一：每次挂载都白走一轮注定失败的流程。** Android 没有 udev/devtmpfs 把内核扫描出的分区落到 `/dev` 下，节点不存在就无从挂载。而能力探测的判据是 `loop_control && max_part > 0`，在 AVD 上两者都为真（`max_part = 7`），于是每次都先分配一个 loop 设备、设置 partscan、发现子设备不存在、清理，再用**另一个**序号重走 `lo_offset`。这是每次挂载都付的确定性开销，换来零收益。

**问题二：能力探测向用户报告了不真实的结果。** `Capabilities.partscan_supported` 只反映「内核参数支持」，而 WebUI 如实显示为「整盘 partscan：支持」。用户看到「支持」的同时，实际每次挂载都在回退。同一份实测资料里早已记录 `loopNpM` 不存在（`docs/ondevice-loop-mount.md` 的实测表），文档与运行时行为长期不一致。

实测依据（x86_64 AVD，Android 17，KernelSU 3.3.0）：

| 检查项 | 实测值 |
|---|---|
| `max_part` | `7`（内核开启分区扫描支持） |
| `losetup -P` | toybox **不支持**（`Unknown option 'P'`） |
| `/dev/block/loopNpM` | **不存在**，`mount` 报 `ENOENT` |

**约束**：不能简单地「删掉一切带 partscan 字样」的代码与配置。`max_part` 本身仍被 `loopdev` 用于推导 loop 设备次设备号（`minor = index * (max_part + 1)`）以补建 Android 未预建的 `/dev/block/loopN` 节点——删掉它会让**剩下唯一**的挂载路径失效。

## Proposal

**删除 partscan 策略层与协议字段，保留底层能力位。**

### 1. 删除的内容

| 项 | 位置 |
|---|---|
| `Strategy` 枚举与其 `as_str` | `gadgetdisk-loop/src/attach.rs` |
| `prefer_partscan` 选择逻辑、partscan 分支、回退诊断输出 | 同上 |
| `Mounted.strategy` / `Mounted.part_devs` | 同上 |
| `AttachRequest::part_device` | 同上 |
| `LoopControl::part_device_exists` 及两处实现 | `gadgetdisk-loop/src/loopdev.rs` |
| `MemLoop::with_part_device` / `MemLoop.part_devices` | 同上 |
| `Capabilities.partscan_supported`（**协议字段**） | `gadgetdisk-proto/src/message.rs` |
| `capabilities` 响应中的 `partscan_supported` | `gadgetdisk-cli/src/serve.rs` |
| WebUI「整盘 partscan」行与「降级为 lo_offset」提示 | `webui/app.js` |

### 2. 保留的内容（关键——删了会坏）

| 项 | 保留理由 |
|---|---|
| `max_part` 读取与 `minor = index * (max_part + 1)` | 补建 `/dev/block/loopN` 节点；Android 预建节点只到 `loop17` 而 `LOOP_CTL_GET_FREE` 返回 `52` |
| `Capabilities.max_part` 字段 | 同上，且仍作为诊断信息如实回报 |
| `LO_FLAGS_PARTSCAN` 常量与位处理 | 偏移路径**必须显式清掉**该位，否则内核在偏移处再解析一次分区表并派生错误子设备 |
| `set_status64` 的 `partscan` 参数 | 同上，调用点恒传 `false` |
| `LoopStatus.partscan` 读回 | 用于断言偏移路径下该位为 0，捕捉内核行为变化 |
| `Attachment.loop_part_devs` | 协议字段，恒为空；保留以兼容既有 REST 消费者，注释已说明不应据此判断挂载方式 |

### 3. 为什么是删除而不是「降级为交叉验证」

曾考虑把 partscan 保留为**诊断手段**：在支持它的内核上用它解析分区表，与手写解析结果互为对照。该想法有真实价值（手写 GPT/MBR 解析缺少独立校验），但仍被否决：

- Android 上该路径恒不可用，保留它意味着代码长期处于「从未在目标平台执行过」的状态；
- 可用的对照价值需要一个有 devtmpfs 的内核，而目前**无此设备可测**（属待验证假设，见 `## Risks`）；
- 保留它就得继续维护能力探测与分支，而探测本身正是问题二的来源。

若将来出现该平台，从 git 历史恢复 `Strategy` 枚举即可，成本可控。

### 4. 行为变化

- **挂载路径唯一**：`raw` 传偏移 `0`，`gpt`/`mbr` 传所选分区偏移；
- **分区镜像挂载只分配一次 loop 设备**（此前为两次：partscan 一次 + 回退一次）；
- **`max_part` 不再影响任何分支**，仅用于次设备号推导；
- WebUI 不再展示分区扫描能力行，也不再出现「将降级」提示。

## Alternatives considered

- **保留 partscan 作为交叉验证手段**：否决，理由见 Proposal 第 3 节。
- **仅修正 `partscan_supported` 的语义（如实探测节点是否存在），保留路径**：否决。字段与路径同源，单独改字段会留下无人使用的策略代码；且 Android 上结果恒为「不可用」，该字段随后没有任何信息量。
- **保留字段但恒报 `false`**：否决。恒为常量的协议字段只会误导消费者，不如移除并在文档中记录移除原因。
- **不删代码，只删 WebUI 文案**：否决。这样问题一（每次挂载的无效开销）依然存在，只是不再被展示——属于把问题藏起来。
- **连同 `max_part` 一起删除**：否决。会破坏 `/dev/block/loopN` 节点补建，导致唯一的挂载路径失效。
- **放弃 `lo_offset` 改回 partscan**：不成立。`lo_offset` 是 AVD 实测唯一可用的路径。

## Acceptance criteria

1. `grep -rn partscan_supported` 在全仓库无结果（`.agents/` 历史记录除外）。
2. `grep -rn partscan crates/` 仅剩：`LO_FLAGS_PARTSCAN` 位常量、`set_status64` 的 `partscan` 参数、`LoopStatus.partscan` 读回，以及与之相关的注释与测试。
3. `LoopControl` trait 不再有 `part_device_exists`；`MemLoop` 不再有 `with_part_device`。
4. `cargo nextest run -p gadgetdisk-loop` 全绿，且以下两条守卫测试通过：
   - `attach::tests::partitioned_image_allocates_loop_device_once` —— 断言 `get_free` 只被调用一次，且成功路径无 `clear_fd`；
   - `attach::tests::max_part_never_changes_the_mount_path` —— `max_part` 取 `0`/`7`/`64` 时调用序列逐字相同。
5. `loopdev::tests::build_status_sets_offset_and_read_only_flag` 仍断言偏移路径下 partscan 位为 0；`loopdev::tests::loop_device_numbers_uses_kernel_assigned_minor` 仍断言 `max_part = 7` 时的次设备号推导。
6. `cargo nextest run -p gadgetdisk-loop -p gadgetdisk-proto -p gadgetdisk-cli -p gadgetdisk-gdd` 全绿。
7. WebUI 设置页不再出现「整盘 partscan」行；`node webui/tests/logic.test.mjs` 与 `structure.test.mjs` 全绿。
8. AVD 实测：`gpt`/`mbr` 镜像挂载成功、`losetup -a` 无残留、日志中无「降级」输出。
9. `verify_agent_gates.py` 门禁全绿。

## Risks

- **协议破坏性变更**：`Capabilities.partscan_supported` 是对外契约字段，删除后旧消费者若仍读取该字段会得到 `undefined`/缺键。本仓库内的消费者（WebUI、`gdd` 测试替身）已同批修改；已在 `docs/protocol.md` 记录移除。**未验证**：是否有仓库外消费者（本模块无 APK，预期无）。
- **失去独立解析对照（已接受）**：移除 partscan 后，分区表解析完全依赖 `gadgetdisk-core::partitions` 的手写 GPT/MBR 实现，失去了「内核解析结果」这一独立校验。缓解手段是纯函数测试与 `verify_mbr`/`verify_gpt` 的写后回读。
- **待验证假设**：有 devtmpfs 的内核上 partscan 是否真的可用——**目前无设备可测**，故不作为保留理由。
- **待验证假设**：恢复成本。若日后需要该路径，需从 git 历史取回 `Strategy` 枚举与 `part_device_exists`；预期成本低，但未实际演练。
- **已知限制**：`Attachment.loop_part_devs` 恒为空。保留而非删除是为了不扩大本次的协议破坏面；**它是残留字段**，若将来确认无消费者，应一并移除。
