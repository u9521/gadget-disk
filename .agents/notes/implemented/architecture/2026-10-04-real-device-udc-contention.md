# Agent Note: 真机 UDC 抢占与 `gdd` 的 gadget 守卫职责

Status: implemented
Category: architecture

## Problem

在 AVD 虚拟机验证通过的 gadget 导出路径，在部分 Android 实体设备（如红魔9 Pro，Android 12，UDC `a600000.dwc3`）上挂载时，Windows 设备管理器提示“该设备无法启动（代码 10：指定不存在的设备）”。

内核审计日志（dmesg）显示 Android `init` 进程高频轮询重配 UDC：
早期模块实现遍历删除了配置目录下的所有符号链接（误删了系统框架的 `f1..f3` 链接），导致系统回收脚本在执行 `rm` 时触发 `ENOENT`；后续触发的 `symlink` 与写 `UDC` 分别报 `EINVAL` 与 `EBUSY`，造成系统状态机与本模块互相死锁，UDC 卡滞于 `addressed` 状态。
此外，由于 Android USB 重配逻辑直接嵌入 init 触发器且系统无独立 HAL 守护进程，试图通过 SIGSTOP 挂起外部进程的策略无效；且默认开启的 `os_desc/use` 会导致 Windows 尝试加载 MTP 描述符。

## Proposal

### 1. 严格约束符号链接管理边界

仅增删指向本模块 function 的符号链接（`mass_storage.gadget-disk`），保留系统原生链接（`f1..f3`），消除系统 `init` 脚本在回收时的 `ENOENT` 异常，解除状态互锁。

### 2. 紧凑执行时序

按「清空 UDC -> 立即重建自有链接 -> 等待并校验 state == configured」时序执行，将配置窗口压缩至系统重配轮询周期之内。

### 3. 动态探测路径与 UDC 控制器

通过 `sys.usb.controller` 属性与 configfs 拓扑动态匹配目标 gadget 根路径与 UDC 实例名称，多候选且无法明确仲裁时显式返回 `configfs_unavailable` 报错，严禁盲猜。

### 4. 介质弹出保持 UDC 绑定

介质弹出（eject）时仅清除 LUN file 属性并解绑自有链接与 function，保持 UDC 当前绑定状态，防止触发 Android `init` 的全量重配导致总线抖动；仅在全量卸载或模块移除时才解绑 UDC。

### 5. 挂载阶段禁用 `os_desc/use`

挂载前将 `os_desc/use` 置 `0`，原配置备份至 `run/gadget-backup.json`，全量卸载时恢复，防止 Windows 驱动枚举冲突。

### 6. 就绪状态以内核真值为准

状态响应字段 `effective` 严格以 `/sys/class/udc/<udc>/state == configured` 为准，严禁仅以 `lun.0/file` 写入成功判定就绪。

## Alternatives considered

- **挂起 USB HAL 守护进程**：否决。实机环境由 PID 1 (init) 直接执行属性动作驱动重配，无独立 HAL 进程可供拦截。
- **提前至 post-fs-data 阶段执行**：否决。无法规避 init 持续性的重配触发器，根本原因在于系统符号链接被误删引发的状态互锁。
- **添加 sepolicy 规则限制 init**：否决。侵入式修改系统全局安全策略风险过高。
- **改写 `sys.usb.config` 属性**：否决。接管整机 USB 状态机会破坏 ADB 与其他复合功能。
- **多 gadget 候选时回退盲猜**：否决。误写框架原生 gadget 会导致间歇性总线故障，直接返回 `configfs_unavailable` 明确阻断更符合确定性原则。

## Acceptance criteria

1. 单测断言拆除流程仅删除自有符号链接，保留系统 `f1..f3` 链接。
2. `verify_bound_*` 系列测试断言 UDC 未达 `configured` 时不返回成功。
3. 单测覆盖 `os_desc/use` 的禁用与卸载恢复时序。
4. 真机测试确认 Windows 设备管理器无代码 10 告警，UDC 状态稳定处于 `configured`。
5. 治理门禁验证通过：`python3 .agents/scripts/gates/verify_agent_gates.py`。

## Risks

- **待验证假设**：真机 `init` 行为随厂商定制 ROM 存在差异，特定低性能设备上 3 秒重绑超时可能需调整。
- **待验证假设**：不同主机操作系统对微软 OS 描述符（OS Descriptors）的处理机制存在差异。
