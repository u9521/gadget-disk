// view-create.js —— 视图 2：创建镜像 + 分区编辑器。
//
// 从 app.js 的「视图 2：创建镜像」与「分区编辑器」两节原样拆出。分区编辑器的
// 状态只保存在内存里，每次增删都整块重绘——行数很少（MBR 最多 4 个主分区或
// 3 主 + 逻辑分区；GPT 上限 128），重绘比精细的 DOM diff 简单得多，也不会
// 出现「状态与界面不同步」。

import { sizeNote, formatBytes, parsePartitionSize, parseSizeInput, validateSize } from './pure/bytes.js';
import { buildCliArgs, buildRestCall, classifyBackendFailure, execProbeSucceeded, nextReconnectDelay, parseApiInfo, parseExecResult, restResultToExecResult, restUrl, shouldBlockActions } from './pure/channel.js';
import { describeFilesystem, describeGptPartitionType, describeInUse, describeLayout, describeMbrPartitionType, describeMode, messageForCode, modeWarning } from './pure/describe.js';
import { CUSTOM_TYPE_VALUE, FILESYSTEMS, MAX_PARTITIONS, MBR_MAX_EXTENDED, MBR_MAX_LOGICAL, MBR_MAX_PRIMARY, buildPartitionOptions, customTypeWire, defaultPartitionType, formatPartitionScan, gptTypeWire, layoutSupportsPartitionNames, layoutSupportsPartitions, mbrSlotUsage, mbrTypeWire, partitionKernelIndex, partitionTypePresets, validatePartitions } from './pure/partitions.js';
import { baseName, joinPath, parentPath, safeImageName, shellQuote } from './pure/paths.js';
import { INQUIRY_STRING_MAX, MAX_LUNS, SLOW_TASK_THRESHOLD_MS, buildImageOptions, describeFormattingSource, describeSlot, imageNameExists, mergeSlotRows, taskProgressLabel, validateIdentityField } from './pure/task.js';
import { toast } from './ksu.js';
import { $, failed, showError } from './dom.js';
import { callCli, callTool, DATA_DIR } from './backend.js';
import { runRefresh, runTask } from './task.js';
import { refreshImages } from './view-images.js';

// ---------------------------------------------------------------- 视图 2：创建镜像

/**
 * 查询可用空间（原始调用，不带状态行）。
 *
 * 与 `refreshAvailableSpace` 分开：`doCreate` 要在自己的忙碌态里复用它，
 * 不能再让内部的状态行把「正在创建镜像…」覆盖成「正在同步状态…」。
 *
 * @returns {Promise<number|null>} 可用字节数；查询失败返回 null
 */
export async function queryAvailableSpace() {
  const result = await callTool({ op: 'df', path: DATA_DIR });
  const target = $('create-avail');

  if (!result.ok) {
    target.textContent = '无法获取剩余存储空间（仍可尝试创建）。';
    return null;
  }

  const available = result.data.available_bytes;
  target.textContent = `可用空间：${formatBytes(available)}`;
  return available;
}

/** 查询可用空间并更新提示（带状态行）。 */
export async function refreshAvailableSpace() {
  return runRefresh('create', queryAvailableSpace);
}

// ---------------------------------------------------------------- 分区编辑器
//
// 状态只保存在内存里，每次增删都整块重绘——分区行数很少（MBR 最多 4 个主
// 分区，或 3 主 + 逻辑分区；GPT 上限 128），
// 重绘比做精细的 DOM diff 简单得多，也不会出现"状态与界面不同步"。

/** 当前编辑中的分区列表。 */
export let createPartitions = [];

/** 当前已知的镜像列表（用于同名预检）。 */
export let knownImages = [];

/**
 * 记录已知镜像（供同名预检复用，避免为了预检多跑一次 `list`）。
 *
 * 用函数而不是让调用方直接赋值：`knownImages` 是跨模块的**活绑定**，
 * 其他模块 import 到的是只读视图，赋值会抛 `TypeError: Assignment to constant variable`。
 */
export function setKnownImages(images) {
  knownImages = images;
}

/** 上一次探测到的 mkfs 结果（来自 capabilities）。 */
export let mkfsProbes = [];

/** 新建一个分区行的初始状态。 */
export function defaultPartitionRow() {
  return {
    // 容量以**用户输入的文本**为准，不预先转字节：`0` 的"占满剩余空间"
    // 语义与单位后缀都由提交时统一解析，避免"解析失败 → 哨兵值 → 再解析"
    // 这条已经出过错的链路。
    sizeText: '0',
    gptType: '',
    gptTypeCustom: '',
    mbrType: '',
    mbrTypeCustom: '',
    name: '',
    // 空串 = 继承全局默认；`none` = 不格式化。
    filesystem: '',
    // `primary`（默认）/ `logical` / `extended`。仅 MBR 有意义：逻辑分区写进
    // EBR 链、序号从 5 起；扩展分区容器占一个槽位但不占序号、没有数据区。
    kind: 'primary',
  };
}

/**
 * 读取界面上某一行分区的输入。
 *
 * **字段名必须与 [`defaultPartitionRow`] 完全一致**——早先渲染读 `sizeText`
 * 而这里写 `sizeBytes`，于是重绘后容量框显示 `undefined`。`structure.test.mjs`
 * 有断言守住这一点。
 */
export function readPartitionRow(index) {
  const size = $(`partition-size-${index}`);
  const gpt = $(`partition-gpttype-${index}`);
  const gptCustom = $(`partition-gptguid-${index}`);
  const mbr = $(`partition-mbrtype-${index}`);
  const mbrCustom = $(`partition-mbrbyte-${index}`);
  const name = $(`partition-name-${index}`);
  const fs = $(`partition-fs-${index}`);
  const kind = $(`partition-kind-${index}`);

  // **归属的回落值必须是该行原有的 kind**，不能硬写 `'primary'`。
  //
  // 容器行与逻辑分区行的字段更少（容器不渲染类型/文件系统选择器），而
  // `syncPartitionRows` 会在每次重绘前逐行读取。若读不到就把 kind 重置成
  // 主分区，用户改一个容量、行就悄悄变回主分区——这是真的会发生的静默错。
  const fallbackKind = (createPartitions[index] && createPartitions[index].kind) || 'primary';

  return {
    sizeText: size ? size.value : '0',
    gptType: gpt ? gpt.value : '',
    gptTypeCustom: gptCustom ? gptCustom.value : '',
    mbrType: mbr ? mbr.value : '',
    mbrTypeCustom: mbrCustom ? mbrCustom.value : '',
    name: name ? name.value : '',
    filesystem: fs ? fs.value : '',
    kind: kind ? kind.value : fallbackKind,
  };
}

/** 同步 DOM 输入回 `createPartitions`（在增删/提交前调用）。 */
export function syncPartitionRows() {
  createPartitions = createPartitions.map((_, i) => {
    // 已被移除的行读不到，保持原值以免丢失用户输入。
    const sizeEl = $(`partition-size-${i}`);
    if (!sizeEl) return createPartitions[i];
    return readPartitionRow(i);
  });
}

/** 构造一个带标签与控件的字段单元（标签必定在控件上方）。 */
export function fieldCell(labelText, control, id) {
  const cell = document.createElement('div');
  cell.className = 'field';
  const label = document.createElement('label');
  label.textContent = labelText;
  label.setAttribute('for', id);
  cell.append(label, control);
  return cell;
}

/** 构造一个下拉框。 */
export function buildSelect(id, options, selected) {
  const select = document.createElement('select');
  select.id = id;
  for (const option of options) {
    const node = document.createElement('option');
    node.value = option.value;
    node.textContent = option.label;
    select.appendChild(node);
  }
  select.value = selected;
  return select;
}

/**
 * 重绘分区编辑器。
 *
 * 每个分区是一个 CSS Grid 容器，字段以 `.field` 单元排布——标签与控件处在
 * 同一个单元里，因此**宽屏下标签会稳定地待在自己控件上方**，不会像早先那样
 * 因为 flex-wrap 按可用宽度重排而让标签夹在别的控件之间。
 */
export function renderPartitions() {
  const layout = $('create-layout').value;
  const container = $('create-partitions');
  const section = $('create-partitions-section');
  if (!container || !section) return;

  // raw 没有分区表，不展示分区编辑器。
  if (!layoutSupportsPartitions(layout)) {
    section.style.display = 'none';
    return;
  }
  section.style.display = '';

  const max = MAX_PARTITIONS[layout] || 1;
  const namesAllowed = layoutSupportsPartitionNames(layout);
  const usage = layout === 'mbr' ? mbrSlotUsage(createPartitions) : null;

  const note = $('create-partitions-note');
  if (note) {
    if (layout === 'mbr') {
      // 提示必须反映**实际**的槽位规则，而不是笼统的"最多 N 个"。
      // MBR 的合法组合不止一种，用户需要知道当前处于哪一种、还能加几个。
      const parts = [`${usage.primaries} 个主分区`];
      if (usage.usesExtended) {
        parts.push(
          usage.logicals > 0
            ? `1 个扩展分区容器（容纳 ${usage.logicals} 个逻辑分区）`
            : '1 个扩展分区容器（空，可在里面添加逻辑分区）',
        );
      }
      note.textContent =
        `MBR 首扇区有 ${MBR_MAX_PRIMARY} 个分区项：${parts.join(' + ')}，` +
        `已用 ${usage.neededSlots} / ${MBR_MAX_PRIMARY}。` +
        (usage.usesExtended
          ? `逻辑分区写在扩展分区内部，序号从 5 起；容器本身不占序号。`
          : `再添加分区时选「逻辑分区」或「扩展分区」即可突破 4 个的限制。`) +
        `容量填 0 表示占满剩余空间（最多一处）。`;
    } else {
      // GPT：名称会写入镜像，因此提示只需覆盖容量规则与数量上限。
      note.textContent = `最多 ${max} 个分区。容量填 0 表示占满剩余空间（最多一个）。`;
    }
  }

  // 分区被删除后若索引越界，收拢到合法范围。
  //
  // **上限对容器要放宽一个**：容器不是分区，不占 `MAX_PARTITIONS` 的名额，
  // 但它确实占一行。用 `max` 截断会把「3 主 + 1 容器 + 64 逻辑」的最后一行
  // （正好是容器或某个逻辑分区）悄悄丢掉。
  const rowLimit = layout === 'mbr' ? max + MBR_MAX_EXTENDED : max;
  if (createPartitions.length > rowLimit) {
    createPartitions = createPartitions.slice(0, rowLimit);
  }
  if (createPartitions.length === 0) {
    createPartitions = [defaultPartitionRow()];
  }

  container.innerHTML = '';

  // ---- 渲染顺序：主分区 → 扩展容器 → 逻辑分区 ----
  //
  // 这与内核槽位顺序一致（主分区占 1–4，容器紧随其后），也让「逻辑分区在
  // 扩展分区内部」这层嵌套能靠缩进表达出来。
  //
  // **但 DOM id 一律用行在 `createPartitions` 里的原始下标**：`readPartitionRow`
  // 与 `syncPartitionRows` 都按原始下标读写，若这里改用渲染序号，重绘后每一行
  // 的输入都会串到别的行上。这是本函数最容易出错的地方，由测试钉住。
  const order = [];
  createPartitions.forEach((entry, index) => {
    if (layout !== 'mbr' || (entry.kind || 'primary') === 'primary') order.push(index);
  });
  if (layout === 'mbr') {
    createPartitions.forEach((entry, index) => {
      if ((entry.kind || 'primary') === 'extended') order.push(index);
    });
    createPartitions.forEach((entry, index) => {
      if ((entry.kind || 'primary') === 'logical') order.push(index);
    });
  }

  /** 逻辑分区行的容器：缩进 + 左边框，表达"位于扩展分区内部"。 */
  let logicalGroup = null;

  order.forEach((index) => {
    const entry = createPartitions[index];
    const row = document.createElement('div');
    row.className = 'partition-row';

    const kind = layout === 'mbr' ? entry.kind || 'primary' : 'primary';
    const ordinal = partitionKernelIndex(createPartitions, index, layout);

    // 行标题：窄屏下明确"这是第几个分区"，并给出**内核序号**与所属区域。
    //
    // 序号不是"第几行"：MBR 主分区占 1–4、逻辑分区从 5 起，且扩展分区容器
    // 本身不占序号。用户要靠这个序号对上 `loopNpM`，因此必须显示实际编号而不是
    // 行下标；逻辑分区还要标出它落在扩展分区内部（这是插入扩展容器的直观体现）。
    const heading = document.createElement('div');
    heading.className = 'partition-heading';

    if (kind === 'extended') {
      // 容器不占序号，因此绝不显示数字——显示"分区 0"会让用户以为存在一个
      // 序号为 0 的设备（后端把容器读回来时 index 正是 0）。
      const inside = mbrSlotUsage(createPartitions).logicals;
      heading.textContent =
        inside > 0
          ? `扩展分区容器（不占序号 · 内含 ${inside} 个逻辑分区）`
          : `扩展分区容器（不占序号 · 空，可在里面添加逻辑分区）`;
      heading.classList.add('partition-heading-extended');
    } else if (kind === 'logical') {
      heading.textContent = `分区 ${ordinal}（逻辑分区 · 位于扩展分区内）`;
      heading.classList.add('partition-heading-logical');
    } else {
      heading.textContent = `分区 ${ordinal}`;
    }
    row.appendChild(heading);

    // 逻辑分区放进缩进分组里，视觉上"在大扩展里面"。
    if (kind === 'logical') {
      if (!logicalGroup) {
        logicalGroup = document.createElement('div');
        logicalGroup.className = 'partition-logical-group';
      }
      logicalGroup.appendChild(row);
    } else {
      // 容器行之后、逻辑分组之外：先把分组收尾，再放这一行。
      if (logicalGroup) {
        container.appendChild(logicalGroup);
        logicalGroup = null;
      }
      container.appendChild(row);
    }

    const fields = document.createElement('div');
    fields.className = 'partition-fields';

    // ---- 容量 ----
    const sizeInput = document.createElement('input');
    sizeInput.type = 'text';
    sizeInput.id = `partition-size-${index}`;
    sizeInput.value = entry.sizeText;
    sizeInput.autocomplete = 'off';
    sizeInput.placeholder = '填 0 占满剩余空间';
    sizeInput.addEventListener('change', () => {
      createPartitions[index] = readPartitionRow(index);
    });
    fields.appendChild(fieldCell('容量', sizeInput, sizeInput.id));

    // ---- 归属（仅 MBR：主分区 / 逻辑分区 / 扩展分区容器）----
    //
    // 逻辑分区与容器都不需要用户指定容器的位置——后端按它们的容量推导。
    // 用户只需表达"这一行是什么"。
    if (layout === 'mbr') {
      const kindSelect = buildSelect(
        `partition-kind-${index}`,
        [
          { value: 'primary', label: '主分区' },
          { value: 'logical', label: '逻辑分区' },
          { value: 'extended', label: '扩展分区（容器）' },
        ],
        entry.kind || 'primary',
      );
      kindSelect.addEventListener('change', () => {
        createPartitions[index] = readPartitionRow(index);
        // 归属改变会改变哪些字段该显示、以及后续行的序号，必须整表重绘。
        renderPartitions();
      });
      fields.appendChild(fieldCell('归属', kindSelect, kindSelect.id));
    }

    if (kind === 'extended') {
      // 容器没有数据区：**不渲染类型与文件系统选择器**。
      //
      // 渲染成禁用的控件比不渲染更糟：用户会以为自己可以填、只是被挡住了。
      // 容器能填的只有容量（预留多大空间）。
      const hint = document.createElement('div');
      hint.className = 'field partition-container-hint';
      const label = document.createElement('label');
      label.textContent = '说明';
      const text = document.createElement('div');
      text.className = 'muted';
      text.textContent = '扩展分区容器由系统标记为 0x05 类型，用于容纳逻辑分区，不可直接格式化。';
      hint.append(label, text);
      fields.appendChild(hint);
    } else {
      // ---- 类型（按布局二选一）----
      const presets = partitionTypePresets(layout).map((value) => ({
        value,
        label:
          layout === 'mbr'
            ? describeMbrPartitionType(value)
            : describeGptPartitionType(value),
      }));
      presets.push({ value: CUSTOM_TYPE_VALUE, label: '自定义…' });

      if (layout === 'mbr') {
        const selected = entry.mbrType || defaultPartitionType('mbr', 'fat32');
        const select = buildSelect(
          `partition-mbrtype-${index}`,
          presets,
          selected === CUSTOM_TYPE_VALUE ? CUSTOM_TYPE_VALUE : selected,
        );
        select.addEventListener('change', () => {
          createPartitions[index] = readPartitionRow(index);
          renderPartitions();
        });
        fields.appendChild(fieldCell('类型（MBR）', select, select.id));

        if (select.value === CUSTOM_TYPE_VALUE) {
          const hex = document.createElement('input');
          hex.type = 'text';
          hex.id = `partition-mbrbyte-${index}`;
          hex.value = entry.mbrTypeCustom;
          hex.placeholder = '十六进制，如 1A';
          hex.maxLength = 4;
          hex.autocomplete = 'off';
          hex.addEventListener('change', () => {
            createPartitions[index] = readPartitionRow(index);
          });
          fields.appendChild(fieldCell('类型字节', hex, hex.id));
        }
      } else {
        const selected = entry.gptType || defaultPartitionType('gpt', 'fat32');
        const select = buildSelect(
          `partition-gpttype-${index}`,
          presets,
          selected === CUSTOM_TYPE_VALUE ? CUSTOM_TYPE_VALUE : selected,
        );
        select.addEventListener('change', () => {
          createPartitions[index] = readPartitionRow(index);
          renderPartitions();
        });
        fields.appendChild(fieldCell('类型（GPT）', select, select.id));

        if (select.value === CUSTOM_TYPE_VALUE) {
          const guid = document.createElement('input');
          guid.type = 'text';
          guid.id = `partition-gptguid-${index}`;
          guid.value = entry.gptTypeCustom;
          guid.placeholder = '12345678-9ABC-DEF0-1234-56789ABCDEF0';
          guid.autocomplete = 'off';
          guid.addEventListener('change', () => {
            createPartitions[index] = readPartitionRow(index);
          });
          fields.appendChild(fieldCell('类型 GUID', guid, guid.id));
        }
      }

      // ---- 文件系统（每分区独立）----
      const fsOptions = [
        { value: '', label: '默认（随上方全局设置）' },
        { value: 'fat32', label: describeFilesystem('fat32') },
        { value: 'exfat', label: describeFilesystem('exfat') },
        { value: 'ext4', label: describeFilesystem('ext4') },
        { value: 'none', label: describeFilesystem('none') },
      ];
      const fsSelect = buildSelect(`partition-fs-${index}`, fsOptions, entry.filesystem);
      fsSelect.addEventListener('change', () => {
        createPartitions[index] = readPartitionRow(index);
        renderFormattingSource();
      });
      fields.appendChild(fieldCell('文件系统', fsSelect, fsSelect.id));
    }

    // ---- 名称（仅 GPT 渲染）----
    //
    // **MBR 下整个字段不渲染**：MBR 首扇区的分区项里没有放名字的地方，填了也
    // 不会生效。渲染成禁用输入框仍会让人以为自己可以填、只是被挡住了，
    // 因此直接不出现——这与容器不渲染类型选择器是同一条原则。
    if (namesAllowed) {
      const nameInput = document.createElement('input');
      nameInput.type = 'text';
      nameInput.id = `partition-name-${index}`;
      nameInput.value = entry.name;
      nameInput.autocomplete = 'off';
      nameInput.maxLength = 36;
      nameInput.addEventListener('change', () => {
        createPartitions[index] = readPartitionRow(index);
      });
      fields.appendChild(fieldCell('名称', nameInput, nameInput.id));
    }

    row.appendChild(fields);

    // ---- 删除 ----
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'action partition-remove';
    remove.textContent = '删除';
    remove.disabled = createPartitions.length <= 1;
    remove.addEventListener('click', () => {
      syncPartitionRows();
      createPartitions.splice(index, 1);
      renderPartitions();
    });
    row.appendChild(remove);
  });

  // 收尾：最后一段逻辑分组（如果有）还没挂上去。
  if (logicalGroup) {
    container.appendChild(logicalGroup);
    logicalGroup = null;
  }

  const addButton = $('btn-add-partition');
  if (addButton) {
    if (layout === 'mbr') {
      // 添加按钮要反映**槽位规则**，而不是分区总数上限。
      //
      // 几种 MBR 组合的"还能不能加"完全不同：
      // - 主分区未满 4 个：还能加（主分区、逻辑分区或容器都行）；
      // - 主分区已满 4 个：**加主分区不行，但加逻辑分区可以**——逻辑分区需要
      //   一个槽位作扩展容器，此时要先把一个主分区改成逻辑分区。
      //
      // 因此这里不在 4 个主分区时禁用按钮（那会让用户以为无法再添加），
      // 而是在达到总数上限或逻辑分区上限时才禁用；具体组合的合法性由
      // `validatePartitions` 在提交时给出可操作的报错。
      const atTotal = usage && usage.countsAsPartition >= max;
      const atLogicalCap = usage && usage.logicals >= MBR_MAX_LOGICAL;
      addButton.disabled = Boolean(atTotal || atLogicalCap);

      if (atTotal) {
        addButton.title = `已达 MBR 分区总数上限（${max} 个，扩展容器不计入）。`;
      } else if (atLogicalCap) {
        addButton.title = `逻辑分区已达上限（${MBR_MAX_LOGICAL} 个）。`;
      } else if (usage && usage.primaries >= MBR_MAX_PRIMARY && !usage.usesExtended) {
        addButton.title =
          `已用满 ${MBR_MAX_PRIMARY} 个主分区槽位。新增分区请选「逻辑分区」，` +
          `并把其中一个主分区改为逻辑分区（扩展分区容器需要占一个槽位）。`;
      } else {
        addButton.title = '';
      }
    } else {
      addButton.disabled = createPartitions.length >= max;
      addButton.title = '';
    }
  }
}

/** 重新读取创建面板的布局相关控件（布局/文件系统变化时调用）。 */
export function refreshCreateForm() {
  syncPartitionRows();
  renderPartitions();
  renderFormattingSource();
}

/**
 * 按探测结果显示当前文件系统的格式化来源。
 *
 * 分区各自可选文件系统，因此这里展示的是**全局默认**的来源；
 * 分区选择了具体文件系统时以 `describeFormattingSource` 的分区查询为准。
 */
export function renderFormattingSource() {
  const target = $('create-fs-source');
  if (!target) return;

  const parts = [];
  for (const fs of FILESYSTEMS) {
    parts.push(`${fs}：${describeFormattingSource(mkfsProbes, fs)}`);
  }
  // 一行一个文件系统（`#create-fs-source` 的 CSS 是 `white-space: pre-line`）。
  // 用 `；` 连成一长句时三种工具名会挤在一起，反而不好扫读。
  target.textContent = parts.join('\n');
}

/** 拉取能力探测（含 mkfs），失败不阻断创建。 */
export async function refreshMkfsProbes() {
  const result = await callCli({ op: 'capabilities' });
  if (result.ok && Array.isArray(result.data.mkfs)) {
    mkfsProbes = result.data.mkfs;
  }
  renderFormattingSource();
}

/**
 * 同名预检：目标名已存在时提示并禁用创建按钮。
 *
 * 后端也会拒绝（那是权威判定），这里只是让用户**立刻**看到原因。
 */
export function checkNameConflict() {
  const name = safeImageName($('create-name').value);
  const conflict = imageNameExists(name, knownImages);
  const banner = $('create-name-conflict');
  const button = $('btn-create');

  if (banner) {
    banner.style.display = conflict ? '' : 'none';
    banner.textContent = conflict ? `已存在同名镜像「${name}」，请改名或先删除它。` : '';
  }
  if (button) button.disabled = conflict;
  return conflict;
}

/** 创建镜像。 */
export async function doCreate() {
  const rawName = $('create-name').value;
  const name = safeImageName(rawName);
  if (!name) {
    showError({ message: '文件名不合法', detail: '请使用字母、数字、点、下划线或连字符。' });
    return;
  }

  // 前端预检同名；后端同样会拒绝，这里是快速反馈而不是唯一防线。
  if (checkNameConflict()) {
    showError({
      message: '同名镜像已存在',
      detail: `「${name}」已存在。请换一个文件名，或先在「镜像管理」中删除它。`,
    });
    return;
  }

  const requested = parseSizeInput($('create-size').value);
  if (requested === null) {
    showError({
      message: '容量格式无法识别',
      detail: '支持输入 64M、1G、4G、1024 等容量格式。',
    });
    return;
  }

  const layout = $('create-layout').value;
  const filesystem = $('create-filesystem').value;
  const label = $('create-label').value || 'GADGETDISK';
  const path = joinPath(`${DATA_DIR}/images`, name);

  syncPartitionRows();
  // 用 `parsePartitionSize`（接受 `0`）而不是 `parseSizeInput`（拒绝非正数）：
  // 分区容量填 `0` 是「占满剩余空间」的合法写法。
  const partitionsForValidation = createPartitions.map((entry) => {
    const parsed = parsePartitionSize(entry.sizeText);
    return {
      // 解析失败的哨兵：留给 validatePartitions 报"容量不合法"。
      sizeBytes: parsed === null ? -1 : parsed,
      gptType: entry.gptType,
      gptTypeCustom: entry.gptTypeCustom,
      mbrType: entry.mbrType,
      mbrTypeCustom: entry.mbrTypeCustom,
      name: entry.name,
      filesystem: entry.filesystem,
      kind: entry.kind || 'primary',
    };
  });

  // 可用空间查询也算进忙碌态：它同样是一次后端往返，否则点击后会有一段
  // 「没有任何变化」的窗口——正是本任务要消除的那种迟钝感。
  await runTask(
    '创建镜像',
    async () => {
      const available = await queryAvailableSpace();
      const check = validateSize(requested, available === null ? Infinity : available);
      if (!check.ok) {
        // 容量层的失败只有两种：容量非法或磁盘真的不够。**分区太小不在这里判**，
        // 由下面的 `validatePartitions` 带上行号与文件系统说明（早先它被误报成
        // "空间不足"，是本次修复的缺陷）。
        showError({ message: check.message });
        return;
      }

      // 分区校验放在容量对齐之后：用的是**对齐后**的实际镜像容量。
      const partitionCheck = validatePartitions(partitionsForValidation, {
        layout,
        filesystem,
        imageBytes: check.alignedBytes,
      });
      if (!partitionCheck.ok) {
        showError({
          message: partitionCheck.message,
          detail: partitionCheck.detail || '',
        });
        return;
      }

      const result = await callCli({
        op: 'create',
        path,
        sizeBytes: check.alignedBytes,
        layout,
        filesystem,
        label,
        // raw 布局没有分区表，不发送 partitions，交给后端按整盘处理。
        partitions: layoutSupportsPartitions(layout) ? partitionsForValidation : [],
      });
      if (failed(result)) return;

      toast('镜像已创建');
      // 只刷新镜像列表，**不跳到挂载页**：创建与挂载是两件事，跨视图自动改
      // 另一个页面的表单状态会让用户分不清自己刚才操作的是哪个视图。新镜像
      // 会出现在镜像管理与挂载页的镜像下拉框里，用户自己选。
      await refreshImages();
    },
    { scope: 'create' },
  );
}