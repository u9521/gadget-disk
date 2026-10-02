// main.js —— GadgetDisk WebUI 入口：DOM 接线、全局兜底与首次刷新。
//
// 零构建：原生 ES 模块，无打包器。可测的纯逻辑在 `pure/` 中（由 Node 测试覆盖），
// 共享设施在 backend.js（后端通道/状态机）、dom.js（界面工具）、task.js（任务反馈
// 与标签切换），各视图在 view-*.js。本文件只负责把它们接起来。
//
// 后端通道是 **REST 优先、CLI 回退**：结构化调用（`{op, ...}`）经 pure/channel.js 映射为
// REST 请求；REST 不可用时回退到 `ksu.exec` 调用 CLI。理由见 docs/webui.md。

import { sizeNote, formatBytes, parsePartitionSize, parseSizeInput, validateSize } from './pure/bytes.js';
import { buildCliArgs, buildRestCall, classifyBackendFailure, execProbeSucceeded, nextReconnectDelay, parseApiInfo, parseExecResult, restResultToExecResult, restUrl, shouldBlockActions } from './pure/channel.js';
import { describeFilesystem, describeGptPartitionType, describeInUse, describeLayout, describeMbrPartitionType, describeMode, messageForCode, modeWarning } from './pure/describe.js';
import { CUSTOM_TYPE_VALUE, FILESYSTEMS, MAX_PARTITIONS, MBR_MAX_EXTENDED, MBR_MAX_LOGICAL, MBR_MAX_PRIMARY, buildPartitionOptions, customTypeWire, defaultPartitionType, formatPartitionScan, gptTypeWire, layoutSupportsPartitionNames, layoutSupportsPartitions, mbrSlotUsage, mbrTypeWire, partitionKernelIndex, partitionTypePresets, validatePartitions } from './pure/partitions.js';
import { baseName, joinPath, parentPath, safeImageName, shellQuote } from './pure/paths.js';
import { INQUIRY_STRING_MAX, MAX_LUNS, SLOW_TASK_THRESHOLD_MS, buildImageOptions, describeFormattingSource, describeSlot, imageNameExists, mergeSlotRows, taskProgressLabel, validateIdentityField } from './pure/task.js';
import { toast } from './ksu.js';
import { $, guard } from './dom.js';
import {
  clearReconnectTimer,
  MODDIR,
  reconnect,
  renderBackendState,
  resetReconnectFailures,
} from './backend.js';
import { selectTab, refreshForTab } from './task.js';
import {
  addDeviceRow,
  doClearIntent,
  doMount,
  doResumeIntent,
  doUnmount,
  refreshStatus,
  resetSlotRows,
} from './view-mount.js';
import {
  checkNameConflict,
  createPartitions,
  defaultPartitionRow,
  doCreate,
  refreshAvailableSpace,
  refreshCreateForm,
  refreshMkfsProbes,
  renderPartitions,
  syncPartitionRows,
} from './view-create.js';
import { doImport, handleFilePicker } from './view-import.js';
import { refreshImages } from './view-images.js';
import {
  doAttachLoop,
  doDetachLoop,
  loadPartitions,
  refreshLoop,
  refreshLoopImages,
} from './view-edit.js';
import {
  loadIdentityConfig,
  loadImageContextConfig,
  loadPrefs,
  refreshCapabilities,
  saveIdentityConfig,
  saveImageContextConfig,
  savePrefs,
} from './view-settings.js';

// ---------------------------------------------------------------- 初始化

/** 绑定全部事件并做首次刷新。 */
export function init() {
  // 标签切换。
  for (const button of document.querySelectorAll('nav.tabs button')) {
    button.addEventListener('click', () => {
      selectTab(button.id.replace('tab-', ''));
    });
  }

  const prefs = loadPrefs();
  if (prefs.defaultMode) {
    // `mount-mode` 已改为每行一个下拉（每个 LUN 可独立设模式），因此默认模式
    // 只作用于设置页的「默认设备模式」，以及新增行的初值。
    $('settings-mode').value = prefs.defaultMode;
  }
  if (prefs.defaultLayout) {
    $('create-layout').value = prefs.defaultLayout;
  }

  // 视图 1
  $('btn-mount').addEventListener('click', doMount);
  $('btn-unmount').addEventListener('click', doUnmount);
  $('btn-refresh-mount').addEventListener('click', refreshStatus);
  $('btn-add-device').addEventListener('click', () => addDeviceRow());
  $('btn-resume-intent').addEventListener('click', doResumeIntent);
  $('btn-clear-intent').addEventListener('click', doClearIntent);

  // 先给一行空槽位，避免首屏面对空卡片。`refreshStatus` 会按后端状态重建整个
  // 列表（本地行会保留），因此这里不需要恢复上次的设备列表——**槽位的真相是
  // 内核状态**，用 localStorage 里的旧列表覆盖它反而会显示过期内容。
  resetSlotRows();
  addDeviceRow();

  // 视图 2
  $('btn-create').addEventListener('click', doCreate);
  $('create-size').addEventListener('input', () => {
    const bytes = parseSizeInput($('create-size').value);
    $('create-size-note').textContent =
      bytes === null
        ? '支持输入 64M、1G、4G 等容量格式。'
        : sizeNote(bytes) || `将创建 ${formatBytes(bytes)} 镜像（对齐到 1 MiB）。`;
  });
  // 文件名变化即做同名预检，让用户在点击前就知道会冲突。
  $('create-name').addEventListener('input', checkNameConflict);
  // 布局决定是否显示分区编辑器；文件系统决定默认分区类型与格式化来源说明。
  $('create-layout').addEventListener('change', refreshCreateForm);
  $('create-filesystem').addEventListener('change', refreshCreateForm);
  $('btn-add-partition').addEventListener('click', () => {
    syncPartitionRows();
    createPartitions.push(defaultPartitionRow());
    renderPartitions();
  });

  // 视图 3：唯一的入口是系统文件选择器（路径浏览器及其 ls/stat 后端已移除）。
  $('btn-import').addEventListener('click', doImport);
  $('import-file').addEventListener('change', (event) => handleFilePicker(event.target.files));

  // 视图 4
  $('btn-images-refresh').addEventListener('click', refreshImages);

  // 视图 5
  $('btn-loop-refresh').addEventListener('click', refreshLoop);
  $('btn-loop-attach').addEventListener('click', doAttachLoop);
  $('btn-loop-detach').addEventListener('click', doDetachLoop);
  // 镜像下拉框变更 → 立即读取该镜像的分区表。用 `change` 而不是 `input`：
  // 下拉框的每次变更都是一次确定的、原子的选择，不需要防抖。
  $('loop-image').addEventListener('change', () => loadPartitions());
  // 也可以手动点「读取分区」重读一次（例如镜像在设备上被外部工具改过）。
  $('btn-loop-partitions').addEventListener('click', () => loadPartitions());
  $('btn-loop-image-refresh').addEventListener('click', refreshLoopImages);

  // 离线横幅的手动重连（立即重试并重置退避计数）。
  $('btn-reconnect').addEventListener('click', () => {
    // 手动重连：立即触发重试并重置退避计数。
    resetReconnectFailures();
    clearReconnectTimer();
    reconnect();
  });

  // 视图 6
  $('btn-caps-refresh').addEventListener('click', refreshCapabilities);
  $('btn-config-save').addEventListener('click', saveIdentityConfig);
  $('btn-config-load').addEventListener('click', loadIdentityConfig);
  // 镜像 SELinux 标签：保存与恢复默认是两条独立动作（`reset` 让后端走「清除」
  // 分支，而不是把空输入框当成一个非法上下文）。
  $('btn-image-context-save').addEventListener('click', () => saveImageContextConfig(false));
  $('btn-image-context-reset').addEventListener('click', () => saveImageContextConfig(true));
  $('settings-mode').addEventListener('change', () => {
    const next = loadPrefs();
    next.defaultMode = $('settings-mode').value;
    savePrefs(next);
    toast('默认模式已保存');
  });
  $('create-layout').addEventListener('change', () => {
    const next = loadPrefs();
    next.defaultLayout = $('create-layout').value;
    savePrefs(next);
  });

  $('diag-moddir').textContent = MODDIR;

  // 首屏先渲染一次状态：若第一次刷新就发现后端不可用，横幅必须立刻出现，
  // 而不是只弹一个错误面板（错误面板会被下一次成功清除，横幅不会）。
  renderBackendState();

  // 首次加载：先取状态（同时充当连通性检查），再按需加载其他视图。
  //
  // 四个刷新并发发起，且都要经过「探测 / 可能拉起 REST 后端」这段异步逻辑，
  // 因此**每个都必须兜底**：任何一个拒绝都不应让界面停在「正在连接后端…」。
  // （更彻底的兜底参见 dom.js 中的全局 unhandledrejection / error 监听。）
  guard(refreshStatus(), '挂载状态');
  guard(refreshAvailableSpace(), '可用空间');
  guard(refreshLoop(), '本地挂载列表');
  guard(refreshLoopImages(), '本地编辑镜像列表');
  guard(refreshCapabilities(), '系统能力探测');
  // 镜像 SE 标签：设置页要显示「现在生效的是什么」，因此首屏就取。
  guard(loadImageContextConfig(), '镜像安全上下文');
  // 分区编辑器与格式化来源说明依赖探测结果，首次进入创建页前先准备好。
  guard(refreshMkfsProbes(), '格式化工具探测');
  renderPartitions();
}

// 不依赖 DOMContentLoaded：`type="module"` 脚本默认 defer。
init();