// view-import.js —— 视图 3：上传/导入。
//
// **唯一入口是系统文件选择器**。内置路径浏览器（`ls`/`stat` 驱动的目录列举）与其
// 后端端点已整体移除：KernelSU 的文件选择器走 `ACTION_GET_CONTENT`，只把
// `content://` 引用交回 WebView，**URI 与文件系统路径都不会进入 JS 面**
// （见 docs/image-upload-and-import.md）。因此「选中文件 → 把路径交给后端复制」
// 在架构上不存在，只能由本模块把字节读出来分块传给后端。
//
// 数据通路：begin → 多次 chunk（原始字节）→ commit；失败或取消走 abort。
// 上传**只有 REST 通道**：`ksu.exec` 无法向子进程写 stdin，CLI 没有等价子命令。

import { formatBytes } from './pure/bytes.js';
import { safeImageName } from './pure/paths.js';
import { toast } from './ksu.js';
import { $, failed, showError } from './dom.js';
import { callCli, callRestRaw, setImportSelection } from './backend.js';
import { runTask } from './task.js';
import { refreshImages } from './view-images.js';

// ---------------------------------------------------------------- 视图 3：上传/导入

/**
 * 每块读取的原始字节数。
 *
 * **实测依据**（AVD，x86_64）：每个 HTTP 请求有约 **26ms 固定开销**
 * （新建连接 + `try_clone` + 线程 + 解析），与载荷大小无关。因此吞吐随分块
 * 大小强烈变化——32 MiB 总量下：
 *
 * | 分块 | 吞吐 |
 * |---|---|
 * | 1 MiB | 23 MiB/s |
 * | 4 MiB | 66 MiB/s |
 * | 8 MiB | 88 MiB/s |
 * | 16 MiB | 100 MiB/s |
 * | 32 MiB | 123 MiB/s |
 *
 * 取 32 MiB：固定开销摊薄到可忽略。代价是内存峰值——但读的是
 * `file.slice()` 的**单块**，不是整个文件，因此峰值≈一块的大小。
 */
export const UPLOAD_CHUNK_BYTES = 32 * 1024 * 1024;

/** 导入视图状态。 */
export const importState = {
  /** 当前选中的 `File`（未选中为 `null`）。 */
  file: null,
  /** 服务端返回的上传 id（仅在一次上传进行中存在）。 */
  uploadId: null,
  jobId: null,
  pollTimer: null,
};

/**
 * 分块数量（用于进度估算与测试）。
 *
 * @param {number} size 总字节数
 * @param {number} [chunkSize]
 * @returns {number}
 */
export function chunkCount(size, chunkSize = UPLOAD_CHUNK_BYTES) {
  if (!Number.isFinite(size) || size <= 0) return 0;
  if (!Number.isFinite(chunkSize) || chunkSize <= 0) return 0;
  return Math.ceil(size / chunkSize);
}

/**
 * 上传进度百分比。
 *
 * `total` 不可信时（provider 可能给 `0`）返回 `null`，让界面显示「已上传 X」
 * 而不是一个会误导人的百分比。
 *
 * @param {number} done
 * @param {number} total
 * @returns {number|null}
 */
export function uploadProgress(done, total) {
  if (!Number.isFinite(total) || total <= 0) return null;
  if (!Number.isFinite(done) || done <= 0) return 0;
  return Math.min(100, Math.round((done / total) * 100));
}

/**
 * 选中文件后的处理：填目标名、显示信息、放开上传按钮。
 *
 * **不在此处读文件内容**——读取推迟到用户点击上传，避免选错文件也先耗一次内存。
 *
 * @param {FileList|File[]} files
 */
export function handleFilePicker(files) {
  if (!files || files.length === 0) return;

  const file = files[0];
  importState.file = file;
  importState.jobId = null;

  // 文件名来自 `File.name`（Chromium 由 provider 的 DISPLAY_NAME 给出）；
  // 没有路径可取末段，这是命名目标的唯一依据。
  $('import-dest').value = safeImageName(file.name) || 'imported.img';
  $('import-selected').textContent = `已选择：${file.name}（${formatBytes(file.size)}）`;
  $('btn-import').disabled = false;
}

/**
 * 用 `FileReader` 读一块。
 *
 * **必须按块 `slice()` 读**：整文件 `readAsArrayBuffer(file)` 会让内存峰值等于
 * 镜像大小，大镜像会直接崩掉 WebView。
 *
 * @param {File} file
 * @param {number} start
 * @param {number} end
 * @returns {Promise<ArrayBuffer>}
 */
function readSlice(file, start, end) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error || new Error('读取文件块失败'));
    reader.readAsArrayBuffer(file.slice(start, end));
  });
}

/** 取消服务端暂存（尽力而为：清理失败不得掩盖真正的错误原因）。 */
async function abortUpload(uploadId) {
  if (!uploadId) return;
  try {
    await callCli({ op: 'upload-abort', uploadId });
  } catch (error) {
    // 忽略。
  }
}

/** 执行上传：begin → 逐块 chunk → commit。 */
export async function doImport() {
  const file = importState.file;
  if (!file) {
    showError({ message: '请先选择要导入的文件' });
    return;
  }

  const dest = safeImageName($('import-dest').value);
  if (!dest) {
    showError({ message: '目标文件名不合法' });
    return;
  }

  await runTask(
    '导入',
    async () => {
      const begin = await callCli({
        op: 'upload-begin',
        destName: dest,
        // 声明大小只用于**进度与空间预检**；provider 可能给 0 或不准，
        // 服务端一律以实际写入字节为准。
        sizeBytes: file.size,
      });
      if (failed(begin)) return;

      // 不能假设 `ok:true` 就一定有合法负载：后端返回了 200 但结构不符
      // （例如空体）时，直接取字段会抛 TypeError，而那个异常经 runTask 包装后
      // 只剩一句没有上下文的「导入失败 / TypeError」。这里显式校验并给出原因。
      const uploadId = begin.data && begin.data.upload_id;
      if (!uploadId) {
        showError({
          message: '后端未返回上传会话 ID',
          detail: `upload/begin response has no upload_id: ${JSON.stringify(begin.data)}`,
        });
        return;
      }
      importState.uploadId = uploadId;
      $('job-card').hidden = false;
      renderUploadProgress(0, file.size);

      try {
        let offset = 0;
        while (offset < file.size) {
          const end = Math.min(offset + UPLOAD_CHUNK_BYTES, file.size);
          const buffer = await readSlice(file, offset, end);

          // 直接发**原始字节**（ArrayBuffer）：`fetch` 会原样发出，没有 base64
          // 的 33% 膨胀——那层编码当初只是为绕开已不存在的请求体上限。
          const chunk = await callRestRaw(
            `/api/v1/upload/chunk?upload_id=${encodeURIComponent(uploadId)}&offset=${offset}`,
            buffer,
          );
          if (!chunk) {
            // `null` = REST 通道不可用，而非业务失败：上传只有这一条通道。
            await abortUpload(uploadId);
            importState.uploadId = null;
            showError({
              message: '上传功能需要 REST 后端支持，当前通道不可用',
              detail:
                '分块上传无法通过命令行（CLI）回退通道执行（ksu.exec 无法向子进程传输数据流）。' +
                '请点击「重连」后重试。',
            });
            return;
          }
          if (failed(chunk)) {
            await abortUpload(uploadId);
            importState.uploadId = null;
            return;
          }

          offset = end;
          renderUploadProgress(offset, file.size);
        }

        const commit = await callCli({ op: 'upload-commit', uploadId });
        importState.uploadId = null;
        if (failed(commit)) return;

        // 同上：不假设负载结构。
        importState.jobId = (commit.data && commit.data.job_id) || null;

        // 与既有导入一致：按「有没有 `job_id`」分流。
        // - 有：`serve` 还活着，job 在后台登记，需要轮询进度；
        // - 无：响应即终态，直接收尾。
        if (importState.jobId) {
          toast('上传完成，正在写入');
          pollJob();
        } else {
          toast('导入完成');
          renderJobStatus(commit.data || {});
          await settleJob(commit.data || {});
        }
      } catch (error) {
        // 读取/上传中途抛错（含页面切走导致 FileReader 失败）：清理服务端暂存，
        // 否则会留下一个 `tmp/*.part` 与一条永远 running 的 job——那会让
        // `serve` **永不空闲退出**。
        await abortUpload(uploadId);
        importState.uploadId = null;
        throw error;
      }
    },
    { scope: 'import' },
  );
}

/**
 * 渲染上传阶段的本地进度。
 *
 * 与 `renderJobStatus` 分开：上传阶段服务端还没有可轮询的 job，进度只能由客户端
 * 按已发送字节算。`total` 不可信时不显示百分比。
 *
 * @param {number} done
 * @param {number} total
 */
export function renderUploadProgress(done, total) {
  const percent = uploadProgress(done, total);
  if (percent === null) {
    $('job-bar').style.width = '100%';
    $('job-text').textContent = `已上传 ${formatBytes(done)}`;
    return;
  }
  $('job-bar').style.width = `${percent}%`;
  $('job-text').textContent = `上传中 ${percent}%（${formatBytes(done)} / ${formatBytes(total)}）`;
}

/**
 * 按 job 状态渲染进度条与文案。
 *
 * 抽出来是为了让「轮询路径」与「一次性路径」共用同一份渲染，
 * 否则两处会随时间漂移（文案或百分比算法改一处忘一处）。
 *
 * @param {{state?: string, bytes_done?: number, bytes_total?: number, error?: string}} status
 */
export function renderJobStatus(status) {
  const total = status.bytes_total || 0;
  const done = status.bytes_done || 0;

  if (status.state === 'done') {
    $('job-bar').style.width = '100%';
    $('job-text').textContent = `完成（${formatBytes(done)}）`;
    return;
  }
  if (status.state === 'failed') {
    $('job-bar').style.width = '100%';
    $('job-text').textContent = `导入失败：${status.error || '未知原因'}`;
    return;
  }
  if (total > 0) {
    const percent = Math.min(100, Math.round((done / total) * 100));
    $('job-bar').style.width = `${percent}%`;
    $('job-text').textContent = `${percent}%（${formatBytes(done)} / ${formatBytes(total)}）`;
  } else {
    // 无法预知总长（流式来源）：只能显示已写入量，此时进度条给满宽度而不是
    // 一个会误导人的百分比。
    $('job-bar').style.width = '100%';
    $('job-text').textContent = `已写入 ${formatBytes(done)}`;
  }
}

/**
 * 按终态收尾：刷新镜像列表或报错。
 *
 * @param {{state?: string, bytes_done?: number, error?: string}} status
 */
export async function settleJob(status) {
  stopPolling();
  if (status.state === 'failed') {
    showError({ message: '导入失败', code: status.error, detail: status.error || '' });
    return;
  }
  if (status.state === 'done') {
    await refreshImages();
  }
}

/** 轮询 job 状态（仅 REST 通道有 job 可轮询）。 */
export async function pollJob() {
  if (!importState.jobId) return;

  const result = await callCli({ op: 'job', jobId: importState.jobId });
  if (failed(result)) {
    stopPolling();
    return;
  }

  const status = result.data || {};
  renderJobStatus(status);

  if (status.state === 'running') {
    // job 在 `serve` 进程内独立执行；页面停留期间持续轮询。
    importState.pollTimer = setTimeout(pollJob, 500);
    return;
  }
  await settleJob(status);
}

/** 停止轮询。 */
export function stopPolling() {
  if (importState.pollTimer) {
    clearTimeout(importState.pollTimer);
    importState.pollTimer = null;
  }
}

// 把「当前是否已选中待导入文件」注入状态机：离线横幅恢复按钮可用性时要用它，
// 而 backend.js 不能反向 import 本模块（会成环）。
setImportSelection(() => importState.file !== null);
