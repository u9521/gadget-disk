// task.js —— 任务反馈、槽位/镜像选项与 USB 身份校验的纯函数。
//
// 慢任务阈值与身份字段约束见各常量注释；不接触 DOM，可在 Node 中直接测试。

import { formatBytes } from './bytes.js';
import { baseName } from './paths.js';

/**
 * 任务耗时超过该阈值即视为「慢」，界面要给出额外提示。
 *
 * 实测本机操作都是毫秒级（mount≈17ms、attach-loop≈115ms、create 64MiB≈158ms），
 * 因此超过 3 秒基本意味着后端在等待（例如 ioctl 阻塞、磁盘抖动），
 * 用户需要知道「还在进行」而不是「界面卡死了」。
 */
export const SLOW_TASK_THRESHOLD_MS = 3000;

/** 自动重连的首次延迟。 */
export const RECONNECT_BASE_MS = 5000;

/** 自动重连延迟的上限。 */
export const RECONNECT_MAX_MS = 30000;

/**
 * 判断目标镜像名是否与已有镜像冲突。
 *
 * **同名阻断的前端预检**：后端也会拒绝（那是权威判定），但在这里先查一次
 * 可以让用户立刻看到原因，而不必等一次往返。
 *
 * @param {string} name 目标文件名
 * @param {Array<{name: string}>} images 已有镜像列表
 * @returns {boolean}
 */
export function imageNameExists(name, images) {
  if (typeof name !== 'string' || name === '') return false;
  const list = Array.isArray(images) ? images : [];
  return list.some((entry) => {
    const existing = entry && typeof entry.name === 'string' ? entry.name : '';
    // 镜像文件系统可能大小写敏感，但用户视角下 `Disk.img` 与 `disk.img`
    // 几乎必然是想指同一个文件，故按不区分大小写比较（更严格的一侧）。
    return existing.toLowerCase() === name.toLowerCase();
  });
}

/**
 * 描述某文件系统的格式化来源（供创建面板展示探测结果）。
 *
 * @param {Array<{filesystem: string, path: string|null, note: string}>} probes
 * @param {string} filesystem
 * @returns {string}
 */
export function describeFormattingSource(probes, filesystem) {
  const list = Array.isArray(probes) ? probes : [];
  const hit = list.find((p) => p && p.filesystem === filesystem);
  if (!hit) {
    // 探测结果缺失（例如后端较旧）时不谎报，如实说明未知。
    return '格式化方式未知（未取得探测结果）';
  }
  if (hit.path) {
    return `将使用系统工具 ${hit.path}`;
  }
  if (filesystem === 'fat32') {
    return '设备未检测到 mkfs.vfat，将使用内置实现格式化为 FAT32';
  }
  return hit.note || '系统中未找到对应格式化工具，创建将无法完成';
}

// ---------------------------------------------------------------- 任务反馈
//
// 操作本身很快（见 SLOW_TASK_THRESHOLD_MS 的注释），慢的是**反馈**：如果界面在
// await 期间什么都不显示，用户就会以为点击没生效而重复点击。这一节的纯函数负责
// 生成「进行中」文案，视图模块负责把它写进各视图的 `role="status"` 元素。

/**
 * 把已用毫秒格式化为简短时长。
 *
 * 1 秒以下保留一位小数（`0.4s`），便于让用户看到时间**确实在走**；1 秒及以上
 * 用整秒，避免状态行每次跳动都改变宽度。
 *
 * @param {number} ms
 * @returns {string}
 */
export function describeTaskElapsed(ms) {
  const value = Number.isFinite(ms) ? Math.max(0, ms) : 0;
  if (value < 1000) return `${(value / 1000).toFixed(1)}s`;
  return `${Math.round(value / 1000)}s`;
}

/**
 * 该耗时是否已越过「慢任务」阈值。
 *
 * @param {number} elapsedMs
 * @param {number} [thresholdMs]
 * @returns {boolean}
 */
export function shouldWarnSlow(elapsedMs, thresholdMs = SLOW_TASK_THRESHOLD_MS) {
  if (!Number.isFinite(elapsedMs)) return false;
  const threshold = Number.isFinite(thresholdMs) ? thresholdMs : SLOW_TASK_THRESHOLD_MS;
  return elapsedMs > threshold;
}

/**
 * 进行中文案：`正在挂载…（已用 0.4s）`。
 *
 * 越过阈值后追加「仍在进行，请勿关闭页面」，让用户知道是后端在等，
 * 而不是页面失去响应。
 *
 * @param {string} label 动作名（不含「正在」与省略号），例如 `挂载`
 * @param {number} elapsedMs
 * @returns {string}
 */
export function taskProgressLabel(label, elapsedMs) {
  const text = typeof label === 'string' && label.trim() !== '' ? label.trim() : '处理';
  const base = `正在${text}…（已用 ${describeTaskElapsed(elapsedMs)}）`;
  return shouldWarnSlow(elapsedMs) ? `${base}（仍在进行，请勿关闭页面）` : base;
}

/**
 * 取出一个「非空字符串」字段，否则返回 `''`。
 *
 * 跨模块共享的内部工具：本模块与 channel.js 都用它，故在此声明并导出，
 * 避免两处各写一份实现（对外的具名导出见任务规范）。
 *
 * @param {any} value
 * @returns {string}
 */
export function stringField(value) {
  return typeof value === 'string' ? value.trim() : '';
}

/** INQUIRY 字符串的长度上限。
 *
 * 依据：内核 `fsg_store_inquiry_string` 用 `snprintf(..., "%-28s", buf)` 定宽
 * 写入，**超长会被静默截断**。因此前端也按 28 校验，让用户立刻看到问题，
 * 而不是「设了却发现没生效」。
 */
export const INQUIRY_STRING_MAX = 28;

/** 内核支持的 LUN 数量上限（与 `gadgetdisk_usb::MAX_LUNS` 一致）。 */
export const MAX_LUNS = 8;

/**
 * 字符串的 **UTF-8 字节数**。
 *
 * 为什么不用 `TextEncoder`：本层是可在 Node 与浏览器里跑的纯函数层
 * （见 docs/webui.md 的可测试性要求）。`TextEncoder` 两边都有，但手写这段只有
 * 几行、无环境依赖，且能直接对拍 Rust 侧的 `String::len()`。
 *
 * 按**码点**遍历（`for...of`），因此代理对（emoji 等）正确地算 4 字节，而不是
 * 被当成两个孤立的 2 字节代理。
 *
 * @param {string} text
 * @returns {number}
 */
export function utf8ByteLength(text) {
  let bytes = 0;
  for (const ch of String(text)) {
    const cp = ch.codePointAt(0);
    if (cp < 0x80) bytes += 1;
    else if (cp < 0x800) bytes += 2;
    else if (cp < 0x10000) bytes += 3;
    else bytes += 4;
  }
  return bytes;
}

/**
 * 是否含控制字符。
 *
 * 内核 `usb_string_copy` 会剥掉**尾部**换行、内嵌 NUL 会在 C 串处截断，两者都会
 * 让「写入值 ≠ 读回值」而报「身份无法应用」。在输入处就挡住，错误才说得清楚。
 *
 * @param {string} text
 * @returns {boolean}
 */
export function hasControlChar(text) {
  // eslint-disable-next-line no-control-regex
  return /[\u0000-\u001f\u007f]/.test(String(text));
}

/**
 * 是否全部为可打印 ASCII（`0x20..0x7e`）。
 *
 * @param {string} text
 * @returns {boolean}
 */
export function isPrintableAscii(text) {
  return /^[\x20-\x7e]+$/.test(String(text));
}

/**
 * 校验 USB 身份描述符字符串字段。
 *
 * `manufacturer`/`product` 经内核转为 UTF-16LE，允许中文；
 * `serial` 仅允许可打印 ASCII 字符（部分主机驱动对非 ASCII 序列号会拒绝枚举）。
 *
 * @param {string} field  `manufacturer` | `product` | `serial`
 * @param {string} text
 * @returns {string|null} 错误消息；合法时返回 `null`
 */
export function validateIdentityField(field, text) {
  if (text === '') return '不能为空';
  if (hasControlChar(text)) {
    return '不能包含控制字符（如换行符、制表符等）';
  }
  if (utf8ByteLength(text) > STRING_FIELD_MAX_BYTES) {
    return `超过 ${STRING_FIELD_MAX_BYTES} 字节（每个中文字符通常占 3 字节）`;
  }
  if (field === 'serial' && !isPrintableAscii(text)) {
    return '序列号只能包含可打印 ASCII 字符（包含非 ASCII 字符会导致宿主机无法识别设备；制造商与产品名支持中文）';
  }
  return null;
}

/**
 * 校验并归一化身份配置。
 *
 * 规则（与 Rust 侧 `Identity::validate` 对齐）：
 * - 至少设置一个字段（否则「保存空身份」没有意义）；
 * - VID/PID 若给出必须是 `0..65535` 的整数；
 * - `manufacturer`/`product`：UTF-8 字节数 ≤ 126，**可含中文**；
 * - `serial`：额外的可打印 ASCII 限制（限制为可打印 ASCII 字符，避免部分宿主机 USB 驱动枚举失败）。
 *
 * @param {any} identity
 * @returns {object|null}
 */
export function normalizeIdentity(identity) {
  if (!identity || typeof identity !== 'object') return null;

  const out = {};
  let any = false;

  for (const [key, prop] of [
    ['id_vendor', 'id_vendor'],
    ['id_product', 'id_product'],
  ]) {
    const value = identity[key];
    if (value === undefined || value === null || value === '') continue;
    const num = typeof value === 'string' ? Number(value) : value;
    if (!Number.isInteger(num) || num < 0 || num > 0xffff) return null;
    out[prop] = num;
    any = true;
  }

  for (const field of ['manufacturer', 'product', 'serial']) {
    const value = identity[field];
    if (value === undefined || value === null || value === '') continue;
    const text = stringField(value);
    if (!text) return null;
    if (validateIdentityField(field, text) !== null) return null;
    out[field] = text;
    any = true;
  }

  return any ? out : null;
}

/** gadget 字符串描述符长度上限，单位是 **UTF-8 字节**（内核 `usb_string_copy`
 * 的 `USB_MAX_STRING_LEN` 用 `strlen` 判定，故中文一个字算 3 字节）。 */
export const STRING_FIELD_MAX_BYTES = 126;

/** 镜像 SELinux 上下文的长度上限（字节），与 Rust 侧 `selinux::MAX_CONTEXT_BYTES` 一致。 */
export const IMAGE_CONTEXT_MAX_BYTES = 256;

/**
 * 校验镜像文件的 SELinux 目标上下文（`u:object_r:media_rw_data_file:s0`）。
 *
 * ## 为什么前端也要校验
 *
 * 后端是权威判定（`selinux::validate_context_format`），但一次往返 + 一个错误
 * 面板才告诉用户「这里不能有空格」太迟了；而且**非法值绝不能落盘**——它会被
 * 之后每一次挂载重新读到并再次失败。前端校验让用户在点击前就改掉。
 *
 * 规则与后端**逐条对齐**（空、缺 `:`、含空白或控制字符、超长）。**不**解析类型
 * 名是否存在：完整语法由内核判定，前端不该假装比它更懂。
 *
 * @param {string} value
 * @returns {string|null} 错误消息（中文，面向界面）；合法时返回 `null`
 */
export function validateImageContext(value) {
  const text = typeof value === 'string' ? value.trim() : '';
  if (text === '') {
    return '输入不能为空；如需恢复默认请点击「恢复默认」。';
  }
  if (!text.includes(':')) {
    return '格式应为 u:object_r:<类型>:s0（例如 u:object_r:media_rw_data_file:s0）。';
  }
  if (/\s/.test(text)) {
    return '不能包含空白字符（空格、制表符、换行）——内核无法解析包含空白的安全标签。';
  }
  if (hasControlChar(text)) {
    return '不能包含控制字符。';
  }
  if (utf8ByteLength(text) > IMAGE_CONTEXT_MAX_BYTES) {
    return `上下文长度超过上限（最大 ${IMAGE_CONTEXT_MAX_BYTES} 字节）。`;
  }
  return null;
}

/**
 * 归一化镜像上下文：合法时回 `trim` 后的值，否则 `null`。
 *
 * 与 [`validateImageContext`] 共用同一条判定，因此「界面显示的错误」与「是否
 * 真的发出去」不可能不一致。
 *
 * @param {any} value
 * @returns {string|null}
 */
export function normalizeImageContext(value) {
  const text = typeof value === 'string' ? value.trim() : '';
  if (text === '' || validateImageContext(text) !== null) return null;
  return text;
}

/**
 * 一个槽位（= 一个 LUN）的界面状态。
 *
 * 「槽位」是本轮的模型：一个 LUN 是一个槽位，弹出了就是**空闲**（参数保留），
 * 空闲槽位可以改参数选镜像再挂载，或者删除。因此界面上有三种形态：
 *
 * - `mounted`：已挂载（`attached`）；
 * - `idle`：存在但空闲（LUN 目录还在、`file` 为空）；
 * - `new`：尚未创建（用户在界面上新加的一行，或意图里但内核还没有的）。
 *
 * @typedef {{ kind: 'mounted'|'idle'|'new', deletable: boolean, label: string }} SlotState
 */

/**
 * 由一条 `LunInfo`（或本地新行）推导槽位状态。
 *
 * 纯函数：视图与测试共用，避免「已挂载/空闲」的判定散落在 DOM 代码里。
 *
 * @param {{attached?: boolean, deletable?: boolean}} [device]
 * @returns {SlotState}
 */
export function describeSlot(device) {
  const attached = device && device.attached === true;
  const deletable = device && device.deletable === true;

  if (attached) {
    return { kind: 'mounted', deletable, label: '已挂载' };
  }
  // 有 `deletable` 字段说明这条来自后端（是真实存在的槽位，只是空闲）；
  // 本地新加的行没有该字段，因此是「尚未创建」。
  const known = device && typeof device.deletable === 'boolean';
  if (known) {
    return { kind: 'idle', deletable, label: '空闲' };
  }
  return { kind: 'new', deletable: false, label: '未创建' };
}

/**
 * 构造镜像下拉的选项列表。
 *
 * **只允许选择 `images/` 目录里的镜像**——这是用户的要求，也让「镜像文件不在
 * 我们目录里」这一警告在 WebUI 路径上不可能发生（CLI 路径仍可能，故仍保留警告）。
 *
 * 当前值若不在列表里（文件已被删、或来自 `intent` 的旧记录），会**额外插入**一个
 * 标注「文件不存在」的选项并选中它，让用户看见问题而不是被静默清空——静默清空
 * 会让用户以为「刚才明明挂着的东西怎么没了」。
 *
 * @param {Array<{path: string, size_bytes?: number}>} images
 * @param {string} [current]
 * @returns {Array<{value: string, label: string, missing?: boolean}>}
 */
export function buildImageOptions(images, current) {
  const options = [{ value: '', label: '（未选择）' }];
  const seen = new Set();

  for (const image of Array.isArray(images) ? images : []) {
    const path = stringField(image && image.path);
    if (!path || seen.has(path)) continue;
    seen.add(path);
    const name = baseName(path);
    const size = Number.isFinite(image && image.size_bytes) ? image.size_bytes : null;
    options.push({
      value: path,
      label: size === null ? name : `${name}（${formatBytes(size)}）`,
    });
  }

  const wanted = stringField(current);
  if (wanted && !seen.has(wanted)) {
    options.push({
      value: wanted,
      label: `${baseName(wanted)}（文件不存在）`,
      missing: true,
    });
  }
  return options;
}

/**
 * 把后端返回的槽位（`status.devices`）与本地新加的行合并成界面行。
 *
 * 后端槽位按序号升序在前，本地新行在后。已挂载与空闲槽位都来自后端，因此它们
 * 的 `deletable` 由内核事实（`lun.0` 不可删）决定，界面不自己判断。
 *
 * @param {Array<object>} devices
 * @param {Array<object>} localRows
 * @returns {Array<object>}
 */
export function mergeSlotRows(devices, localRows) {
  const rows = [];
  const sorted = (Array.isArray(devices) ? devices : [])
    .slice()
    .sort((a, b) => (a.index ?? 0) - (b.index ?? 0));
  for (const device of sorted) {
    rows.push({ ...device, local: false });
  }
  for (const row of Array.isArray(localRows) ? localRows : []) {
    rows.push({ ...row, local: true });
  }
  return rows;
}
