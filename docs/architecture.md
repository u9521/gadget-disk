# 架构

## 总览

**按需进程模型**：开机不常驻任何进程。WebUI 主通道是回环 REST，`ksu.exec` + CLI + socket 是回退通道。`gdd` 只在**有镜像被导出为 USB 设备**期间存在。

```
┌──────────────┐  ① 主：fetch http://127.0.0.1:<port> + Bearer token
│  WebUI (H5)  │     密钥与端口经同源 webroot/api.json 取得
│  webroot/    │ ───────────────────────────────────────────────┐
└──────────────┘  ② 回退：ksu.exec → stdout JSON               │
        │                                                      ▼
        │                                     ┌────────────────────────┐
        │                                     │ gadgetdisk serve       │
        │                                     │ (按需, 空闲 60s 退出)   │
        │                                     └───┬────────────────────┘
        │                                         │ loop ioctl + mount(2)
        │                                         │ 镜像创建/删除/查询 / 分块上传
        │                                         │ 分区表解析 / mkfs（经 loop）/ df
        │                                         │ 身份配置（idVendor/strings）
        │                                         │ run/state.json（导出意图）
        ▼                                         │
┌──────────────────────┐                          │
│ gadgetdisk cli       │                          │
│ (一次性进程, su)      │                          │
└──────────┬───────────┘                          │
           │  AF_UNIX 路径 socket + SO_PEERCRED   │
           │  仅 mount / unmount / status / rebind│
           ▼                                      │
┌──────────────────────┐  ◄───────────────────────┘
│ gdd                  │   只做 mass_storage：
│ (仅导出期间存在)      │   绑 LUN / 弹出 / 拆除 / 重绑 UDC
└──────────┬───────────┘
           │
    configfs /config/usb_gadget/g1
           ▼
      kernel UDC

互斥判据（两侧都读**内核真值**，不靠进程内状态）：
  gadget 侧   configfs functions/mass_storage.gadget-disk/lun.N/file
  loop 侧     /sys/block/loopN/loop/backing_file
```

**`gdd` 只做 mass_storage 挂载**；镜像创建/删除/查询、分块上传、loop 挂载、
能力探测、USB 身份（`idVendor`/字符串）与全部状态文件都归 CLI。这条边界由类型
（CLI 只实现只读的 `GadgetView`）与源码扫描测试双重保证，见
[gdd 拆分 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

设计理由（为何按需、为何引入 REST、为何 hand-roll HTTP）见 [按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)；socket 与消息格式见 [协议](protocol.md)；相关 IPC 设计推演参见上述 Note。

## 运行期布局

**模块目录**（由安装器设置权限与 SELinux 上下文）

```
/data/adb/modules/gadget-disk/
├── module.prop
├── service.sh
├── uninstall.sh
├── bin/gadgetdisk                # 主 CLI / REST 后端（按需）
├── bin/gdd                       # mass_storage 执行进程（按需，无状态）
├── bin/mkfs.vfat                 # 自带 FAT32 格式化工具（设备上无 dosfstools）
├── webroot/
│   ├── index.html
│   ├── main.js                   # 入口：初始化、tab 切换、事件绑定、全局兜底
│   ├── backend.js                # 通道：api.json 探测、REST/CLI 分派、按需拉起 serve
│   ├── dom.js                    # DOM 与错误面板助手
│   ├── task.js                   # 忙碌态与任务反馈
│   ├── ksu.js                    # window.ksu 的 Promise 封装
│   ├── view-*.js                 # 六个视图各一模块（mount/create/import/images/edit/settings）
│   ├── pure/                     # 纯函数层（bytes/paths/describe/partitions/channel/task）
│   ├── style.css
│   └── api.json                  # 运行期由 serve 写入（0600，退出即删），不入包
```

> **视图模块必须平铺在 `webroot/` 顶层**：打包脚本只非递归扫描顶层文件（`pure/`
> 是唯一被显式登记的子目录）。放进 `views/` 之类的子目录会导致**本机测试全绿、
> 打包却丢文件**，症状只在设备上出现。

> **包内是 `bin/<abi>/`，安装后是扁平的 `bin/`**：`customize.sh` 探测架构后把匹配
> 的那份 `gadgetdisk`、`gdd` 与 `mkfs.vfat` 移到扁平位置并删除其余 ABI 目录。
> 运行期只有一条路径——WebUI 读不到 `ro.product.cpu.abi`，运行期探测猜错会让所有
> 命令失败且症状是「后端不可达」。见 [构建与发布](build-and-release.md)。
>
> **没有 `post-fs-data.sh`**：该阶段已删除，其职责归 `service.sh` +
> `gadgetdisk boot`（见下）。
>
> **不带 `sepolicy.rule`**：`bin/gadgetdisk`、`bin/gdd` 与 `bin/mkfs.vfat` 都跑在 root
> 管理器的 `su` 域，模块不携带 `sepolicy.rule`，也不规划独立 SELinux 域。socket 的
> 边界由 `0700` 目录与 `SO_PEERCRED` 提供，不要为「加固」重新引入。
>
> **WebUI 源在仓库顶层的 `webui/`**：打包时把 `webui/*`（不含 `tests/`）复制成模块的
> `webroot/`，因此 `webui/` 与运行期的 `webroot/` 是同一批文件；KernelSU 的入口常量
> 固定为 `webroot/index.html`。

**数据目录**

```
/data/adb/gadget-disk/
├── images/     # 用户镜像
├── run/        # 0700 root:root；运行期状态（socket、锁、意图、缓存、日志）
│   ├── gdd.sock                # UDS
│   ├── ops.lock                # 跨进程 flock
│   ├── state.json              # 导出意图（CLI 写；重启恢复的唯一依据）
│   ├── gadget-backup.json      # Android 原始身份（CLI 写）
│   ├── offsets.json            # 分区偏移缓存（CLI 写）
│   └── loop-attachments.json   # 活跃 loop 附件登记（CLI 写）
├── logs/       # 0700 root:root；日志
│   ├── cli.log                 # CLI 自身（含镜像上下文警告）
│   ├── gdd.log                 # gdd（路径由 CLI 经 --log-file 传入）
│   ├── service.log             # 开机对账
│   └── serve.log               # serve 的标准流（WebUI 拉起时重定向）
├── config/     # 持久配置（用户可见、可手改）
│   └── gadget.json             # VID/PID/制造商/产品/序列号 + image_context
├── mnt/        # loop 挂载点
└── tmp/        # 分块上传过程中的临时文件（完成后原子改名）
```

**`run/`、`logs/` 与 `config/` 的分工**：`config/` 是**用户意图**（删掉只意味着
回到 Android 默认身份），`run/` 是**运行期事实与意图**（删掉会丢失「上次导出到哪」），
`logs/` 是**诊断输出**（删掉只影响排查）。此前两者混放于 `state/` 目录下，导致无法明确区分持久配置与易失状态。理由见
[run state Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

日志格式统一为 `2026-10-04T06:12:33Z [INFO] 消息`（ISO 8601 + 级别 + 消息正文），
超过 256 KiB 轮转一代（保留 `.1`）。CLI 与 `gdd` 分别输出至 `cli.log` 与 `gdd.log`，
实现日志物理隔离，保证独立进程的时序因果链清晰可溯。

> 不要手动设置 `webroot` 的权限或 SELinux 上下文，安装器会自动处理。

## Crate 划分

按**可测试性边界**切分，而非按传统分层。

| crate | 职责 | 主机可测 |
|---|---|---|
| `gadgetdisk-core` | 镜像创建（GPT/MBR/FAT32）、容量与对齐计算、分区偏移、只读文件系统查询（`fsinfo`） | ✅ 完全 |
| `gadgetdisk-proto` | 消息编解码、版本握手、帧读写、**共享的 `VERSION` 常量**（`gadgetdisk` 与 `gdd` 的自报版本来自这里，由构建脚本注入） | ✅ 完全 |
| `gadgetdisk-usb` | configfs 读写、UDC 选择、gadget 备份与还原 | ⚠️ 抽出 `ConfigFs` trait 后部分可 |
| `gadgetdisk-loop` | loop ioctl、`mount(2)`/`umount`、能力探测、释放顺序 | ⚠️ 抽出 `LoopControl`/`Mounter` 后可测决策与顺序 |
| `gadgetdisk-gdd` | socket 服务（UDS）、全局锁、mass_storage 编排（绑/弹/拆/重绑）、**空闲退出** | ✅ 编排逻辑完全（经 `MassStorageOps` 替身） |
| `gadgetdisk-cli` | 子命令解析、JSON 输出、退出码、**REST 传输（`http`/`rest`/`serve`）**、**分块上传（`upload`）**、loop 挂载、身份配置、导出意图、开机对账、`df`、**`mkfs` 探测与经 loop 的格式化执行**、**镜像 SELinux 上下文（`image_context`/`selinux`：三条挂载路径共用的挂载前修正）** | ✅ 完全 |
| `gadgetdisk-mkfsvfat` | 自带的 FAT 格式化工具（独立二进制 `mkfs.vfat`，命令行对齐 dosfstools） | ✅ 完全 |

**关键设计**：`gadgetdisk-usb` 与 `gadgetdisk-loop` 把内核访问抽象为 trait。生产实现操作真实 `/config` 与 `/dev`，测试实现使用内存结构或临时目录。这样**最高风险的「操作顺序」也能在主机上断言**，例如：

- configfs 的 `file` 属性必须**最后**写入；
- 重新挂到 gadget 前必须完成 `sync` → `umount` → `LOOP_CLR_FD` → 校验；
- loop 的 `umount` 失败时**绝不**执行 `LOOP_CLR_FD`
  （否则底层设备被从已挂载的文件系统下抽走）；
- 只有一条挂载路径（`lo_offset` 分区偏移）；挂载失败必须清掉已分配的 loop；
- 同一镜像不得同时出现在 gadget LUN 与 loop 附件中。

`gadgetdisk-gdd` 另有一层**独立于上述两个 crate 的能力抽象**，且按
**只读/可写**拆成两个 trait：

| trait | 谁能用 | 能力 |
|---|---|---|
| `GadgetView` | `gdd` 与 CLI | **只读** configfs 真值（UDC、LUN、某镜像是否在导出） |
| `MassStorageOps` | **仅 `gdd`** | 绑 LUN / 弹出 / 拆除 / 重绑 UDC |
| `LoopOps` | `gdd` 与 CLI | **只读**（能力探测、loop 附件） |

`gdd` 只依赖 `gadgetdisk-proto` 与 `gadgetdisk-usb`，底层能力在它的
`usb_adapter` 处组装。依赖方向因此无环，且编排逻辑可完全在主机测试。
理由见
[daemon 编排 Note](../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)
与 [gdd 拆分 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

**注意三个 trait 中只有 `MassStorageOps` 可写**：loop 挂载与身份配置都在 CLI，
CLI 侧只实现 `GadgetView`，因此它在**类型层面**不可能绕过 `gdd` 去改 LUN。

REST 传输的 HTTP 层（`http.rs`）是**手写**的，只覆盖需要的最小子集；
`rest.rs` 是路由与鉴权（与具体内核能力解耦，故可主机测试）；`serve.rs` 是
把两者接到真实能力的装配点（`LiveBackend`）。见
[按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

workspace 根 `Cargo.toml` 统一 `edition`、依赖版本与 `license = "GPL-3.0-only"`。

## 数据流

**USB 挂载（gadget 方向）**

1. WebUI 经 REST `POST /api/v1/mount`（或回退：`ksu.exec` 调 CLI）。
2. `serve` 先应用身份（若 `config/gadget.json` 存在）、再转发给 `gdd`（必要时先把它拉起），`gdd` 校验 `SO_PEERCRED`。
3. `gdd` 加锁 → 校验镜像（存在、常规文件、同一镜像不得出现在两个 LUN）→ 按「准备阶段 + 条件性紧凑段」重配 configfs（见 [Android 集成](android-integration.md)）。
4. CLI 用 `gdd` 返回的**内核真值**原子写 `run/state.json`（导出意图）。
5. 返回 JSON 状态，WebUI 刷新。

**本地编辑（loop 方向）**

1. WebUI 先确认镜像未作为 gadget LUN 挂载（`serve` 读 configfs 真值），否则要求先卸载。
2. **`serve` 就地执行**（不经 `gdd`）：解析镜像布局取得分区偏移 —— `raw` → 偏移 `0`（整盘）；`gpt`/`mbr` → 所选分区的偏移。统一走 `lo_offset`。
3. `mount(2)` 到 `/data/adb/gadget-disk/mnt/<name>`（默认加入 init 全局 mount namespace 以最大化可见性）。
4. 用户在文件管理器中编辑；完成后 `detach-loop` 执行 `sync` → `umount` → `losetup -d`。

**镜像创建**

1. WebUI 经 REST `POST /api/v1/create`（或回退：`ksu.exec` 调 CLI `create`）。
2. `create::build` 按布局求解出**纯数据**的写入计划：GPT/MBR 的分区项、每个分区的
   绝对偏移与容量、EBR 链位置。位置与容量**全部由求解器算出**，用户只表达意图
   （大小/类型/归属），因此区间重叠校验与剩余容量约束均可通过纯函数在开发主机上进行穷举测试。
   见 [磁盘镜像格式](disk-image-format.md)。
3. 目标已存在时**直接拒绝**（`already_exists`，HTTP 409）——覆盖是不可逆的数据丢失。
4. 写入分区表，然后**逐分区**格式化：`losetup -o <offset> --sizelimit <len>` →
   `mkfs`（FAT32 用自带 `bin/mkfs.vfat`，exFAT/ext4 用系统工具）→ `losetup -d`。
   **必须串行**（loop 是有限资源），且各工具的偏移能力差异只在 `losetup` 一处表达。
5. 写入完成前失败会删除半成品；格式化失败的镜像不会被登记。

**镜像上传**

1. WebUI 用**系统文件选择器**取得 `File` 对象。JavaScript 运行环境**无法获取本地文件路径**（亦无法直接访问
   `content://` URI），因此只能由前端读取二进制字节流并通过 REST 接口传输至后端——见 [上传与导入](image-upload-and-import.md)。
2. `POST /api/v1/upload/begin` 受理并**登记一个 running job**（防止 `serve` 进程因空闲超时退出而导致大文件上传异常中断），返回 `upload_id`。
3. `POST /api/v1/upload/chunk?upload_id=&offset=` **流式**追加字节，写入 `tmp/<upload_id>.part`。
   偏移必须与已写长度一致（**顺序追加，不留空洞**）。请求体不设 HTTP 层上限。
4. `POST /api/v1/upload/commit` **原子改名**到 `images/`，返回 `job_id`；
   UI 轮询 `GET /api/v1/jobs/{id}` 获取进度。失败或取消走 `upload/abort` 清理暂存。
   分块上传**只有 REST 通道**（`ksu.exec` 无法向目标进程的标准输入流式写入数据）。

## 并发与一致性

- **进程内**：`gdd` 持有 `GlobalLock`（try-lock），串行化 configfs 操作；持锁期间第二个请求返回 `busy` 而非竞态写入（`service.sh` 与 `serve` 是两个进程，故还需跨进程锁，见下）。
- **跨进程**：`flock(LOCK_EX|LOCK_NB)` on `run/ops.lock`（`oplock`）。`serve`/CLI 与 `gdd` 是不同进程，都需要串行化。选择 `flock` 而非锁文件机制，是因为在进程异常终止或退出时内核会自动释放文件锁，不会留下陈旧锁；锁文件方案必须自己判断 pid 存活，而 pid 会被复用。
- **gadget 与 loop 的互斥**：双方均基于内核真值（configfs 与 sysfs）动态判定，无进程间共享内存状态机。即使 `gdd` 或 CLI 进程退出重启，镜像占用状态依然以内核真实挂载点为准，规避了状态缓存漂移风险。
- 长任务（镜像创建、分块上传）不在锁内完成，只在其状态转换的临界区持锁。

## 进程生命周期

| 进程 | 何时存在 | 何时退出 |
|---|---|---|
| `gdd` | 有镜像被导出为 USB 设备时 | 卸载后空闲 `--idle-timeout`（默认 60s）；**有挂载时永不因空闲退出** |
| `gadgetdisk serve` | WebUI 首次访问（经 `ksu.exec` 引导）到空闲 | 空闲 `--idle-timeout`（默认 60s）；有 **running job**（进行中的上传）时不退出 |
| 一次性 CLI | 单次调用 | 立即 |
| `gadgetdisk boot` | 开机时由 `service.sh` 调用一次 | 立即 |

因此**开机后没有任何常驻进程**：`boot` 完成对账后立即退出，`serve` 与 `gdd` 均按需唤起。

## 失败与恢复

- **开机对账**：`service.sh` 仅单次触发 `gadgetdisk boot`，由其读取 `run/state.json`（导出意图）与内核真值执行幂等收敛：存在有效意图则恢复导出，仅存残留则清理，均无则跳过。对账失败保留意图供下次重试，不再使用基于 shell 的状态标记与熔断计数。详见 [Android 集成](android-integration.md) 与 [按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。
- **loop 遗留清理**：设备重启或进程被杀会残留 loop 设备与挂载点；`LoopMounts::new` 在使用前检查并清理**后备文件位于本模块 `images/` 下**的挂载点（严格避免影响系统或其他模块的挂载资源）。
- **上传中断**：临时文件与最终文件分离，避免半成品被当作完整镜像。

## 目标环境与构建约束

构建系统遵循以下目标平台与工具链约束：

- NDK 由公认环境变量探测（**只认 `ANDROID_NDK_HOME` 与 `ANDROID_HOME`**），**不硬编码任何路径**；
  prebuilt host 目录通过动态扫描实际包含 clang 包装脚本（wrapper）的路径确定，不枚举固定的目录名。
- NDK 必须与构建宿主**同平台**：Linux 宿主用 Linux 版 NDK，其 clang 是原生可执行文件，
  直接解析 rustc 传入的路径。**仓库不提供链接器覆盖机制**，也不做跨平台路径翻译——
  在 Linux 宿主上指向 Windows 版 NDK 属于错误配置（其 clang 属于调用 `clang.exe` 的包装脚本，
  无法解析 Linux 格式的路径参数），正确做法是换成同平台 NDK。
- 静态 `aarch64-linux-android` 与 `x86_64-linux-android` 二进制**已验证可编译并在设备上执行**。
- 构建使用用户默认的 `CARGO_HOME`（`~/.cargo`），仓库内不再自建 `CARGO_HOME`。
- 已安装 Rust target：`aarch64-linux-android`、`x86_64-linux-android`。`armv7-linux-androideabi` **未安装**，故 MVP 不支持 armv7。

详见 [构建与发布](build-and-release.md)。
