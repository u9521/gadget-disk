// backend.js —— 后端通道与后端状态机。
//
// 从 app.js 的「后端通道」「后端状态机」两节原样拆出：模块/数据目录、REST 引导
// 信息（api.json）的探测与按需拉起 serve、REST→CLI 回退的单一入口 `callBackend`，
// 以及离线/在线状态机与指数退避重连。
//
// **后端状态的唯一更新点**是 `updateBackendState`，且只被 `callBackend` 的单一
// 出口调用——离线横幅与变更按钮可用性都由它驱动，见 docs/webui.md。

import { exec, toast, moduleInfo } from './ksu.js';
import { sizeNote, formatBytes, parsePartitionSize, parseSizeInput, validateSize } from './pure/bytes.js';
import { buildCliArgs, buildRestCall, classifyBackendFailure, execProbeSucceeded, nextReconnectDelay, parseApiInfo, parseExecResult, restResultToExecResult, restUrl, shouldBlockActions } from './pure/channel.js';
import { describeFilesystem, describeGptPartitionType, describeInUse, describeLayout, describeMbrPartitionType, describeMode, messageForCode, modeWarning } from './pure/describe.js';
import { CUSTOM_TYPE_VALUE, FILESYSTEMS, MAX_PARTITIONS, MBR_MAX_EXTENDED, MBR_MAX_LOGICAL, MBR_MAX_PRIMARY, buildPartitionOptions, customTypeWire, defaultPartitionType, formatPartitionScan, gptTypeWire, layoutSupportsPartitionNames, layoutSupportsPartitions, mbrSlotUsage, mbrTypeWire, partitionKernelIndex, partitionTypePresets, validatePartitions } from './pure/partitions.js';
import { baseName, joinPath, parentPath, safeImageName, shellQuote } from './pure/paths.js';
import { INQUIRY_STRING_MAX, MAX_LUNS, SLOW_TASK_THRESHOLD_MS, buildImageOptions, describeFormattingSource, describeSlot, imageNameExists, mergeSlotRows, taskProgressLabel, validateIdentityField } from './pure/task.js';
import { $, failed, showError } from './dom.js';

// ---------------------------------------------------------------- 后端通道

/** 模块目录；在 KernelSU WebUI 中由 ksu.moduleInfo() 提供。 */
export const MODDIR = resolveModDir();

/** 数据目录（与 Rust 侧 `DEFAULT_DATA_ROOT` 一致）。 */
export const DATA_DIR = '/data/adb/gadget-disk';

/** REST 引导信息文件名；与页面同源（WebViewAssetLoader 把源映射到 webroot/）。 */
export const API_JSON_URL = 'api.json';

/**
 * 后端通道状态。
 *
 * - `unknown`：尚未有过一次结果（首屏）；
 * - `online`：**至少一条**通道可用（REST 或 CLI 回退成功）；
 * - `offline`：REST 与 CLI **都**失败，变更类操作必须暂停。
 *
 * 只在 `callBackend` 的单一出口处更新，避免多处写导致状态漂移。
 *
 * @type {{status: 'unknown'|'online'|'offline', lastError: {message?: string, detail?: string}|null}}
 */
export const backendState = { status: 'unknown', lastError: null };

/** 连续的重连失败次数（决定退避延迟）；任一次成功即归零。 */
export let reconnectFailures = 0;

/** 自动重连的定时器句柄；同一时刻只允许一个。 */
export let reconnectTimer = null;

/** 进行中的重连；手动点击与自动重试共用它，避免并发拉起两个 serve。 */
export let reconnectInFlight = null;

/** serve 的日志（诊断用）；无界增长会拖垮 /data，故超过阈值时截断。 */
export const SERVE_LOG = `${DATA_DIR}/logs/serve.log`;
export const SERVE_LOG_MAX_BYTES = 1024 * 1024;

/** 拉起 serve 后等待 api.json 出现的参数：8 × 250ms，本机启动是毫秒级。 */
export const SERVE_POLL_ATTEMPTS = 8;
export const SERVE_POLL_INTERVAL_MS = 250;

/**
 * 允许的 serve 启动尝试次数。
 *
 * serve 空闲 60 秒就退出，因此「拉起」是常态而非异常；但若它反复崩溃（例如
 * `/data` 不可写、端口被 SELinux 拦），无限重启会成为重启风暴，故设上限。
 */
export const MAX_SERVE_START_ATTEMPTS = 2;

/**
 * 后端二进制路径。
 *
 * `customize.sh` 安装时会把**本机架构**的那份二进制从 `bin/<abi>/` 移到扁平的
 * `bin/gadgetdisk`（`gdd` 同理），因此运行时只有一条路径——不需要再按 ABI
 * 探测。安装期就确定架构，比运行期每次探测更可靠（WebUI 读不到
 * `ro.product.cpu.abi`，猜错会让所有命令都失败且症状是「后端不可达」）。
 *
 * 仍保留兼容项：早期安装的模块里二进制在 `bin/<abi>/` 下，升级后未重装时
 * 仍应可用。
 *
 * @type {readonly string[]}
 */
export const BIN_CANDIDATES = [
  joinPath(MODDIR, 'bin/gadgetdisk'),
  // 兼容旧布局（安装脚本未重跑时）。
  ...['arm64-v8a', 'x86_64'].map((abi) => joinPath(MODDIR, `bin/${abi}/gadgetdisk`)),
];

/** 当前生效的二进制路径；探测成功后缓存，避免每次调用都探测。 */
export let resolvedBin = '';

/**
 * 探测可用的后端二进制路径。
 *
 * 优先检查执行权限，返回值经 execProbeSucceeded 校验。
 * @returns {Promise<string>}
 */
export async function resolveBin() {
  if (resolvedBin) return resolvedBin;
  for (const candidate of BIN_CANDIDATES) {
    const result = await exec(`[ -x ${shellQuote(candidate)} ] && echo yes`);
    if (execProbeSucceeded(result)) {
      resolvedBin = candidate;
      return resolvedBin;
    }
  }
  // 都不可用：返回首选路径，让后续命令报出真实的错误（找不到文件），
  // 而不是在这里抛异常导致整个页面不可用。
  resolvedBin = BIN_CANDIDATES[0];
  return resolvedBin;
}

/**
 * 已解析的 `api.json` 缓存。
 *
 * - `undefined`：尚未探测过；
 * - `null`：探测过但不可用（此后走 CLI 回退）；
 * - `{port, token}`：可用。
 *
 * @type {{port: number, token: string}|null|undefined}
 */
let apiInfo;

/** `api.json` 是否被判定为陈旧（上一次 REST 调用在**网络层**失败）。 */
export let apiInfoSuspect = false;

/** 被判定陈旧的端口；重启 serve 后必须等到不同的端口（见 probeApiInfo）。 */
export let stalePort = null;

/** 进行中的探测；首次加载的多个刷新共享同一次探测，避免各自拉起一次 serve。 */
export let apiInfoProbe = null;

/** 已尝试拉起 serve 的次数（见 MAX_SERVE_START_ATTEMPTS）。 */
export let serveStartAttempts = 0;

/**
 * 读取并解析 `api.json`。
 *
 * 用的是**相对 URL**：页面由 KernelSU 的 WebViewAssetLoader 提供，`api.json`
 * 与页面同源，因此不需要（也不能）去猜绝对路径。
 *
 * @returns {Promise<{port: number, token: string}|null>}
 */
export async function readApiInfo() {
  try {
    const response = await fetch(API_JSON_URL, { cache: 'no-store' });
    if (!response.ok) return null;
    return parseApiInfo(await response.text());
  } catch (error) {
    // 文件不存在（serve 没起来）与读取失败是同一类情况：没有 REST，走 CLI。
    return null;
  }
}

/**
 * 取得可用的 REST 引导信息；必要时按需拉起 serve。
 *
 * @returns {Promise<{port: number, token: string}|null>}
 */
export async function loadApiInfo() {
  if (apiInfo) return apiInfo;
  if (!apiInfoProbe) {
    apiInfoProbe = probeApiInfo().finally(() => {
      apiInfoProbe = null;
    });
  }
  return apiInfoProbe;
}

/**
 * 探测一次 REST 后端：先看现成的 `api.json`，没有再拉起 serve 并等它写出文件。
 *
 * @returns {Promise<{port: number, token: string}|null>}
 */
export async function probeApiInfo() {
  // 判定陈旧的 api.json 不再采信：serve 可能已被杀（来不及删文件），
  // 也可能是上一次运行留下的端口。启动中的 serve 会原子地覆盖它。
  let info = apiInfoSuspect ? null : await readApiInfo();
  if (!info && serveStartAttempts < MAX_SERVE_START_ATTEMPTS) {
    serveStartAttempts += 1;
    await startServe();
    // 陈旧情形下必须等到**端口变化**：旧文件原地不动，立刻读回它等于没重启。
    info = await pollApiInfo(apiInfoSuspect ? stalePort : null);
  }
  apiInfo = info;
  apiInfoSuspect = false;
  stalePort = null;
  return info;
}

/**
 * 轮询等待 `api.json` 出现（或换了端口）。
 *
 * @param {number|null} avoidPort 不接受这个端口（陈旧文件的端口）
 * @returns {Promise<{port: number, token: string}|null>}
 */
export async function pollApiInfo(avoidPort) {
  for (let i = 0; i < SERVE_POLL_ATTEMPTS; i += 1) {
    const info = await readApiInfo();
    if (info && (avoidPort === null || info.port !== avoidPort)) return info;
    await delay(SERVE_POLL_INTERVAL_MS);
  }
  return null;
}

/**
 * 按需拉起 REST 后端（`gadgetdisk serve`）。
 * 通过 setsid 脱离会话并重定向 stdio，避免 KernelSU 回收父进程组时触发 SIGKILL。
 * @returns {Promise<void>}
 */
export async function startServe() {
  const bin = await resolveBin();
  // 日志截断：serve 每次启动与每个错误都写日志，长期使用会无界增长。
  // 用 `;` 连接：截断失败（例如缺 stat）不应妨碍 serve 启动。
  const rotate =
    `[ -s ${shellQuote(SERVE_LOG)} ] && ` +
    `[ "$(stat -c %s ${shellQuote(SERVE_LOG)} 2>/dev/null || echo 0)" -gt ${SERVE_LOG_MAX_BYTES} ] && ` +
    `: > ${shellQuote(SERVE_LOG)}`;
  const spawn =
    `setsid ${shellQuote(bin)} serve --data-dir ${shellQuote(DATA_DIR)} ` +
    `--module-dir ${shellQuote(MODDIR)} >> ${shellQuote(SERVE_LOG)} 2>&1 < /dev/null &`;
  const result = await exec(`${rotate}; ${spawn}`);
  if (result.errno !== 0) {
    apiInfoSuspect = true;
  }
}

/**
 * 丢弃缓存的引导信息。
 *
 * @param {boolean} suspect 是否为**网络层**失败（true 表示 api.json 本身已不可信，
 *   下次探测不再采信磁盘上的旧文件）
 */
export function invalidateApiInfo(suspect, info) {
  apiInfo = null;
  apiInfoSuspect = suspect === true;
  // 记下被判定陈旧的端口：重启 serve 后必须等到**不同的**端口才认为它可用。
  stalePort = suspect === true && info ? info.port : null;
}

/**
 * 统一的调用入口：REST 优先，网络层失败回退 CLI。
 *
 * **后端状态的唯一更新点**：REST 失败但 CLI 回退成功仍算 `online`——CLI 是
 * 文档化的安全网（docs/webui.md「后端通道」），只有两条通道都失败才是 `offline`。
 * 集中在这里更新，是为了让「界面认为后端在线」与「实际拿到过数据」永远一致。
 *
 * @param {{op: string, [key: string]: any}} call 结构化调用（映射见 pure/channel.js）
 * @returns {Promise<{ok: true, data: any}|{ok: false, kind: string, message: string, detail: string}>}
 */
export async function callBackend(call) {
  const rest = buildRestCall(call);
  let result = null;

  if (rest) {
    const info = await loadApiInfo();
    if (info) {
      const raw = await callRest(info, rest);
      // raw 为 null 表示网络层失败，且已 invalidateApiInfo()：本次改走 CLI 回退。
      if (raw) result = parseExecResult(raw);
    }
  }

  if (result === null) {
    // `buildRestCall` 返回 null 有两种含义：未知 op（真内部错误），或该 op
    // **没有 REST 映射**（例如分区表读取）。两者都要先看 CLI 是否有等价命令。
    result = await runCli(call);

    // 无 CLI 等价物：这是**该操作**的通道缺失，不是整条后端不可用
    // （CLI 可能对其它操作完全正常）。因此 kind 保持 no_cli_fallback，
    // 由 classifyBackendFailure 归为 online——绝不因为读不了分区表就
    // 把挂载/卸载等操作一并锁死。
    if (result.kind === 'no_cli_fallback') {
      result = {
        ok: false,
        kind: 'no_cli_fallback',
        message: result.message,
        detail: '该操作需要 REST 后端通道支持，后台服务（serve）未运行或不可用。',
      };
    }
  }

  // **唯一出口**：后端状态只在这里更新。REST 失败但 CLI 回退成功仍是 online
  // （CLI 是文档化的安全网），只有两条通道都失败才是 offline。
  updateBackendState(result);
  return result;
}

/**
 * 写入后端状态并驱动界面。
 *
 * @param {{ok: boolean, kind?: string, code?: string, message?: string, detail?: string}} result
 */
export function updateBackendState(result) {
  if (result && result.ok) {
    backendState.lastError = null;
    if (backendState.status !== 'online') {
      const wasOffline = backendState.status === 'offline';
      backendState.status = 'online';
      reconnectFailures = 0;
      renderBackendState();
      if (wasOffline) toast('已重新连接');
    }
    return;
  }

  const next = classifyBackendFailure(result);
  backendState.lastError = result
    ? { message: result.message || '后端服务未响应', detail: result.detail || '' }
    : { message: '后端服务未响应', detail: '' };
  if (next === 'offline') {
    backendState.status = 'offline';
    renderBackendState();
    scheduleReconnect();
  }
  // 业务错误（image_in_use / no_space…）不改状态：后端活着并正常答复，
  // 只有单次操作的错误面板需要更新（由调用方的 failed() 负责）。
}

/**
 * 发起一次 REST 请求。
 *
 * 地址必须是 IPv4 字面量（`REST_HOST`）：实测用回环**主机名**会解析到 IPv6
 * `::1`，而 serve 只绑定 IPv4 回环，结果是 `Failed to fetch`。
 *
 * @param {{port: number, token: string}} info
 * @param {{method: string, path: string, body: string|null}} rest
 * @returns {Promise<{errno: number, stdout: string, stderr: string}|null>}
 *   网络层失败（`Failed to fetch`）返回 `null`，由调用方回退 CLI。
 */
export async function callRest(info, rest) {
  try {
    const response = await fetch(restUrl(info, rest.path), {
      method: rest.method,
      headers: {
        // 带 Authorization 会让请求变成「非简单请求」，浏览器会先发预检，
        // 因此 Content-Type 必须显式给出，且与服务端的 Allow-Headers 一致。
        'Content-Type': 'application/json',
        Authorization: `Bearer ${info.token}`,
      },
      body: rest.body,
    });
    const text = await response.text();
    if (response.status === 401) {
      // token 不对说明我们的 api.json 已经过期（例如 serve 重启后换了 token）：
      // 丢弃缓存，下次重新读。这类 401 不是网络故障，故不算「陈旧」。
      invalidateApiInfo(false, null);
    }
    return restResultToExecResult(response.status, text);
  } catch (error) {
    // TypeError: Failed to fetch —— serve 已退出、端口被占或 api.json 陈旧。
    invalidateApiInfo(true, info);
    return null;
  }
}

/**
 * 发一次**原始字节**请求（仅分块上传使用）。
 *
 * 为什么不复用 `callRest`：那里把 `body` 当 JSON 字符串发（`Content-Type:
 * application/json`），而上传块是二进制。若把块 base64 进 JSON，体积会多约 33%，
 * 而 HTTP 层已不再有 1 MiB 请求体上限（见 `http.rs` 的 `RequestBody`），
 * base64 原来的唯一理由消失了。
 *
 * **返回 `null` 表示通道不可用**（与 `callRest` 一致），调用方据此提示用户
 * 「上传只有 REST 通道」而不是把它当成一次业务失败。
 *
 * @param {string} path 含查询串的路径（如 `/api/v1/upload/chunk?upload_id=..&offset=..`）
 * @param {BodyInit} body 原始请求体
 * @returns {Promise<{errno: number, stdout: string, stderr: string}|null>}
 */
export async function callRestRaw(path, body) {
  const info = await loadApiInfo();
  if (!info) return null;
  try {
    const response = await fetch(restUrl(info, path), {
      method: 'POST',
      headers: {
        // 不设 Content-Type：让浏览器按体自行判定。仍带 Authorization，
        // 因此这是「非简单请求」，预检由服务端的 OPTIONS 处理。
        Authorization: `Bearer ${info.token}`,
      },
      body,
    });
    const text = await response.text();
    if (response.status === 401) {
      invalidateApiInfo(false, null);
    }
    // **必须**与 `callBackend` 一样把结果规整成 `{ok, data|message, ...}`：
    // 早期版本直接返回 `restResultToExecResult(...)` 的**原始 exec 形状**
    // （`{errno, stdout, stderr}`），而调用方是按 `{ok, ...}` 判断成败的。
    // 后果是**成功也被当成失败**：`result.ok` 为 `undefined`（falsy），
    // 于是界面报「后端返回了无法识别的失败结果」，而服务端其实已经收下全部字节。
    const result = parseExecResult(restResultToExecResult(response.status, text));
    updateBackendState(result);
    return result;
  } catch (error) {
    // TypeError: Failed to fetch —— serve 已退出、端口被占或 api.json 陈旧。
    invalidateApiInfo(true, info);
    return null;
  }
}

/**
 * CLI 回退路径：把结构化调用映射为参数串，用 `ksu.exec` 执行。
 *
 * 这是**安全网**：REST 后端不在（没起来、被杀、空闲退出）时界面仍然完全可用。
 *
 * @param {{op: string, [key: string]: any}} call
 * @returns {Promise<{ok: boolean, data?: any, message?: string}>}
 */
export async function runCli(call) {
  const args = buildCliArgs(call);
  if (!args) {
    // 区分「该 op 本来就没有 CLI 等价物」与「未知 op」：前者是**该操作**
    // 缺通道（后端不在时它就用不了），后者才是真正的内部错误。
    //
    // 目前只有两个 op 属于前者：读分区表，以及轮询 job 状态（job 注册表是
    // `serve` 进程内的内存，CLI 没有、也不可能有等价子命令）。因此文案不能
    // 只提「分区表」。
    if (call && buildRestCall(call)) {
      return {
        ok: false,
        kind: 'no_cli_fallback',
        message: '该操作需要 REST 后端支持（后台服务未运行或不可用）',
        detail: JSON.stringify(call),
      };
    }
    return {
      ok: false,
      kind: 'internal',
      message: '内部错误：无法识别的后端指令',
      detail: JSON.stringify(call),
    };
  }

  const bin = await resolveBin();
  // 所有子命令都是一次性的顶层命令，拼法完全一致；`--data-dir` 是**全局**参数，
  // 因此放在子命令之前即可（Rust 侧声明为 `global = true`）。
  const command =
    `${shellQuote(bin)} --data-dir ${shellQuote(DATA_DIR)} ${args}`;

  const result = await exec(command);
  return parseExecResult(result);
}

/**
 * 调用后端（REST 优先）。
 *
 * @param {{op: string, [key: string]: any}} call
 * @returns {Promise<{ok: boolean, data?: any, message?: string}>}
 */
export async function callCli(call) {
  return callBackend(call);
}

/**
 * 调用只读工具。
 *
 * 语义上与 `callCli` 相同（都由 `callBackend` 统一入口决定通道），分开命名只为
 * 在调用点表明「这是不经 `gdd` 的只读操作」。
 *
 * @param {{op: string, [key: string]: any}} call
 * @returns {Promise<{ok: boolean, data?: any, message?: string}>}
 */
export async function callTool(call) {
  return callBackend(call);
}

/**
 * 解析模块目录。
 *
 * `ksu.moduleInfo()` 在部分版本返回 `{modDir}`，部分返回字符串；
 * 两种形态都要容忍，否则会拼出错误路径导致全部命令失败。
 *
 * @returns {string}
 */
export function resolveModDir() {
  const fallback = '/data/adb/modules/gadget-disk';
  try {
    const info = moduleInfo();
    if (!info) return fallback;
    if (typeof info === 'string') {
      try {
        const parsed = JSON.parse(info);
        return parsed.modDir || parsed.moduleDir || fallback;
      } catch (error) {
        return fallback;
      }
    }
    return info.modDir || info.moduleDir || fallback;
  } catch (error) {
    return fallback;
  }
}

/** 重置退避计数：手动重连时立即重试，从最短延迟重新开始。 */
export function resetReconnectFailures() {
  reconnectFailures = 0;
}

/**
 * @param {number} ms
 * @returns {Promise<void>}
 */
export function delay(ms) {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

/** 状态归属：用户偏好可存 localStorage，权威状态一律来自后端。 */
export const PREFS_KEY = 'gadgetdisk.prefs';


// ---------------------------------------------------------------- 后端状态机
//
// 与 #error-panel 的分工（docs/webui.md）：
// - 错误面板 = **单次操作**失败，下次成功即清除；
// - 离线横幅 = **通道不可用**，持久显示直到恢复。两者可以同时可见。

/**
 * 「导入视图当前是否已选中文件」的读取口；由 view-import.js 在加载时注入。
 *
 * 用注入而不是直接 import：`renderBackendState` 恢复变更按钮可用性时要知道
 * 导入按钮是否仍该禁用，而 view-import.js 反过来要 import 本模块的 `callCli`，
 * 互相直接引用会形成模块循环。默认返回 null（= 未选中），与初始状态一致。
 *
 * @type {() => (string|null)}
 */
export let importSelection = () => null;

/** @param {() => (string|null)} read */
export function setImportSelection(read) {
  importSelection = read;
}

/**
 * 渲染离线/在线状态：横幅可见性 + 变更类按钮可用性。
 *
 * 只在状态**变化**时提示「已重新连接」，避免每次成功刷新都弹一次。
 */
export function renderBackendState() {
  const banner = $('offline-banner');
  const offline = shouldBlockActions(backendState.status);

  if (offline) {
    const error = backendState.lastError || {};
    $('offline-reason').textContent = error.message || '后端服务未响应';
    const detail = $('offline-detail');
    if (error.detail) {
      detail.textContent = error.detail;
      detail.hidden = false;
    } else {
      detail.hidden = true;
    }
  }

  banner.hidden = !offline;

  // 变更类按钮：离线时全部禁用。刷新按钮不带 `data-role="mutation"`，保持可用
  // （它们是探针）。用属性选择器而不是固定 id 列表，是为了覆盖**动态生成**的按钮
  // （镜像列表里的「删除」）——它们同样必须被暂停。
  for (const button of document.querySelectorAll('[data-role="mutation"]')) {
    if (offline) {
      button.disabled = true;
    } else if (button.getAttribute('aria-busy') !== 'true') {
      // 仅在没有任务占用该按钮时恢复；否则会把进行中的操作放开成可重复点击。
      button.disabled = false;
    }
  }

  // 「导入选中文件」自身还有「未选择文件」这一层禁用条件，不能被这里放开。
  if (!offline) {
    const importButton = $('btn-import');
    if (importButton && !importSelection()) importButton.disabled = true;
  }
}

/**
 * 健康检查：用只读 `status` 走一次完整通道（含按需拉起 serve）。
 *
 * @returns {Promise<boolean>} 后端是否可用
 */
export async function backendHealthy() {
  const result = await callBackend({ op: 'status' });
  return Boolean(result && result.ok);
}

/**
 * 重连：作废缓存的 `api.json` → 重新探测（可能按需拉起 serve）→ 健康检查。
 *
 * **单飞（single-flight）**：手动点击与自动重试共用同一个 Promise。否则两者并发
 * 会各自拉起一次 serve（文档明确禁止的重启风暴），还可能互相作废刚拿到的 token。
 *
 * @returns {Promise<boolean>}
 */
export async function reconnect() {
  if (reconnectInFlight) return reconnectInFlight;

  reconnectInFlight = (async () => {
    const indicator = $('offline-retrying');
    if (indicator) indicator.hidden = false;

    // 缓存作废：serve 可能已经退出并换了端口/token，旧 api.json 只会让健康检查
    // 打到一个死端口上。（suspect=true 还要求重启后必须等到**不同**的端口。）
    invalidateApiInfo(true, apiInfo);

    let ok = false;
    try {
      await loadApiInfo();
      ok = await backendHealthy();
    } catch (error) {
      ok = false;
    } finally {
      if (indicator) indicator.hidden = true;
    }

    if (ok) {
      // 成功路径由 callBackend → updateBackendState 收尾（含「已重新连接」提示）。
      reconnectFailures = 0;
      clearReconnectTimer();
      return true;
    }

    // 仍然不可用：累加失败次数并安排下一次退避重试。
    backendState.status = 'offline';
    renderBackendState();
    scheduleReconnect();
    return false;
  })().finally(() => {
    reconnectInFlight = null;
  });

  return reconnectInFlight;
}

/** 取消已排期的自动重连。 */
export function clearReconnectTimer() {
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
}

/**
 * 安排一次自动重连（指数退避，上限 RECONNECT_MAX_MS）。
 *
 * 只允许一个待执行的定时器：每次失败都排一个的话，离线期间会积累出多个
 * 并发重连。
 */
export function scheduleReconnect() {
  // 注意**不**在这里检查 reconnectInFlight：重连自身失败时它仍为真（清理在
  // finally 里、晚于本调用），检查它会让「重连失败 → 再次排期」这条路径断掉。
  if (reconnectTimer !== null) return;
  reconnectFailures += 1;
  const delay = nextReconnectDelay(reconnectFailures);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    // 离线状态下重试；期间若被别的成功调用恢复，重试会自然变成一次无害的探测。
    if (backendState.status === 'offline') reconnect();
  }, delay);
}
