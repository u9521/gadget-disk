# Agent Note: 镜像文件的 SELinux 上下文决定内核能否读写后备镜像

Status: implemented
Category: bug-fix

## Problem

镜像的后备文件由**内核工作线程**读写，而 SELinux 按该线程所属安全域判定其访问权限：

| 使用场景 | 读写者 |
|---|---|
| 导出为 USB 设备（gadget LUN） | `f_mass_storage` 派生的 `file-storage` 内核线程（`kernel_read` / `kernel_write`） |
| 本地 loop 挂载 | loop 工作线程 |
| 经 loop 的格式化（`mkfs`） | loop 工作线程 |

用户态进程运行于 su 域并不代表内核线程具备同等访问权限。若镜像文件继承了未向该域开放读写权限的类型（如默认继承的 `adb_data_file`），会导致两类隐蔽故障：

1. **读权限缺失**：Host 端枚举块设备成功但无法读取分区表与文件系统（报 `critical medium error`）；
2. **写权限缺失**：写操作被静默抑制，底层变更未实际写入镜像文件。此时 `lun.N/ro` 仍汇报 0，guest 操作系统报告写入成功且 dmesg 无明细只读报错，仅能通过校验镜像实际哈希方能发现。

实验证实动态追加 sepolicy allow 规则在部分内核上无法解除 avc 拦截，直接修正镜像文件安全标签为唯一高可靠解法。

**本地 loop 路径同样受限（真机报告）**：真机实测反馈 loop 挂载的镜像同样受内核工作线程所属安全域限制。因此安全标签修正不可局限于 gadget 导出路径，本地 loop 挂载与经 loop 执行格式化在触碰内核**之前**亦须完成检查与修正。该结论目前依据真机现象报告与 gadget 侧实测类推，**独立真机确认列为待验证假设**（见 [路线图](../../../../docs/roadmap.md)）。

## Proposal

在**全部会挂载镜像的路径**上，于触碰内核之前主动检查后备镜像的 SELinux 上下文，必要时修正为允许内核线程双向读写的类型。默认目标为 `u:object_r:media_rw_data_file:s0`。

### 1. 内核读写权限真值基准（x86_64 AVD / Android 17）

通过 `/sys/fs/selinux/access` 查询并结合 guest 写入后镜像 md5 校验和交叉验证：

| 上下文 | kernel `read` | kernel `write` | guest 写入落盘 | 新增 avc 拦截 |
|---|---|---|---|---|
| `u:object_r:media_rw_data_file:s0` | ALLOW | ALLOW | YES | 0 |
| `u:object_r:system_file:s0` | ALLOW | DENY | NO（哈希不变） | 10（均为 `denied { write }`） |
| `u:object_r:vendor_file:s0` | DENY | DENY | 不可用 | — |
| `u:object_r:adb_data_file:s0`（继承默认） | DENY | DENY | 不可用 | — |

读写权限必须同时满足：若仅验证读权限（如误选 `system_file`），将导致写入数据在不报错的状态下被静默丢弃。

### 2. 三条路径与统一的检查入口

检查逻辑单点收敛于 `crates/gadgetdisk-cli/src/image_context.rs`（判定在 `src/selinux.rs`），三条路径共用，避免任一侧漏改：

| 路径 | 入口 | 修正时机 |
|---|---|---|
| gadget 导出 | `run_mount` / `serve::gdd_op` | 写 `lun.N/file` 之前 |
| loop 本地挂载 | `run_attach_loop` / `serve::loop_attach` | `LOOP_SET_FD` 之前 |
| 经 loop 的格式化 | `MkfsFormatter::format` | 第一个 `losetup` 之前 |

**执行时序具有严格约束**：内核在打开后备文件时即按当时的安全标签锁定访问权限（pin），后续修改对当前已建立的挂载无效。该顺序在主机开发环境不可观测（无 loop 块设备），故由调用点源码断言机制予以保障（`attach_loop_fixes_the_context_before_touching_the_kernel`、`create_passes_the_image_context_to_the_formatter`），与 `gadgetdisk-gdd/tests/scope.rs` 采用相同的验证手法。

`MkfsFormatter` 通过 `take_warnings` 将告警信息交付调用方，而非扩充 `gadgetdisk-core` 的 `FormattedVolume` 契约——后者属于 core 层通用类型，避免侵入核心定义及全部测试替身。

### 3. 目标上下文动态覆盖与两条写通道

默认安全标签定义为 `media_rw_data_file`，支持在 `config/gadget.json` 的 `image_context` 字段中自定义覆盖，以适配不同厂商定制 ROM 的 SELinux 策略差异。两条写入通道：

- CLI：`config security get|set|clear`；
- REST：`GET|POST /api/v1/config/security`（WebUI「设置与诊断 → 镜像安全上下文」）。

两者共用 `selinux::validate_context_format`（格式形态校验：非空、含 `:`、无空白/控制字符、≤256 字节）与 `GadgetConfig::store_image_context`（原子读—改—写）。**先校验再落盘**：非法值一旦持久化，后续每次挂载均将重新加载并再度失败。安全上下文与 USB 身份配置同存于 `config/gadget.json`，因此直接整文件覆盖将导致彼此的配置被静默清除——这与既有的 `store_identity` 是同一架构不变量的对称约束。

### 4. 实现边界与保护机制

- 读取 `security.selinux` 扩展属性，非 SELinux 设备静默跳过；
- **目录隔离保护**：非 `images/` 目录下的外部路径仅输出告警，绝对不执行标签修改，避免破坏宿主其他业务依赖；
- `images/` 目录下的镜像若安全标签不匹配则执行修正；标签修改失败仅告警，不阻断挂载/格式化；
- 告警随应答回报（gadget 与 loop 侧写日志与 stderr，loop 挂载/创建的 REST 应答含 `warnings`），界面显示中文结论 + 英文明细。

### 5. 内核行为特征与生效约束

- **严禁依赖 `lun.N/ro`**：`fsg_lun_open` 失败时回退但属性回显逻辑存在特殊性，写入受阻时该属性仍可能读到 0；
- **标签修正须在打开前完成**：内核在向 `lun.N/file` 写入路径（或 loop 打开后备文件）时以当时的上下文打开并锁定文件，已处于活跃状态的挂载无法动态刷新标签生效；
- **只读挂载同样应用目标标签**：不区分 ro/rw 方可保障“先只读挂载、随后改读写”流程无缝衔接且无需二次修改标签（取舍见 Risks）。

## Alternatives considered

- **动态注入 sepolicy allow 规则**：否决。实测规则被应用后底层 AVC 拦截依然存在，且全局规则膨胀易引发系统级连锁拒绝。
- **用户态自研 USB Mass Storage 协议栈**：否决。在用户态实现 SCSI/BOT 状态机复杂度过高且丢失内核页缓存与调度优化。
- **将镜像迁移至系统公共路径（如 `/sdcard`）**：否决。FUSE 文件系统性能不足且破坏模块目录边界。
- **采用 `system_file` 标签**：否决。仅允许读权限，写入被静默抑制。
- **侵入式修改 `/vendor` 系统策略**：否决。违背免改系统分区的模块化分发原则。
- **只在 gadget 导出路径修正，loop 侧沿用现状**：否决。真机实测报告表明 loop 挂载同样受限；遗漏单侧路径的后果（读取失败、写入静默丢失）极难从表面现象反推根因。
- **把上下文检查下沉到 `gadgetdisk-loop`**：否决。该 crate 仅应专注内核 loop 接口（ioctl、`mount(2)` 与释放时序），不应为扩展属性操作引入配置依赖；检查收敛于外部调用方并在打开前完成。
- **把告警塞进 `gadgetdisk-core` 的 `FormattedVolume`**：否决。为单条诊断信息扩充 core 层通用类型契约将侵入全部相关测试替身，收益不成比例。
- **复用 `/api/v1/config` 承载上下文写入**：否决。USB 身份为按字段增量合并，而安全上下文为完整取值替换，共用端点存在误覆盖身份配置的风险。

## Acceptance criteria

1. 单测断言默认上下文必须为 `media_rw_data_file`，且显式断言不为 `system_file`（`default_context_must_allow_kernel_write`）。
2. 镜像目录越界防护测试全部通过（外部文件只告警不修改，`files_outside_the_images_dir_are_never_relabeled`、`outside_the_images_dir_never_blocks_formatting`）。
3. 配置文件支持正确解析并持久化 `image_context`，且两个方向都不互相清空（`store_identity_preserves_image_context`、`store_image_context_preserves_identity`）。
4. 输入校验拒绝非法值且不落盘：`validate_context_format_rejects_obviously_wrong_input`、`config_security_rejects_invalid_input_without_persisting`。
5. 调用顺序由源码断言机制保障：`attach_loop_fixes_the_context_before_touching_the_kernel`、`create_passes_the_image_context_to_the_formatter`（主机上无 loop 设备，顺序不可用行为断言表达）。
6. 端到端写入验收（真机）：挂载后在 Host 端写入数据，比对设备端镜像文件校验和必须发生改变，dmesg 无新增 AVC 拒绝。
7. 治理门禁验证通过：`python3 .agents/scripts/gates/verify_agent_gates.py`。

## Risks

- **定制 ROM 策略差异**：特定 OEM 定制系统若对 `media_rw_data_file` 施加了更严格的限制，需通过配置文件自定义覆盖标签。
- **生命周期时序**：标签修改仅对后续新挂载生效，已在导出/挂载中的镜像需重新挂载方可应用新权限。
- **只读挂载亦同步更新标签**：即使用户指定 `--mode ro`，后备镜像亦会被修正为目标标签。理由是不区分 ro/rw 方可保障“先只读检视、后续改读写”无需繁琐地二次调整标签；其代价是仅有单次只读检视需求的文件亦会被修改标签。该行为严格限制在模块专有的 `images/` 目录下，属于经过权衡的**明确取舍**。
- **loop 侧限制为待验证假设**：真机报告 loop 挂载同样受 SELinux 域限制，但当前证据为现象报告 + gadget 侧实测类推，**尚未在真机上独立确认**（见 [路线图](../../../../docs/roadmap.md) 待验证假设 #25 / #26）。若真机确认不成立，多出的修正是一次无害的幂等 xattr 写。
- **格式化告警不阻断创建**：镜像已创建成功但标签未改时，用户会在后续挂载/格式化时再次遇到同一问题；告警因此必须随应答回报（已实现），否则只剩日志可查。
