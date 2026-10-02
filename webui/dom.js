// dom.js —— 界面工具：元素查询、错误面板与 Promise 兜底。
//
// 从 app.js 的「界面工具」一节原样拆出。这一层不认识后端，只负责把失败变成
// **可见**的界面反馈：三类异常都必须有提示，不能只写控制台。

import { messageForCode } from './pure/describe.js';

// ---------------------------------------------------------------- 界面工具

/** @param {string} id */
export function $(id) {
  return document.getElementById(id);
}

/**
 * 显示全局错误面板。
 *
 * 三类异常都必须有**可见提示**，不能只写控制台。
 *
 * @param {{message: string, detail?: string, code?: string}} error
 */
export function showError(error) {
  const panel = $('error-panel');
  $('error-title').textContent = error.code ? messageForCode(error.code) : '操作发生错误';
  $('error-message').textContent = error.message || '未知错误';
  const detail = $('error-detail');
  if (error.detail) {
    detail.textContent = error.detail;
    detail.hidden = false;
  } else {
    detail.hidden = true;
  }
  panel.hidden = false;
}

/** 清除错误面板。 */
export function clearError() {
  $('error-panel').hidden = true;
}

/**
 * 全局 Promise 未捕获拒绝兜底，防止异步错误静默导致界面卡死。
 */
/**
 * 把任意抛出物格式化为可读文本（`detail` 只接受字符串）。
 *
 * @param {any} thrown
 * @returns {string}
 */
export function describeThrown(thrown) {
  if (thrown instanceof Error) return `${thrown.name}: ${thrown.message}`;
  if (typeof thrown === 'string') return thrown;
  try {
    return JSON.stringify(thrown);
  } catch (error) {
    return String(thrown);
  }
}

/**
 * 给 Promise 加兜底错误提示。
 *
 * 首次加载的多个刷新是**并发发起**的，任何一个拒绝都不会被同步的 try/catch
 * 捕获；`guard` 把拒绝转成可见的错误面板，而不是静默停在加载态。
 *
 * @param {Promise<any>} promise
 * @param {string} label 出错时展示的上下文（中文）
 * @returns {Promise<any>}
 */
export function guard(promise, label) {
  return Promise.resolve(promise).catch((error) => {
    showError({ message: `加载失败：${label}`, detail: describeThrown(error) });
  });
}

/**
 * 统一处理失败结果：显示错误并返回 false。
 *
 * @param {{ok: boolean, message?: string, detail?: string, code?: string}} result
 * @returns {boolean}
 */
export function failed(result) {
  if (result.ok) {
    clearError();
    return false;
  }
  // 兜底：失败结果必须包含明确可读的用户提示文案。早期实现直接透传 result，于是任何
  // 「ok:false 但没有 message」的结果都会在界面上显示成「未知错误」——
  // 用户拿不到任何排查线索。这里把整个结果序列化进 detail。
  showError({
    ...result,
    message: result.message || '后端返回了无法识别的失败结果',
    detail: result.detail || JSON.stringify(result),
  });
  return true;
}

// 模块顶层即注册（`type="module"` 脚本默认 defer，无需等待 DOMContentLoaded）。

window.addEventListener('unhandledrejection', (event) => {
  showError({
    message: '页面发生未处理的异常，界面可能暂停更新',
    detail: describeThrown(event.reason),
  });
});

window.addEventListener('error', (event) => {
  showError({
    message: '页面脚本运行错误',
    detail: describeThrown(event.error || event.message),
  });
});