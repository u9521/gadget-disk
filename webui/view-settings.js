// view-settings.js —— 视图 6：设置与诊断（USB 身份 + 默认模式 + 能力探测）。
//
// 从 app.js 的「视图 6：设置与诊断」（原文有两个同名小节）与「偏好」两节拆出。
// 偏好只是界面默认值，**不是**权威状态，故存 localStorage 且读写失败静默忽略。
//
// `refreshCapabilities` 的**唯一归属**在本模块：它同时被视图 5（本地编辑）与
// 视图 6 的切换分支调用，而原文只有一份定义（落在视图 6 区域），保持单一实现。

import { validateIdentityField, validateImageContext } from './pure/task.js';
import { toast } from './ksu.js';
import { $, failed, showError } from './dom.js';
import { callCli } from './backend.js';
import { refreshStatus } from './view-mount.js';
import { runRefresh, runTask } from './task.js';

// ---------------------------------------------------------------- 偏好

/** 状态归属：用户偏好可存 localStorage，权威状态一律来自后端。 */
const PREFS_KEY = 'gadgetdisk.prefs';

/** 读取偏好（仅界面默认值，不含权威状态）。 */
export function loadPrefs() {
  try {
    return JSON.parse(localStorage.getItem(PREFS_KEY) || '{}');
  } catch (error) {
    return {};
  }
}

/** @param {object} prefs */
export function savePrefs(prefs) {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch (error) {
    // localStorage 不可用时忽略：偏好只是便利，不是权威状态。
  }
}

// ---------------------------------------------------------------- 视图 6：设置与诊断

/** 刷新能力探测结果。 */
export async function refreshCapabilities() {
  return runRefresh('settings', async () => {
    const result = await callCli({ op: 'capabilities' });
    if (failed(result)) return;

    const caps = result.data || {};
    const list = $('caps-list');
    list.innerHTML = '';

    const rows = [
      ['loop 控制设备', caps.loop_control ? '可用' : '不可用'],
      ['loop max_part', String(caps.max_part ?? 0)],
      ['内核文件系统', (caps.filesystems || []).join('、') || '未检测到 vfat/exfat'],
      ['USB 大容量存储 (mass_storage)', caps.mass_storage_supported ? '支持' : '不支持'],
      ['SELinux', caps.selinux_enforcing ? 'Enforcing' : 'Permissive 或未启用'],
    ];

    for (const [key, value] of rows) {
      const dt = document.createElement('dt');
      dt.textContent = key;
      const dd = document.createElement('dd');
      dd.textContent = value;
      list.appendChild(dt);
      list.appendChild(dd);
    }

    // loop 不可用时明确引导 USB 编辑路径，而不是笼统报错。
    //
    // 不再有「partscan 不可用 → 降级」的分支：该挂载路径已移除，
    // 分区偏移（lo_offset）是**唯一**路径，不存在降级一说。
    const capability = $('loop-capability');
    if (!caps.loop_control) {
      capability.textContent =
        '当前内核未提供 /dev/loop-control 设备，不支持本地挂载；请将镜像导出为 USB 设备后在电脑端连接编辑。';
    } else {
      capability.textContent = '';
    }
  });
}

// ---------------------------------------------------------------- 视图 6：设置与诊断

/** 把十六进制字符串解析为 0..65535 的整数；非法返回 null。 */
export function parseHex16(value) {
  const text = String(value || '').trim();
  if (text === '') return null;
  const parsed = /^0x/i.test(text)
    ? Number.parseInt(text.slice(2), 16)
    : Number.parseInt(text, 16);
  if (!Number.isInteger(parsed) || parsed < 0 || parsed > 0xffff) return null;
  return parsed;
}

/** 把整数格式化为不带前缀的十六进制（表单里显示为 `18d1` 而非 `0x18d1`）。 */
export function formatHex16(value) {
  return Number.isInteger(value) ? value.toString(16) : '';
}

/** 读取 USB 设备身份表单。 */
export function readIdentityForm() {
  const raw = {
    id_vendor: $('cfg-vid').value.trim(),
    id_product: $('cfg-pid').value.trim(),
    manufacturer: $('cfg-manufacturer').value.trim(),
    product: $('cfg-product').value.trim(),
    serial: $('cfg-serial').value.trim(),
  };

  const identity = {};
  for (const [key, field] of [
    ['id_vendor', 'VID'],
    ['id_product', 'PID'],
  ]) {
    const text = raw[key];
    if (text === '') continue;
    const parsed = parseHex16(text);
    if (parsed === null) {
      return { error: `${field} 必须是 0x0000..0xffff 的十六进制数（如 18d1）` };
    }
    identity[key] = parsed;
  }

  for (const [key, label] of [
    ['manufacturer', '制造商'],
    ['product', '产品名'],
    ['serial', '序列号'],
  ]) {
    const text = raw[key];
    if (text === '') continue;
    // 校验逻辑与 REST/CLI 两条通道**共用** pure/task.js 的纯函数，避免三处漂移
    // （制造商/产品名可含中文，序列号只能 ASCII）。
    const problem = validateIdentityField(key, text);
    if (problem) {
      return { error: `${label}${problem}` };
    }
    identity[key] = text;
  }

  if (Object.keys(identity).length === 0) {
    return { error: '请至少填写一项，未填写的字段将保持现有配置不变。' };
  }
  return { identity };
}

/** 把身份填回表单。 */
export function fillIdentityForm(identity) {
  if (!identity) return;
  $('cfg-vid').value = formatHex16(identity.id_vendor);
  $('cfg-pid').value = formatHex16(identity.id_product);
  $('cfg-manufacturer').value = identity.manufacturer || '';
  $('cfg-product').value = identity.product || '';
  $('cfg-serial').value = identity.serial || '';
}

/** 读取身份配置（回显保存值 + 内核当前生效值）。 */
export async function loadIdentityConfig() {
  return runRefresh('settings', async () => {
    const result = await callCli({ op: 'config' });
    if (failed(result)) return;
    const data = result.data || {};
    // 优先显示保存值；没有保存值时用内核当前值预填，让用户看到「现在是什么」，
    // 而不是一个空表单。
    //
    // **不要再写独立的说明元素**：`#cfg-note` 已随文案精简从 index.html 移除，
    // 在这里写它只会得到 `TypeError: Cannot set properties of null`，让整个设置页
    // 的刷新失败（而其余视图正常，症状很容易被误读成"后端有问题"）。
    // 生效时机与字段限制已由卡片顶部的静态文案说明。
    fillIdentityForm(data.config && Object.keys(data.config).length ? data.config : data.effective);
  });
}

/** 保存身份配置。 */
export async function saveIdentityConfig() {
  const parsed = readIdentityForm();
  if (parsed.error) {
    showError({ message: parsed.error });
    return;
  }
  await runTask(
    '保存身份',
    async () => {
      const result = await callCli({ op: 'config-set', identity: parsed.identity });
      if (failed(result)) return;
      // 不要说「已应用」：身份只在主机**重新枚举**时生效，而本操作刻意不重绑
      // USB（否则只是改个产品名也会让设备消失再出现）。
      toast('身份已保存，下次连接 USB 后生效');
      await loadIdentityConfig();
      await refreshStatus();
    },
    // 用 `config` 而不是 `settings`：反馈要落在「USB 设备身份」这张卡片里，
    // 那才是用户点击的地方（`settings` 的状态行在下面的能力探测卡片）。
    { scope: 'config' },
  );
}

// ---------------------------------------------------------------- 镜像 SE 标签

/**
 * 读取镜像 SELinux 目标上下文。
 *
 * 走独立的 `config-security`（`GET /api/v1/config/security`，CLI 回退为
 * `config security get`）而不是复用身份那条 `config`：两者是**独立的两件事**，
 * 共用一条调用会让「保存其中一个」有覆盖另一个的风险。
 *
 * 显示的是**生效值**（后端已解析缺省），并在未显式配置时点明这是内置默认——
 * 否则用户会以为「输入框里有值 = 我设过」。
 */
export async function loadImageContextConfig() {
  return runRefresh('settings', async () => {
    const result = await callCli({ op: 'config-security' });
    if (failed(result)) return;
    const data = result.data || {};

    const input = $('cfg-image-context');
    input.value = data.image_context || '';
    // 已显式配置的值进 `configured`；默认值不进（用户没设过就不该显示成「他设的」）。
    input.dataset.configured = data.configured ? 'true' : 'false';

    $('cfg-image-context-note').textContent = data.configured
      ? `当前为自定义配置（形如 u:object_r:<类型>:s0），适用于 images/ 目录下的镜像；修改将在下一次挂载时生效。`
      : `当前使用系统内置默认值 ${data.image_context || ''}（实测同时允许内核读写）。` +
        '仅当该默认标签被定制 ROM 策略拦截时才建议修改。';
  });
}

/**
 * 保存（或重置）镜像 SELinux 目标上下文。
 *
 * @param {boolean} [reset] `true` = 恢复内置默认
 */
export async function saveImageContextConfig(reset) {
  // 前端校验与后端逐条对齐（空、缺 `:`、含空白/控制字符、超长）：合法值永远
  // 不该被持久化，因为它会被之后每一次挂载重新读到并再次失败。
  const raw = reset === true ? '' : $('cfg-image-context').value;
  if (reset !== true) {
    const problem = validateImageContext(raw);
    if (problem) {
      showError({ message: '镜像 SELinux 上下文格式不合法', detail: problem });
      return;
    }
  }

  await runTask(
    reset === true ? '恢复默认镜像标签' : '保存镜像标签',
    async () => {
      const result = await callCli(
        reset === true
          ? { op: 'config-security-set', reset: true }
          : { op: 'config-security-set', imageContext: raw.trim() },
      );
      if (failed(result)) return;
      // 不说「已应用」：内核在打开后备文件时按当时的标签固定权限，已在挂载中的
      // 镜像不会因为改标签而重新校验。
      toast(reset === true ? '已恢复默认安全标签，下次挂载时生效' : '镜像安全标签已保存，下次挂载时生效');
      await loadImageContextConfig();
    },
    // 与身份共享 `config` 作用域：同一张状态行、同一块「设置」区域。
    { scope: 'config' },
  );
}
