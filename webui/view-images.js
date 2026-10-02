// view-images.js —— 视图 4：镜像管理。
//
// 从 app.js 的「视图 4：镜像管理」一节原样拆出。删除按钮是**动态生成**的，
// 没有稳定 id，因此不能在 TASK_SCOPES 里写死，而要在 runTask 的 `elements`
// 里传入（见 task.js 的 `enterBusy`）。

import { sizeNote, formatBytes, parsePartitionSize, parseSizeInput, validateSize } from './pure/bytes.js';
import { buildCliArgs, buildRestCall, classifyBackendFailure, execProbeSucceeded, nextReconnectDelay, parseApiInfo, parseExecResult, restResultToExecResult, restUrl, shouldBlockActions } from './pure/channel.js';
import { describeFilesystem, describeGptPartitionType, describeInUse, describeLayout, describeMbrPartitionType, describeMode, messageForCode, modeWarning } from './pure/describe.js';
import { CUSTOM_TYPE_VALUE, FILESYSTEMS, MAX_PARTITIONS, MBR_MAX_EXTENDED, MBR_MAX_LOGICAL, MBR_MAX_PRIMARY, buildPartitionOptions, customTypeWire, defaultPartitionType, formatPartitionScan, gptTypeWire, layoutSupportsPartitionNames, layoutSupportsPartitions, mbrSlotUsage, mbrTypeWire, partitionKernelIndex, partitionTypePresets, validatePartitions } from './pure/partitions.js';
import { baseName, joinPath, parentPath, safeImageName, shellQuote } from './pure/paths.js';
import { INQUIRY_STRING_MAX, MAX_LUNS, SLOW_TASK_THRESHOLD_MS, buildImageOptions, describeFormattingSource, describeSlot, imageNameExists, mergeSlotRows, taskProgressLabel, validateIdentityField } from './pure/task.js';
import { toast } from './ksu.js';
import { $, failed } from './dom.js';
import { callCli } from './backend.js';
import { runRefresh, runTask } from './task.js';
import { checkNameConflict, setKnownImages } from './view-create.js';

// ---------------------------------------------------------------- 视图 4：镜像管理

/** 刷新镜像列表。 */
export async function refreshImages() {
  return runRefresh('images', async () => {
    const result = await callCli({ op: 'list' });
    if (failed(result)) return;

    const images = result.data.images || [];
    // 记下已知镜像供「创建镜像」的同名预检复用，避免为了预检多跑一次 list。
    setKnownImages(images.map((image) => ({ name: baseName(image.path) })));
    checkNameConflict();
    const list = $('images-list');
    list.innerHTML = '';
    $('images-empty').hidden = images.length > 0;

    for (const image of images) {
      const item = document.createElement('li');

      const title = document.createElement('div');
      title.className = 'title';
      title.textContent = baseName(image.path);
      item.appendChild(title);

      const meta = document.createElement('div');
      meta.className = 'meta';
      const offset =
        image.partition_offset_bytes === null || image.partition_offset_bytes === undefined
          ? ''
          : ` · 分区偏移 ${formatBytes(image.partition_offset_bytes)}`;
      meta.textContent =
        `${formatBytes(image.size_bytes)} · ${describeLayout(image.layout)} · ` +
        `状态：${describeInUse(image.in_use)}${offset}`;
      item.appendChild(meta);

      const row = document.createElement('div');
      row.className = 'row tight';

      const delButton = document.createElement('button');
      delButton.className = 'action danger';
      delButton.textContent = '删除';
      // 标记为变更类：离线时由 renderBackendState 统一禁用。
      delButton.setAttribute('data-role', 'mutation');
      // 被占用时后端会拒绝；按钮保持可用以便用户看到明确原因。
      delButton.addEventListener('click', () => doDelete(image.path, delButton));
      row.appendChild(delButton);

      item.appendChild(row);
      list.appendChild(item);
    }
  });
}

/**
 * 删除镜像。
 *
 * @param {string} path
 * @param {HTMLElement} [button] 触发删除的按钮（动态生成，用于忙碌态禁用）
 */
export async function doDelete(path, button) {
  const name = baseName(path);
  if (typeof window !== 'undefined' && window.confirm && !window.confirm(`确定要彻底删除镜像「${name}」吗？此操作无法撤销。`)) {
    return;
  }
  await runTask(
    '删除',
    async () => {
      const result = await callCli({ op: 'delete', path });
      if (failed(result)) return;

      toast('已删除');
      await refreshImages();
    },
    { scope: 'images', elements: [button] },
  );
}