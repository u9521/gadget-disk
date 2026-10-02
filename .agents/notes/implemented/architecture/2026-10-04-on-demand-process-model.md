# Agent Note: 按需进程模型与 CLI 持有的运行期状态

Status: implemented
Category: architecture

## Problem

GadgetDisk 将镜像导出为 USB 设备，同时支持在设备本地 loop 挂载同一镜像以供编辑。由于共享同一后备文件，并发写入将破坏文件系统完整性，必须确立严谨可靠的互斥仲裁机制。

原早期架构存在以下局限：常驻 root 守护进程增加系统内存开销；业务逻辑与状态过度集中于进程内存，重启易导致互斥上下文丢失；且导出与本地挂载可能分属不同短命进程，基于进程内存维护资源占用必然与内核实际状态（configfs 与 sysfs）产生漂移。

## Proposal

### 1. 按需进程模型：开机零常驻

| 进程 | 运行周期 | 退出时机 |
|---|---|---|
| `gadgetdisk serve` | WebUI 请求回环 REST 时按需拉起 | 空闲超时（默认 60s）且无 running job（进行中的上传）时退出，并清理 `webroot/api.json` |
| `gdd` | 有镜像导出为 USB 设备期间 | 全部 LUN 弹出且空闲超时（默认 60s）后退出；存在活跃挂载时永不因空闲退出 |
| 一次性 CLI 子命令 | 命令执行期间 | 执行完毕立即退出，无后台遗留进程 |

系统启动阶段仅由 `service.sh` 单次触发 `gadgetdisk boot` 完成对账并立即退出，保证开机后进程列表中无长期常驻守护进程。

CLI 大多数子命令（`status`/`list`/`create`/`delete`/`capabilities`/`attach-loop`/`detach-loop`/`config` 等）均由独立进程就地执行；仅 `mount`/`unmount`/`delete-slot`/`rebind` 通过 Unix Socket 委托 `gdd` 处理，并由 `serve::ensure_gdd` 在需要时自动拉起。

### 2. `gdd` 无状态化与职责收敛

`gdd` 严格收敛为纯 mass_storage 操作：绑定 LUN、按 LUN 介质弹出（保留 LUN 目录与参数）、拆除自身 function 与链接并断开 UDC、重绑 UDC，以及清理自身痕迹。

`gdd` 不持有镜像管理、导入、本地 loop 挂载、能力探测或 USB 身份配置，不读写持久化业务状态文件。所有操作均动态读取 configfs 内核状态，支持随时按需拉起与安全终止。CLI 拥有全部状态与导出意图（`run/state.json`）。

`gdd` 仅保留面向诊断的输出日志（`gdd.log`），不构成业务状态。

### 3. 类型系统与静态扫描双重边界保护

- **类型解耦**：抽象为两个正交 trait。只读视图 `GadgetView` 供 CLI 与 `gdd` 共同使用；写操作 `MassStorageOps` 仅由 `gdd` 独占实现。CLI 无法在类型层面直接篡改 LUN。
- **静态扫描门禁**：通过 `crates/gadgetdisk-gdd/tests/scope.rs` 源码扫描断言，拦截 `gdd` 代码中对身份属性及状态文件的直接引用。

### 4. IPC 协议收敛

协议精简为 CLI 与 `gdd` 之间的 11 条核心消息，剔除无调用方的冗余消息定义：

| id | 消息 | 方向 |
|---|---|---|
| `0x01` | `Error` | `gdd` → CLI |
| `0x10` / `0x11` | `StatusRequest` / `StatusResponse` | 请求 / 应答 |
| `0x20` / `0x21` | `MountRequest` / `MountResponse` | 请求 / 应答 |
| `0x30` / `0x31` | `UnmountRequest` / `UnmountResponse` | 请求 / 应答 |
| `0x32` / `0x33` | `RebindRequest` / `RebindResponse` | 请求 / 应答 |
| `0x34` / `0x35` | `DeleteSlotRequest` / `DeleteSlotResponse` | 请求 / 应答 |

### 5. Socket 通信与跨进程并发控制

- 通信采用 `AF_UNIX` 路径套接字（`run/gdd.sock`），杜绝使用可被任意应用枚举的抽象套接字；目录赋权 `0700 root:root`，连接建立后校验 `SO_PEERCRED UID == 0`。
- 跨进程操作通过 `run/ops.lock` 上的 `flock(LOCK_EX|LOCK_NB)` 串行化，依靠内核在进程退出时自动释放文件锁。
- 统一命名规范：废除通用 `daemon` 称谓，确立 `gdd` 二进制与服务标识。

### 6. 数据目录规整：彻底移除 `state/` 目录

```
/data/adb/gadget-disk/
├── config/gadget.json  # 用户配置意图（USB 描述符与上下文覆盖）
├── run/        # 权限 0700；运行期状态与套接字（gdd.sock, ops.lock, state.json, offsets.json, loop-attachments.json）
├── logs/       # 权限 0700；进程物理隔离日志（cli.log, gdd.log, service.log, serve.log）
└── images/、mnt/、tmp/  # 镜像存储、loop 挂载点与导入暂存
```

### 7. 导出意图与当前内核状态分离

`run/state.json` 仅记录持久化的**导出意图**（供系统重启后恢复 LUN 挂载），不作为实时状态真值；实时挂载状态统一由 `status` 命令动态查询 configfs 与 sysfs。

### 8. 长任务注册表与后端锁隔离

镜像写入（当前只有 `POST /api/v1/upload/*` 分块上传一条路径）沿两个阶段分离：

1. `upload/begin`：同步完成参数校验、目标文件冲突与可用空间预检，并**登记一个
   running job**；
2. `upload/chunk` / `upload/commit`：在请求线程内边读边落盘，进度与终态写入
   `JobRegistry`（`upload/commit` 先 `sync_all` 再原子改名）。

**登记 running job 是必需的，不是记账**：`serve` 判空闲退出看的就是它
（`has_running_jobs`），遗漏登记将导致大镜像上传因服务空闲超时回收而异常中断。

写入过程不持有后端全局锁（`backend.lock`），确保长耗时传输期间 WebUI 的进度轮询
（`GET /api/v1/jobs/{id}`）与状态查询能够毫秒级实时响应（实测响应耗时从 524ms 降至
24ms）。`JobRegistry` 以目标名为键做互斥，因此同一镜像不会同时被上传与 `create` 写。

## Alternatives considered

- **维持常驻守护进程模型**：否决。增加系统长期内存驻留，与权限最小化原则冲突。
- **在 `gdd` 内置 REST 服务**：否决。导致 mass_storage 底层驱动与上层 HTTP 协议栈严重耦合，阻碍按需退出机制。
- **CLI 全量代理至 `gdd` 查询**：否决。只读校验无需经过 IPC 守护进程，分离只读视图（直接查询内核）与写入操作（gdd 独占）可降低耦合与单点故障。
- **state.json 记录实时状态**：否决。易在外部插拔或内核状态改变时失真，当前挂载状态必须动态读取 configfs。
- **状态探测时自动触发重导**：否决。只读状态查询不应附带副作用，且物理断开场景下重试将引入无效错误告警。

## Acceptance criteria

1. 源码扫描测试通过，断言 `gdd` 无越权文件访问与持久状态持有。
2. 协议消息测试通过，严格约束为 11 条核心指令。
3. 长任务契约测试通过：
   - `upload/begin` 立即返回 `upload_id` 且持锁时间小于 30ms；
   - 上传期间 `has_running_jobs()` 为真，`serve` 不判空闲退出；上传结束后可正常退出。
4. 空闲回收与套接字生命周期单测通过。
5. 治理门禁验证通过：`python3 .agents/scripts/gates/verify_agent_gates.py`。

## Risks

- **进程异常终止风险**：发布构建启用 `panic = "abort"`，导入子线程若发生未捕获 panic 将导致 `serve` 进程退出。必须保证导入路径代码杜绝 unwrap 及越界异常。
- **外部清理风险**：极端低内存场景下系统 OOM Killer 可能回收后台服务，导致长耗时导入任务中断。
- **意图与状态暂态不一致**：在设备物理断开期间，意图与实际状态可能出现脱节，WebUI 需提供可操作的引导提示。
