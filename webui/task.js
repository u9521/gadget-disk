// task.js —— 任务反馈：忙碌态、进度状态行、只读刷新包装与标签切换。
//
// 从 app.js 的「任务反馈」一节原样拆出。操作本身是毫秒级的，缺的是**反馈**：
// await 期间界面毫无变化，用户会以为点击没生效。因此变更类操作统一走 `runTask`
// （同步进入忙碌态 + 每 200ms 刷新已用时间），只读刷新走 `runRefresh`（不禁用
// 按钮——刷新按钮在离线时是探测手段，禁用等于自断恢复路径）。
//
// `selectTab` / `refreshForTab` 也留在这里：主题同为「界面反馈」，且这样可以让
// 视图模块只依赖 dom/backend/task，不出现「视图 A 依赖视图 B 的切换函数」。


import { $, describeThrown, showError } from './dom.js';
import { SLOW_TASK_THRESHOLD_MS, taskProgressLabel } from './pure/task.js';
import { renderBackendState } from './backend.js';
import { refreshStatus } from './view-mount.js';
import { refreshAvailableSpace } from './view-create.js';
import { refreshImages } from './view-images.js';
import { refreshLoop, refreshLoopImages } from './view-edit.js';
import { loadIdentityConfig, loadImageContextConfig, refreshCapabilities } from './view-settings.js';

// ---------------------------------------------------------------- 任务反馈
//
// 操作本身是毫秒级的（实测 mount≈17ms、attach-loop≈115ms、create 64MiB≈158ms），
// 缺的是**反馈**：await 期间界面毫无变化，用户会以为点击没生效。因此每次调用都
// 同步进入忙碌态（禁用按钮 + aria-busy + 状态行），并按 200ms 刷新已用时间。

/** 状态行的刷新间隔：够快让人看到时间在走，又不至于每帧都改 DOM。 */
export const TASK_TICK_MS = 200;

/**
 * 各视图的状态元素与对应的变更类按钮。
 *
 * `buttons` 只列**变更类**按钮：刷新按钮保持可用，因为离线时它们同时是探针。
 */
export const TASK_SCOPES = {
  mount: { status: 'mount-task-status', buttons: ['btn-mount', 'btn-unmount'] },
  create: { status: 'create-task-status', buttons: ['btn-create'] },
  import: { status: 'import-task-status', buttons: ['btn-import'] },
  images: { status: 'images-task-status', buttons: [] },
  loop: { status: 'loop-task-status', buttons: ['btn-loop-attach', 'btn-loop-detach'] },
  settings: { status: 'settings-task-status', buttons: [] },
  // 身份与镜像标签共享这一个作用域：两者同在「设置」页、都写
  // `config/gadget.json`，因此共用状态行与那三个变更按钮。分开写会让两个操作
  // 能并发提交同一个文件（读—改—写的最后一个覆盖前一个）。
  config: {
    status: 'config-task-status',
    buttons: ['btn-config-save', 'btn-image-context-save', 'btn-image-context-reset'],
  },
};

/** 由视图名反查作用域；未知名回落到空作用域（不抛异常）。 */
export function taskScope(scope) {
  if (typeof scope === 'string') return TASK_SCOPES[scope] || { status: '', buttons: [] };
  return scope || { status: '', buttons: [] };
}

/**
 * 正在进行变更类任务的状态元素 id 集合。
 *
 * 任务内部的列表刷新不能覆盖任务的进度文案（见 `runRefresh`）。
 */
export const busyStatusIds = new Set();

/**
 * 写入某个作用域的状态行。
 *
 * @param {string|{status: string, buttons: string[]}} scope
 * @param {string} text
 * @param {{slow?: boolean}} [options]
 */
export function setTaskStatus(scope, text, options = {}) {
  const target = $(taskScope(scope).status);
  if (!target) return;
  target.textContent = text;
  target.classList.toggle('slow', options.slow === true);
}

/**
 * 同步进入忙碌态：禁用该操作的变更按钮、标记 `aria-busy`，并立刻显示「进行中」。
 *
 * **必须在 await 之前调用**——这正是「点击后立即有反馈」的关键。
 *
 * @param {string|object} scope
 * @param {string} label 动作名（如 `挂载`）
 * @returns {() => void} 清理函数（幂等）
 */
export function enterBusy(scope, label) {
  const resolved = taskScope(scope);
  // 动态生成的按钮（镜像列表里的「删除」）没有稳定 id，由调用方经
  // `options.elements` 直接传入。
  const buttons = Array.isArray(resolved.elements)
    ? resolved.elements.filter(Boolean)
    : resolved.buttons.map((id) => $(id)).filter(Boolean);

  for (const button of buttons) {
    button.disabled = true;
    button.setAttribute('aria-busy', 'true');
  }
  setTaskStatus(resolved, taskProgressLabel(label, 0));
  return () => {
    for (const button of buttons) {
      button.removeAttribute('aria-busy');
    }
    // 重新按后端状态决定可用性（离线时不得恢复成可点）。
    renderBackendState();
  };
}

/**
 * 统一的变更类任务包装：同步进入忙碌态 → 每 200ms 刷新耗时 → `finally` 清理。
 *
 * `finally` 保证无论 `fn` 是 resolve、reject 还是**同步抛错**，忙碌态与定时器
 * 都会被清掉；否则一次异常就会让按钮永久变灰、状态行永远停在「正在…」。
 *
 * @template T
 * @param {string} label 动作名（如 `挂载`）
 * @param {() => Promise<T>} fn
 * @param {{scope?: string|object}} [options]
 * @returns {Promise<T|undefined>}
 */
export async function runTask(label, fn, options = {}) {
  // 解析成对象：`options.elements` 用于动态生成的按钮（没有稳定 id），
  // 必须并进作用域，否则 doDelete 的「删除」按钮不会被禁用。
  const scope = { ...taskScope(options.scope), elements: options.elements };
  const startedAt = Date.now();
  const cleanup = enterBusy(scope, label);
  if (scope.status) busyStatusIds.add(scope.status);
  const ticker = setInterval(() => {
    const elapsed = Date.now() - startedAt;
    setTaskStatus(scope, taskProgressLabel(label, elapsed), { slow: elapsed > SLOW_TASK_THRESHOLD_MS });
  }, TASK_TICK_MS);

  try {
    return await fn();
  } catch (error) {
    // fn 抛错也必须给出可见提示，且不得把异常继续抛给调用方（否则调用方的
    // 后续刷新会被跳过，界面停在半更新状态）。
    showError({ message: `${label}失败`, detail: describeThrown(error) });
    return undefined;
  } finally {
    // finally 覆盖三条路径：resolve、reject、同步抛错。少任何一条都会留下
    // 永久变灰的按钮与永远停在「正在…」的状态行。
    clearInterval(ticker);
    if (scope.status) busyStatusIds.delete(scope.status);
    cleanup();
    setTaskStatus(scope, '');
  }
}

/**
 * 只读刷新的反馈包装：显示「正在同步状态…」，但**不禁用按钮**。
 *
 * 刷新按钮在离线时是探测手段，禁用它等于自断恢复路径。
 *
 * @template T
 * @param {string|object} scope
 * @param {() => Promise<T>} fn
 * @param {string} [label]
 * @returns {Promise<T|undefined>}
 */
export async function runRefresh(scope, fn, label = '正在同步状态…') {
  const resolved = taskScope(scope);
  // 有变更类任务正在进行时**不覆盖**它的进度文案：任务内部常常要刷新列表，
  // 若让刷新把「正在挂载…（已用 1.2s）」刷成「正在同步状态…」，用户就看不到
  // 真正的耗时了（那正是本任务要解决的问题）。
  const shouldShow = resolved.status !== '' && !busyStatusIds.has(resolved.status);
  if (shouldShow) setTaskStatus(resolved, label);
  try {
    return await fn();
  } finally {
    if (shouldShow) setTaskStatus(resolved, '');
  }
}

/**
 * 切换视图。
 *
 * @param {string} name
 */
export function selectTab(name) {
  for (const button of document.querySelectorAll('nav.tabs button')) {
    const selected = button.id === `tab-${name}`;
    button.setAttribute('aria-selected', String(selected));
  }
  for (const panel of document.querySelectorAll('section[role="tabpanel"]')) {
    panel.classList.toggle('active', panel.id === `panel-${name}`);
  }
  // 切到某视图时按其性质刷新。
  refreshForTab(name);
}

/** @param {string} name */
export function refreshForTab(name) {
  if (name === 'mount') refreshStatus();
  else if (name === 'images') refreshImages();
  else if (name === 'edit') {
    refreshLoop();
    // 镜像下拉框也要刷新：镜像可能刚在别的视图里创建/删除/上传。
    refreshLoopImages();
    refreshCapabilities();
  }
  // 视图 3（上传/导入）无需刷新：文件选择器没有服务端状态要同步。
  else if (name === 'settings') {
    refreshCapabilities();
    loadIdentityConfig();
    // 镜像 SE 标签可能在别处被改（CLI 或另一次会话），进设置页就重取一次：
    // 表单里的值必须是后端当前值，而不是上次打开时的缓存。
    loadImageContextConfig();
  }
  else if (name === 'create') refreshAvailableSpace();
}