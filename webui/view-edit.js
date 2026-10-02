// view-edit.js —— 视图 5：本地编辑（loop 挂载）+ 分区选择。
//
// 从 app.js 的「视图 5：本地编辑」与「分区选择」两节原样拆出。后端**真的**按
// `partition_index` 挂载（此前忽略该字段），因此 UI 必须让用户从真实分区表里选，
// 而不是让他手打一个可能不存在的序号；扫描失败不阻塞用户，退回「整盘」并说明。

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

// ---------------------------------------------------------------- 视图 5：本地编辑

/**
 * 拉取镜像列表并刷新 `#loop-image` 下拉框。
 *
 * **自己拉取而不是复用挂载页的 `availableImages`**：用户可能直接点开本地编辑页，
 * 那时挂载页的 `refreshStatus` 还没跑过，复用会得到一个空下拉框。多一次
 * `GET /api/v1/images` 换取「任何入口进来都能用」，这个代价是值得的。
 *
 * `list` 在 REST 与 CLI 两条通道上都支持，因此离线降级时依然可用。
 */
export async function refreshLoopImages() {
  const result = await callCli({ op: 'list' });
  // 失败不阻断：下拉框保留占位项，用户操作时会得到明确的错误提示。
  if (failed(result)) return;
  renderLoopImageOptions((result.data && result.data.images) || []);
}

/** 刷新 loop 附件列表。 */
export async function refreshLoop() {
  return runRefresh('loop', async () => {
    const result = await callCli({ op: 'list-loop' });
    if (failed(result)) return;

    const attachments = result.data.attachments || [];
    const list = $('loop-list');
    list.innerHTML = '';
    $('loop-empty').hidden = attachments.length > 0;

    for (const attachment of attachments) {
      const item = document.createElement('li');

      const title = document.createElement('div');
      title.className = 'title';
      title.textContent = baseName(attachment.image);
      item.appendChild(title);

      const meta = document.createElement('div');
      meta.className = 'meta';
      meta.textContent =
        `${attachment.loop_dev}${attachment.read_only ? '（只读）' : ''}\n` +
        `挂载点：${attachment.mountpoint}`;
      meta.style.whiteSpace = 'pre-wrap';
      item.appendChild(meta);

      const tag = document.createElement('span');
      tag.className = 'tag ok';
      tag.textContent = '已挂载';
      item.appendChild(tag);

      list.appendChild(item);
    }
  });
}

// ---------------------------------------------------------------- 分区选择
//
// 后端**真的**按 `partition_index` 挂载（此前忽略该字段），因此 UI 必须让
// 用户从真实分区表里选，而不是让他手打一个可能不存在的序号；扫描失败不阻塞用户，
// 退回「整盘」并说明。
//
// 镜像本身也只能**选**，不能手填路径：与挂载页一致，选项来自镜像目录
// （`GET /api/v1/images`）。用户手打路径既能填出不存在的文件，也能指向镜像目录
// 之外的任意位置——两页行为不一致时，用户还会以为本地编辑支持更多来源。

/** 分区扫描的请求序号：慢响应回来时不得覆盖更新的结果。 */
export let partitionScanSeq = 0;

/** 最近一次分区扫描对应的镜像路径；用于丢弃过期响应。 */
export let partitionScanPath = '';

/**
 * 用当前镜像列表填充 `#loop-image` 下拉框。
 *
 * 复用挂载页的 `buildImageOptions`（含「（未选择）」占位与「（文件不存在）」
 * 标记）：两个页面的镜像选择必须用同一套规则，否则一处能选到的镜像另一处选不到。
 *
 * @param {Array<{path: string, size_bytes?: number}>} images
 */
export function renderLoopImageOptions(images) {
  const select = $('loop-image');
  const current = select.value;
  select.innerHTML = '';
  for (const option of buildImageOptions(images, current)) {
    const element = document.createElement('option');
    element.value = option.value;
    element.textContent = option.label;
    if (option.missing) element.dataset.missing = 'true';
    select.appendChild(element);
  }
  // 新镜像加进来后要保住用户已经选中的那一个；`buildImageOptions` 会把
  // 「当前值已不在列表里」的情况补成「（文件不存在）」，因此这里赋值总有效。
  select.value = current;
  // 列表变更后旧的分区表可能已不对应（镜像被删/换），退回「整盘」。
  if (select.value === '') renderPartitionOptions({ partitions: [] });
}

/**
 * 用扫描结果填充 `#loop-partition-select`。
 *
 * @param {{partitions?: Array<any>, default_index?: number|null, layout?: string}|null} scan
 */
export function renderPartitionOptions(scan) {
  const select = $('loop-partition-select');
  const options = buildPartitionOptions(scan);

  select.innerHTML = '';
  for (const option of options) {
    const element = document.createElement('option');
    element.value = option.value;
    element.textContent = option.label;
    element.selected = option.selected === true;
    select.appendChild(element);
  }
  // 浏览器对 `selected` 属性的处理在动态插入时不一致，显式设一遍更可靠。
  const chosen = options.find((option) => option.selected);
  select.value = chosen ? chosen.value : '';
  $('loop-partition-note').textContent = formatPartitionScan(scan);
}

/**
 * 读取镜像分区表并填充下拉框。
 *
 * 失败或没有分区表时**不阻塞用户**：退回「整盘」并给出说明。
 *
 * @param {string} [path] 缺省取 `#loop-image` 的当前值
 * @returns {Promise<object|null>} 成功时返回扫描结果
 */
export async function loadPartitions(path) {
  const image = (path !== undefined ? path : $('loop-image').value).trim();
  if (!image) {
    $('loop-partition-note').textContent = '请先选择镜像。';
    return null;
  }

  const seq = ++partitionScanSeq;
  partitionScanPath = image;
  const select = $('loop-partition-select');
  select.disabled = true;
  $('loop-partition-note').textContent = '正在读取分区表…';

  try {
    const result = await callCli({ op: 'image-partitions', path: image });

    // 过期响应（用户已改了路径或又点了一次）：丢弃，别覆盖新结果。
    if (seq !== partitionScanSeq) return null;

    if (!result.ok || !result.data) {
      renderPartitionOptions({ partitions: [] });
      $('loop-partition-note').textContent = '未能读取分区表，将作为整盘挂载';
      if (!result.ok) showError(result);
      return null;
    }

    renderPartitionOptions(result.data);
    return result.data;
  } finally {
    if (seq === partitionScanSeq) select.disabled = false;
  }
}

/** 读取下拉框当前选中的分区；`''`（整盘）→ `null`。 */
export function selectedPartition() {
  const value = $('loop-partition-select').value;
  if (value === '') return null;
  const index = Number(value);
  return Number.isInteger(index) ? index : null;
}

/** 挂载到本地。 */
export async function doAttachLoop() {
  const image = $('loop-image').value.trim();
  if (!image) {
    showError({ message: '请先选择镜像' });
    return;
  }

  const partition = selectedPartition();

  await runTask(
    '挂载到本地',
    async () => {
      const result = await callCli({
        op: 'attach-loop',
        image,
        mode: 'rw',
        // '' → null（整盘）；否则是真实分区序号。后端现在会按它读取分区表。
        partition,
        readOnly: $('loop-readonly').checked,
      });
      if (failed(result)) return;

      // 挂载前的 SELinux 标签修正结果：**改不动也必须让用户看到后果**。
      // 后端原文是英文（面向 CLI/API 使用者，无稳定性承诺），因此只作标题下方的
      // 排查线索，中文结论由这里给出——与错误面板 detail 的处理方式一致。
      renderLoopContextNote(result.data && result.data.warnings);

      toast('已挂载到本地');
      await refreshLoop();
    },
    { scope: 'loop' },
  );
}

/**
 * 显示（或清除）挂载前的镜像 SELinux 上下文提示。
 *
 * @param {string[]|undefined} warnings 后端返回的英文诊断文本
 */
export function renderLoopContextNote(warnings) {
  const note = $('loop-context-note');
  const list = Array.isArray(warnings) ? warnings.filter((w) => typeof w === 'string' && w) : [];
  if (list.length === 0) {
    note.textContent = '';
    note.hidden = true;
    return;
  }
  note.textContent =
    '镜像安全上下文未能自动修正，内核可能无法正常读写后备镜像（电脑端常表现为「能识别设备但读不出内容」或写入失效）。' +
    '可在「设置与诊断 → 镜像安全上下文」中调整目标标签后重新挂载。\n' +
    list.join('\n');
  note.hidden = false;
}

/** 按镜像卸载。 */
export async function doDetachLoop() {
  const image = $('loop-image').value.trim();
  if (!image) {
    showError({ message: '请先选择镜像' });
    return;
  }

  await runTask(
    '卸载',
    async () => {
      const result = await callCli({ op: 'detach-loop', image });
      if (failed(result)) return;

      toast('已卸载');
      await refreshLoop();
    },
    { scope: 'loop' },
  );
}