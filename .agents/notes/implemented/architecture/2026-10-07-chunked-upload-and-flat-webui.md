# Agent Note: 分块上传与前端扁平模块化

Status: implemented

## Problem

**用系统文件选择器上传镜像**同时撞上两道约束，两者都必须先被证实或证伪，
否则整个方案无从设计。

**约束一：JS 拿不到文件路径，也拿不到 `content://`。**

KernelSU 的文件选择器（`WebViewHelper.kt` 的 `onShowFileChooser`）用
`fileChooserParams.createIntent()`，即 `ACTION_GET_CONTENT`；结果在
`WebUIScreen.kt` 的 `rememberFileLauncher` 里只从 `clipData` / `data.data` 取 `Uri`，
交给 `WebUIState.filePathCallback: ValueCallback<Array<Uri>>`。**全程没有任何
「URI → 路径」的转换**。Chromium 侧把 `content://` 留在 browser 进程，renderer 只得到
可读句柄与 `displayName`/`size`。

因此「选中文件 → 把路径交给后端复制」在架构上不存在，只能由 WebView 读出字节再传。

**约束二：HTTP 层不允许大请求体，且不能靠 base64 绕。**

`http.rs` 对所有请求体统一限 1 MiB，理由是注释里写的「与协议帧上限一致」。但
`MAX_FRAME_BYTES` 管的是 `serve`/CLI ↔ `gdd` 的 AF_UNIX 帧，而上传字节**不经过 gdd**
（直接写文件系统）——两者不在同一条数据路径上，是**假耦合**。

同时，若只把上限抬高，`vec![0u8; len]` 仍会**一次性分配整个体**，内存峰值等于镜像大小。

## Proposal

### 1. HTTP 请求体改为「按用途惰性读取」

拆掉假耦合，并把「怎么读」交给路由决定：

| 读法 | 上限 | 用途 |
|---|---|---|
| `RequestBody::read_json` | 1 MiB，**分配前**拒绝（413） | 控制类端点的小 JSON 体 |
| `RequestBody::stream` | **无** | 上传端点，边读边落盘 |

`parse_request` 只解析头部并把 `BufReader` 交给 `RequestBody`——**不预读体**。
`parse_request` 的接收者由 `&mut R` 改为 `R`（按值），因为 `serve_connection` 需要
同时持有读半（交给 handler）与写半（写回响应）；用 `try_clone()` 分开两半即可。

**不支持 chunked**，且是有意的：客户端是 WebView 的 `fetch`，它对已知长度的
`Blob`/`File` 会发 `Content-Length`，没有 chunked 的调用方；而实现它等于引入
「请求边界解析」这条最危险的代码路径，收益为零。

**连接复用（keep-alive）**：原设计是「一请求一连接」。实测每个请求有约 **26ms 固定
开销**（建连 + 线程 + 解析），与载荷无关；分块上传会发很多次请求，8 MiB 分块下这笔
开销占请求耗时的 30%。改为同一连接连续处理多个请求：

- `serve_connection` 的 handler 由 `FnOnce` 改为 `FnMut` 并循环；
- **HTTP/1.1 默认复用、HTTP/1.0 默认关闭**（老客户端兼容），`Connection: close`
  一律关闭；
- **解析失败时绝不复用**——此时请求边界已不可信，继续读可能把下一个请求的头当成体；
- 空闲 [`IO_TIMEOUT`] 后断开，复用不会变成「永久占住一个线程」。

### 2. 分块上传端点

| 方法 | 路径 | 请求 | 响应 |
|---|---|---|---|
| `POST` | `/upload/begin` | JSON `{dest_name, size_bytes}` | `{upload_id}` |
| `POST` | `/upload/chunk?upload_id=&offset=` | **原始字节流** | `{bytes_done}` |
| `POST` | `/upload/commit` | JSON `{upload_id}` | `{job_id, state, …}` |
| `POST` | `/upload/abort` | JSON `{upload_id}` | `{}` |

- 字节写 `tmp/<upload_id>.part`，**仅 `commit` 时**原子改名到 `images/`。
- `chunk` 送**原始字节而非 base64**：去掉上限后 base64 的唯一理由消失，直接发省约 33%。
- `begin` **必须登记 running job**——`serve` 判空闲退出看的就是它，不登记会让上传被
  空闲回收从中间掐断。
- `offset` 必须等于已写长度，**绝不 `seek` 补洞**：空洞文件会变成「大小看着对、
  内容缺一段」的镜像。

### 3. 只读工具面收敛为 `df`

`ls`/`stat` 提供了「枚举设备任意目录」的能力，而 JS 面根本拿不到路径（约束一），
因此它们没有任何合法调用方——留着就是纯攻击面。`POST /api/v1/import` 与 CLI
`import` 同理（分块上传已取代它）。两者均已不存在；保留 `df`（及其
`available_bytes` / `nearest_existing_ancestor`）与 `MAX_FRAME_BYTES`。

### 4. 前端按职责拆为扁平 ES 模块

`app.js`（2741 行）与 `logic.js`（2160 行）拆为：

```
webui/
├── main.js         入口（index.html 指向它）
├── backend.js      通道：api.json 探测、REST/CLI 分派、按需拉起 serve、重连
├── dom.js          DOM 与错误面板助手
├── task.js         忙碌态与任务反馈
├── view-*.js       六个视图各一（**平铺**）
└── pure/*.js       纯函数层（bytes/paths/describe/partitions/channel/task）
```

**视图模块必须平铺在 `webui/` 顶层**：`webui_sources()` 只扫顶层文件
（`iterdir()` 不递归），`pure/` 是唯一被显式登记的子目录。放进 `views/` 会导致
**本机测试全绿、打包却丢文件**，症状只在设备上出现。

**ESM 的跨模块绑定是只读的**：导入方不能给 `export let` 赋值。为此提供具名 setter
（`resetSlotRows`、`setKnownImages`、`resetReconnectFailures`），并由 `setImportSelection`
注入「是否已选文件」，避免 `backend.js` ↔ `view-import.js` 成环。

## Alternatives considered

| 方案 | 否决理由 |
|---|---|
| 保留自建路径浏览器，只是「也支持」选择器 | 它提供了没有调用方的任意目录枚举能力，而 JS 面拿不到路径，浏览器本身用不上 |
| 由 root shell 读 `content://` URI 再复制 | URI **不进 renderer**，JS 根本拿不到它；该能力无从发起 |
| 把 1 MiB 上限直接抬高（如 16 MiB） | 治标不治本：`vec![0u8; len]` 仍是一次性分配，大镜像直接 OOM |
| 实现 `Transfer-Encoding: chunked` | 无调用方（`fetch` 对已知长度会发 `Content-Length`），却引入最危险的边界解析路径 |
| 把块 base64 进 JSON 以复用 `callRest` | 上限既已去掉，base64 只剩 33% 膨胀的代价；改为 `callRestRaw` 发原始字节 |
| 视图放 `webui/views/` 子目录 | 打包脚本不递归 ⇒ 文件不进包，且本机测试发现不了（**已实测确认**该风险） |
| 保留 `logic.js` 作为 barrel 重导出 | barrel 会掩盖真实依赖方向，读者无法从 import 看出实际依赖 |

## Acceptance criteria

- 单个块可远超 1 MiB，且请求体**不整块驻留内存**（`http.rs` 有测试）。
- 分块拼接结果与源**逐字节一致**；乱序 `offset` 被拒绝且 `tmp/` 无空洞；
  `abort` / 失败后 `tmp/` 无残留。
- 上传期间 `has_running_jobs()` 为真（`serve` 不空闲退出）；结束后可正常退出。
- `ls`/`stat`/`import` 在前后端、CLI、文档中全部消失，有测试钉住。
- 打包 ZIP 含全部前端模块且保留 `pure/` 结构。
- 前端静态结构测试对**全量模块**做「具名 import ⊆ 目标模块 export」的链接期校验。

## Performance（实测）

| 配置 | 吞吐 |
|---|---|
| 8 MiB 分块、一请求一连接（改造前） | 88 MiB/s |
| 8 MiB 分块、连接复用 | 126 MiB/s |
| 32 MiB 分块、连接复用（当前） | 319–346 MiB/s |

96 MiB（3×32 MiB、连接复用）实测 0.57s 且逐字节一致。`chunk` 不再每块 fsync，
耐久性统一由 `commit` 的「先 `sync_all` 再原子改名」保证。

## Risks

- **分块上传吞吐未实测**。逐块经 WebView 读出、base64 已去除但仍有 HTTP 往返，
  预期低于设备侧文件复制；4 GiB 级镜像的实际耗时未知。（**待验证假设**）
- **`File.size` 不可尽信**：provider 可能返回 `0` 或不符。故 `size_bytes` 只作进度基准与
  空间预检，`commit` 以实际写入字节为准。（**待验证假设**：各 provider 的实际表现）
- **`WebView` 能否读出 `content://` 字节未在真机验证**。本方案依赖 renderer 侧的
  `FileReader`，与「root shell 能否读 `content://`」是**两个不同命题**；AVD 上未做
  端到端上传验证。（**待验证假设**）
- 前端拆分后，模块间依赖由 import 图表达；新增模块若忘了登记进 `WEBUI_SUBDIRS`
  或放错层级，只会在设备上暴露。
