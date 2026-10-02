// bytes.js —— 容量、对齐与容量输入解析的纯函数。
//
// 下限/对齐常量与 Rust 侧 crates/gadgetdisk-core/src/layout.rs 及
// docs/disk-image-format.md 的容量约束保持一致。

/** 各文件系统的**单分区**容量下限（字节）。
 *
 * 与 Rust 侧 `crates/gadgetdisk-core/src/layout.rs` 的 `MIN_FAT32_BYTES` /
 * `MIN_EXFAT_BYTES` / `MIN_EXT4_BYTES` 及 docs/disk-image-format.md 的容量约束一致。
 *
 * **镜像容量本身没有下限**：下限是"每个分区在其文件系统下是否够大"。
 * 早先这里叫 `MIN_IMAGE_BYTES = 64 MiB` 并当成镜像下限，于是 64 MiB 镜像里
 * 一个占满剩余的分区会被报成"空间不足"（实测缺陷）。
 */
export const FILESYSTEM_MIN_BYTES = {
  fat32: 33 * 1024 * 1024,
  exfat: 1024 * 1024,
  ext4: 2 * 1024 * 1024,
};

/** 默认容量：4 GiB。 */
export const DEFAULT_IMAGE_BYTES = 4 * 1024 * 1024 * 1024;

/** 分区起始对齐：1 MiB。 */
export const ALIGNMENT_BYTES = 1024 * 1024;

// ---------------------------------------------------------------- 容量

/**
 * 某个文件系统的单分区下限；`none`（不格式化）或未知类型没有下限。
 *
 * @param {string} filesystem
 * @returns {number|null}
 */
export function minPartitionBytes(filesystem) {
  if (typeof filesystem !== 'string') return null;
  return FILESYSTEM_MIN_BYTES[filesystem] ?? null;
}

/**
 * 校验容量请求。
 *
 * @param {number} requestedBytes 用户请求的字节数
 * @param {number} availableBytes 目标文件系统可用字节数
 * @returns {{ok: true, bytes: number, alignedBytes: number} | {ok: false, message: string}}
 */
export function validateSize(requestedBytes, availableBytes) {
  if (!Number.isFinite(requestedBytes) || requestedBytes <= 0) {
    return { ok: false, message: '请输入有效的容量' };
  }

  const alignedBytes = alignUp(requestedBytes, ALIGNMENT_BYTES);

  // 预检可用空间。稀疏文件在写入时才占空间，故这里必须拦截。
  if (Number.isFinite(availableBytes) && alignedBytes > availableBytes) {
    return {
      ok: false,
      message: `空间不足：需要 ${formatBytes(alignedBytes)}，可用 ${formatBytes(availableBytes)}`,
    };
  }

  return { ok: true, bytes: requestedBytes, alignedBytes };
}

/**
 * 向上对齐到 `align` 的整数倍。
 *
 * @param {number} value
 * @param {number} align
 * @returns {number}
 */
export function alignUp(value, align) {
  if (!align || align <= 0) return value;
  const remainder = value % align;
  return remainder === 0 ? value : value + (align - remainder);
}

/**
 * 把字节数格式化为人读形式。
 *
 * @param {number} bytes
 * @returns {string}
 */
export function formatBytes(bytes) {
  if (!Number.isFinite(bytes)) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded = value >= 100 || Number.isInteger(value) ? Math.round(value) : value.toFixed(1);
  return `${rounded} ${units[unit]}`;
}

/**
 * 把 `${n}M` / `${n}G` 之类的输入解析为字节数。
 *
 * @param {string} text
 * @returns {number|null}
 */
export function parseSizeInput(text) {
  if (typeof text !== 'string') return null;
  // 接受 `64M`、`64MB`、`64MiB`、`64 M`、`1024` 等形式。
  const match = text.trim().match(/^(\d+(?:\.\d+)?)\s*([kKmMgGtT]?)(?:i?[bB])?$/);
  if (!match) return null;

  const value = Number.parseFloat(match[1]);
  if (!Number.isFinite(value) || value <= 0) return null;

  const multiplier = {
    '': 1,
    k: 1024,
    m: 1024 ** 2,
    g: 1024 ** 3,
    t: 1024 ** 4,
  }[match[2].toLowerCase()];

  if (!multiplier) return null;
  return Math.round(value * multiplier);
}

/**
 * 解析**分区容量**输入，与 [`parseSizeInput`] 分开。
 *
 * ## 为什么必须是两个函数
 *
 * 分区容量允许 `0`，语义是「占满剩余空间」；而镜像容量必须为正数。
 * 早先两者共用 `parseSizeInput`，结果是 `0` 被解析成 `null`，再经调用方的
 * `?? -1` 变成一个非法的哨兵值，界面上表现为容量框显示 `undefined` 或空
 * ——这正是本次修复的缺陷。
 *
 * 两个函数共用同一套单位后缀与语法，只有「是否接受 0」这一条不同。
 *
 * @param {string} text
 * @returns {number|null} 字节数；`0` 表示占满剩余空间
 */
export function parsePartitionSize(text) {
  if (typeof text !== 'string') return null;
  const trimmed = text.trim();
  // `0` 及其带单位写法（`0M`）都表示"占满剩余空间"。
  if (/^0\s*[kKmMgGtT]?(?:i?[bB])?$/.test(trimmed)) return 0;
  return parseSizeInput(trimmed);
}

/**
 * 容量说明文案（稀疏文件行为）。
 *
 * **不再有"低于下限"分支**：镜像容量本身没有下限，真正要拦的是"某个分区在其
 * 文件系统下太小"，那由 [`minPartitionBytes`] 与 `validatePartitions` 判定并
 * 带上行号。这里只负责解释"为什么填 4G 不会立刻占满存储"。
 *
 * @param {number} bytes
 * @returns {string|null} 非有限值时返回 `null`（调用方退回自己的提示）
 */
export function sizeNote(bytes) {
  if (!Number.isFinite(bytes)) return null;
  // 与镜像格式文档一致：镜像以稀疏文件创建，大容量不会立即占用实际存储。
  return '镜像采用稀疏文件机制创建，分配大容量不会立即占满实际存储空间。';
}
