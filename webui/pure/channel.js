// channel.js —— 后端通道：ksu.exec 输出解析、REST 路由与 CLI 参数编译。
//
// REST 优先 + CLI 回退（docs/webui.md「后端通道」）；本节不发请求、不碰 DOM，
// 真正的 `fetch` 与进程启动留在 backend.js。

import { messageForCode, truncate } from './describe.js';
import { MODES, normalizePartitions } from './partitions.js';
import { shellQuote } from './paths.js';
import { INQUIRY_STRING_MAX, MAX_LUNS, RECONNECT_BASE_MS, RECONNECT_MAX_MS, normalizeIdentity, normalizeImageContext, stringField } from './task.js';

// ---------------------------------------------------------------- 解析

/**
 * 解析一次 `ksu.exec` 的结果。
 *
 * 必须处理三类异常（docs/webui.md，缺一即可能白屏）：
 * 1. 命令失败：`errno !== 0`；
 * 2. 空输出：`stdout.trim() === ''`；
 * 3. JSON 解析失败。
 *
 * stdout 可能混入 shell 警告或 busybox 提示，故对前后空白宽容；
 * 若整段无法解析，则尝试提取首个 `{...}` 或 `[...]` 片段。
 *
 * **错误 JSON 也可能在 stderr**：CLI 的错误输出走 stderr（`output::JsonOutput`
 * 的 `to_stderr = true`，见 crates/gadgetdisk-cli/src/output.rs），因此
 * `{"error":"gdd_unreachable",…}` 从不出现在 stdout。早期实现只看 stdout，
 * 于是这类通道级错误被降级成 `command_failed`，WebUI 的离线状态机永远不触发
 * （实测：两条通道都断时横幅仍不出现）。两条流都要找。
 *
 * @param {{errno: number, stdout: string, stderr: string}} result
 * @returns {{ok: true, data: any} | {ok: false, kind: string, message: string, detail: string, errno?: number}}
 */
export function parseExecResult(result) {
  const errno = result && typeof result.errno === 'number' ? result.errno : -1;
  const stdout = result && typeof result.stdout === 'string' ? result.stdout : '';
  const stderr = result && typeof result.stderr === 'string' ? result.stderr : '';
  const trimmed = stdout.trim();

  // 情形 1：命令失败。优先展示后端返回的 JSON 错误（若有）。
  if (errno !== 0) {
    const embedded = findEmbeddedError(stdout, stderr);
    if (embedded) {
      return {
        ok: false,
        kind: 'backend_error',
        code: embedded.error,
        // **错误码优先，后端 message 只作兜底**。
        //
        // 后端 message 是英文的（面向 CLI/API 使用者），而本界面是中文的。
        // 直接展示它会让中文界面里冒出英文句子。`error` 才是稳定契约
        // （docs/protocol.md：`error` 稳定、`message` 无稳定性承诺），
        // 因此文案由本地的 `messageForCode` 独占，后端原文降级进 `detail`。
        //
        // 回退顺序：已知错误码 → 后端 message（未知码时它至少是一句人话）
        // → 通用兜底。
        message: messageForCode(embedded.error, embedded.message),
        // 详情优先给**另一条**流：错误码已在 message 里，这里要的是排查上下文。
        detail: pickDetail(embedded.source, stdout, stderr, embedded.message),
        errno,
      };
    }
    return {
      ok: false,
      kind: 'command_failed',
      // 保留 errno：分类器要靠它区分「通道不可达（3 / -1）」与普通命令失败。
      errno,
      message: `Command failed (exit code ${errno})`,
      detail: stderr || trimmed || '(no output)',
    };
  }

  // 情形 2：空输出。
  if (trimmed === '') {
    return {
      ok: false,
      kind: 'empty_output',
      message: 'Backend did not respond (command succeeded but produced no output)',
      detail: stderr || `exit code ${errno}`,
    };
  }

  // 情形 3：JSON 解析。容忍前后空白；失败时尝试提取 JSON 片段。
  const parsed = tryParseJson(trimmed);
  if (parsed.ok) {
    return { ok: true, data: parsed.data };
  }

  const extracted = tryExtractJson(trimmed);
  if (extracted) {
    return { ok: true, data: extracted };
  }

  return {
    ok: false,
    kind: 'bad_json',
    message: 'Malformed output (not valid JSON)',
    // 原样展示截断后的 stdout，便于用户复制反馈。
    detail: truncate(trimmed, 400),
  };
}

/**
 * 在 stdout / stderr 中寻找后端返回的错误 JSON。
 *
 * 顺序：先 stdout 后 stderr。成功路径的数据只在 stdout；但**错误**走 stderr，
 * 所以两者都必须检查，否则通道级错误码（`gdd_unreachable`）会被漏掉。
 *
 * @param {string} stdout
 * @param {string} stderr
 * @returns {{error: string, message: string, source: 'stdout'|'stderr'} | null}
 */
function findEmbeddedError(stdout, stderr) {
  for (const [source, text] of [
    ['stdout', stdout],
    ['stderr', stderr],
  ]) {
    const candidate = extractErrorObject(text);
    if (candidate) return { ...candidate, source };
  }
  return null;
}

/**
 * 从一段文本里取出 `{error, message}`。
 *
 * 先整体解析，再退回片段提取——stderr 上可能混有 shell 的警告行。
 *
 * @param {string} text
 * @returns {{error: string, message: string} | null}
 */
function extractErrorObject(text) {
  const trimmed = typeof text === 'string' ? text.trim() : '';
  if (!trimmed) return null;

  const parsed = tryParseJson(trimmed);
  const data = parsed.ok ? parsed.data : tryExtractJson(trimmed);
  if (!data || typeof data !== 'object' || typeof data.error !== 'string' || data.error === '') {
    return null;
  }
  return {
    error: data.error,
    message: typeof data.message === 'string' ? data.message : '',
  };
}

/**
 * 为已识别的错误挑选 `detail`：给排查上下文。
 *
 * 面板上已有本地中文文案（见 `parseExecResult`），detail 的价值在于补充上下文。
 *
 * 三条来源按信息量排序，**后两者要拼接而不是二选一**：
 *
 * 1. **另一条流的原文** —— CLI 通道下错误 JSON 走 stderr，stdout 常有 shell 警告，
 *    那是真正的上下文；
 * 2. **后端 message** —— 英文诊断文本（面向 CLI/API 使用者）。它**不进标题位**
 *    （见 `parseExecResult` 的错误码优先），但必须保留，否则用户拿不到具体原因
 *    （例如「partition 1 (fat32) is 1024 bytes, which is below the fat32 minimum of
 *    34603008 bytes」）；
 * 3. 都没有 → 空串。
 *
 * REST 通道下第 1 条恰好是 `HTTP <code>` 这种贫信息文本（`restResultToExecResult`
 * 把它放进 stderr），因此**必须**与第 2 条拼接——只取「另一条流」会让 detail 退化成
 * 一行状态码，把后端唯一的诊断线索丢掉。（实测踩到：`size_below_minimum` 的
 * detail 只剩 `HTTP 400`。）
 *
 * @param {'stdout'|'stderr'} source 错误 JSON 所在的流
 * @param {string} stdout
 * @param {string} stderr
 * @param {string} [backendMessage] 后端 message
 * @returns {string}
 */
function pickDetail(source, stdout, stderr, backendMessage = '') {
  const other = (source === 'stderr' ? stdout : stderr).trim();
  const same = (source === 'stderr' ? stderr : stdout).trim();
  const context = (other || same).trim();
  const message = typeof backendMessage === 'string' ? backendMessage.trim() : '';

  // 上下文与 message 相同（CLI 通道常见：错误 JSON 就是那两条流之一）时不要重复。
  if (context && message && !context.includes(message)) {
    return truncate(`${context}\n${message}`, 400);
  }
  if (context) return truncate(context, 400);
  return truncate(message, 400);
}

/**
 * 尝试直接解析 JSON。
 *
 * @param {string} text
 * @returns {{ok: true, data: any} | {ok: false}}
 */
function tryParseJson(text) {
  if (!text) return { ok: false };
  try {
    return { ok: true, data: JSON.parse(text) };
  } catch (error) {
    return { ok: false };
  }
}

/**
 * 从混杂输出中提取首个完整 JSON 对象或数组。
 *
 * 应对 stdout 混入 shell 警告的场景。使用括号配对扫描（跳过字符串内的括号），
 * 避免正则无法处理嵌套的问题。
 *
 * @param {string} text
 * @returns {any|null}
 */
export function tryExtractJson(text) {
  if (!text) return null;

  const starts = [];
  for (let i = 0; i < text.length; i += 1) {
    const ch = text[i];
    if (ch === '{' || ch === '[') {
      starts.push(i);
      const slice = text.slice(i);
      const end = findJsonEnd(slice);
      if (end > 0) {
        const candidate = slice.slice(0, end);
        const parsed = tryParseJson(candidate);
        if (parsed.ok) return parsed.data;
      }
    }
  }
  return null;
}

/**
 * 找出从位置 0 开始的 JSON 值结束位置（含），失败返回 -1。
 *
 * @param {string} text
 * @returns {number}
 */
function findJsonEnd(text) {
  const open = text[0];
  if (open !== '{' && open !== '[') return -1;

  const stack = [];
  let inString = false;
  let escaped = false;

  for (let i = 0; i < text.length; i += 1) {
    const ch = text[i];

    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (ch === '\\') {
        escaped = true;
      } else if (ch === '"') {
        inString = false;
      }
      continue;
    }

    if (ch === '"') {
      inString = true;
    } else if (ch === '{' || ch === '[') {
      stack.push(ch);
    } else if (ch === '}' || ch === ']') {
      const expected = ch === '}' ? '{' : '[';
      if (stack.pop() !== expected) return -1;
      if (stack.length === 0) return i + 1;
    }
  }
  return -1;
}

/**
 * 把一次后端失败归类为**通道级**还是**业务级**。
 *
 * 只有通道级失败才代表「后端不可用」：`unreachable`（网络层）与
 * `gdd_unreachable`（REST 与 CLI 两条通道都没接上）。业务错误
 * （`image_in_use`、`no_space`…）说明后端**活着**并且正常答复，绝不能据此
 * 暂停界面。
 *
 * 另外两类同样是**通道级**，必须一并归为离线——它们正是「CLI 回退也走不通」
 * 时的真实形态（实测于 AVD）：
 *
 * - `errno === EXIT_CODE_UNREACHABLE`（3）：CLI 自己判定连不上 `gdd`；
 * - `errno === -1` 且输出为空：`ksu.exec` 根本没能执行（ksu 接口不可用、
 *   二进制缺失或不可执行）——此时两条通道都不可用，界面必须停下来，
 *   否则用户会对着一个永远「正在…」的按钮反复点击。
 *
 * @param {{kind?: string, code?: string, errno?: number}|null} result
 * @returns {'offline'|'online'}
 */
export function classifyBackendFailure(result) {
  if (!result || typeof result !== 'object') return 'online';
  if (result.kind === 'unreachable' || result.code === 'gdd_unreachable') return 'offline';
  // CLI 自报「连不上」的退出码（`ExitCode::Unreachable`）。
  if (result.errno === EXIT_CODE_UNREACHABLE) return 'offline';
  // ksu.exec 完全没跑起来（ksu 接口不可用、二进制缺失/不可执行）：errno -1
  // 是 ksu.js 的合成值，意味着**两条通道都没有可用执行路径**。
  if (result.errno === -1) return 'offline';
  return 'online';
}

/**
 * 离线时是否应暂停所有**变更类**操作。
 *
 * 只读刷新保持可用：它们同时充当探测（重新拉起 serve 的探针）。
 *
 * @param {string} backendStatus
 * @returns {boolean}
 */
export function shouldBlockActions(backendStatus) {
  return backendStatus === 'offline';
}

/**
 * 自动重连的指数退避：`base * 2^(failures-1)`，上限 `max`。
 *
 * @param {number} failures 连续失败次数（<=0 视为第一次）
 * @param {number} [base]
 * @param {number} [max]
 * @returns {number}
 */
export function nextReconnectDelay(failures, base = RECONNECT_BASE_MS, max = RECONNECT_MAX_MS) {
  const start = Number.isFinite(base) && base > 0 ? base : RECONNECT_BASE_MS;
  const cap = Number.isFinite(max) && max > 0 ? max : RECONNECT_MAX_MS;
  const count = Number.isFinite(failures) ? Math.max(1, Math.floor(failures)) : 1;
  // 先取上限再乘，避免 2 ** 大数 溢出成 Infinity（`Math.min` 对 Infinity 仍返回上限，
  // 但显式夹住指数更直观且不依赖浮点边界）。
  const exponent = Math.min(count - 1, 30);
  const delay = start * 2 ** exponent;
  return Math.min(delay, cap);
}

// ---------------------------------------------------------------- 后端通道
//
// REST 优先 + CLI 回退（规格见 docs/webui.md「后端通道」）。
//
// Rust 侧提供按需启动的 REST 后端 `gadgetdisk serve`：启动时把 `{port, token}`
// 原子写入模块 `webroot/api.json`，退出时删除该文件；空闲 60 秒自动退出。
// KernelSU 的 WebViewAssetLoader 把 `https://mui.kernelsu.org` 映射到模块的
// `webroot/`，因此 WebUI 能**同源**读到 api.json，再去 `http://<ipv4>:<port>`
// 调 REST（带上 Bearer token）。
//
// 为什么 REST 优先：每次操作都 fork 一个 CLI 进程（外加一个 shell）的开销与
// 延迟都明显高于一次 HTTP 请求。
//
// 为什么**必须**保留 CLI 回退：serve 不是常驻进程（没起来、被杀、空闲退出都会
// 让 REST 不可用），而界面不能因此不可用。
//
// 本节全部是**纯函数**（不发网络请求、不碰 DOM），故路由映射与结果转换都能在
// Node 中穷举测试；真正的 `fetch` 与进程启动留在 backend.js。

/**
 * REST 后端的地址字面量。
 *
 * 必须是 IPv4 字面量：实测用回环**主机名**会解析到 IPv6 `::1`，而 serve 只
 * 绑定 IPv4 回环，结果是 `Failed to fetch`。
 */
export const REST_HOST = '127.0.0.1';

/**
 * REST 调用失败时的退出码，与 Rust 侧 `ExitCode` 保持一致
 * （`crates/gadgetdisk-cli/src/output.rs`）。
 */
export const EXIT_CODE_UNREACHABLE = 3;
export const EXIT_CODE_SERVER = 4;

/**
 * 判断一次 `ksu.exec` 的结果是否代表探测成功。
 * 校验返回值对象结构（包含数字型 errno 与字符串 stdout），要求 stdout 去除空白后为 "yes"。
 * @param {{errno: number, stdout: string, stderr: string}|any} result
 * @returns {boolean}
 */
export function execProbeSucceeded(result) {
  if (!result || typeof result !== 'object') return false;
  if (typeof result.errno !== 'number' || result.errno !== 0) return false;
  if (typeof result.stdout !== 'string') return false;
  return result.stdout.trim() === 'yes';
}

/**
 * 解析 `api.json` 的内容（REST 引导信息）。
 *
 * 任何不合法的输入都返回 `null`（而不是抛异常）：文件可能不存在（serve 没起
 * 来）、可能是被中断写入的残片，也可能被别的进程改坏——这些都必须退化成
 * 「走 CLI 回退」，而不是让整个界面出错。
 *
 * @param {string} text
 * @returns {{port: number, token: string}|null}
 */
export function parseApiInfo(text) {
  if (typeof text !== 'string' || text.trim() === '') return null;

  let data;
  try {
    data = JSON.parse(text);
  } catch (error) {
    return null;
  }

  if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
  // 端口必须是合法 TCP 端口：serve 用临时端口，0 或越界都说明这份 api.json
  // 不可信（parseInt 也接受 "39481abc" 这类前缀数字，故用 Number.isInteger）。
  if (!Number.isInteger(data.port) || data.port < 1 || data.port > 65535) return null;
  if (typeof data.token !== 'string' || data.token === '') return null;

  return { port: data.port, token: data.token };
}

/**
 * 拼出 REST 请求的绝对 URL。
 *
 * @param {{port: number}} info
 * @param {string} path 以 `/` 起始的路径（含查询串）
 * @returns {string}
 */
export function restUrl(info, path) {
  const port = info && Number.isInteger(info.port) ? info.port : 0;
  return `http://${REST_HOST}:${port}${path}`;
}

/**
 * 将结构化调用对象映射为对应的 REST 请求定义。
 *
 * 统一接收 `{op, ...}` 对象，分别编译为 REST 请求与 CLI 参数序列，
 * 避免在前端运行期反向解析命令行字符串引入歧义。
 *
 * @param {{op: string, [key: string]: any}} call
 * @returns {{method: 'GET'|'POST', path: string, body: string|null}|null}
 */
export function buildRestCall(call) {
  if (!call || typeof call !== 'object') return null;

  const get = (path) => ({ method: 'GET', path, body: null });
  const post = (path, body) => ({ method: 'POST', path, body: JSON.stringify(body) });

  switch (call.op) {
    case 'status':
      return get('/api/v1/status');
    case 'list':
      return get('/api/v1/images');
    case 'list-loop':
      return get('/api/v1/loop');
    case 'capabilities':
      return get('/api/v1/capabilities');

    case 'job': {
      const id = stringField(call.jobId);
      if (!id) return null;
      // job id 由后端生成，但仍需编码：它来自 URL，不能让其中的 `/` 改变路径结构。
      return get(`/api/v1/jobs/${encodeURIComponent(id)}`);
    }

    // 只读工具只剩 `df`：`ls` / `stat` 只为已删除的内置路径浏览器服务，其 REST
    // 路由与 CLI 子命令都已移除，故这里也不再保留分支（留着只会拼出一条打不通
    // 的路径）。见 docs/image-upload-and-import.md 与
    // .agents/notes/implemented/architecture/2026-10-07-chunked-upload-and-flat-webui.md。
    case 'df': {
      const path = stringField(call.path);
      if (!path) return null;
      return get(`/api/v1/tool/df?path=${encodeURIComponent(path)}`);
    }

    // 分区表读取只有 REST 一条通道（CLI 没有对应子命令，见 buildCliArgs）。
    // 路径同样必须编码：它常含中文目录名与空格。
    case 'image-partitions': {
      const path = stringField(call.path);
      if (!path) return null;
      return get(`/api/v1/image/partitions?path=${encodeURIComponent(path)}`);
    }

    case 'create': {
      const path = stringField(call.path);
      if (!path || !Number.isFinite(call.sizeBytes)) return null;
      const body = {
        path,
        size_bytes: call.sizeBytes,
        layout: stringField(call.layout) || 'gpt',
        filesystem: stringField(call.filesystem) || 'fat32',
      };
      // 卷标与分区现在都由 REST 契约承载（历史上只有 CLI 回退路径会用卷标）。
      const label = stringField(call.label);
      if (label) body.volume_label = label;
      const partitions = normalizePartitions(call.partitions, call.layout);
      if (partitions.length > 0) body.partitions = partitions;
      return post('/api/v1/create', body);
    }

    case 'delete': {
      const path = stringField(call.path);
      if (!path) return null;
      return post('/api/v1/delete', { path });
    }

    // 分块上传。注意 `upload-chunk` **不在这里**：它发的是原始字节流而非 JSON，
    // 由 `backend.js` 的 `callRestRaw` 直接处理（见 view-import.js）。
    case 'upload-begin': {
      const destName = stringField(call.destName);
      if (!destName) return null;
      const body = { dest_name: destName };
      // 声明大小只用于进度与空间预检；缺失/为 0 时不发送，交由服务端跳过预检。
      if (Number.isFinite(call.sizeBytes) && call.sizeBytes > 0) {
        body.size_bytes = call.sizeBytes;
      }
      return post('/api/v1/upload/begin', body);
    }

    case 'upload-commit': {
      const uploadId = stringField(call.uploadId);
      if (!uploadId) return null;
      return post('/api/v1/upload/commit', { upload_id: uploadId });
    }

    case 'upload-abort': {
      const uploadId = stringField(call.uploadId);
      if (!uploadId) return null;
      return post('/api/v1/upload/abort', { upload_id: uploadId });
    }

    case 'mount': {
      const devices = normalizeDevices(call.devices);
      if (!devices) return null;
      const body = { devices };
      // `rebind` 只在身份改动后需要（让主机看到新描述符）。缺省不发送，
      // 保持请求体最小。
      if (call.rebind === true) body.rebind = true;
      return post('/api/v1/mount', body);
    }

    case 'rebind':
      return post('/api/v1/rebind', {});

    case 'delete-slot':
      return Number.isInteger(call.lun)
        ? post('/api/v1/slot/delete', { lun: call.lun })
        : null;

    case 'config':
      return get('/api/v1/config');

    case 'config-set': {
      const identity = normalizeIdentity(call.identity);
      if (!identity) return null;
      return post('/api/v1/config', identity);
    }

    // 镜像 SELinux 目标上下文：**独立端点**，不能塞进 `config-set`（那会与身份
    // 的按字段合并语义纠缠，且「只改标签」将有覆盖身份的风险）。
    case 'config-security':
      return get('/api/v1/config/security');

    case 'config-security-set': {
      // `reset` 与 `imageContext` 互斥；两者都没有时**不发送**（本地报错更直接）。
      if (call.reset === true) return post('/api/v1/config/security', { reset: true });
      const context = normalizeImageContext(call.imageContext);
      if (!context) return null;
      return post('/api/v1/config/security', { image_context: context });
    }

    case 'unmount': {
      // lun 省略即「全部卸载」，与服务端 `Option<u8>` 的语义一致。
      const body = Number.isInteger(call.lun) ? { lun: call.lun } : {};
      return post('/api/v1/unmount', body);
    }

    case 'attach-loop': {
      const image = stringField(call.image);
      if (!image) return null;
      return post('/api/v1/loop/attach', {
        image,
        // 只读既可由 read_only 表达，也可由 mode=ro 表达（两者等价）。
        read_only: call.readOnly === true || call.mode === 'ro',
        partition_index: Number.isInteger(call.partition) ? call.partition : null,
      });
    }

    case 'detach-loop': {
      const image = stringField(call.image);
      const loopDev = stringField(call.loopDev);
      if (!image && !loopDev) return null;
      return post('/api/v1/loop/detach', {
        image: image || null,
        loop_dev: loopDev || null,
      });
    }

    default:
      return null;
  }
}

/**
 * 把一次结构化调用映射为 CLI 参数串（**回退路径专用**）。
 *
 * REST 不可用时用 `ksu.exec` 执行它。字段定义与 `buildRestCall` 共用同一个
 * `call` 对象，因此两条通道不会各自漂移。
 *
 * @param {{op: string, [key: string]: any}} call
 * @returns {string|null} 不含二进制路径与 `--data-dir` 的参数字串
 */
export function buildCliArgs(call) {
  if (!call || typeof call !== 'object') return null;

  switch (call.op) {
    // 只读工具：直接是顶层子命令。同样只剩 `df`（`ls`/`stat` 子命令已随路径
    // 浏览器一并移除）。
    case 'df':
      return stringField(call.path) ? `df ${shellQuote(call.path)}` : null;

    case 'status':
      return 'status';
    case 'list':
      return 'list';
    case 'list-loop':
      return 'list-loop';
    case 'capabilities':
      return 'capabilities';

    // 以下两个操作**只有 REST 通道**，显式返回 null 让 callBackend 走
    // 「无回退通道」分支给出可读提示，而不是拼出一条必然「参数不合法」的命令。
    //
    // `job`：job 注册表是 `serve` 进程内的内存，而 CLI 是一次性进程——它既没有
    // `job` 子命令（已删除），也不可能查到别的进程登记的 job。
    case 'job':
      return null;

    // `image-partitions`：`gadgetdisk` 无此子命令。
    case 'image-partitions':
      return null;

    case 'create': {
      if (!stringField(call.path) || !Number.isFinite(call.sizeBytes)) return null;
      let args = `create ${shellQuote(call.path)} --size ${call.sizeBytes}`;
      args += ` --layout ${shellQuote(stringField(call.layout) || 'gpt')}`;
      const filesystem = stringField(call.filesystem);
      if (filesystem) args += ` --filesystem ${shellQuote(filesystem)}`;
      const label = stringField(call.label);
      if (label) args += ` --label ${shellQuote(label)}`;
      // 分区规格按 `SIZE/GPT-TYPE/MBR-TYPE/NAME/FS/KIND` 逐项传给 `--partition`。
      //
      // **用 `/` 而非 `:` 分隔**：类型线格式自带 `gpt:` / `mbr:` 前缀，
      // 同一个冒号无法既当分隔符又当前缀（见 CLI 的 parse_partition_args）。
      // 空段表示"用默认值"，故必须保留占位而不是省略。
      const partitions = normalizePartitions(call.partitions, call.layout);
      for (const p of partitions) {
        const spec = [
          String(p.size_bytes),
          p.gpt_type || '',
          p.mbr_type || '',
          p.name || '',
          p.filesystem || '',
          // 归属段在**末尾**：主分区留空（走后端默认值），只有逻辑分区才填，
          // 这样老请求拼出的命令行与从前逐字节一致。
          p.kind === 'logical' ? 'logical' : '',
        ].join('/');
        args += ` --partition ${shellQuote(spec)}`;
      }
      return args;
    }

    case 'delete':
      return stringField(call.path) ? `delete ${shellQuote(call.path)}` : null;

    // 分块上传**只有 REST 通道**，显式返回 null 让调用方走「该操作没有回退通道」
    // 分支给出可读提示，而不是拼一条必然失败的命令：
    // - `ksu.exec` 无法向子进程写 stdin，一次性进程也承载不了「多个 chunk 请求 +
    //   一个 commit」这组跨请求状态；
    // - 后端也没有对应的 CLI 子命令。
    case 'upload-begin':
    case 'upload-commit':
    case 'upload-abort':
    case 'upload-chunk':
      return null;

    case 'mount': {
      const devices = normalizeDevices(call.devices);
      if (!devices) return null;
      // 位置参数是镜像路径，其余是**可重复选项、按下标对齐**（与 CLI 一致）。
      const images = devices.map((d) => shellQuote(d.image_path)).join(' ');
      const modes = devices.map((d) => `--mode ${shellQuote(d.mode)}`).join(' ');
      const luns = devices
        .filter((d) => Number.isInteger(d.lun))
        .map((d) => `--lun ${d.lun}`)
        .join(' ');
      const inquiries = devices
        .filter((d) => typeof d.inquiry_string === 'string' && d.inquiry_string !== '')
        .map((d) => `--inquiry ${shellQuote(d.inquiry_string)}`)
        .join(' ');
      let args = `mount ${images} ${modes}`;
      if (luns) args += ` ${luns}`;
      if (inquiries) args += ` ${inquiries}`;
      if (call.rebind === true) args += ' --rebind';
      return args;
    }

    case 'rebind':
      return 'rebind';

    case 'delete-slot':
      return Number.isInteger(call.lun) ? `delete-slot --lun ${call.lun}` : null;

    case 'config':
      return 'config get';

    case 'config-set': {
      const identity = normalizeIdentity(call.identity);
      if (!identity) return null;
      let args = 'config set';
      if (Number.isInteger(identity.id_vendor)) args += ` --vid ${identity.id_vendor}`;
      if (Number.isInteger(identity.id_product)) args += ` --pid ${identity.id_product}`;
      if (identity.manufacturer) args += ` --manufacturer ${shellQuote(identity.manufacturer)}`;
      if (identity.product) args += ` --product ${shellQuote(identity.product)}`;
      if (identity.serial) args += ` --serial ${shellQuote(identity.serial)}`;
      return args;
    }

    // 与 REST 一样走独立的 `config security` 子命令（而不是 `config set`）：
    // 后者写的是身份，混在一起会让「只改标签」有覆盖身份的风险。
    case 'config-security':
      return 'config security get';

    case 'config-security-set': {
      if (call.reset === true) return 'config security clear';
      const context = normalizeImageContext(call.imageContext);
      if (!context) return null;
      return `config security set --image-context ${shellQuote(context)}`;
    }

    case 'unmount':
      return Number.isInteger(call.lun) ? `unmount --lun ${call.lun}` : 'unmount';

    case 'attach-loop': {
      if (!stringField(call.image)) return null;
      const mode = call.readOnly === true ? 'ro' : stringField(call.mode) || 'rw';
      let args = `attach-loop ${shellQuote(call.image)} --mode ${shellQuote(mode)}`;
      if (Number.isInteger(call.partition)) args += ` --partition ${call.partition}`;
      if (call.readOnly === true) args += ' --read-only';
      return args;
    }

    case 'detach-loop': {
      const image = stringField(call.image);
      const loopDev = stringField(call.loopDev);
      if (!image && !loopDev) return null;
      let args = 'detach-loop';
      if (image) args += ` --image ${shellQuote(image)}`;
      if (loopDev) args += ` --loop-dev ${shellQuote(loopDev)}`;
      return args;
    }

    default:
      return null;
  }
}

/**
 * 把 HTTP 结果转换为 `parseExecResult` 认识的结构。
 *
 * 这样**全部**既有错误处理（非零 errno → 后端 JSON 错误码 → 中文文案）都能原样
 * 复用：REST 的错误体与服务端一直是同一套 `{"error","message"}`。
 *
 * @param {number} status HTTP 状态码
 * @param {string} text 响应体文本
 * @returns {{errno: number, stdout: string, stderr: string}}
 */
export function restResultToExecResult(status, text) {
  const body = typeof text === 'string' ? text : '';
  const code = typeof status === 'number' ? status : 0;

  // 网络层失败（根本没收到 HTTP 响应）用 3，与 `ExitCode::Unreachable` 一致。
  if (code <= 0) {
    return {
      errno: EXIT_CODE_UNREACHABLE,
      stdout: '',
      stderr: body || 'Cannot reach the REST backend',
    };
  }

  // 未到 4xx 且响应体非空才算成功。空响应体无法通过 parseExecResult 的 JSON
  // 校验，必须归为失败，而不是让它伪装成「输出格式异常」。
  if (code < 400 && body.trim() !== '') {
    return { errno: 0, stdout: body, stderr: '' };
  }

  return {
    errno: EXIT_CODE_SERVER,
    // 响应体放进 stdout：parseExecResult 会从中还原 `error` / `message`。
    stdout: body,
    stderr: code < 400 ? `HTTP ${code} (REST returned an empty body)` : `HTTP ${code}`,
  };
}

/**
 * 校验并归一化设备列表（`mount` 的 devices）。
 *
 * 规则（与 CLI 的 `to_command` 对齐，但这里做的是**前端**校验以获得即时反馈）：
 * - 至少一项；
 * - 每项必须有非空 `image_path`；
 * - `mode` 必须在白名单内，缺省 `rw`；
 * - `lun` 若给出必须是 `0..MAX_LUNS` 的整数；
 * - `inquiry_string` 若给出必须 ≤ `INQUIRY_STRING_MAX`；
 * - 同一 `image_path` 不得出现两次（内核侧会拒，这里提前拦）。
 *
 * @param {any} devices
 * @returns {Array<{image_path: string, mode: string, lun?: number, inquiry_string?: string}>|null}
 *   非法时返回 `null`（调用方据此不发送请求）。
 */
export function normalizeDevices(devices) {
  if (!Array.isArray(devices) || devices.length === 0) return null;
  if (devices.length > MAX_LUNS) return null;

  const out = [];
  const seen = new Set();
  for (const device of devices) {
    if (!device || typeof device !== 'object') return null;
    const imagePath = stringField(device.image_path);
    if (!imagePath) return null;
    if (seen.has(imagePath)) return null;
    seen.add(imagePath);

    const mode = stringField(device.mode) || 'rw';
    // 复用文件顶部导出的白名单，避免两处枚举漂移。
    if (!MODES.includes(mode)) return null;

    const entry = { image_path: imagePath, mode };

    if (device.lun !== undefined && device.lun !== null && device.lun !== '') {
      if (!Number.isInteger(device.lun) || device.lun < 0 || device.lun >= MAX_LUNS) {
        return null;
      }
      entry.lun = device.lun;
    }

    if (device.inquiry_string !== undefined && device.inquiry_string !== null) {
      const inquiry = stringField(device.inquiry_string);
      if (inquiry.length > INQUIRY_STRING_MAX) return null;
      if (inquiry !== '') entry.inquiry_string = inquiry;
    }

    out.push(entry);
  }
  return out;
}
