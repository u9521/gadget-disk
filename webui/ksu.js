// ksu.js —— 对 KernelSU 注入的 window.ksu 的最小 Promise 封装。
// 适配零构建约束（详见 docs/webui.md），提供无额外依赖的异步接口封装。

/**
 * 执行一条 shell 命令。
 *
 * KernelSU 的 `ksu.exec(cmd, optionsJson, callbackName)` 以回调形式返回结果：
 * 它会把 `window[callbackName]` 当作回调调用，参数为 `(errno, stdout, stderr)`。
 *
 * 本函数把该回调式接口包装为 Promise，并**集中处理三类异常**
 * （docs/webui.md 要求，缺一即可能白屏）：
 *
 * 1. 命令失败（`errno !== 0`）—— 不 reject，而是解析为正常结果，
 *    由调用方决定如何展示，并保留 stderr 供排查；
 * 2. 空输出（`stdout.trim() === ''`）—— 同样解析为正常结果，
 *    由上层提示「后端无响应」；
 * 3. `ksu` 不存在或回调抛错 —— 解析为 `errno = -1` 的结果而非 reject，
 *    避免未捕获的 Promise 拒绝导致界面无响应。
 *
 * 设计选择：**永不 reject**。调用方只需处理一个返回结构，
 * 不必同时写 `.then()` 与 `.catch()`，降低漏处理导致白屏的风险。
 *
 * @param {string} command 要执行的命令
 * @param {{cwd?: string, env?: Record<string,string>}} [options] exec 选项
 * @returns {Promise<{errno: number, stdout: string, stderr: string}>}
 */
export function exec(command, options) {
  return new Promise((resolve) => {
    if (typeof window === 'undefined' || typeof window.ksu === 'undefined') {
      resolve({
        errno: -1,
        stdout: '',
        stderr: 'ksu API unavailable: not running inside a KernelSU/APatch WebUI',
      });
      return;
    }

    const callbackName = uniqueCallbackName('exec');
    let settled = false;

    const finish = (result) => {
      if (settled) return;
      settled = true;
      delete window[callbackName];
      resolve(result);
    };

    window[callbackName] = (errno, stdout, stderr) => {
      finish({
        errno: typeof errno === 'number' ? errno : -1,
        stdout: typeof stdout === 'string' ? stdout : '',
        stderr: typeof stderr === 'string' ? stderr : '',
      });
    };

    try {
      window.ksu.exec(command, JSON.stringify(options || {}), callbackName);
    } catch (error) {
      finish({ errno: -1, stdout: '', stderr: String(error) });
    }
  });
}

/**
 * 生成唯一的回调函数名。
 *
 * 时间戳 + 自增计数：同一毫秒内的多次调用也不会冲突。
 *
 * @param {string} prefix
 * @returns {string}
 */
function uniqueCallbackName(prefix) {
  uniqueCallbackName.counter = (uniqueCallbackName.counter || 0) + 1;
  return `${prefix}_callback_${Date.now()}_${uniqueCallbackName.counter}`;
}

/**
 * 显示一个原生 Toast（若宿主支持）。
 *
 * @param {string} message
 */
export function toast(message) {
  if (typeof window !== 'undefined' && window.ksu && window.ksu.toast) {
    try {
      window.ksu.toast(message);
      return;
    } catch (error) {
      // 忽略：Toast 失败不应影响主流程。
    }
  }
  // 回退到控制台，避免静默丢失信息。
  console.log('[toast]', message);
}

/**
 * 查询模块信息（路径等）。
 *
 * @returns {object|null}
 */
export function moduleInfo() {
  if (typeof window !== 'undefined' && window.ksu && window.ksu.moduleInfo) {
    try {
      return window.ksu.moduleInfo();
    } catch (error) {
      return null;
    }
  }
  return null;
}
