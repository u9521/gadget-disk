# 上传与导入镜像

本文件定义如何把镜像文件放进模块数据目录。设计推演见
[分块上传 Note](../.agents/notes/implemented/architecture/2026-10-07-chunked-upload-and-flat-webui.md)。

## 结论：只能由 WebView 读出字节，逐块传给后端

镜像**不能**通过「选中文件 → 把路径交给后端复制」进入模块。这不是实现取舍，而是
架构事实——已对 **KernelSU 上游源码双向核对**：

| 环节 | 事实 | 证据 |
|---|---|---|
| 文件选择器 | `onShowFileChooser` 用 `fileChooserParams.createIntent()`（即 `ACTION_GET_CONTENT`），**只回 `Uri`** | `WebViewHelper.kt` 的 `onShowFileChooser`（上游各 fork 逐字相同） |
| 结果回传 | 只从 `clipData` / `data.data` 取 `Uri`，转交 `ValueCallback<Array<Uri>>` | `WebUIScreen.kt` 的 `rememberFileLauncher`、`WebUIState.onFileChooserResult` |
| 状态承载 | `WebUIState` 只有 `filePathCallback: ValueCallback<Array<Uri>>`，**全程无路径转换** | `WebUIState.kt` |
| renderer 侧 | `content://` URI 留在 browser 进程，renderer 只得到可读句柄与 `displayName`/`size` | Chromium `ContentUriUtils` |

因此 JS 既拿不到**文件系统路径**，也拿不到**`content://` 字符串**。

> **易误读点**：KernelSU 源码里还有 `MimeUtil` / `guessMimeType` /
> `getSelectedFilePath` 等符号，容易被误认为「暴露了路径」。实际
> `MimeUtil.getMimeFromFileName(fileName)` 只做**扩展名 → MIME** 映射（沿用自 AOSP
> 的 WebView 文件类型映射表），与文件选择器的返回值无关。

结论：上传必须由 WebView 把字节读出来，经 REST 分块送达。

## 为什么不用 base64、也不设 1 MiB 上限

早期的 HTTP 实现把 REST 请求体上限设为 1 MiB，理由是「与 gdd 协议帧上限保持一致」。
**那是假耦合**：`MAX_FRAME_BYTES` 管的是 `serve`/CLI ↔ `gdd` 的 AF_UNIX 帧，而上传
字节**根本不经过 gdd**（直接写文件系统）。两者不在同一条数据路径上。

现在请求体是**惰性**的，按用途分两种读法：

| 读法 | 上限 | 用途 |
|---|---|---|
| `RequestBody::read_json` | 1 MiB（分配前拒绝） | 控制类端点的小 JSON 体 |
| `RequestBody::stream` | **无** | 上传端点的镜像字节，边读边落盘 |

去掉了上限，base64 便失去意义（它当初只为绕开上限），故 `chunk` 直接发**原始字节**，
少约 33% 传输量。`upload_id` 与 `offset` 走查询参数。

## 端点

| 方法 | 路径 | 请求 | 响应 |
|---|---|---|---|
| `POST` | `/api/v1/upload/begin` | JSON `{dest_name, size_bytes}` | `{upload_id}` |
| `POST` | `/api/v1/upload/chunk?upload_id=&offset=` | **原始字节流** | `{bytes_done}` |
| `POST` | `/api/v1/upload/commit` | JSON `{upload_id}` | `{job_id, state, bytes_done, path}` |
| `POST` | `/api/v1/upload/abort` | JSON `{upload_id}` | `{}` |

**流程**

1. WebUI 用 `<input type="file">` 取得 `File`，显示名称与大小。
2. `begin` 完成全部前置校验（目标名合法、**同名不覆写**、空间预检）并**登记
   running job**，返回 `upload_id`。
3. 反复 `chunk`：按 `file.slice()` 分块读（每块 8 MiB），发原始字节。
4. `commit` 原子改名到 `images/`；UI 按响应**有没有 `job_id`** 决定是否轮询
   `GET /api/v1/jobs/{id}`。
5. 失败或用户取消 → `abort` 清理 `tmp/<upload_id>.part`。

## 两条不变量（均由测试守住）

1. **`begin` 必须登记 running job。** `serve` 判空闲退出时看的正是「有没有 running
   job」；未登记将导致大镜像上传因服务空闲超时回收而异常中断。
2. **块必须顺序追加。** `offset` 与当前长度不符即拒绝（`400`），**绝不 `seek` 补洞**——
   空洞文件会变成一个「大小看着对、内容其实缺一段」的镜像。

## 进度与终态

- `chunk` 是**单个** HTTP 请求但可能很大，因此后端在写入过程中按 `PROGRESS_STRIDE`
  持续更新 job 进度，客户端在此期间即可看到前进。
- 分块上传**只有 REST 通道**：`ksu.exec` 无法向子进程写 stdin，CLI 没有等价子命令。
  CLI 回退可用时该功能不可用，界面须如实说明。
- **不做断点续传**：中断即作废，需重新选择文件。

## 性能（实测数据）

在 AVD（x86_64）上实测，瓶颈**不在磁盘**——原始写入 1230 MiB/s，纯 loopback HTTP 也能到
100+ MiB/s。真正的成本是**每个请求的固定开销**：**26ms**，与载荷大小无关（新建连接 +
线程 + 解析 + 落盘）。

因此优化方向是「减少请求次数」与「摊薄单请求成本」，三处改动叠加：

| 改动 | 效果 |
|---|---|
| 去掉每块的 `fsync`（改为 `commit` 时落盘一次） | 每块省约 42ms |
| 分块 8 → 32 MiB | 固定开销摊薄到可忽略 |
| **连接复用（keep-alive）** | 消除每个请求约 26ms 的建连与握手开销 |

实测吞吐（32 MiB 总量，同一设备）：

| 配置 | 吞吐 |
|---|---|
| 1 MiB 分块、一请求一连接 | 23 MiB/s |
| 8 MiB 分块、一请求一连接（改造前） | 88 MiB/s |
| 8 MiB 分块、连接复用 | 126 MiB/s |
| 32 MiB 分块、连接复用（**当前**） | 319–346 MiB/s |

96 MiB 镜像（3×32 MiB、连接复用）实测 **0.57s 完成且逐字节一致**。

### 真正的瓶颈在**读取源文件**，不在上传

浏览器内实测 450 MiB 上传（15×32 MiB）的分段耗时：

| 阶段 | 总计 | 每 32 MiB | 速率 |
|---|---|---|---|
| `read`（`FileReader` 读 `content://`） | 4513ms | 301ms | **106 MiB/s** |
| `send`（`fetch` 上传） | 3857ms | 257ms | 124 MiB/s |

**读取占 54%**，而它由系统 provider 决定，后端再快也绕不过——数据必须先被读进
WebView 内存才能发出去。这是「经 WebView 中转」这条路的结构性成本。

分块大小扫描（纯读取，同一文件，256 MiB 预算）：

| 分块 | 读取吞吐 |
|---|---|
| 8 MiB | 46 MiB/s |
| 16 MiB | 74 MiB/s |
| **32 MiB** | **109 MiB/s（峰值）** |
| 64 MiB | 90 MiB/s |
| 128 MiB | 29 MiB/s |

**32 MiB 是最优点**：继续加大反而更慢（128 MiB 掉到 29 MiB/s，推测是单次分配
128 MiB `ArrayBuffer` 触发内存压力/GC）。因此 [`UPLOAD_CHUNK_BYTES`] 不应再调大。

> 想要**一个数量级**的提升只能放弃 WebView 中转，改由后端直接读设备文件
> （设备侧复制可到 1000+ MiB/s）。但那需要文件路径，而系统选择器只给
> `content://`——这正是本方案存在的前提。

**`chunk` 不再每块落盘**：写的是 `tmp/<id>.part`，未 `commit` 前不是任何合法镜像，
传输过程中异常掉电仅会残留临时的未完成文件。耐久性由 `commit` 统一保证——**先 `sync_all` 再原子改名**，
因此「改名成功」等价于「内容已落盘」。

## 错误信息可读性要求与故障排查规范

**实测缺陷**：上传时报「出错了 / 未知错误」，且控制台无任何输出——报告者与开发者
都无法定位。

根因有两处，都是「假设响应结构永远正确」：

1. `failed(result)` 直接透传后端结果，而 `showError` 在 `message` 为空时只显示通用的
   「未知错误」，把**全部**排查信息丢掉；
2. `begin.data.upload_id` / `commit.data.job_id` 在 `ok:true` 但负载结构不符时抛
   `TypeError`，经 `runTask` 包装后只剩一句没有上下文的「导入失败」。

现在：`failed()` **必须**为缺失的 message 兜底，并把整个结果序列化进 `detail`；
取 `upload_id`/`job_id` 前**必须**判 `data` 存在。两条约束均已由前端回归测试用例严格覆盖
（`webui/tests/structure.test.mjs`）。

> 通用原则：**任何失败路径都必须携带可读原因**。笼统的“未知错误”不仅无法为用户提供有效操作指引，
> 亦会导致问题根因定位受阻。

## 已知限制

- **`size_bytes` 不可尽信**：`File.size` 取决于 provider，可能为 `0` 或与实际不符。
  它只用于**进度基准与空间预检**；写入长度以实际字节为准（`commit` 亦然）。
- 大镜像经 WebView 逐块搬运，吞吐低于设备侧文件复制（**目前缺少真机实测数据**，参见
  [路线图](roadmap.md) 中的待验证假设）。

## 验收

- 选中文件后可完整导入 `images/`，内容与源**逐字节一致**（校验和比对）。
- 单块可超过 1 MiB，且请求体**不整块驻留内存**。
- 乱序分块被拒绝且 `tmp/` 无空洞；中断后不产生半成品。
- 空间不足、同名冲突均在 `begin` 阶段报出，并给出具体数值或名称。
- 上传期间 `serve` 不空闲退出；结束后可正常空闲退出。
