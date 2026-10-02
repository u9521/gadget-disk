// describe.js —— 面向用户的文案映射（错误码、布局、模式、占用、分区类型）。
//
// 只产出字符串，不碰 DOM：零构建约束下这些映射由 Node 内置测试覆盖。

import { isValidGuid } from './partitions.js';

/**
 * 稳定错误码 → 中文文案。
 *
 * 与 docs/protocol.md 的错误码表一一对应。文案要**说明原因与下一步**，
 * 而不是重复错误码本身。
 */
export const ERROR_MESSAGES = {
  busy: '另一操作正在进行，请稍后重试',
  image_not_found: '镜像不存在，请检查路径',
  image_in_use: '镜像正被占用，请先卸载',
  not_regular_file: '该路径不是常规文件',
  unsupported_layout: '无法识别的磁盘布局',
  no_udc: '无可用 USB 控制器（USB 可能未连接）',
  mass_storage_unsupported: '该内核不支持 USB 大容量存储功能',
  loop_unsupported: '该设备不支持本地挂载，请改用 USB 挂载后在电脑上编辑',
  filesystem_unsupported: '内核缺少所需的文件系统驱动',
  // 分区下限**按文件系统**判定，因此这里不带具体数字：后端的 `message` 已说明
  // 「第几行、哪个文件系统、下限多少、实际多少」，界面把它放进 detail 展示。
  // 写成固定数字会在三种文件系统之间说谎。
  size_below_minimum: '分区容量低于该文件系统的下限',
  no_space: '存储空间不足',
  permission_denied: '权限不足（可能受 SELinux 策略限制，详见设置与诊断）',
  invalid_argument: '参数无效',
  // 同名阻断：后端会拒绝创建，这里给出**可操作**的指引而不是笼统报错。
  already_exists: '同名镜像已存在，请换一个文件名，或先删除该镜像',
  internal: '内部错误',
  gdd_unreachable: '后端无响应，请确认模块已启用并重启设备',
  configfs_unavailable: '无法确定可用的 USB gadget（configfs 未就绪）',
  not_active: 'USB 配置已建立但宿主机未接受（设备管理器可能显示为「代码 10」）',
};

// ---------------------------------------------------------------- 文案

/**
 * 错误码 → 文案。
 *
 * **本表是界面错误文案的唯一来源**：后端 `message` 是英文的（面向 CLI/API
 * 使用者），不能直接上屏，否则中文界面里会冒出英文句子。因此已知错误码一律
 * 用本地文案，后端原文只作为**未知码**时的兜底（那时本地无话可说，后端那句
 * 至少说明了发生了什么）。
 *
 * @param {string} code
 * @param {string} [backendMessage] 后端 message，仅在码未知时兜底
 * @returns {string}
 */
export function messageForCode(code, backendMessage = '') {
  if (typeof code === 'string' && Object.prototype.hasOwnProperty.call(ERROR_MESSAGES, code)) {
    return ERROR_MESSAGES[code];
  }
  // 未知码：后端 message 是英文的，但比「未知错误」更有信息量；用括号标出
  // 错误码，便于用户对照 docs/protocol.md 反馈。
  if (typeof backendMessage === 'string' && backendMessage !== '') {
    return code ? `${backendMessage}（${code}）` : backendMessage;
  }
  return `未知错误${code ? `（${code}）` : ''}`;
}

/**
 * 布局 → 文案。
 *
 * @param {string} layout
 * @returns {string}
 */
export function describeLayout(layout) {
  return (
    {
      raw: '无分区表（整盘一个卷）',
      gpt: 'GPT 分区表',
      mbr: 'MBR 分区表',
      unknown: '无法识别',
    }[layout] || '无法识别'
  );
}

/**
 * 模式 → 文案。
 *
 * @param {string} mode
 * @returns {string}
 */
export function describeMode(mode) {
  return (
    {
      rw: '可读写',
      ro: '只读（写保护）',
      cdrom: '光驱（CD-ROM）',
    }[mode] || mode
  );
}

/**
 * 占用状态 → 文案。
 *
 * @param {string} state
 * @returns {string}
 */
export function describeInUse(state) {
  return (
    {
      none: '空闲',
      gadget: 'USB 挂载中',
      loop: '本地挂载中',
      importing: '导入中',
    }[state] || state
  );
}

/**
 * 设备模式 → 说明性警告（若有）。
 *
 * 用于在 UI 上解释底层机制的硬约束，而不是只报错。
 *
 * @param {string} mode
 * @returns {string|null}
 */
export function modeWarning(mode) {
  if (mode === 'cdrom') {
    return '光驱模式需保持只读；建议搭配 .iso 光盘镜像使用。';
  }
  if (mode === 'ro') {
    return '只读模式下宿主机无法写入，防止数据被修改。';
  }
  return null;
}

/**
 * 截断字符串，超长时附加省略标记。
 *
 * @param {string} text
 * @param {number} limit
 * @returns {string}
 */
export function truncate(text, limit) {
  if (typeof text !== 'string') return '';
  if (text.length <= limit) return text;
  return `${text.slice(0, limit)}…（已截断，共 ${text.length} 字符）`;
}

// ---------------------------------------------------------------- 分区与文件系统
//
// 这一节全是纯函数：零构建约束下没有前端测试框架，只有不接触 DOM 的逻辑
// 才能被 Node 内置 test runner 覆盖（docs/webui.md 的明确要求）。

/**
 * 文件系统 → 文案。
 *
 * @param {string} filesystem
 * @returns {string}
 */
export function describeFilesystem(filesystem) {
  return (
    {
      fat32: 'FAT32（兼容性最好）',
      exfat: 'exFAT（大文件友好）',
      ext4: 'ext4（Linux 原生）',
      none: '不格式化',
    }[filesystem] || '未知文件系统'
  );
}

/**
 * GPT 分区类型（线格式名）→ 文案。
 *
 * 自定义 GUID 原样回显（截断显示，完整值在输入框里）。
 *
 * @param {string} type
 * @returns {string}
 */
export function describeGptPartitionType(type) {
  const preset = {
    'gpt:efi_system': 'EFI System',
    'gpt:microsoft_basic': 'Microsoft basic data',
    'gpt:microsoft_reserved': 'Microsoft reserved',
    'gpt:windows_recovery': 'Windows recovery',
    'gpt:linux_filesystem': 'Linux filesystem',
    'gpt:linux_swap': 'Linux swap',
    'gpt:linux_lvm': 'Linux LVM',
    'gpt:linux_raid': 'Linux RAID',
    'gpt:bios_boot': 'BIOS boot',
  }[type];
  if (preset) return preset;

  if (typeof type === 'string' && type.startsWith('gpt:')) {
    const guid = type.slice(4);
    if (isValidGuid(guid)) return `自定义（${truncate(guid, 18)}）`;
  }
  return '未知类型';
}

/**
 * MBR 分区类型（线格式名）→ 文案。
 *
 * @param {string} type
 * @returns {string}
 */
export function describeMbrPartitionType(type) {
  const preset = {
    'mbr:fat32_lba': 'FAT32 (LBA)',
    'mbr:fat32_chs': 'FAT32 (CHS)',
    'mbr:fat16_lba': 'FAT16 (LBA)',
    'mbr:ntfs_exfat': 'NTFS / exFAT',
    'mbr:linux': 'Linux',
    'mbr:linux_swap': 'Linux swap',
    'mbr:linux_lvm': 'Linux LVM',
    'mbr:efi_system': 'EFI System',
    // 保留映射：旧镜像的扩展分区项仍可能被读出来（虽然现在的解析会跳过它），
    // 且存量请求里可能出现这个线格式名。
    'mbr:extended': '扩展分区',
  }[type];
  if (preset) return preset;

  if (typeof type === 'string' && type.startsWith('mbr:')) {
    return `自定义（${type.slice(4)}）`;
  }
  return '未知类型';
}

/**
 * 按布局把分区类型描述为文案。
 *
 * @param {string} layout
 * @param {string} gptType
 * @param {string} mbrType
 * @returns {string}
 */
export function describePartitionType(layout, gptType, mbrType) {
  return layout === 'mbr'
    ? describeMbrPartitionType(mbrType)
    : describeGptPartitionType(gptType);
}
