// paths.js —— 路径拼接/取段与 shell 转义的纯函数。
//
// 镜像名安全化与 Rust 侧 `component_from_file_name` 同约束。

// ---------------------------------------------------------------- 路径

/**
 * 拼接目录与文件名，处理多余/缺失的斜杠。
 *
 * @param {string} dir
 * @param {string} name
 * @returns {string}
 */
export function joinPath(dir, name) {
  if (!dir) return name || '';
  if (!name) return dir;
  const base = dir.endsWith('/') ? dir.slice(0, -1) : dir;
  const leaf = name.startsWith('/') ? name.slice(1) : name;
  return `${base}/${leaf}`;
}

/**
 * 取路径的父目录。
 *
 * @param {string} path
 * @returns {string}
 */
export function parentPath(path) {
  if (!path || path === '/') return '/';
  const trimmed = path.endsWith('/') ? path.slice(0, -1) : path;
  const index = trimmed.lastIndexOf('/');
  if (index <= 0) return '/';
  return trimmed.slice(0, index);
}

/**
 * 取路径的最后一段。
 *
 * @param {string} path
 * @returns {string}
 */
export function baseName(path) {
  if (!path) return '';
  const trimmed = path.endsWith('/') ? path.slice(0, -1) : path;
  const index = trimmed.lastIndexOf('/');
  return index === -1 ? trimmed : trimmed.slice(index + 1);
}

/**
 * 把任意字符串安全化为可用的镜像文件名。
 *
 * 与 Rust 侧 `component_from_file_name` 的约束一致：只允许
 * 字母、数字、`.`、`_`、`-`；并确保不会产生 `..`。
 *
 * @param {string} name
 * @param {string} [fallback]
 * @returns {string|null}
 */
export function safeImageName(name, fallback = 'image.img') {
  if (typeof name !== 'string') return null;
  // 替换非法字符，再去掉开头连续的点/下划线/连字符：
  // 前者可能来自路径穿越尝试（`../evil.img` → `.._evil.img`），
  // 后者会产出 `_evil.img` 这类前导下划线的非规范文件名。
  const sanitized = name
    .trim()
    .replace(/[^A-Za-z0-9._-]/g, '_')
    .replace(/^[._-]+/, '');

  if (!sanitized) return fallback;
  if (sanitized === '.' || sanitized === '..') return fallback;
  if (!sanitized.includes('.')) return `${sanitized}.img`;
  return sanitized;
}

/**
 * 单引号转义，避免路径中的空格或特殊字符破坏命令。
 *
 * CLI 回退路径用；REST 路径走 JSON body，不需要转义。
 *
 * @param {string} value
 * @returns {string}
 */
export function shellQuote(value) {
  return `'${String(value).replace(/'/g, `'\\''`)}'`;
}
