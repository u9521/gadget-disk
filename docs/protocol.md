# 通信协议

WebUI 有两种到后端的通道：**主通道是回环 REST**（经 `gadgetdisk serve`），**回退通道是 `ksu.exec` + CLI + AF_UNIX socket**。本文件定义两者的传输、访问控制与消息格式。设计理由与否决方案参见 [按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

> **AF_UNIX 通道是 CLI ↔ `gdd` 的私有通道，不是 WebUI 的通用后端。**
> `gdd` 只做 mass_storage 挂载（绑 LUN、弹出、拆除、重绑 UDC），因此协议里
> **只有**这些消息。镜像增删查、导入、loop 挂载、能力探测、身份配置全部由 CLI
> **就地执行**，不经过任何 IPC。

## 传输与访问控制

### 通道一（主）：回环 HTTP

| 项 | 取值 |
|---|---|
| 类型 | HTTP/1.1 over TCP，**仅绑 `127.0.0.1`** |
| 端口 | 临时端口（`bind :0`），写入 `webroot/api.json` |
| 认证 | `Authorization: Bearer <token>`，常数时间比较 |
| token | 每次 `serve` 启动用 `/dev/urandom` 生成 32 字节（64 位十六进制） |
| 投递 | `{port, token}` 原子写入模块 `webroot/api.json`（`0600`），退出即删 |
| CORS | 仅回显 `https://mui.kernelsu.org`（不用 `*`） |
| 空闲退出 | 默认 60s 无请求即退出（有导入任务在跑时不退） |
| 请求延迟 | 非阻塞 `accept` 轮询间隔 **25ms**（与空闲超时**无关**） |

**绝不使用 `localhost`，必须用 `127.0.0.1`**：实测前者解析到 IPv6 `::1` 而失败（`Failed to fetch`）。

**accept 轮询与空闲超时解耦**：监听循环的非阻塞 `accept` 轮询间隔固定为 25ms，保证请求接入排队延迟在 30ms 以内；空闲超时独立由 `last_activity.elapsed() >= idle_timeout` 判定，避免轮询步长与超时阈值耦合导致的高延迟排队。

`api.json` 是唯一能让 WebView 拿到密钥、而其他应用拿不到的通道：WebView 经 `WebViewAssetLoader` 从 `webroot/` 同源读取；其他应用既无法遍历 `/data/adb`（`0700 root`），也无法解析 `mui.kernelsu.org` 这个合成源。

### 通道二（回退）：AF_UNIX 路径 socket

| 项 | 取值 |
|---|---|
| 类型 | `AF_UNIX` **路径** socket（非抽象套接字） |
| 路径 | `/data/adb/gadget-disk/run/gdd.sock` |
| 目录权限 | `0700`，owner `root:root` —— **无 `x` 权限即无法连接**，主防线 |
| socket 权限 | `0600`（权限位对 socket 有效，但不构成访问控制，仅作加固） |
| 对端校验 | `accept` 后读 `SO_PEERCRED`，uid 必须为 `0`，否则立即关闭 |
| 生命周期 | 仅当**有镜像被导出为 USB 设备**时存在；卸载后空闲自动退出（`gdd` 无状态，可随时被杀） |

### 为何不用抽象套接字

抽象套接字不出现在文件系统中，**任何应用都能从 `/proc/net/unix` 枚举名字并直接 `connect()`**。权限位完全失效，SELinux 成为唯一防线。路径 socket 使非 root 进程在文件系统层面即被 `0700` 目录挡住。

### REST 通道安全模型与约束

设备端 WebView 环境实测允许对 `127.0.0.1` 发起跨源 fetch 请求，混合内容策略不构成阻断。但 Android 系统中回环端口对所有 UID 开放，普通应用（如 shell UID 2000）均可通过 `/proc/net/tcp` 枚举端口并直连。

因此 Bearer Token 是必须的鉴权防线：每次 `serve` 启动时生成 32 字节高熵令牌，仅通过权限为 `0600` 的 `webroot/api.json` 投递给同源 WebView，外部非 root 进程因文件系统权限拦截无法读取，从而阻断未授权访问。

REST 取代 AF_UNIX + CLI 作为 WebUI 主通道的核心价值：`serve` 空闲即退出（无常驻 root 进程），且 WebUI 避免了高频 fork 进程的性能损耗。

### 陈旧 socket 处理

bind 前先尝试 `connect()`：

- 连接成功 → `gdd` 存活，本次启动放弃并退出；
- 返回 `ECONNREFUSED` → 属残留文件，删除后重新 bind。

`gdd` 退出时 `Listener` 的 `Drop` 会 unlink socket，二者的兜底关系见 [按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

## 连接模型

**一请求一连接**：`connect` → 握手 → 请求 → 响应 → `close`。无跨请求特权状态。

长任务（镜像导入）在 **REST 通道**上不在单次连接内完成：请求立即返回 job id，后续用 `JobStatus` 轮询（见「长任务（job）」）。

## 握手

| 方向 | 内容 |
|---|---|
| client → `gdd` | `u8` 协议版本，当前为 `1` |
| `gdd` → client | `u8`：`1` = 支持；`0` = 不支持（client 应报告版本不匹配） |

版本不匹配时 `gdd` 以 `0` 应答，而非静默断开。

## 帧格式

```
+--------+------------------+---------------------+
| u8 id  | u32 LE length    | length 字节 UTF-8 JSON |
+--------+------------------+---------------------+
```

- `id`：消息类型（见下表）。
- `length`：JSON 负载字节数。实现须校验上限并拒绝异常大的声明长度，避免内存放大。
  **上限为 1 MiB（`MAX_FRAME_BYTES = 1024 * 1024`）**：本模块全部负载都是小型
  状态描述，1 MiB 已有两个数量级余量；上限的作用是让「声明超大长度」在
  **分配内存之前**即被拒绝。
- 所有消息（含错误）均使用 JSON 负载，便于 CLI 调试与 WebUI 直接消费。

## 消息表

AF_UNIX 协议收敛边界：该通道严格限定为 CLI 与 `gdd` 之间 mass_storage 生命周期的私有控制链路。镜像管理、本地挂载及能力探测等操作均由 CLI 就地执行，不经过 socket IPC。协议报文严格限定为以下 11 条：

| id | 名称 | 方向 | 负载 |
|---|---|---|---|
| `0x01` | `ErrorResponse` | `gdd` → client | `{code, message}` |
| `0x10` | `StatusRequest` | → `gdd` | `{}` |
| `0x11` | `StatusResponse` | → client | `{udc, devices:[LunInfo]}` |
| `0x20` | `MountRequest` | → `gdd` | `{devices:[{lun?, image_path, mode, inquiry_string?}], rebind?}` |
| `0x21` | `MountResponse` | → client | `{devices:[LunInfo]}`（**操作后的内核真值**） |
| `0x30` | `UnmountRequest` | → `gdd` | `{lun?}` |
| `0x31` | `UnmountResponse` | → client | `{released:[lun], devices:[LunInfo]}` |
| `0x32` | `RebindRequest` | → `gdd` | `{}` |
| `0x33` | `RebindResponse` | → client | `{udc}` |
| `0x34` | `DeleteSlotRequest` | → `gdd` | `{lun}` |
| `0x35` | `DeleteSlotResponse` | → client | `{devices:[LunInfo]}`（操作后的内核真值） |

### 关键语义

**`StatusRequest` 必须保留**：CLI 的 `ensure_gdd` 用它做**真正的**健康检查——
只看 socket 文件存在会把崩溃留下的陈旧节点误判为存活。

**`RebindRequest`（`0x32`/`0x33`）**：不带任何镜像，只重绑一次 UDC，促使主机重新
枚举。CLI 的顶层 `rebind` 子命令直接使用它（`gadgetdisk rebind`），因此「让新身份
立即生效」既不必走 `mount --rebind`、也不必自带镜像。

**`MountRequest.rebind`**：即使没有结构性改动也强制走一次「断 UDC → 重绑」。
用途是让**身份改动生效**：CLI 写完 `idVendor`/字符串后，新的描述符只在下次 bind
时才会被主机看到。

**`UnmountRequest.lun` 的两种语义区别很大**：

- `Some(n)`：只弹出第 `n` 个 LUN 的**介质**。LUN 目录、配置链接、function 与 UDC
  **全部保留**，主机侧只看到该介质消失。恢复只需重写 `lun.n/file`，无需重配 USB。
- `None`：**全部弹出并拆除**（清全部 file、删链接、删 function）。对应于“终止将手机作为 USB 大容量存储设备使用”的完整卸载语义。

**`MountResponse.devices` / `UnmountResponse.devices` / `DeleteSlotResponse.devices`
是操作后的内核真值**，而非请求中所声明的预期参数。CLI 据此写 `run/state.json`
（导出意图），因此它必须反映事实：某个 LUN 可能建失败、内核可能不支持
`inquiry_string`、镜像可能在写入前被删。

**`DeleteSlotRequest`：删除一个空闲槽位。** 删除 = 弹掉介质 + `rmdir lun.N`，
使该序号回到「不存在」。**仅允许删除 `lun.1` 及以上序号的槽位**：`lun.0` 由内核随 function 创建
（`fsg_alloc_inst` 注册为默认组），`rmdir` 返回 `EPERM`（已实测），只能弹出。
内核在 `fsg_lun_drop` 里还会 `unregister_gadget_item`，即**删 LUN 会隐式解绑整个
gadget**，因此 `gdd` 删完必须重建链接并重绑 UDC——这也是为什么应答要带回内核真值
而不是让调用方推断。

### 结构定义

**`LunInfo`**（一条记录 = 一个**槽位**）
```
{ index: u8, image_path: string, size_bytes: u64,
  mode: "rw" | "ro" | "cdrom", inquiry_string?: string,
  attached: bool, effective: bool, deletable: bool }
```
`effective=false` 表示已配置但未生效（例如 USB 未连接 / 无 UDC）。
`inquiry_string` 缺省时不出现在 JSON 里（可选字段一律省略）。**每个 LUN 独立**：
模式、INQUIRY 都按 LUN 分别设置。

`deletable` 表示该槽位能否删除（`index != 0`）。把这条内核知识放进协议，旨在避免前端重复实现针对 0 号槽位的特殊判断逻辑。

**槽位语义**：`attached=true` 是「已挂载」，`attached=false` 且该槽位存在
（`index` 在列表里）是「**空闲**」——LUN 目录还在、参数保留，可以直接改参数再挂载。
「空闲」与「不存在」的区别就是 `unmount --lun N`（保留）与 `delete-slot --lun N`
（移除）的区别。

**`ImageInfo`**（**REST 专用**）
```
{ path: string, size_bytes: u64, mtime: i64,
  layout: "raw" | "gpt" | "mbr" | "unknown",
  partition_offset_bytes: u64 | null,
  in_use: "none" | "gadget" | "loop" | "importing" }
```

**`LoopAttachment`**（**REST 专用**）
```
{ image: string, loop_dev: string, loop_part_devs: [string],
  mountpoint: string, read_only: bool }
```

**`CapabilitiesResponse`**（**REST 专用**，不再是 socket 消息）
```
{ loop_control: bool, max_part: u32,
  filesystems: [string],          // 例如 ["vfat","exfat"]
  mass_storage_supported: bool,
  selinux_enforcing: bool,
  max_luns: u8,                   // 多 LUN 上限（UI 据此限制「添加设备」行数）
  inquiry_string_max: u8 }        // INQUIRY 长度上限（UI 据此限制输入）
```

> `partscan_supported` 字段已于 2026-10-06 **移除**：它由 `loop_control && max_part > 0`
> 派生，在 Android 上报告 `true` 而该路径恒回退（内核不建 `loopNpM` 节点）。
> `max_part` 保留，但仅用于推导 loop 设备次设备号，**不参与挂载路径选择**。

> 原先还有 `gadget_hal_present`，随「删除 gadget HAL 暂停」一并删除：模块不再
> 对任何 HAL 发信号，保留该字段只会得到一个恒为 `false`、语义已死的值。见
> [Note](../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)。

**`JobStatus`**（**REST 专用**，经 `GET /api/v1/jobs/{id}`）的 `state`：`"running" | "done" | "failed"`

### 设备模式 `mode`

| 值 | 含义 | 备注 |
|---|---|---|
| `rw` | 可读写 U 盘 | 默认 |
| `ro` | 只读（写保护） | 设置 LUN 的 `ro` 属性 |
| `cdrom` | 光驱 | 设置 `cdrom` 属性；建议配合 `.iso` 且只读 |

## 错误码

`ErrorResponse.code` 使用稳定字符串，便于 WebUI 分支处理与文案映射：

| code | 含义 |
|---|---|
| `busy` | 另一操作正在进行 |
| `image_not_found` | 镜像不存在 |
| `image_in_use` | 镜像已被 gadget 或 loop 占用 |
| `not_regular_file` | 路径不是常规文件 |
| `unsupported_layout` | 无法识别的磁盘布局 |
| `no_udc` | 无可用 USB 控制器 |
| `mass_storage_unsupported` | 内核不支持 mass_storage function |
| `loop_unsupported` | loop 能力不可用 |
| `filesystem_unsupported` | 内核缺少所需文件系统 |
| `size_below_minimum` | **某个分区**的容量低于其文件系统的下限（`message` 含行号、文件系统、下限与实际值） |
| `no_space` | 目标文件系统空间不足（分区总和超过镜像可容纳的区间） |
| `permission_denied` | 权限或 SELinux 拒绝 |
| `invalid_argument` | 参数非法 |
| `already_exists` | 目标已存在（创建镜像时同名文件已存在） |
| `configfs_unavailable` | 无法确定可用的 gadget（configfs 未就绪 / 多 gadget 无法判定） |
| `not_active` | USB 配置已建立但主机未接受（真机上常表现为「代码 10」） |

`already_exists` 用于「创建镜像时同名文件已存在」：**拒绝而不是覆盖**，因为覆盖会
静默丢掉用户镜像里的全部数据。HTTP 映射为 `409`（与当前资源状态冲突，换个名字即可
成功），WebUI 据此给出「改名或先删除」的具体指引。

`configfs_unavailable` 与 `not_active` 是**设备/内核层面**的失败，与调用方参数无关，
因此 HTTP 映射为 `500`。`not_active` 的存在是为了让「挂载成功」不再依赖乐观判断：只有当
`/sys/class/udc/<udc>/state` 为 `configured` 且后端文件已绑定时才算生效
（见 [Android 集成](android-integration.md) 的 `effective` 判据）。

## HTTP API

`gadgetdisk serve` 提供的路由。请求体与应答体均为 JSON；错误应答统一为
`{"error": "<稳定错误码>", "message": "<英文说明>"}`——字段名与 CLI 契约
一致，使 WebUI 的错误文案映射可两条通道共用。

> **`error` 是唯一稳定判据；`message` 是英文诊断文本，无稳定性承诺。**
>
> `message` 面向 CLI/API 使用者，因此**一律英文**（见
> [语言边界](README.md#语言边界操作者输出用英文)）。它可能随文案调整而变化，
> **不得**被程序解析或当作分支依据。界面上的中文文案由前端
> `webui/pure/describe.js` 的 `ERROR_MESSAGES` 按 `error` 码独占生成；
> 后端 `message` 只用于**未知码**的兜底，以及错误面板 `detail` 里的排查线索。

| 方法与路径 | 作用 |
|---|---|
| `GET /api/v1/status` | UDC 与各 LUN 状态 |
| `GET /api/v1/images` | 镜像列表 |
| `GET /api/v1/image/partitions?path=` | 镜像的分区列表（供 UI 选择挂载哪个分区） |
| `GET /api/v1/loop` | 本地 loop 附件 |
| `GET /api/v1/capabilities` | 能力探测（含 `max_luns`、`inquiry_string_max`） |
| `GET /api/v1/config` | USB 设备身份（保存值 + 内核当前生效值）+ 镜像目标上下文 |
| `GET /api/v1/jobs/{id}` | 上传任务状态 |
| `GET /api/v1/tool/df?path=` | 可用空间（经 `nearest_existing_ancestor`）：回 `path`、`available_bytes`、`total_bytes`、`default_image_bytes`，以及 **`min_partition_bytes`**（形如 `{"fat32":34603008,"exfat":1048576,"ext4":2097152}` 的**按文件系统**分区下限对象）。**没有"镜像下限"字段**——镜像容量本身没有下限 |
| `POST /api/v1/create` | 创建镜像 |
| `POST /api/v1/delete` | 删除镜像 |
| `POST /api/v1/upload/begin` | 受理分块上传（校验 + 登记 job；返回 `upload_id`） |
| `POST /api/v1/upload/chunk?upload_id=&offset=` | 追加一块**原始字节**（流式，无请求体上限） |
| `POST /api/v1/upload/commit` | 原子改名到 `images/`；返回 `job_id` |
| `POST /api/v1/upload/abort` | 放弃上传并清理暂存 |
| `POST /api/v1/loop/attach` | 本地挂载（编辑用） |
| `POST /api/v1/loop/detach` | 释放本地挂载 |
| `POST /api/v1/config` | 保存并应用身份（写 configfs；**不断开 USB**，下次连接生效） |
| `POST /api/v1/config/security` | 设置或清除**镜像文件的 SELinux 目标上下文**（体 `{image_context}` 或 `{reset:true}`；与身份分开，避免互相覆盖） |
| `POST /api/v1/mount` | 导出为 USB 设备（**转发 `gdd`**） |
| `POST /api/v1/unmount` | 解除导出（**转发 `gdd`**；`lun` 缺省为全部拆除） |
| `POST /api/v1/rebind` | 重新绑定 UDC（**手动逃生口**；保存身份**不会**重绑——新身份在下次连接时生效。等价于 CLI 的 `rebind` 子命令，但**已无自动调用方**） |
| `POST /api/v1/slot/delete` | 删除一个空闲槽位（体 `{lun}`；`lun.0` 会被拒） |
| `OPTIONS *` | CORS 预检，回 `204`，**不校验 token** |

请求体字段与对应 `*Request` 结构一致（见上文消息表）。

`POST /api/v1/create` 的请求体：

```json
{ "path": "…/images/disk.img", "size_bytes": 4294967296, "layout": "gpt",
  "filesystem": "fat32", "volume_label": "GADGETDISK",
  "partitions": [
    { "size_bytes": 1073741824, "gpt_type": "gpt:efi_system",
      "mbr_type": "mbr:fat32_lba", "name": "BOOT", "filesystem": "fat32" },
    { "size_bytes": 0, "gpt_type": "gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0",
      "name": "DATA", "filesystem": "none" },
    { "size_bytes": 67108864, "mbr_type": "mbr:linux", "kind": "logical",
      "name": "EXTRA", "filesystem": "ext4" } ] }
```

- `layout` 缺省 `gpt`（`raw` / `gpt` / `mbr`）；
- `filesystem`（全局）缺省 `fat32`（`fat32` / `exfat` / `ext4`，接受 `vfat`、`ext2` 等别名），
  作为各分区的**默认值**；
- `volume_label` 缺省 `GADGETDISK`，规整为 11 字节；
- **`partitions` 缺省时保持向后兼容**：等价于单个占满剩余空间的分区。这是不破坏
  既有调用方的关键——不带该字段的老请求体行为完全不变；
- `partitions[].size_bytes` 为 `0` 表示占满剩余空间，**最多一处**（分区与扩展容器
  **共用**这个名额，见下）；
- **`partitions[].gpt_type` 与 `partitions[].mbr_type` 互不相关**：哪个生效由 `layout`
  决定，无关的那个被忽略。两者都缺省时按该分区的文件系统推断
  （见 [磁盘镜像格式](disk-image-format.md) 的两套类型表）；
- **`partitions[].filesystem`**：缺省继承全局 `filesystem`；`"none"` 表示该分区
  **不格式化**（只写分区表）。归属为 `"extended"` 的行**必须**为 `"none"` 或缺省
  ——容器没有数据区，指定文件系统会被拒绝；
- `partitions[].name` 只在 GPT 下写入镜像（MBR 无分区名字段）；
- **`partitions[].kind`**：`"primary"`（缺省）、`"logical"` 或 `"extended"`。
  **仅 `layout: "mbr"` 有意义**——缺省不下发该字段时行为与从前完全一致。
  - `"logical"` 写进 EBR 链、序号从 **5** 起；容器由后端按逻辑分区的数量与容量
    自动推导，用户不必声明；
  - `"extended"` 声明一个**扩展分区容器**行：它占一个首扇区槽位，但**不是分区**
    （不占内核序号、没有数据区、不可格式化）。里面可以暂时**没有**逻辑分区
    （预留空间），此时其区间由该行的 `size_bytes` 决定、**不产生任何 EBR**；
  - **一张表最多一个容器**（首扇区里只有一个扩展分区项可写），多个会被拒绝；
  - 显式容器与逻辑分区**同时存在**时，容器区间**一律由 EBR 链推导**，该行填的
    `size_bytes` 被**忽略**——否则「容器多大」会有两个矛盾的来源；
  - 在 GPT/raw 下给 `"logical"` 或 `"extended"` 会被拒绝（那两种布局没有扩展分区
    机制），**不静默当作主分区**。
  - **至少要有一个真正的分区**：只有容器的表在 Host 上什么都挂不上，会被拒绝。

  详见 [MBR 扩展分区与逻辑分区](disk-image-format.md#mbr-扩展分区与逻辑分区)。

应答：

```json
{ "path": "…/images/disk.img", "size_bytes": 4294967296, "layout": "gpt",
  "partition_offset_bytes": 1048576,
  "partitions": [ { "index": 1, "name": "BOOT",
                    "type": "gpt:efi_system",
                    "gpt_type": "gpt:efi_system", "mbr_type": "mbr:fat32_lba",
                    "offset_bytes": 1048576, "size_bytes": 1073741824,
                    "filesystem": "fat32", "label": "GADGETDISK",
                    "tool": "/data/adb/modules/gadget-disk/bin/mkfs.vfat" } ] }
```

- `type` 是**按布局回显**的那一套线格式名；`gpt_type` / `mbr_type` 两套都回，
  便于前端切换布局时不必重新推断；
- `partition_offset_bytes` 是**兼容字段**（首个分区的偏移），多分区场景请用 `partitions`；
- `filesystem` 为 `null` 表示该分区**未格式化**（用户选了 `none`），此时 `tool` 也是 `null`；
- 目标已存在时回 `already_exists`（409），**不会覆盖**。

`GET /api/v1/capabilities` 额外回 `mkfs` 与 `max_partitions`，供 WebUI 如实展示
「本机能用什么格式化」并限制分区行数：

```json
{ "mkfs": [ { "filesystem": "fat32",
              "path": "/data/adb/modules/gadget-disk/bin/mkfs.vfat",
              "note": "格式化经 loop 设备逐个分区进行" },
            { "filesystem": "exfat", "path": "/system/bin/mkfs.exfat",
              "note": "格式化经 loop 设备逐个分区进行" },
            { "filesystem": "ext4", "path": "/system/bin/mkfs.ext4",
              "note": "格式化经 loop 设备逐个分区进行" } ],
  "max_partitions": { "raw": 1, "mbr": 67, "gpt": 128 },
  "mbr_max_primary": 4, "mbr_max_extended": 1, "mbr_max_logical": 64 }
```

`max_partitions.mbr` 是**分区总数**上界（3 主 + 最多 64 逻辑，**扩展容器不计入**），
不是主分区上限——因此 `mbr_max_primary` 单独给出主分区槽位数，UI 靠它判断「再加一个
就必须是逻辑分区了」。`mbr_max_extended` 恒为 `1`（首扇区里只有一个扩展分区项可写），
`mbr_max_logical` 是逻辑分区数上限。精确的组合合法性由后端校验：MBR 可以是「4 主」、
「3 主 + 逻辑分区」或「N 主 + 1 个空扩展容器」，**4 主 + 1 个容器需要 5 个槽位，
会被拒绝**。

`path` 为 `null` 表示未探测到该工具（此时选它创建会失败并给出说明）。FAT32 的工具
是**模块自带的** `bin/mkfs.vfat`——设备上原本不存在该工具。

`POST /api/v1/mount` 的请求体：

```json
{ "devices": [ { "lun": 0, "image_path": "…/a.img", "mode": "rw", "inquiry_string": "DISK" },
               { "image_path": "…/b.iso", "mode": "cdrom" } ],
  "rebind": false }
```

- `lun` 缺省时由 `gdd` 分配到第一个空闲 LUN；
- `inquiry_string` 最长 28（内核定长写入，超长会静默截断，因此**写入前拒绝**）；
- `rebind` 是**显式**请求才重绑 UDC 的来源（见上文 `MountRequest.rebind`）。
  身份保存与挂载都不再隐式触发它——保存身份不断开 USB，下次连接才生效；仅在
  需要免物理插拔并立即强制主机重新枚举总线时显式指定，或改用不带镜像的 CLI `rebind` 子命令。

`POST /api/v1/config` 的请求体（**字段全部可选**，只写给出的项）：

```json
{ "id_vendor": 6353, "id_product": 20199,
  "manufacturer": "GadgetDisk", "product": "GD Storage", "serial": "ABC123" }
```

镜像的 SELinux 目标上下文通过**独立端点**（`/api/v1/config/security`）交互，不包含在上述
请求体中。拆分为独立端点出于明确的架构考量：USB 身份配置采用按字段增量合并语义，而安全上下文为完整取值替换；
合并至同一请求将带来误覆盖 USB 设备身份的风险。`GET /api/v1/config`
仍会**回显** `image_context` / `image_context_configured` / `default_image_context`，
便于排查 Host 端无法读取镜像内容等故障。

`GET /api/v1/config/security` 的应答（`path` 为配置文件路径）：

```json
{ "image_context": "u:object_r:media_rw_data_file:s0",
  "configured": null,
  "default": "u:object_r:media_rw_data_file:s0",
  "path": "…/config/gadget.json" }
```

`configured` 为 `null` 表示用户**未**显式配置自定义安全上下文，当前采用系统内置默认值（界面据此
区分“用户自定义配置”与“系统内置默认”）。`POST` 请求体为二选一格式：

```json
{ "image_context": "u:object_r:vendor_file:s0" }
{ "reset": true }
```

- 仅执行**格式形态**校验（非空、含 `:`、无空白及控制字符、≤256 字节），完整策略语法交由内核
  判定——校验失败返回错误码 `invalid_argument`；
- **先校验再落盘**，且落盘采用原子读—改—写机制：非法值不予持久化（避免造成后续每次挂载连续报错），
  且写入操作绝不覆盖同一配置文件中的 USB 身份配置；
- 应答包含 `"applies_on": "next-mount"` 标识：修改标签须**重新挂载**后方可生效（内核在打开
  后备镜像文件的瞬间即按当时标签锁定文件句柄上下文）。

`POST /api/v1/loop/attach` 与 `POST /api/v1/create` 的应答包含 `warnings` 字段
（字符串数组）：记录挂载或格式化**之前**尝试修正镜像 SELinux 上下文产生的告警（位于目录外仅告警、
修改失败时汇报具体原因）。该字段为**英文诊断文本**，与错误结构中的 `message` 具备同等契约地位——**无稳定性
承诺**，WebUI 仅展示中文结论并附带其原文作为故障排查线索。空数组表示无额外提示。

`GET /api/v1/status` 的应答增加两个字段：

```json
{ "udc": "a600000.dwc3", "devices": [ … ],
  "pending_intent": false,
  "intent": [ { "index": 0, "image_path": "…/a.img", "mode": "rw" } ] }
```

`pending_intent=true` 表示 `run/state.json` 里有导出意图，但内核里没有任何我们的
LUN 被绑定——典型是「设备刚被拔出、`gdd` 已清理，而 CLI 还没被叫到」。
**这是如实报告而不是静默改写**：UI 应把它显示成一个可操作的提示（恢复或清除），
而不是红色错误。`status` 本身**只读**，不写任何文件。

`GET /api/v1/image/partitions` 的应答：

```json
{"path": "/data/adb/gadget-disk/images/a.img", "layout": "mbr",
 "partitions": [{"index": 1, "start_lba": 2048, "offset_bytes": 1048576,
                 "size_bytes": 66043392, "type_label": "FAT32 (LBA)",
                 "kind": "primary"},
                {"index": 0, "start_lba": 133120, "offset_bytes": 68157440,
                 "size_bytes": 33554432, "type_label": "扩展分区",
                 "kind": "extended"},
                {"index": 5, "start_lba": 135169, "offset_bytes": 69206528,
                 "size_bytes": 66043392, "type_label": "Linux",
                 "kind": "logical"}],
 "default_index": 1}
```

- `layout` 取 `gpt` / `mbr` / `raw`。
- **无分区表时 `partitions` 为空数组，`default_index` 为 `null`——这不是错误**，
  调用方据此按整盘处理。
- `index` 是 **1 起序数且跳过空项**，与内核 `loopNpM` 的 `M` 对应；GPT 分区项
  数组里夹着大量空项，按数组下标编号会与设备名对不上。**MBR 下逻辑分区从 5 起**，
  扩展分区容器本身**不占序号**（它不是可挂载分区）。
- `kind` 取 `primary` / `logical` / `extended`，由 `index` 推出（逻辑分区 ≥5；
  **容器为 0**）。
- **`kind: "extended"` 的条目就是扩展分区容器**：`index` 为 `0`，不可挂载，调用方
  **不得**把它填进 `attach-loop` 的等待挂载列表。它只在 EBR 链**为空**时出现
  （即用户显式建的空容器）；链上有逻辑分区时容器不再单独列出，避免同一个区间既
  显示为容器又显示为里面的分区。
- `offset_bytes` 一律是**绝对**偏移，可直接作为 `LOOP_SET_STATUS64` 的 `lo_offset`
  ——读取侧已把 EBR 项里的相对偏移换算过来，调用方不必再关心这一点。
- `path` 必须是 `images/` 下的直接子项（与 `delete` 同一越界校验），否则
  `invalid_argument`；缺 `path` 参数回 `400`。

错误码到 HTTP 状态码的映射：`invalid_argument`/`size_below_minimum`/`unsupported_layout`
→ `400`；`permission_denied` → `401`；`image_not_found` → `404`；
`image_in_use`/`busy`/`not_regular_file`/`already_exists` → `409`；`no_space` → `507`；
`configfs_unavailable`/`not_active`/`filesystem_unsupported` → `500`
（设备/内核层面的失败，与调用方参数无关）；其余 → `500`。

**身份配置的错误也走同一套**：`POST /api/v1/config` 的字符串**超过 126 UTF-8 字节**、
**序列号非 ASCII**、**含控制字符**或 VID/PID 越界都返回 `invalid_argument`（400）；
「写了但读回不一致」返回 `internal`（500），因为那说明 configfs 没有接受我们的写入，
属于环境问题而非用户输入问题。

> `manufacturer`/`product` **允许中文**（内核经 `utf8s_to_utf16s` 正确转 UTF-16LE）；
> 只有 `serial` 要求可打印 ASCII——真机实测表明，非 ASCII 序列号会导致主机端（Host）无法成功识别或连接设备。
> 长度上限的单位是**字节**（内核 `usb_string_copy` 判 `strlen`）。

HTTP 层只实现一个**很小的子集**，其余一律显式拒绝：仅 `GET`/`POST`/`OPTIONS`；
仅 `HTTP/1.1`（与 `HTTP/1.0`）；**拒绝 `Transfer-Encoding`**（不支持 chunked——
客户端 `fetch` 对已知长度的 `Blob` 会发 `Content-Length`，没有 chunked 的调用方）。
**连接可复用（keep-alive）**：HTTP/1.1 默认复用、HTTP/1.0 默认关闭，`Connection: close`
一律关闭；空闲 10s 后断开。复用的动机是实测的每请求约 26ms 固定开销（见
[上传与导入](image-upload-and-import.md)）。**解析失败时不复用**——此时请求边界已不可信。

**请求体上限按读法区分**（`http::RequestBody`）：

| 读法 | 上限 | 用途 |
|---|---|---|
| `read_json` | 1 MiB，**分配前**拒绝（`413`） | 控制类端点的小 JSON 体 |
| `stream` | **无上限** | `upload/chunk` 的镜像字节，边读边落盘 |

> 早期实现对所有请求体统一限 1 MiB，理由是「与 gdd 协议帧上限一致」。那是
> **假耦合**：`MAX_FRAME_BYTES` 管的是 `serve`/CLI ↔ `gdd` 的 AF_UNIX 帧，而上传
> 字节不经过 gdd（直接写文件系统）。详见 [上传与导入](image-upload-and-import.md)。

## CLI 契约

```
gadgetdisk <command> [args...]
```

`--data-dir` 与 `--socket` 是**全局**选项，写在子命令前后都可以
（`gadgetdisk --data-dir <dir> status` 与 `gadgetdisk status --data-dir <dir>` 等价）。

- **成功**：stdout 输出**单个 JSON 对象**，退出码 `0`。
- **失败**：stdout（或 stderr）输出含 `error` 字段的 JSON，退出码非零。
- WebUI 必须容忍 stdout 尾随换行与可能混入的额外空白。

- 当 CLI 无法连接 `gdd` socket 时，向 stderr 输出稳定错误标识 `gdd_unreachable`，并以退出码 3（`ExitCode::Unreachable`）退出；前端据此判定 gdd 离线。
- CLI 每个操作均为顶层子命令（无 `client` 嵌套分组）：仅 `mount`/`unmount`/`delete-slot`/`rebind` 经 socket 委托 `gdd` 处理，其余子命令（`status`/`create`/`delete`/`list`/`attach-loop`/`detach-loop`/`list-loop`/`capabilities`/`config`）均由 CLI 就地执行。分块上传**没有** CLI 子命令——`ksu.exec` 无法向子进程传递标准输入（stdin），一次性进程也无法承载跨请求状态。
- 异步 job 任务仅由 `gadgetdisk serve` 在其进程内存中维护；CLI 执行的任务均为同步阻塞执行，因此 CLI 不提供 job 查询子命令。长任务轮询统一通过 REST 通道（`GET /api/v1/jobs/{id}`）。

`mount` 的选项是**可重复且按下标与位置参数对齐**的：

```
gadgetdisk mount a.img b.iso --mode rw --mode cdrom --inquiry DISK
```

即第 i 个镜像取第 i 个 `--mode` / `--lun` / `--inquiry`，缺省项用默认值
（`rw` / 自动分配 / 不设置）。选项个数多于镜像个数属用法错误（通常由遗漏镜像路径参数引发），
而不是静默忽略。

`create` 的 `--partition` 同样是**可重复**的，格式为
`SIZE[/GPT-TYPE[/MBR-TYPE[/NAME[/FS[/KIND]]]]]`：

```sh
gadgetdisk create disk.img --size 4G --layout gpt --filesystem fat32   --partition '1G/gpt:efi_system//EFI/fat32'   --partition '2G/gpt:linux_filesystem/mbr:linux/ROOT/ext4'   --partition '0/gpt:microsoft_basic//DATA/none'

# MBR 逻辑分区：第 6 段为 `logical`（缺省 = 主分区）
gadgetdisk create disk.img --size 4G --layout mbr   --partition '1G//mbr:fat32_lba/BOOT/fat32'   --partition '1G//mbr:linux/DATA/ext4/logical'

# MBR 空扩展分区容器：第 6 段为 `extended`（预留一片空间，里面暂时没有逻辑分区）
gadgetdisk create disk.img --size 4G --layout mbr   --partition '1G//mbr:fat32_lba/BOOT/fat32'   --partition '0//mbr:extended//none/extended'
```

- **用 `/` 分隔而非 `:`**：类型线格式自带 `gpt:` / `mbr:` 前缀，冒号不能同时作为字段分隔符与类型前缀标识（`SIZE:gpt:linux` 会被切成三段）。
- 空段表示「用默认值」，因此占位不可省略（`1G//mbr:linux//` 中第 2、5 段为空）。
- `FS` 为 `none` 表示该分区**不格式化**；缺省继承 `--filesystem`。
  `KIND` 为 `extended` 时**必须**为 `none` 或缺省（容器没有数据区）。
- `KIND` 为 `primary`（缺省）、`logical` 或 `extended`。**放在末尾**以免破坏既有的
  5 段语法；仅 MBR 有意义，逻辑分区序号从 5 起，容器**不占序号**。
- `--filesystem` / `--label` / `--partition` 现在都会生效（历史上的静默忽略缺陷已修复）。

`--module-dir` 是**全局**选项，指向模块根目录以定位自带的 `bin/mkfs.vfat`；
缺省按数据目录推出（`<data-dir>/../modules/gadget-disk`）。

## 长任务（job）

**分块上传只有 REST 通道**，因为仅有 `serve` 进程能够在多个请求之间保持常驻生命周期，也只有它
能承载「一次上传 = 多个 `chunk` 请求 + 一个 `commit`」这组跨请求状态：

| 阶段 | 响应 | 进度 |
|---|---|---|
| `upload/begin` | `{upload_id}` | — |
| `upload/chunk`（可多次） | `{bytes_done}` | 响应本身即累计进度；同时写入 job 供轮询 |
| `upload/commit` | `{job_id, state, bytes_done, path}` | 轮询 `GET /api/v1/jobs/{id}` |

- 字节写入 `tmp/<upload_id>.part`，**仅 `commit` 时**原子改名到 `images/`，
  避免中断产生被误当作完整镜像的半成品。
- job 在 `gadgetdisk serve` 内独立于发起连接存在；WebUI 断开或页面重载不影响其继续。
- **`begin` 必须登记 running job**：`serve` 判空闲退出看的就是它，未登记将导致上传任务因超时回收而中断。
- **块必须顺序追加**：`offset` 与已写长度不符回 `invalid_argument`（400），
  严禁使用 `seek` 跳跃填充（防止产生空洞文件）。
- **判据只有一条**：响应里**有 `job_id`** ⇔ 存在处于运行态的任务（running job）、需要轮询。
