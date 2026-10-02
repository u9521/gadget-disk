// partitions.js —— 分区表领域逻辑（布局、归属、槽位、类型解析与预校验）。
//
// 判定与后端 `partspec` / `create` 对齐，且**不接触 DOM** 以便 Node 测试。

import { formatBytes, minPartitionBytes } from './bytes.js';
import { truncate } from './describe.js';

/** 可选的磁盘布局。 */
export const LAYOUTS = ['gpt', 'mbr', 'raw'];

/** 可选的设备模式。 */
export const MODES = ['rw', 'ro', 'cdrom'];

/** 可选的文件系统（`none` 表示该分区不格式化，见 partitionRow 的用法）。 */
export const FILESYSTEMS = ['fat32', 'exfat', 'ext4'];

/** MBR 主分区槽位数（首扇区分区项数组固定 4 项）。 */
export const MBR_MAX_PRIMARY = 4;

/** MBR 逻辑分区数量上限（与后端 `partspec::MBR_MAX_LOGICAL` 一致）。 */
export const MBR_MAX_LOGICAL = 64;

/**
 * 各布局允许的最大分区数（**总数**，MBR 含逻辑分区但**不含**扩展容器）。
 *
 * MBR 有两种合法组合：4 个主分区，或 3 主分区 + 1 个扩展分区容器（容器里放
 * 任意多个逻辑分区，也可以是空的）。因此 4 不再是硬上限，`MBR_MAX_PRIMARY`
 * 才是主分区数量的上限。
 *
 * 与后端 `partspec::max_total_partitions` 必须一致：不一致会让 UI 放行一个
 * 必然失败的请求，或拦下一个本该能建的组合。**扩展容器不计入**该数——它不占
 * 内核序号、没有数据区，见 {@link mbrSlotUsage} 的 `countsAsPartition`。
 */
export const MAX_PARTITIONS = {
  gpt: 128,
  mbr: MBR_MAX_PRIMARY - 1 + MBR_MAX_LOGICAL,
  raw: 1,
};

/**
 * 分区归属：主分区 / 逻辑分区 / 扩展分区容器（仅 MBR 有意义）。
 *
 * 后端会拒绝在 GPT/raw 下使用 `logical` 或 `extended`——那两种布局没有扩展
 * 分区机制，静默当作主分区会让用户在 Host 上得到与预期不符的分区表。
 *
 * **扩展分区是容器，不是分区**：它占一个首扇区槽位，但不占内核序号、没有数据
 * 区、不可格式化。两种用法：
 *
 * - 放了逻辑分区时，容器必然存在（用户不必声明，见 {@link mbrSlotUsage}）；
 * - 想**预留**一片空间以后再放逻辑分区时，显式声明一个**空容器**。
 */
export const PARTITION_KINDS = ['primary', 'logical', 'extended'];

/** MBR 扩展分区容器数量上限（首扇区里只有一个这样的项可写）。 */
export const MBR_MAX_EXTENDED = 1;

/**
 * 某一行的**内核序号**（不是行下标）。
 *
 * MBR 的编号规则：主分区按出现顺序占 1–4，逻辑分区按出现顺序从 **5** 起，且
 * 扩展分区容器本身**不占序号**。这与后端 `resolve_mbr_layout` 的编号完全一致，
 * 因此 UI 显示的序号可以直接对上内核 `loopNpM` 的 `M`。
 *
 * GPT/raw 下序号就是 1 起的出现顺序。
 *
 * @param {Array<{kind?: string}>} partitions
 * @param {number} rowIndex 该行在列表里的下标
 * @param {string} layout
 * @returns {number|null} 内核序号；扩展分区容器返回 `null`（它没有序号）
 */
export function partitionKernelIndex(partitions, rowIndex, layout) {
  const list = Array.isArray(partitions) ? partitions : [];
  const entry = list[rowIndex];
  if (!entry) return rowIndex + 1;

  // 扩展分区容器不占序号：`loopNpM` 里没有对应的 `M`。返回 `null` 而不是 0，
  // 是为了让调用方**必须**显式处理（把 0 当序号渲染出来是错的，而 `null` 会在
  // 模板字符串里变成 "null"，一眼可见）。
  if (layout === 'mbr' && (entry.kind || 'primary') === 'extended') return null;

  // 非 MBR：序号即出现顺序。
  if (layout !== 'mbr') return rowIndex + 1;

  if ((entry.kind || 'primary') === 'logical') {
    // 逻辑分区的序号 = 5 + 它在**逻辑分区内部**的出现次序。
    let seen = 0;
    for (let i = 0; i < rowIndex; i += 1) {
      if ((list[i].kind || 'primary') === 'logical') seen += 1;
    }
    return MBR_MAX_PRIMARY + 1 + seen;
  }

  // 主分区：按主分区的出现次序编号（容器不计入）。
  let seen = 0;
  for (let i = 0; i < rowIndex; i += 1) {
    if ((list[i].kind || 'primary') === 'primary') seen += 1;
  }
  return seen + 1;
}

/**
 * 统计一组分区规格占用的 MBR 首扇区槽位。
 *
 * **这是唯一计算槽位的地方**：创建表单的提示文案、添加按钮的禁用条件、以及
 * `validatePartitions` 的合法性判定都必须用它，否则三者会各自漂移（早先提示
 * 文案就与实际规则不符）。
 *
 * 规则：主分区各占一个槽位，扩展分区容器**共同**占一个（无论它是因为有逻辑
 * 分区而隐式产生，还是用户显式声明的空容器）。因此「4 主」与「3 主 + 1 容器」
 * 都合法，而「4 主 + 1 容器」需要 5 个槽位。
 *
 * @param {Array<{kind?: string}>} partitions
 * @returns {{primaries: number, logicals: number, extendeds: number,
 *   neededSlots: number, usesExtended: boolean, freeSlots: number,
 *   hasEmptyExtended: boolean, countsAsPartition: number}}
 */
export function mbrSlotUsage(partitions) {
  const list = Array.isArray(partitions) ? partitions : [];
  const primaries = list.filter((p) => (p.kind || 'primary') === 'primary').length;
  const logicals = list.filter((p) => (p.kind || 'primary') === 'logical').length;
  const extendeds = list.filter((p) => (p.kind || 'primary') === 'extended').length;

  // 容器只需要一个：显式声明与「有逻辑分区」是同一件事的两种来源。
  const usesExtended = logicals > 0 || extendeds > 0;
  const neededSlots = primaries + (usesExtended ? 1 : 0);

  return {
    primaries,
    logicals,
    extendeds,
    neededSlots,
    usesExtended,
    freeSlots: MBR_MAX_PRIMARY - neededSlots,
    // 有容器但里面还没有逻辑分区：界面要说明它是「预留」。
    hasEmptyExtended: usesExtended && logicals === 0,
    // **容器不是分区**，不该计入分区总数（那会让「3 主 + 1 容器 + 64 逻辑」
    // 被错误地算成 68 个而拦下）。
    countsAsPartition: primaries + logicals,
  };
}

/**
 * GPT 分区类型预设（线格式名）。
 *
 * **与 MBR 类型是两个独立的集合**：GPT 用 128 位类型 GUID，MBR 只用一个字节，
 * 二者没有对应关系（同一个「FAT32」在 GPT 里是 Microsoft Basic Data，
 * 在 MBR 里是 `0x0C`）。因此界面上按当前布局切换可选项。
 */
export const GPT_PARTITION_TYPES = [
  'gpt:microsoft_basic',
  'gpt:efi_system',
  'gpt:microsoft_reserved',
  'gpt:windows_recovery',
  'gpt:linux_filesystem',
  'gpt:linux_swap',
  'gpt:linux_lvm',
  'gpt:linux_raid',
  'gpt:bios_boot',
];

/** MBR 分区类型预设（线格式名）。 */
export const MBR_PARTITION_TYPES = [
  'mbr:fat32_lba',
  'mbr:fat32_chs',
  'mbr:fat16_lba',
  'mbr:ntfs_exfat',
  'mbr:linux',
  'mbr:linux_swap',
  'mbr:linux_lvm',
  'mbr:efi_system',
];

/**
 * 扩展分区（`0x05`）**刻意不在类型预设里**：容器的类型字节恒为本值，由后端
 * 生成，不是用户可选的「分区类型」。要表达「这一行是容器」请改**归属**
 * （见 {@link PARTITION_KINDS}）；要表达「放进容器里」请改成逻辑分区。
 *
 * 这个常量仍然需要：`validatePartitions` 用它拦下"把容器类型填在普通分区上"
 * 这种矛盾输入，并指引用户改用归属。后端也仍能解析 `mbr:extended`（存量请求
 * 不报错）。
 */
export const MBR_CONTAINER_TYPE = 'mbr:extended';

/** 自定义类型的下拉哨兵值。选中它才显示 GUID / 十六进制输入框。 */
export const CUSTOM_TYPE_VALUE = '__custom__';

/**
 * 校验 GPT 类型 GUID 字面量。
 *
 * 接受标准 `8-4-4-4-12` 十六进制形式，大小写不敏感。
 *
 * @param {string} text
 * @returns {boolean}
 */
export function isValidGuid(text) {
  if (typeof text !== 'string') return false;
  return /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/.test(
    text.trim(),
  );
}

/**
 * 校验 MBR 类型字节（`0x1A` 或裸十六进制 `1a`）。
 *
 * @param {string} text
 * @returns {boolean}
 */
export function isValidMbrByte(text) {
  if (typeof text !== 'string') return false;
  const trimmed = text.trim().replace(/^0[xX]/, '');
  return /^[0-9a-fA-F]{1,2}$/.test(trimmed);
}

/**
 * 把用户输入的自定义类型规整为线格式名。
 *
 * 非法的自定义值返回 `null`，由调用方决定如何提示。
 *
 * @param {string} layout
 * @param {string} text
 * @returns {string|null}
 */
export function customTypeWire(layout, text) {
  if (layout === 'mbr') {
    if (!isValidMbrByte(text)) return null;
    const hex = text.trim().replace(/^0[xX]/, '').toUpperCase().padStart(2, '0');
    return `mbr:0x${hex}`;
  }
  if (!isValidGuid(text)) return null;
  return `gpt:${text.trim().toUpperCase()}`;
}

/**
 * 该布局是否支持配置分区。
 *
 * `raw` 没有分区表，整个镜像就是一个卷，因此不展示分区编辑器。
 *
 * @param {string} layout
 * @returns {boolean}
 */
export function layoutSupportsPartitions(layout) {
  return layout === 'gpt' || layout === 'mbr';
}

/**
 * 该布局是否会把分区名写入镜像（即界面上是否**渲染**名称字段）。
 *
 * **MBR 没有分区名字段**（首扇区 16 字节的分区项里没有放名字的地方），因此
 * 用户填了也不会生效。界面对此**整个不渲染**该字段——比"渲染成禁用状态"更
 * 诚实：一个禁用的输入框仍会让人以为自己可以填，只是被挡住了。
 *
 * @param {string} layout
 * @returns {boolean}
 */
export function layoutSupportsPartitionNames(layout) {
  return layout === 'gpt';
}

/**
 * 该布局支持的分区类型预设列表。
 *
 * @param {string} layout
 * @returns {Array<string>}
 */
export function partitionTypePresets(layout) {
  return layout === 'mbr' ? MBR_PARTITION_TYPES : GPT_PARTITION_TYPES;
}

/**
 * 文件系统在**指定布局**下的默认分区类型。
 *
 * 与后端 `create::default_types_for` 的映射保持一致，否则「显式填类型」与
 * 「留空让后端推断」会得到不同的分区表。
 *
 * **两套类型空间各自独立**：同一个文件系统在 GPT 与 MBR 下的默认类型是
 * 两个不同的值（例如 ext4 → GPT 的 Linux filesystem / MBR 的 `0x83`）。
 *
 * @param {string} layout
 * @param {string} filesystem
 * @returns {string} 线格式名（带 `gpt:` / `mbr:` 前缀）
 */
export function defaultPartitionType(layout, filesystem) {
  if (layout === 'mbr') {
    if (filesystem === 'ext4') return 'mbr:linux';
    if (filesystem === 'exfat') return 'mbr:ntfs_exfat';
    return 'mbr:fat32_lba';
  }
  if (filesystem === 'ext4') return 'gpt:linux_filesystem';
  if (filesystem === 'exfat') return 'gpt:microsoft_basic';
  return 'gpt:microsoft_basic';
}

/**
 * 校验分区列表。
 *
 * 与后端 `partspec::validate` + `resolve_sizes` 的判定一致，但在这里**提前**
 * 拦下来，让用户不必等一次后端往返才知道填错了。
 *
 * @param {Array<{sizeBytes: number, gptType: string, mbrType: string,
 *   name: string, filesystem: string, kind: string}>} partitions
 * @param {{layout: string, filesystem: string, imageBytes: number}} context
 * @returns {{ok: boolean, message?: string, detail?: string}}
 */
export function validatePartitions(partitions, context) {
  const list = Array.isArray(partitions) ? partitions : [];
  const layout = context && context.layout;
  const imageBytes = context && context.imageBytes;

  if (!layoutSupportsPartitions(layout)) {
    return { ok: true };
  }

  if (list.length === 0) {
    return {
      ok: false,
      message: '至少需要一个分区',
      detail: '点击「添加分区」创建第一个分区。',
    };
  }

  // 逻辑分区与扩展容器都是 MBR 独有的机制（EBR 链）。GPT 没有这个概念，
  // 后端会明确拒绝——前端提前拦下，避免用户等一次往返才发现。
  if (layout !== 'mbr') {
    const extendedRow = list.findIndex((p) => (p.kind || 'primary') === 'extended');
    if (extendedRow >= 0) {
      return {
        ok: false,
        message: '当前布局不支持扩展分区容器',
        detail:
          `第 ${extendedRow + 1} 行被标为扩展分区容器。` +
          '扩展分区是 MBR 的机制（GPT 没有 EBR 链），' +
          '请把该行的「归属」改为主分区，或把布局改为 MBR。',
      };
    }
  }

  const max = MAX_PARTITIONS[layout] || 1;
  // **扩展容器不计入分区数**：它不是分区（不占序号、没有数据区）。若按
  // `list.length` 判断，「3 主 + 1 容器 + 64 逻辑」会被算成 68 个而错误拦下。
  const partitionCount =
    layout === 'mbr' ? mbrSlotUsage(list).countsAsPartition : list.length;
  if (partitionCount > max) {
    return {
      ok: false,
      message: `${layout.toUpperCase()} 最多支持 ${max} 个分区`,
      detail: layout === 'mbr' ? '逻辑分区最多 64 个。' : '',
    };
  }

  // **MBR 槽位计数**：主分区各占一个首扇区槽位，扩展分区容器**共同**占一个
  // （无论它是因有逻辑分区而隐式产生，还是用户显式声明的空容器）。这正是
  // 「3 主 + 1 容器」的由来。用总数判断是错的：4 个分区在「4 主」下合法，
  // 在「3 主 + 1 逻辑」下也合法，但「4 主 + 1 容器」需要 5 个槽位，必须拦下。
  if (layout === 'mbr') {
    const usage = mbrSlotUsage(list);

    // 容器最多一个：首扇区里只有一个扩展分区项可写。
    if (usage.extendeds > MBR_MAX_EXTENDED) {
      return {
        ok: false,
        message: `MBR 只能有一个扩展分区容器`,
        detail:
          `当前有 ${usage.extendeds} 个。请把所有逻辑分区放进同一个容器，` +
          `删除多余的容器行。`,
      };
    }

    if (usage.neededSlots > MBR_MAX_PRIMARY) {
      return {
        ok: false,
        message: `MBR 首扇区只有 ${MBR_MAX_PRIMARY} 个分区项，当前分区配置超出限制`,
        detail:
          `${usage.primaries} 个主分区` +
          (usage.usesExtended
            ? usage.logicals > 0
              ? ` + 1 个扩展分区容器（容纳 ${usage.logicals} 个逻辑分区）`
              : ' + 1 个扩展分区容器'
            : '') +
          ` 需要 ${usage.neededSlots} 个槽位。` +
          (usage.usesExtended
            ? '请把其中一个主分区改为逻辑分区，或减少一个主分区。'
            : ''),
      };
    }

    // 逻辑分区总数上限（与后端 `MBR_MAX_LOGICAL` 一致）。总数上界
    // `MAX_PARTITIONS.mbr` 只在"3 主 + 逻辑"时才是紧的，因此这里单独判一次，
    // 否则"1 主 + 65 逻辑"会被放行而必然被后端拒绝。
    if (usage.logicals > MBR_MAX_LOGICAL) {
      return {
        ok: false,
        message: `MBR 逻辑分区最多 ${MBR_MAX_LOGICAL} 个`,
        detail: `当前有 ${usage.logicals} 个逻辑分区。`,
      };
    }

    // 至少要有一个真正的分区：只有容器的表在 Host 上什么都挂不上。
    if (usage.countsAsPartition === 0) {
      return {
        ok: false,
        message: '至少要有一个主分区或逻辑分区',
        detail: '扩展分区容器本身不可挂载，不能作为唯一内容。',
      };
    }
  }

  for (let i = 0; i < list.length; i += 1) {
    const p = list[i];
    const isExtended = layout === 'mbr' && (p.kind || 'primary') === 'extended';
    const what = isExtended ? `第 ${i + 1} 行的扩展分区容器` : `第 ${i + 1} 个分区`;

    if (!Number.isFinite(p.sizeBytes) || p.sizeBytes < 0) {
      return {
        ok: false,
        message: `${what}的容量不合法`,
        detail: '请填写字节数，或写 0 表示占满剩余空间。',
      };
    }

    // 容器没有数据区：不能让用户以为那 32 MiB 里有个文件系统。
    if (isExtended) {
      if (p.filesystem && p.filesystem !== 'none') {
        return {
          ok: false,
          message: `${what}不能被格式化`,
          detail: '扩展分区容器没有数据区，里面只能放逻辑分区。',
        };
      }
      // 容器的类型字节恒为 0x05，由后端生成；用户填的类型在这里没有意义，
      // 因此不校验类型（界面在容器行上也不渲染类型选择器）。
      continue;
    }

    // 类型按布局**分域**校验：只关心当前布局生效的那一套。
    // 留空表示"用布局默认值"，**不是**错误；只有真的填错才拦。
    if (layout === 'mbr') {
      // 扩展分区不是可选**类型**：要表达容器请改「归属」。
      if ((p.mbrType || '') === MBR_CONTAINER_TYPE) {
        return {
          ok: false,
          message: `第 ${i + 1} 个分区的类型不能直接设置为「扩展分区」`,
          detail:
            '扩展分区是容纳逻辑分区的容器，不是分区类型。' +
            '要把这一行变成容器，请把它的「归属」改为「扩展分区」；' +
            '要把它放进扩展分区，请改为「逻辑分区」。',
        };
      }
      if (resolveMbrType(p).kind === 'invalid') {
        return {
          ok: false,
          message: `第 ${i + 1} 个分区的 MBR 类型不合法`,
          detail: '请选择一个预设类型，或填入 1–2 位十六进制类型字节（如 1A）。',
        };
      }
    } else if (resolveGptType(p).kind === 'invalid') {
      return {
        ok: false,
        message: `第 ${i + 1} 个分区的 GPT 类型 GUID 不合法`,
        detail: 'GUID 形如 12345678-9ABC-DEF0-1234-56789ABCDEF0。',
      };
    }

    if (p.filesystem && p.filesystem !== 'none') {
      if (!FILESYSTEMS.includes(p.filesystem)) {
        return { ok: false, message: `第 ${i + 1} 个分区的文件系统无法识别` };
      }
    }

    // **下限按该分区自己的文件系统判定**（留空则用全局默认）。
    //
    // 后端 `partspec::resolve_sizes` 判的是同一件事；这里提前拦下，用户不必等一次
    // 往返。关键是报出**哪一行**、**哪个文件系统**、**下限与实际值**——早先的
    // 实现把 FAT32 的 64 MiB 当成镜像下限并报 `no_space`，于是"存储空间不足"
    // 这种说法在 64 MiB 镜像 + 32 MiB 分区时完全指不到真正的原因。
    if (p.sizeBytes > 0) {
      const floor = floorProblem(i, p, context, p.sizeBytes);
      if (floor) return floor;
    }
    // 只有 GPT 会写入名字，故只在 GPT 下校验长度（UTF-16 码元）。
    if (layoutSupportsPartitionNames(layout) && typeof p.name === 'string') {
      if (p.name.length > 36) {
        return {
          ok: false,
          message: `第 ${i + 1} 个分区的名称过长`,
          detail: 'GPT 分区名最长为 36 个字符。',
        };
      }
    }
  }

  // 「占满剩余空间」的名额**分区与容器共用**：容器的容量同样占用镜像空间，
  // 两者都填 0 时"谁拿剩余"取决于后端实现细节，用户无法从界面推断。
  const autoCount = list.filter((p) => p.sizeBytes === 0).length;
  if (autoCount > 1) {
    return {
      ok: false,
      message: '最多只能有一处「占满剩余空间」',
      detail:
        `当前有 ${autoCount} 处容量为 0（分区与扩展分区容器共用这一个名额）；` +
        `请为其余项填写具体容量。`,
    };
  }

  if (Number.isFinite(imageBytes)) {
    const fixed = list.reduce((sum, p) => sum + (p.sizeBytes || 0), 0);
    if (fixed > imageBytes) {
      return {
        ok: false,
        message: '分区容量总和超过镜像容量',
        detail: `已分配 ${formatBytes(fixed)}，镜像仅 ${formatBytes(imageBytes)}。`,
      };
    }

    // 「占满剩余」的那一行也要过它自己的文件系统下限：它只会拿到 `imageBytes - fixed`，
    // 拿不下就建不出该文件系统。判定与后端一致，行号与下限都在文案里。
    if (autoCount === 1) {
      const index = list.findIndex((p) => p.sizeBytes === 0);
      const p = list[index];
      const isExtended = layout === 'mbr' && (p.kind || 'primary') === 'extended';
      // 容器没有文件系统，只要求放得下 EBR（后端常量 1 MiB）。
      if (!isExtended) {
        const remaining = imageBytes - fixed;
        const floor = floorProblem(index, p, context, remaining);
        if (floor) return floor;
      }
    }
  }

  return { ok: true };
}

/**
 * 判定某个分区在其文件系统下是否低于下限。
 *
 * @param {number} index 分区下标（0 起）
 * @param {object} p 分区行
 * @param {{filesystem?: string}} context 上下文（`filesystem` 为全局默认）
 * @param {number} sizeBytes 该分区实际会得到的字节数
 * @returns {{ok: false, message: string, detail: string}|null}
 */
function floorProblem(index, p, context, sizeBytes) {
  const filesystem = p.filesystem || (context && context.filesystem) || '';
  const minimum = minPartitionBytes(filesystem);
  if (minimum === null || sizeBytes >= minimum) return null;

  return {
    ok: false,
    message: `第 ${index + 1} 个分区的容量低于 ${filesystem} 下限`,
    detail:
      `${filesystem} 至少需要 ${formatBytes(minimum)}，` +
      `该分区只有 ${formatBytes(sizeBytes)}。` +
      `请增大容量、改用更小的文件系统（如 exFAT），或把该分区设为「不格式化」。`,
  };
}

/**
 * 把分区列表编译为后端契约的字段数组。
 *
 * 同时供 `buildRestCall` 与 `buildCliArgs` 使用，避免两条通道各写一份。
 *
 * @param {Array<{sizeBytes: number, gptType: string, mbrType: string,
 *   name: string, filesystem: string, kind: string}>} partitions
 * @param {string} layout 当前布局（决定下发哪一套类型）
 * @returns {Array<object>}
 */
export function normalizePartitions(partitions, layout) {
  const list = Array.isArray(partitions) ? partitions : [];
  return list.map((p) => {
    const out = { size_bytes: Number.isFinite(p.sizeBytes) ? p.sizeBytes : 0 };

    // 只下发**当前布局生效**的那一套类型；另一套不下发，
    // 避免后端把它当作显式指定而在切换布局后产生意外结果。
    if (layout === 'mbr') {
      const wire = mbrTypeWire(p);
      if (wire) out.mbr_type = wire;
    } else {
      const wire = gptTypeWire(p);
      if (wire) out.gpt_type = wire;
    }

    // 归属只在 MBR 下有意义。**主分区不发字段**：缺省即主分区，让老请求的
    // 形状逐字节不变（这是回归底线，由测试钉住）。
    if (layout === 'mbr' && (p.kind === 'logical' || p.kind === 'extended')) {
      out.kind = p.kind;
    }

    if (typeof p.name === 'string' && p.name !== '') out.name = p.name;

    // 文件系统：空 = 用全局默认；`none` = 显式不格式化。
    if (typeof p.filesystem === 'string' && p.filesystem !== '') {
      out.filesystem = p.filesystem;
    }

    // **扩展分区容器最后覆盖文件系统**：它没有数据区，任何文件系统都不该下发。
    // 必须放在上面那条通用规则**之后**——否则容器行残留的 `fat32` 会把它覆盖
    // 回来（实测过：顺序反了就会发出 `fat32`，后端拒绝）。
    if (layout === 'mbr' && p.kind === 'extended') {
      out.filesystem = 'none';
    }
    return out;
  });
}

/**
 * 分区类型的解析结果。
 *
 * **三态而非 `string|null`**：`null` 同时表示"没填"与"填错了"会让
 * UI 把「用默认类型」误判成错误（实测确实如此：留空的类型被报成"不合法"）。
 *
 * @typedef {{kind: 'inherit'|'value'|'invalid', wire?: string}} TypeResolution
 */

/// 未指定：交由后端按布局与文件系统推断。
const TYPE_INHERIT = Object.freeze({ kind: 'inherit' });
/// 无法解析（用户填错了）。
const TYPE_INVALID = Object.freeze({ kind: 'invalid' });

/**
 * 解析分区行的 GPT 类型。
 *
 * @param {object} row
 * @returns {TypeResolution}
 */
export function resolveGptType(row) {
  const selected = row && typeof row.gptType === 'string' ? row.gptType : '';
  if (selected === '') return TYPE_INHERIT;
  if (selected === CUSTOM_TYPE_VALUE) {
    const wire = customTypeWire('gpt', row.gptTypeCustom);
    return wire === null ? TYPE_INVALID : { kind: 'value', wire };
  }
  return GPT_PARTITION_TYPES.includes(selected) ? { kind: 'value', wire: selected } : TYPE_INVALID;
}

/**
 * 解析分区行的 MBR 类型。
 *
 * @param {object} row
 * @returns {TypeResolution}
 */
export function resolveMbrType(row) {
  const selected = row && typeof row.mbrType === 'string' ? row.mbrType : '';
  if (selected === '') return TYPE_INHERIT;
  if (selected === CUSTOM_TYPE_VALUE) {
    const wire = customTypeWire('mbr', row.mbrTypeCustom);
    return wire === null ? TYPE_INVALID : { kind: 'value', wire };
  }
  return MBR_PARTITION_TYPES.includes(selected) ? { kind: 'value', wire: selected } : TYPE_INVALID;
}

/**
 * 由分区行得出 GPT 类型线格式名（`null` 表示继承默认）。
 *
 * 仅用于**构造请求**；需要区分"没填"与"填错"的场景请用
 * [`resolveGptType`]。
 *
 * @param {object} row
 * @returns {string|null}
 */
export function gptTypeWire(row) {
  const r = resolveGptType(row);
  return r.kind === 'value' ? r.wire : null;
}

/**
 * 由分区行得出 MBR 类型线格式名（`null` 表示继承默认）。
 *
 * @param {object} row
 * @returns {string|null}
 */
export function mbrTypeWire(row) {
  const r = resolveMbrType(row);
  return r.kind === 'value' ? r.wire : null;
}

// ---------------------------------------------------------------- 分区
//
// `GET /api/v1/image/partitions` 的响应（实测见 docs/webui.md 引用的后端实现）：
//
//   {"layout":"gpt","partitions":[{index,offset_bytes,size_bytes,start_lba,type_label}],
//    "default_index":1,"path":"…"}
//
// `partitions` 为空**不是错误**：那是无分区表的整盘镜像，应按整盘挂载。

/**
 * 后端分区类型标签（英文）→ 界面文案（中文）。
 *
 * 后端的分区类型标签是**面向 CLI/API 使用者的英文**（`docs/protocol.md`），
 * 而本界面是中文的。让英文原文直接进下拉框会中英混杂，因此在展示层翻译。
 *
 * 表中没有的标签**原样返回**：那是后端新加的类型（例如自定义 GUID 的
 * `GUID DEADBEEF…`），原样显示比丢弃信息更有用。
 *
 * @param {string} label
 * @returns {string}
 */
export function describeTypeLabel(label) {
  const table = {
    Extended: '扩展分区',
    'GPT protective': 'GPT 保护分区',
    'EFI System': 'EFI 系统分区',
    'Microsoft basic data': '基本数据分区',
    'Microsoft reserved': 'Microsoft 保留分区',
    'Linux filesystem': 'Linux 文件系统',
    'Linux swap': 'Linux swap',
    'Linux LVM': 'Linux LVM',
    NTFS: 'NTFS',
    'NTFS / exFAT': 'NTFS / exFAT',
    Linux: 'Linux',
    'macOS HFS': 'macOS HFS',
    FAT12: 'FAT12',
    FAT16: 'FAT16',
    FAT32: 'FAT32',
    'FAT32 (LBA)': 'FAT32 (LBA)',
  };
  if (typeof label !== 'string') return '';
  const trimmed = label.trim();
  if (Object.prototype.hasOwnProperty.call(table, trimmed)) return table[trimmed];
  // `Type 0x99` → `类型 0x99`；其余原样。
  const typed = /^Type (0x[0-9A-F]{2})$/.exec(trimmed);
  if (typed) return `类型 ${typed[1]}`;
  return trimmed;
}

/**
 * 单个分区 → 下拉项文案。
 *
 * @param {{index?: number, size_bytes?: number, type_label?: string}} entry
 * @returns {string}
 */
export function describePartition(entry) {
  if (!entry || typeof entry !== 'object') return '分区';

  // 扩展分区**容器**：它不是分区（不占内核序号、不可挂载），因此不能显示成
  // 「分区 0」——那会让用户以为存在一个序号为 0 的设备。空容器也要显示，
  // 否则用户建完一个预留用的扩展分区后回头看详情会发现它消失了。
  if (entry.kind === 'extended' || entry.index === 0) {
    const parts = ['扩展分区容器（内部可放逻辑分区）'];
    if (Number.isFinite(entry.size_bytes)) {
      parts.push(formatBytes(entry.size_bytes).replace('.0 ', ' '));
    }
    return parts.join(' · ');
  }

  const index = Number.isInteger(entry.index) ? entry.index : '?';
  // 逻辑分区必须标出来：它的序号从 5 起（主分区占 1–4），只显示序号会让用户
  // 以为中间缺了几个分区。同时提示它位于扩展分区内部——用户看不到容器本身
  // （容器不占序号、也不是可挂载分区）。
  const isLogical = entry.kind === 'logical' || (Number.isInteger(entry.index) && entry.index > 4);
  const parts = [isLogical ? `分区 ${index}（逻辑）` : `分区 ${index}`];
  // 去掉 `63.0 MiB` 里多余的 `.0`：分区容量是概览信息，`63 MiB` 更易读。
  if (Number.isFinite(entry.size_bytes)) parts.push(formatBytes(entry.size_bytes).replace('.0 ', ' '));
  const label = typeof entry.type_label === 'string' ? describeTypeLabel(entry.type_label) : '';
  if (label) parts.push(label);
  return parts.join(' · ');
}

/**
 * 分区扫描结果 → `<select>` 选项列表。
 *
 * 首项恒为「整盘」（`value: ''`），随后是各分区（`value` 为序号的字符串）。
 * 无分区表时只有整盘一项；否则默认选中 `default_index`。
 *
 * @param {{partitions?: Array<any>, default_index?: number|null}|null} scan
 * @returns {Array<{value: string, label: string, selected: boolean}>}
 */
export function buildPartitionOptions(scan) {
  const partitions =
    scan && Array.isArray(scan.partitions) ? scan.partitions.filter((p) => p && typeof p === 'object') : [];
  const defaultIndex = scan && Number.isInteger(scan.default_index) ? scan.default_index : null;

  // 无分区表：只提供整盘，且它必须是选中项（后端会按整盘处理）。
  const options = [{ value: '', label: '整盘（无分区表）', selected: partitions.length === 0 }];

  for (const entry of partitions) {
    if (!Number.isInteger(entry.index)) continue;
    options.push({
      value: String(entry.index),
      label: describePartition(entry),
      selected: defaultIndex !== null && entry.index === defaultIndex,
    });
  }

  // 有分区但 default_index 不在列表里（后端异常）：仍要有一个选中项，
  // 否则浏览器会静默选中第一项，与后端的默认行为不一致。
  if (!options.some((option) => option.selected)) options[0].selected = true;

  return options;
}

/**
 * 分区扫描结果 → 状态行摘要（`GPT · 1 个分区` / `无分区表（整盘）`）。
 *
 * @param {{layout?: string, partitions?: Array<any>}|null} scan
 * @returns {string}
 */
export function formatPartitionScan(scan) {
  if (!scan || typeof scan !== 'object') return '无分区表（整盘）';
  const partitions = Array.isArray(scan.partitions) ? scan.partitions : [];
  const layout = typeof scan.layout === 'string' ? scan.layout.toUpperCase() : '';
  if (partitions.length === 0) return '无分区表（整盘）';
  const head = layout === 'GPT' || layout === 'MBR' ? `${layout} · ` : '';
  return `${head}${partitions.length} 个分区`;
}
