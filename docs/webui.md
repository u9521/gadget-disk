# WebUI

本模块**没有**配套 APK，WebUI 是唯一界面。本文件定义其文件组成、后端通道、视图与错误处理。设计理由见 [WebUI Note](../.agents/notes/implemented/feature/2026-10-04-webui-zero-build.md)。

## 运行环境

- 由 KernelSU 的 `WebViewAssetLoader` 提供，源（origin）为 `https://mui.kernelsu.org`。
- 入口文件必须是 `webroot/index.html`（KernelSU 常量 `MODULE_WEB_DIR = "webroot"`）。
- **不要手动设置 `webroot` 的权限或 SELinux 上下文**，安装器会自动处理。
- WebView 设置（来自 KernelSU `WebViewHelper`）：`javaScriptEnabled = true`、`domStorageEnabled = true`、`allowFileAccess = false`。

## 文件组成

前端按职责拆为多个**原生 ES 模块**（仍是零构建，无打包器）：

```
webui/
├── index.html    入口与六个视图的静态骨架
├── style.css     样式
├── main.js       入口：初始化、tab 切换、事件绑定、全局错误兜底
├── backend.js    后端通道：api.json 探测、REST/CLI 分派、按需拉起 serve、重连状态机
├── dom.js        DOM 与错误面板助手（$、showError、guard、failed）
├── task.js       忙碌态与任务反馈（runTask、runRefresh、TASK_SCOPES）
├── ksu.js        对 window.ksu 的最小 Promise 封装
├── view-*.js     六个视图模块（平铺于顶层：mount/create/import/images/edit/settings）
└── pure/         纯函数层，按域拆分（bytes/paths/describe/partitions/channel/task）
```

**布局是平铺的，且必须保持平铺**：打包脚本 `webui_sources()` 只扫描 `webui/` **顶层**
文件（`iterdir()`，不递归），因此视图模块直接放在顶层；`pure/` 作为唯一子目录被
**显式登记**并保留目录结构——拍平会让 `pure/` 内部的相对 import 在设备上解析失败。

`pure/` 与其余模块的分工是**可测试性要求**而非风格偏好：零构建约束下没有前端
测试框架，只有把可测逻辑做成**不接触 DOM/window 的纯函数**，才能用 Node 内置
test runner 覆盖（在 `webui/` 下执行 `node --test tests/`）。视图模块只做 DOM 绑定
与后端调用，不得混入可测逻辑。

**零构建约束**：不使用 npm、打包器或前端框架；不引用任何外部 CDN 资源（模块须离线可用）。

**不依赖 npm 的 `kernelsu` 包**：其 v3.x 为 ES 模块，需要模块系统或打包器。改为自写薄封装：

**实测签名（Android 17 / KernelSU 3.3.0，已在本模块的 AVD 上验证）**：`ksu.exec` 是
**回调式**的，且回调**通过名字**注册在 `window` 上，而非直接传函数：

```js
// 实测：ksu.exec(cmd, optionsJson, callbackName)
// 内核会把 window[callbackName] 当作回调调用，参数为 (errno, stdout, stderr)
export function exec(command, options) {
  return new Promise((resolve) => {
    const callbackName = `exec_callback_${Date.now()}_${counter++}`;
    window[callbackName] = (errno, stdout, stderr) => {
      delete window[callbackName];           // 防泄漏
      resolve({ errno, stdout, stderr });
    };
    window.ksu.exec(command, JSON.stringify(options || {}), callbackName);
  });
}
```

> 封装层必须集中处理签名差异（APatch 或其他 KernelSU 版本可能不同），避免散落各处。
> 本封装**永不 reject**：无 `window.ksu`、命令失败、回调抛错都解析为 `errno = -1`
> 的正常结果，避免未捕获的 Promise 拒绝导致白屏。

## 后端通道

**主通道是回环 REST**（`gadgetdisk serve`），`ksu.exec` + CLI 是**回退通道**。

### 双通道架构设计

- **主通道**：本地回环 REST API（`gadgetdisk serve`），通过 HTTP 长连接降低进程拉起与 Shell 初始化开销，提升 UI 交互响应。
- **降级通道**：`ksu.exec` + CLI 一次性进程。在 serve 进程未就绪、空闲超时退出或崩溃时，WebUI 自动无缝降级至 CLI 执行，保证全功能可用。两者并非冗余，而是互为补充的可用性保障网。

### REST 引导：`api.json`

serve 启动时把 `{"port": <内核分配的临时端口>, "token": "<Bearer token>"}` 原子写入
**模块的 `webroot/api.json`**（`0600`），退出时删除。WebUI 与它同源（KernelSU 的
`WebViewAssetLoader` 把 `https://mui.kernelsu.org` 映射到模块的 `webroot/`），
因此用**相对 URL** 读取即可：

```js
const response = await fetch('api.json', { cache: 'no-store' });
```

`cache: 'no-store'` 不可省：否则 WebView 缓存会将已退出 serve 进程的历史无效端口持续返回给前端。

解析在 `pure/channel.js` 的 `parseApiInfo`（纯函数）：端口不是 1–65535 的整数、缺 `token`、
JSON 畸形——一律返回 `null`，调用方据此走 CLI 回退。

**地址必须是 IPv4 字面量** `http://<IPv4 字面量>:<port>`（`pure/channel.js` 的 `REST_HOST`）：
实测用回环**主机名**会解析到 IPv6 `::1`，而 serve 只绑定 IPv4 回环，结果是
`Failed to fetch`。

### 按需拉起 serve

serve 空闲 60 秒即退出，因此「它不在」是常态而非异常。读不到 `api.json` 时，WebUI
按需拉起它：

```sh
setsid <bin> serve --data-dir /data/adb/gadget-disk --module-dir <MODDIR> \
  >> logs/serve.log 2>&1 < /dev/null &
```

三个细节缺一不可（实测证据见 [按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)）：

- **`setsid`**：KernelSU 会在 `exec` 的 shell 返回后回收其**进程组**，普通 `&` 拉起的
  serve 会被 SIGKILL（rc=137）；`nohup` 挡不住进程组回收，不能替代；
- **stdout/stderr 重定向到 `logs/serve.log`**（并按 1 MiB 截断，避免无界增长）；
- **stdin 断开**（`< /dev/null`）：标准流不重定向会让本次 `exec` 一直等管道关闭。

拉起后轮询 `api.json` 最多 8 × 250ms，失败即回退 CLI——**拉起失败不是错误**，界面不能
因为优化通道不可用而不可用。每个页面最多尝试 2 次，避免 serve 反复崩溃时形成重启风暴。

> **待验证假设**：WebView 经 `ksu.exec` 拉起 serve 后，serve 能否稳定存活到空闲超时
> （`setsid` 能移出会话，但落在回收窗口内的首次启动仍可能被杀，见上述 Note）。真机
> 验证前只依赖「REST 可用时更快」，不依赖「REST 一定可用」。

### 调用分派与错误规整

调用层统一接收结构化操作描述符（`{op, ...}`），由 `pure/channel.js` 的纯函数分别编译为 REST 请求定义（`buildRestCall` → `{method, path, body}`）与 CLI 参数序列（`buildCliArgs`），以保证双通道参数语义与校验规则严格一致。

HTTP 结果经 `restResultToExecResult(status, text)` 归一为**与 `ksu.exec` 完全相同的
`{errno, stdout, stderr}`**：

| 情况 | errno | stdout |
|---|---|---|
| `status < 400` 且响应体非空 | 0 | 响应体 |
| 4xx / 5xx | 4（`ExitCode::Server`） | 响应体（`error`/`message` 原样保留） |
| 未到 400 但响应体为空 | 4 | 空 |
| 网络层失败（`Failed to fetch`） | 3（`ExitCode::Unreachable`） | 空 |

因此 `parseExecResult` 可直接复用 REST 的响应体。网络层失败时作废缓存并在
**本次调用**就回退 CLI；401 同样作废缓存（serve 重启会换 token），因为那说明
当前持有的 `api.json` 已失效过期。

**错误文案的归属：错误码优先，后端 `message` 只作兜底。** 后端 `message` 是
**英文**的（面向 CLI/API 使用者，见 [文档规范](README.md#语言边界操作者输出用英文)），
而本界面是中文的，直接显示会导致中文界面出现未本地化的英文诊断语句。因此：

1. 有 `error` 码且本地映射表认识它 → 用 `messageForCode(code)` 的中文文案；
2. 码未知 → 用后端 `message` 兜底（此时本地映射表无对应文案，显示英文原句优于无实质信息的「未知错误」），
   并在末尾括号里带上错误码便于对照 `docs/protocol.md`；
3. 两者都没有 → `messageForCode` 的通用兜底。

后端 `message` 原文进 `detail`（错误面板的可展开区域），确保调试排查线索完整保留。
`webui/pure/describe.js` 的 `ERROR_MESSAGES` 是**界面错误文案的唯一来源**。

**必须处理的三类异常**（缺一即可能白屏）：

| 异常 | 检测方式 | 处理 |
|---|---|---|
| 命令失败 | `errno !== 0` | 显示错误，保留 `stderr` 供排查 |
| 空输出 | `stdout.trim() === ''` | 提示 "Backend did not respond"，附命令与退出码 |
| JSON 解析失败 | `JSON.parse` 抛异常 | 提示 "Malformed output"，并原样展示截断后的 stdout |

> 上表的提示文案是**通道级**诊断字符串（`webui/pure/channel.js`），与后端
> `message` 一样属于「操作者输出」，因此用英文。它们不来自错误码映射表——
> 拿到这种结果说明连一次合法的结构化应答都没有。

因为 stdout 可能混入 shell 警告或 busybox 提示，解析层应对尾随换行与前后空白宽容。

「设置与诊断」视图曾有一个「排查提示」卡片，展示 `dmesg | grep avc` 与一个打印后端原始
输出的按钮。该卡片**已移除**（用户判定无实际用途）：失败时由错误面板给出具体原因，
需要原始输出时直接看 `logs/`，不必在界面上常驻一块排查说明。

### 不可静默失败

界面的可用性由两层保证，缺一不可：

1. `ksu.exec` 的封装**永不 reject**（见上文），REST 失败也只回退不抛错；
2. `dom.js` / `main.js` 注册 `unhandledrejection` 与 `error` 全局监听，将未捕获异常转化为可见的错误面板；初始加载的各项刷新操作均由 `guard()` 包装保护。
3. **调用结果类型约束**：`ksu.exec` 返回格式固定为对象 `{errno, stdout, stderr}`，禁止当成字符串直接调用 `.trim()` 等方法，避免抛出未捕获的 `TypeError` 导致前端流程静默卡死。

### 状态反馈与防重入机制

写操作触发时立即进入 busy 状态（`runTask`）：禁用对应操作按钮并设置 `aria-busy="true"`，在操作卡片状态区实时展示已耗用时间（如 `正在挂载…（已用 0.4s）`，每 200ms 刷新），并在 `finally` 块中无条件重置（覆盖成功、业务失败及异常分支），防止状态锁死或并发重入。

刷新操作同步展示 `正在同步状态…` 提示，消除后台处理与前端渲染之间的时间差感知。

**不给 `create` 做百分比进度**：它亚秒级（实测 64 MiB=158ms、2 GiB=426ms），
做阶段级百分比需要把 `create` job 化并改协议契约，收益不抵成本。超过 3s
（`SLOW_TASK_THRESHOLD_MS`）时追加「仍在进行，请勿关闭页面」的提示。

**不做乐观更新**：挂载与占用状态一律来自后端。本项只解决等待期的反馈，不猜测结果。

### 后端不可用：持续横幅 + 重连 + 阻断变更

单次操作失败用 `#error-panel`（下次成功即清除）。**通道整体不可用**是另一回事，
用 `#offline-banner`（不自动消失）：

- 显示具体原因、「重连」按钮与「重试中…」指示；
- **禁用全部变更按钮**（挂载/卸载/创建/删除/导入/本地挂载与卸载）；
- **保留只读刷新可用**——刷新本身就是探测手段，禁用它会让用户无法自行恢复；
- 自动重试：5s 起、指数退避、上限 30s（`RECONNECT_BASE_MS`/`RECONNECT_MAX_MS`）；
  手动点「重连」立即重试并重置退避；`reconnect()` 用单一 in-flight promise，
  避免手动点击与自动重试并发拉起两个 `serve`。

**判定关键是「两条通道都失败才算离线」**：REST 失败但 CLI 回退成功仍是 `online`，
因为 CLI 是安全网（见上文），此时界面完全可用，显示「已断开」并禁用按钮是错的。
判定由 `pure/channel.js` 的纯函数 `classifyBackendFailure` 承担，因此可被 Node 测试覆盖。
判为通道级失败的三类信号：

| 信号 | 来源 |
|---|---|
| `kind === 'unreachable'` | REST 网络层失败且 CLI 也没能给出结果 |
| `code === 'gdd_unreachable'` | CLI 自报连不上 `gdd`（**写在 stderr**）。该码直接命名它实际指向的进程 `gdd`（旧名 `daemon_unreachable` 指向一个并不存在的进程）；离线状态机以这个字符串为判据，改名会同时改动两侧 |
| `errno === 3` / `errno === -1` | `ExitCode::Unreachable` / `ksu.exec` 根本没执行 |

> **实测教训（AVD）**：CLI 的错误 JSON 走 **stderr**（`output::JsonOutput` 的
> `to_stderr = true`），而早期 `parseExecResult` 只解析 stdout，于是通道级错误码
> 被降级成 `command_failed`，离线态**永不触发**——两条通道全断时横幅仍不出现、
> 按钮停在「正在…」。现在两条流都找（stdout 优先），并保留 `errno` 供分类器使用。

## 视图

### 1. 挂载状态

- 显示 UDC 名、各 LUN 的镜像路径、容量、模式（`rw`/`ro`/`cdrom`）、是否生效。
- `effective=false` 时标记「已配置但未生效」（例如 USB 未连接）。
- 操作：挂载、卸载、切换模式。

### 2. 创建镜像

- 输入：文件名、容量、布局（`raw`/`gpt`/`mbr`）、**全局文件系统**（`fat32`/`exfat`/`ext4`）、卷标。
  全局文件系统是各分区的**默认值**。
- **同名预检**：文件名与已有镜像重名时，输入框下方显示提示并**禁用创建按钮**。
  后端同样会拒绝（回 `already_exists`，409），那是权威判定；前端预检只是让用户
  不必等一次往返就看到原因。
- **分区编辑器**：仅 `gpt`/`mbr` 展示（`raw` 没有分区表）。每行可填
  **容量 / 归属（仅 MBR）/ 类型 / 文件系统 / 名称**，支持添加/删除；分区数上限按
  布局限制（GPT 128、MBR 总 67，**扩展容器不计入**）。
  - 容量填 `0` 表示占满剩余空间，**最多一处**（分区与扩展容器**共用**这个名额——
    容器的容量同样占用镜像空间，两者都填 0 时「谁拿剩余」没有唯一答案）。
  - **类型按布局分域**：GPT 行显示 GPT 类型下拉（含「自定义…」→ 出现 GUID 输入框），
    MBR 行显示 MBR 类型下拉（含「自定义…」→ 出现类型字节输入框）。两套类型空间
    互不相关，同一个「FAT32」在两侧是不同的类型。
    类型**留空表示用布局默认值**，不是错误。
  - **MBR 行的「归属」有三项**：主分区 / 逻辑分区 / **扩展分区（容器）**。
    - **逻辑分区**写进 EBR 链、序号从 **5** 起；容器由后端按逻辑分区的数量与容量
      自动推导，用户不填容器容量。
    - **扩展分区（容器）**声明一个**容器行**：它占一个首扇区槽位，但**不是分区**
      （不占内核序号、没有数据区、不可格式化）。用户可用它**预留**一片空间，
      里面暂时没有逻辑分区也可以（「1 主 + 1 空扩展」）。
    - 类型下拉里**没有「扩展分区」**——容器由**归属**表达，类型字节恒为 `0x05`、
      由后端生成。把 `mbr:extended` 当分区类型提交会被拦下并指引改用归属。
    - **容器行不渲染类型与文件系统选择器**：它没有数据区，渲染成禁用控件比不渲染
      更糟（用户会以为自己可以填、只是被挡住了）。容器行只显示容量 + 一句说明。
    - **槽位计数用「主分区数 + 是否启用扩展容器」，不是分区总数**：「4 主」与
      「3 主 + 1 容器」都合法，但「4 主 + 1 容器」需要 5 个槽位，必须拦下。报错文案
      要说明原因（容器占一个槽位），而不是只给一个数字上限。
    - **容器最多一个**：首扇区里只有一个扩展分区项可写，两个容器行会被拦下并说明。
    - **提示文案与添加按钮必须与校验同源**：三处都调用 `mbrSlotUsage`
      （`pure/partitions.js`），否则「文案说最多 4 个、实际能加 67 个」这类漂移会再现。
      文案要实时报出「N 个主分区 + 1 个扩展分区容器（容纳 M 个逻辑分区 / 空）」，
      已用 X / 4，并说明“再添加时选逻辑分区或扩展分区即可突破 4 个的限制”。
    - **添加按钮不在「4 个主分区」时禁用**：此时加主分区不行、但加逻辑分区可以
      （需先把一个主分区改成逻辑分区）。禁用会让用户以为再也加不了，故只在达到
      总数上限或逻辑分区上限时禁用，并用 `title` 说明当前该怎么加。
    - **每行标题显示内核序号而非行下标**（`partitionKernelIndex`）：主分区 1–4、
      逻辑分区从 5 起，且扩展容器**不占序号**——容器行返回 `null` 并按「扩展分区
      容器（不占序号 · …）」渲染，**绝不显示数字**（后端把容器读回来时 `index`
      正是 `0`，显示「分区 0」会让用户以为存在一个序号 0 的设备）。
    - **渲染顺序为「主分区 → 扩展容器 → 逻辑分区」**，与内核槽位顺序一致；逻辑分区
      包在 `.partition-logical-group` 缩进分组里、加左边框，视觉上表达「在扩展分区
      里面」。
    - **DOM id 一律用行在 `createPartitions` 里的原始下标**，不是渲染序号：
      `readPartitionRow`/`syncPartitionRows` 都按原始下标读写，用渲染序号会让重绘后
      每行的输入串到别的行上。`structure.test.mjs` 抓出**所有** `.id = \`partition-…\``
      赋值逐个断言插值表达式是 `index`。
    - **`readPartitionRow` 的 `kind` 回落值必须是该行原有的 kind**，不能硬写
      `'primary'`：容器行与逻辑分区行不渲染类型/文件系统控件，回落写错会让用户
      改一个容量就把该行悄悄变回主分区。
  - **每分区文件系统**：可选「默认（随全局设置）」/ FAT32 / exFAT / ext4 / **不格式化**。
  - **MBR 下名称字段整个不渲染**（不是禁用）：该布局没有分区名字段，静默丢弃会让
    用户以为填的名字生效了；而禁用的输入框仍会让人以为自己可以填、只是被挡住了。
    这条与「容器行不渲染类型选择器」是同一条原则。
- **格式化来源展示**：按 `capabilities.mkfs` 列出三种文件系统各自将用哪个工具
  （例如 `/system/bin/mkfs.exfat`，或模块自带的 `bin/mkfs.vfat`）。**每种一行**
  （`#create-fs-source` 配 `white-space: pre-line`），挤成一长句时三种工具名难以扫读。
- 校验：容量合法性、可用空间（`statvfs` 预检）、路径合法性、
  **每个分区在其所选文件系统下的下限**、分区总和与上限、
  类型合法性（仅自定义值会报错，留空放行）、MBR 槽位组合。
  **超限报错而不裁剪**——裁剪会让用户以为拿到的是自己填的容量。
  - 下限报错必须**指到行**并说清文件系统、下限与实际值（"第 2 个分区的容量低于 fat32 下限"），
    而不是笼统的"空间不足"；笼统说法会让用户在一个多分区镜像里找不到问题所在。
  - **镜像容量本身不设下限**：下限是"每个分区在其文件系统下是否够大"。空文件的容量框
    只拦 0/负数。
- 说明：稀疏文件行为（创建大镜像不会立即占满空间）。

> 分区校验与预检逻辑（`validatePartitions`、`normalizePartitions`、`imageNameExists`、
> `parsePartitionSize`、`resolveGptType`/`resolveMbrType` 等）全部放在 `pure/` 的
> **纯函数**里：零构建约束下只有不接触 DOM 的逻辑才能被 Node 内置 test runner 覆盖。

### 布局（宽屏与窄屏）

分区编辑器每个分区是一个**两行结构**：标题行 + 字段网格。字段网格用
`grid-template-columns: repeat(auto-fit, minmax(150px, 1fr))`，因此宽屏一行排开多个
字段、窄屏自动堆叠成单列。

**标签与控件处在同一个 `.field` 单元内**（纵向排列），所以标签**必定**在自己控件
的上方，与可用宽度无关。早先用单层 flex + `flex-wrap`，宽屏下浏览器按可用宽度重排，
「类型」标签会被夹在两个控件之间（实际截图可见），用户分不清标签属于谁。

### 3. 上传 / 导入镜像

- **唯一入口是系统文件选择器**（`<input type="file">`）。内置路径浏览器与其
  `ls`/`stat` 后端已整体移除——JS 拿不到文件路径（见
  [上传与导入](image-upload-and-import.md)），且留着会多一条「可枚举任意目录」的通道。
- 选中后经 `upload/begin` → 多次 `upload/chunk`（原始字节，每块 32 MiB）→
  `upload/commit` 落盘；失败或取消走 `upload/abort` 清理暂存。
- 长任务显示 job 进度（轮询 `JobStatus`）。界面按响应**有没有 `job_id`** 分流：
  有则轮询、无则按终态收尾。
- **必须按块 `file.slice()` 读取**，禁止整文件 `readAsArrayBuffer`（可能导致 WebView 内存耗尽发生 OOM）。
  32 MiB 是实测的读取吞吐峰值（64/128 MiB 反而更慢），见
  [上传与导入](image-upload-and-import.md) 的性能一节。
- **不堆砌原理与限制说明**：这些属于实现细节，写在文档里即可；界面上只保留
  一句用法提示。失败时由错误面板给出具体原因（而不是常驻的「注意事项」卡片）。

### 4. 镜像管理

- 列出 `images/` 下的镜像：名称、大小、修改时间、布局、占用状态。
- 删除：被占用（gadget 或 loop）时拒绝并提示先卸载。
- **不提供「用于挂载」快捷按钮**：它的效果等价于「切到挂载页 → 在下拉框里选这个
  镜像」，而挂载页下拉框的选项本来就来自同一份镜像列表。去掉它既没有损失能力，
  又消除了跨视图状态副作用——点按钮会悄悄改掉另一个页面的表单状态，用户容易分不清
  自己刚才操作的是哪个视图。

### 5. 本地编辑挂载

- 操作：`AttachLoop` / `DetachLoop`。
- **镜像只能选、不能填**：`#loop-image` 是 `<select>`，选项来自 `GET /api/v1/images`，
  与挂载页复用同一套选项构造（`buildImageOptions`）。两页的「能选到什么」必须一致；
  若这里允许手填路径，用户既能填出不存在的文件，也能指向镜像目录之外的任意位置。
  下拉框旁有「刷新列表」；进入本视图时自动重新拉取一次（镜像可能刚在别的视图里
  创建/删除/上传），**不依赖用户先访问过挂载页**。
- **分区选择**：切换镜像后立即读取分区表（`GET /api/v1/image/partitions`），以下拉
  列出「整盘」与各分区（序号 · 容量 · 类型），默认选中后端返回的 `default_index`。
  无分区表时只提供「整盘」，并提示 `未能读取分区表，将按整盘挂载`——**不阻断**操作。
  读取失败不是致命错误：损坏的镜像仍应能按整盘/缓存尝试。
  镜像下拉框是 `change` 事件直接触发读取，**不做防抖**——一次变更就是一次原子选择；
  但仍保留「过期响应不得覆盖新结果」的请求序号保护。
- 显示：loop 设备名、分区子设备（若有）、**精确挂载点路径**。
- 能力不可用时给出明确说明与 USB 编辑引导，而非笼统报错。
- 可见性说明：明确告知**精确挂载路径**，并建议使用支持 root 权限的文件管理器。
  界面**不承诺**第三方文件管理器自动可见（能否看到取决于该 app 自身的 mount
  namespace），但也不冗述机制——写清路径与建议即可。

### 6. 设置与诊断

- 默认设备模式。
- **USB 设备身份**（`idVendor`/`idProduct` 与三个字符串描述符）。界面有硬性要求：
  - **解释 VID/PID 是什么**（`Vendor ID`/`Product ID`、取值范围），不能只给两个十六进制输入框；
  - 说明**保存后不会立刻断开 USB**，需等主机**重新枚举**才生效；
  - 说明**断开 USB 后 init 会重置 VID/PID**，并据此给出可操作建议：**在断开 USB 连接的情况下保存**。
  - 字段长度上限（126 字节）由输入框的 `maxlength` 与运行时校验承担，界面不再常驻
    解释制造商/产品名可填中文、序列号限可打印 ASCII 这类字段级细节。
- **镜像安全上下文（SE 标签）**：编辑 `config/gadget.json` 的 `image_context`
  （挂载与格式化前对 `images/` 下镜像自动应用的目标标签）。界面要求：
  - 说明模块在**挂载或格式化之前**修正标签、默认值为 `media_rw_data_file`、
    且改动**下一次挂载才生效**；
  - 说明只作用于 `images/` 目录下的镜像（目录外的文件仅告警、**绝不修改**）；
  - 「恢复默认」设计为独立的显式操作按钮（避免提交空输入框造成语义歧义）；
  - 读写交互通过**独立端点**（`GET|POST /api/v1/config/security`）而非复用身份配置端点
    `config`：二者同存于 `config/gadget.json`，拆分接口方可确保操作互不覆盖。
  - 背景与风险的**机制描述**（内核线程按自己的安全域判定、只读标签导致写入被静默丢弃等）
    归本文档与 [Android 集成](android-integration.md)，界面不重复——界面只保留
    用户据此能采取行动的事实。
- 能力探测结果（loop、`max_part`、文件系统支持、mass_storage 支持、SELinux 状态）。
  只列**原始事实**，不展示由它们派生的挂载方式结论（挂载路径只有一条）。
- **不设常驻排查面板**（原「排查提示」卡片已移除）。失败信息由错误面板承担；
  进一步排查看 `logs/` 与 `dmesg | grep avc`（见 [测试规范](testing.md) 的真机验收清单）。

### 5. 本地编辑（上下文告警）

挂载前修正 SELinux 标签的告警**不阻断挂载**，但必须在 `#loop-context-note` 区域
明确提示潜在后果（中文结论 + 后端英文原文），并引导用户前往设置页的安全标签卡片——避免用户仅看到
“挂载成功”提示，后续在电脑端遭遇无法读取内容等异常时无法排查根因。

## 状态归属

- **用户偏好**（主题、默认模式）可存 `localStorage`。
- **磁盘、挂载、loop、编辑会话状态一律以后端为唯一来源**，不从 `localStorage` 读取权威状态。
- **分区列表以镜像文件本身为唯一来源**（后端读分区表），不缓存到 `localStorage`：
  镜像可能被外部工具改写，缓存会与实际不符。

理由：`localStorage` 会随管理器应用卸载而丢失；且存在 CLI 与 WebUI 两个入口，本地缓存易与真实状态不一致。可缓存以优化渲染，但必须标注为「上次已知状态」并在后台查询后覆盖。

## 必须在界面上给出的硬约束

这两条约束来自底层机制，用户操作时容易困惑，因此界面必须**明确说出来**（而不是等
后端报错才发现）：

1. **切换镜像必须先卸载**：configfs 的 `lun.N/file` 属性只能在 LUN 全新创建、尚未绑定文件时写入，无法覆写。见 [Android 集成](android-integration.md)。
2. **挂载到本地前必须先卸载 USB**：同一镜像不可同时作为 gadget LUN 与 loop 附件，否则双写会损坏文件系统。

> 界面给出的是**约束本身**（可操作的指令）。背后的机理（configfs 属性为何不可覆写、
> 双写为何损坏文件系统）由本文档与 [Android 集成](android-integration.md) 承载，
> 界面不重复——零构建的窄屏页面上，长篇原理会把操作项挤出首屏。

## 无障碍与尺寸

- 窄屏（手机）优先；布局须适配安全区（insets）。
- 交互元素触控目标不小于 44×44 dp。
- 不使用仅靠颜色传达状态的方式（状态需有文字标注）。
