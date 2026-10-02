// view-mount.js —— 视图 1：挂载状态（多槽位编辑器 + 导出意图）。
//
// 从 app.js 的「视图 1：挂载状态」一节原样拆出。槽位的真相一律来自内核
// （后端 status），因此每次刷新都**整块重建**列表，而不是增量更新。

import { sizeNote, formatBytes, parsePartitionSize, parseSizeInput, validateSize } from './pure/bytes.js';
import { buildCliArgs, buildRestCall, classifyBackendFailure, execProbeSucceeded, nextReconnectDelay, parseApiInfo, parseExecResult, restResultToExecResult, restUrl, shouldBlockActions } from './pure/channel.js';
import { describeFilesystem, describeGptPartitionType, describeInUse, describeLayout, describeMbrPartitionType, describeMode, messageForCode, modeWarning } from './pure/describe.js';
import { CUSTOM_TYPE_VALUE, FILESYSTEMS, MAX_PARTITIONS, MBR_MAX_EXTENDED, MBR_MAX_LOGICAL, MBR_MAX_PRIMARY, buildPartitionOptions, customTypeWire, defaultPartitionType, formatPartitionScan, gptTypeWire, layoutSupportsPartitionNames, layoutSupportsPartitions, mbrSlotUsage, mbrTypeWire, partitionKernelIndex, partitionTypePresets, validatePartitions } from './pure/partitions.js';
import { baseName, joinPath, parentPath, safeImageName, shellQuote } from './pure/paths.js';
import { INQUIRY_STRING_MAX, MAX_LUNS, SLOW_TASK_THRESHOLD_MS, buildImageOptions, describeFormattingSource, describeSlot, imageNameExists, mergeSlotRows, taskProgressLabel, validateIdentityField } from './pure/task.js';
import { toast } from './ksu.js';
import { $, failed, showError } from './dom.js';
import { callCli } from './backend.js';
import { runRefresh, runTask } from './task.js';

// ---------------------------------------------------------------- 视图 1：挂载状态

/** 刷新挂载状态。 */
export async function refreshStatus() {
  return runRefresh('mount', async () => {
    const result = await callCli({ op: 'status' });
    if (failed(result)) {
      $('status-line').textContent = '后端服务不可用';
      return;
    }

    const { udc, devices, pending_intent: pending } = result.data || {};
    $('mount-udc').textContent = udc || '（无可用控制器）';
    renderPendingIntent(pending, result.data || {});

    // 槽位编辑器需要镜像列表（下拉框的选项）。失败不阻断状态显示——用户至少
    // 还能看到当前挂载情况。
    const images = await callCli({ op: 'list' });
    if (!failed(images)) {
      availableImages = (images.data && images.data.images) || [];
    }
    renderSlots(devices || []);

    const list = $('mount-devices');
    if (!devices || devices.length === 0) {
      list.className = 'empty';
      list.textContent = '尚未挂载任何设备。';
      $('mount-effective').textContent = '未挂载';
      $('status-line').textContent = udc ? `USB 控制器：${udc}` : '无可用 USB 控制器';
      return;
    }

    list.className = '';
    list.innerHTML = '';
    for (const device of devices) {
      const item = document.createElement('div');
      item.style.marginBottom = '10px';

      const title = document.createElement('div');
      title.className = 'title';
      title.textContent = device.image_path || '（未绑定）';
      item.appendChild(title);

      const meta = document.createElement('div');
      meta.className = 'meta';
      meta.textContent = `容量 ${formatBytes(device.size_bytes)} · ${describeMode(device.mode)}`;
      item.appendChild(meta);

      const tag = document.createElement('span');
      // effective=false 必须显式标注「已配置但未生效」，不得误报成功。
      if (device.attached && device.effective) {
        tag.className = 'tag ok';
        tag.textContent = '已生效';
      } else if (device.attached) {
        tag.className = 'tag warn';
        tag.textContent = '已配置但未生效（USB 可能未连接）';
      } else {
        tag.className = 'tag';
        tag.textContent = '未绑定';
      }
      item.appendChild(tag);

      // 每个 LUN 一个独立的「弹出」按钮：只弹这一块介质，LUN 与配置都保留，
      // 因此随时可以再挂上，不需要重配 USB。
      if (device.attached) {
        const actions = document.createElement('div');
        actions.className = 'row tight';
        actions.style.marginTop = '6px';
        const eject = document.createElement('button');
        eject.className = 'action';
        eject.dataset.role = 'mutation';
        eject.textContent = `弹出 LUN ${device.index}`;
        eject.addEventListener('click', () => doUnmountLun(device.index));
        actions.appendChild(eject);
        item.appendChild(actions);
      }

      list.appendChild(item);
    }

    const allEffective = devices.every((d) => d.effective);
    $('mount-effective').textContent = allEffective ? '已生效' : '部分未生效';
    $('status-line').textContent = allEffective
      ? `已挂载 ${devices.length} 个设备`
      : '已配置但未生效（USB 未连接或无 UDC）';
  });
}

/** 当前可选的镜像列表（由 refreshStatus 填充）。 */
export let availableImages = [];

/** 界面上的槽位行（含后端槽位与本地新加的行）。 */
export let slotRows = [];

/**
 * 清空槽位行（供入口初始化调用）。
 *
 * 用函数而不是让调用方直接赋值：`slotRows` 是**跨模块的活绑定**，
 * 别的模块 `import` 到的是只读视图，赋值会抛
 * `TypeError: Assignment to constant variable`。改动静止状态必须回到
 * 声明它的模块里做。
 */
export function resetSlotRows() {
  slotRows = [];
}

/**
 * 读取一个槽位行的当前输入。
 *
 * @param {HTMLElement} row
 * @returns {{device?: object, invalid?: string}}
 */
export function readSlotRow(row) {
  const image = row.querySelector('[data-field="image"]').value.trim();
  const device = {
    image_path: image,
    mode: row.querySelector('[data-field="mode"]').value,
  };
  const inquiry = row.querySelector('[data-field="inquiry"]').value.trim();
  if (inquiry) device.inquiry_string = inquiry;
  // 槽位序号：已有槽位固定不可改（改号等于换一个槽位，用删除+新增表达更清楚）；
  // 本地新行可指定，留空由后端分配。
  const lun = row.querySelector('[data-field="lun"]').value.trim();
  if (lun !== '') {
    const parsed = Number(lun);
    if (!Number.isInteger(parsed)) return { invalid: 'LUN 必须是整数' };
    device.lun = parsed;
  }
  return { device };
}

/**
 * 收集全部槽位行。
 *
 * 空行（没选镜像）**直接跳过**而不是报错：空闲槽位与「刚点了添加但还没填」的行
 * 都长这样，报错会让用户没法只挂其中几个。
 */
export function collectDevices() {
  const rows = [...$('mount-device-rows').querySelectorAll('.slot-row')];
  const devices = [];
  for (const row of rows) {
    const { device, invalid } = readSlotRow(row);
    if (invalid) return { error: invalid };
    if (!device.image_path) continue;
    devices.push(device);
  }
  return { devices };
}

/**
 * 渲染一个槽位行。
 *
 * 三种形态（见 `describeSlot`）：已挂载 / 空闲 / 未创建。区别体现在
 * 序号输入框是否可编辑、以及有哪些按钮。
 *
 * @param {object} [slot] 槽位数据（后端 `LunInfo` 或本地新行）
 */
export function addDeviceRow(slot = {}) {
  const container = $('mount-device-rows');
  const state = describeSlot(slot);

  const row = document.createElement('div');
  row.className = 'slot-row card';
  row.style.margin = '0 0 8px';
  if (typeof slot.index === 'number') row.dataset.index = String(slot.index);

  // 标题行：序号 + 状态标签。
  const header = document.createElement('div');
  header.className = 'row tight';
  header.style.marginBottom = '6px';
  const title = document.createElement('strong');
  title.textContent =
    typeof slot.index === 'number' ? `槽位 lun.${slot.index}` : '新槽位';
  header.appendChild(title);
  const tag = document.createElement('span');
  tag.className = state.kind === 'mounted' ? 'tag ok' : 'tag';
  tag.textContent = state.label;
  header.appendChild(tag);
  row.appendChild(header);

  // 镜像：**下拉框**，只能选镜像目录里的文件。
  const image = document.createElement('select');
  image.dataset.field = 'image';
  image.setAttribute('aria-label', '镜像');
  for (const option of buildImageOptions(availableImages, slot.image_path)) {
    const node = document.createElement('option');
    node.value = option.value;
    node.textContent = option.label;
    if (option.missing) node.dataset.missing = 'true';
    image.appendChild(node);
  }
  image.value = slot.image_path || '';
  row.appendChild(image);

  const controls = document.createElement('div');
  controls.className = 'row tight';
  controls.style.marginTop = '6px';

  const mode = document.createElement('select');
  mode.dataset.field = 'mode';
  mode.setAttribute('aria-label', '设备模式');
  for (const [value, label] of [
    ['rw', '可读写（U 盘）'],
    ['ro', '只读（写保护）'],
    ['cdrom', '光驱（CD-ROM）'],
  ]) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = label;
    mode.appendChild(option);
  }
  mode.value = slot.mode || 'rw';

  const lun = document.createElement('input');
  lun.type = 'number';
  lun.dataset.field = 'lun';
  lun.placeholder = 'LUN';
  lun.min = '0';
  lun.max = String(MAX_LUNS - 1);
  lun.style.flex = '0 0 90px';
  lun.value = typeof slot.index === 'number' ? String(slot.index) : '';
  lun.setAttribute('aria-label', 'LUN 序号（留空自动分配）');
  // 已有槽位的序号是它的身份，不可改（要换号请删除后新增）。
  lun.disabled = typeof slot.index === 'number';

  const inquiry = document.createElement('input');
  inquiry.type = 'text';
  inquiry.dataset.field = 'inquiry';
  inquiry.placeholder = 'INQUIRY（可选）';
  inquiry.maxLength = INQUIRY_STRING_MAX;
  inquiry.value = slot.inquiry_string || '';
  inquiry.setAttribute('aria-label', 'SCSI INQUIRY 字符串');

  controls.appendChild(mode);
  controls.appendChild(lun);
  controls.appendChild(inquiry);
  row.appendChild(controls);

  // 按钮区：已挂载 → 弹出；空闲且可删 → 删除槽位；本地新行 → 移除这一行。
  const actions = document.createElement('div');
  actions.className = 'row tight';
  actions.style.marginTop = '6px';

  if (state.kind === 'mounted') {
    const eject = document.createElement('button');
    eject.className = 'action';
    eject.dataset.role = 'mutation';
    eject.textContent = '弹出';
    eject.addEventListener('click', () => doUnmountLun(slot.index));
    actions.appendChild(eject);
  }

  if (state.kind === 'idle' && state.deletable) {
    const remove = document.createElement('button');
    remove.className = 'action danger';
    remove.dataset.role = 'mutation';
    remove.textContent = '删除槽位';
    remove.addEventListener('click', () => doDeleteSlot(slot.index));
    actions.appendChild(remove);
  } else if (state.kind === 'idle' && !state.deletable) {
    // lun.0 不能删——把原因写在界面上，而不是让用户找不到按钮。
    const note = document.createElement('span');
    note.className = 'muted';
    note.textContent = 'lun.0 为系统底层默认槽位，只能弹出不能删除。';
    actions.appendChild(note);
  }

  if (state.kind === 'new') {
    const drop = document.createElement('button');
    drop.className = 'action';
    drop.textContent = '移除此行';
    drop.addEventListener('click', () => {
      row.remove();
      updateDeviceCount();
    });
    actions.appendChild(drop);
  }

  if (actions.childNodes.length > 0) row.appendChild(actions);
  row.addEventListener('input', updateDeviceCount);

  container.appendChild(row);
  updateDeviceCount();
}

/** 更新槽位计数提示。 */
export function updateDeviceCount() {
  const rows = [...$('mount-device-rows').querySelectorAll('.slot-row')];
  $('mount-device-count').textContent = `${rows.length} / ${MAX_LUNS} 个槽位`;
  // 到上限后禁用「添加」，避免用户白填。
  $('btn-add-device').disabled = rows.length >= MAX_LUNS;
}

/**
 * 按后端状态重建整个槽位列表。
 *
 * **每次刷新都重建**（而不是增量更新）：槽位状态由内核真值决定（谁挂载、谁空闲、
 * 谁能删），增量更新要自己维护「哪个 DOM 对应哪个槽位」，一旦不同步就会显示
 * 过期的可删性。重建的代价是几十个节点，可忽略。
 *
 * 用户正在编辑的本地新行会被保留（它们不在后端状态里）。
 */
export function renderSlots(devices) {
  const localRows = slotRows.filter((row) => row.local);
  slotRows = mergeSlotRows(devices, localRows);

  $('mount-device-rows').innerHTML = '';
  if (slotRows.length === 0) {
    addDeviceRow();
    return;
  }
  for (const slot of slotRows) addDeviceRow(slot);
}

/**
 * 渲染「上次导出未兑现」提示。
 *
 * 显示条件由后端给出（`pending_intent`）：它表示 `run/state.json` 里有导出意图，
 * 但内核里没有任何我们的 LUN 被绑定。典型场景是设备被拔出后 `gdd` 已清理，
 * 而意图还留着。**如实报告而不是静默改写**，让用户自己决定。
 */
export function renderPendingIntent(pending, data) {
  const box = $('pending-intent');
  if (!pending) {
    box.hidden = true;
    return;
  }
  const luns = data.intent || [];
  const names = luns.map((lun) => lun.image_path).join('、');
  $('pending-intent-detail').textContent =
    names === ''
      ? '存在待恢复的导出记录，但当前未挂载任何设备。'
      : `待恢复的镜像记录：${names}`;
  box.hidden = false;
}

/** 按上次的导出意图重新挂载。 */
export async function doResumeIntent() {
  await runTask(
    '恢复导出',
    async () => {
      const status = await callCli({ op: 'status' });
      if (failed(status)) return;
      const luns = (status.data && status.data.intent) || [];
      if (luns.length === 0) {
        showError({ message: '没有可恢复的记录' });
        return;
      }
      // 用意图里的每一项填好编辑器，让用户能先看清再点「挂载」——
      // 直接静默重挂会让「怎么突然多了一块盘」成为困惑。
      $('mount-device-rows').innerHTML = '';
      slotRows = [];
      for (const lun of luns) {
        // 意图里的槽位在内核里可能还没有（设备被拔出后 gdd 已清理），因此按
        // 「本地新行」处理并带上序号——用户点「挂载 / 应用」时会重新创建。
        addDeviceRow({
          image_path: lun.image_path,
          mode: lun.mode,
          index: lun.index,
          inquiry_string: lun.inquiry_string,
          deletable: lun.index !== 0,
        });
      }
      toast('已填入上次导出配置，请点击「挂载 / 应用」执行');
    },
    { scope: 'mount' },
  );
}

/** 清除导出意图。 */
export async function doClearIntent() {
  await runTask(
    '清除记录',
    async () => {
      // 清除意图 = 全量卸载（后端会在全部弹出后删掉 state.json）。
      const result = await callCli({ op: 'unmount' });
      if (failed(result)) return;
      toast('已清除导出记录');
      await refreshStatus();
    },
    { scope: 'mount' },
  );
}

/** 挂载 / 应用。 */
export async function doMount() {
  const collected = collectDevices();
  if (collected.error) {
    showError({ message: collected.error });
    return;
  }
  const devices = collected.devices;
  if (!devices || devices.length === 0) {
    showError({
      message: '请至少选择一个镜像',
      detail: '可先在「创建镜像」或「上传/导入」中准备镜像。',
    });
    return;
  }

  // 不乐观更新挂载状态：结果一律以随后刷新的后端数据为准。
  await runTask(
    '挂载',
    async () => {
      const result = await callCli({ op: 'mount', devices });
      if (failed(result)) return;
      toast(`已应用 ${devices.length} 个槽位`);
      await refreshStatus();
    },
    { scope: 'mount' },
  );
}

/** 弹出单个 LUN 的介质（保留 LUN 与配置）。 */
export async function doUnmountLun(lun) {
  await runTask(
    `弹出 LUN ${lun}`,
    async () => {
      const result = await callCli({ op: 'unmount', lun });
      if (failed(result)) return;
      toast(`槽位 lun.${lun} 已弹出（空闲）`);
      await refreshStatus();
    },
    { scope: 'mount' },
  );
}

/** 删除一个空闲槽位。 */
export async function doDeleteSlot(lun) {
  await runTask(
    `删除槽位 lun.${lun}`,
    async () => {
      const result = await callCli({ op: 'delete-slot', lun });
      if (failed(result)) return;
      toast(`槽位 lun.${lun} 已删除`);
      await refreshStatus();
    },
    { scope: 'mount' },
  );
}

/** 全部卸载（拆除 function 与配置链接，并把设备身份还给系统）。 */
export async function doUnmount() {
  if (typeof window !== 'undefined' && window.confirm && !window.confirm('确定要全部卸载所有 USB 设备吗？若电脑端正在读写，可能会导致数据丢失。')) {
    return;
  }
  await runTask(
    '卸载',
    async () => {
      const result = await callCli({ op: 'unmount' });
      if (failed(result)) return;
      toast('已全部卸载');
      await refreshStatus();
    },
    { scope: 'mount' },
  );
}