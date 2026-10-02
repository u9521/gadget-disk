// structure.test.mjs —— WebUI 静态结构的回归测试。
//
// 零构建约束下没有前端框架，但「六视图齐备、无外部 CDN、零构建」这些
// 验收项（docs/webui.md、docs/roadmap.md M4）都可以用文本断言守住，
// 避免后续改动悄悄破坏它们。
//
// **本轮重构后的形态**：`app.js` / `logic.js` 两个大文件已删除，前端拆成
// webui/ 下的**扁平一组**原生 ES 模块（入口 `main.js`），纯函数层移到
// `pure/`。因此本文件不再按「哪个文件」断言，而是：
//
//   - `allJs`  —— 全部模块源码的拼接（「前端某处做了某事」用这个）；
//   - `modules`—— 相对路径 → 源码（「某个模块做了某事」用这个）；
//   - 模块**链接期**检查：每个具名 import 都必须在目标模块里真实导出。
//
// 不变量与理由见 docs/webui.md「前端模块拆分」。
//
//   node --test tests/structure.test.mjs

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync, readdirSync, statSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join, relative, resolve, sep } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const WEBUI = join(here, '..');
const TESTS_DIR = here;

const fail = (message) => assert.fail(message);

// ---------------------------------------------------------------- 模块枚举

/** 本测试**不**扫描的目录（相对 `webui/`）。 */
const SKIP_DIRS = new Set(['tests']);

/**
 * 递归收集 `webui/` 下所有 `.js` 文件，返回 `相对路径 → 源码`。
 *
 * 相对路径统一用 `/` 分隔（Windows 上也是），这样断言里的字面量
 * （`'pure/bytes.js'`）在任何平台上都一致。
 *
 * @returns {Map<string, string>}
 */
function collectModules(root) {
  const found = new Map();
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) {
        if (dir === root && SKIP_DIRS.has(entry.name)) continue;
        walk(path);
        continue;
      }
      if (!entry.isFile() || !entry.name.endsWith('.js')) continue;
      found.set(relative(root, path).split(sep).join('/'), readFileSync(path, 'utf8'));
    }
  };
  walk(root);
  return new Map([...found].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}

/** `相对路径 → 绝对路径`（供 `node --check` 与模块 import 使用）。 */
function absPath(relativePath) {
  return join(WEBUI, ...relativePath.split('/'));
}

const modules = collectModules(WEBUI);
const moduleNames = [...modules.keys()];

/**
 * 全部模块源码的拼接。
 *
 * 前端是**一组**模块，因此「某处做了某事」这类断言必须扫全部源码，否则把函数
 * 搬个文件就会假失败（或更糟：把断言悄悄删掉）。
 */
const allJs = moduleNames.map((name) => modules.get(name)).join('\n');

const html = readFileSync(join(WEBUI, 'index.html'), 'utf8');
const ksuJs = modules.get('ksu.js');
const css = readFileSync(join(WEBUI, 'style.css'), 'utf8');

/** 取出某个模块的源码；模块缺失时给出可读的失败信息而不是 undefined。 */
function moduleSource(name) {
  const source = modules.get(name);
  assert.ok(source !== undefined, `缺少模块 ${name}（webui/ 下不存在该 .js 文件）`);
  return source;
}

/** 取出一个函数体（`function name(...)` 或 `async function name(...)`）。 */
function functionBody(source, name) {
  const match = source.match(new RegExp(`(?:async\\s+)?function\\s+${name}\\s*\\([\\s\\S]*?\\n\\}`));
  assert.ok(match, `应能定位函数 ${name}`);
  return match[0];
}

// ---------------------------------------------------------------- 元素引用一致性

test('每个字面 $(id) 引用的元素都真实存在于 index.html', () => {
  // 实测缺陷：精简「USB 设备身份」卡片时删掉了 `#cfg-note` 元素，却漏了
  // view-settings.js 里对它的 `textContent` 赋值。症状是**一打开设置页就抛**
  // `TypeError: Cannot set properties of null`，整页刷新失败，而其余视图正常——
  // 很容易被误读成"后端有问题"。
  //
  // 为什么之前的检查漏掉了：另外两条测试只覆盖「导入的模块/名字是否存在」与
  // 「被调用的标识符是否已定义」，都不涉及"这个 id 在 HTML 里有没有"。
  const ids = new Set(
    [...html.matchAll(/\sid="([A-Za-z0-9_-]+)"/g)].map((match) => match[1]),
  );
  assert.ok(ids.size > 50, `应收集到足够多的 id，实际 ${ids.size}`);

  const offenders = [];
  for (const [name, source] of modules) {
    for (const match of source.matchAll(/\$\(\s*'([A-Za-z0-9_-]+)'\s*\)/g)) {
      const id = match[1];
      if (!ids.has(id)) offenders.push(`${name} 引用了不存在的 id: ${id}`);
    }
  }
  assert.deepEqual(offenders, [], `index.html 中不存在的元素被引用：\n  ${offenders.join('\n  ')}`);
});

test('格式化来源是多行文本，且有匹配的 white-space 规则', () => {
  // `renderFormattingSource` 每种文件系统输出一行。前端把它拼成 `\n` 分隔的
  // 字符串——**必须**配套 `white-space: pre-line`，否则 HTML 会把换行折叠成空格，
  // 三种工具名挤成一长句（"看起来只是没换行"，很容易被忽略）。
  const source = moduleSource('view-create.js');
  assert.match(
    source,
    /parts\.join\('\\n'\)/,
    '格式化来源应按行拼接（join(\'\\n\')）',
  );
  assert.match(
    css,
    /#create-fs-source\s*\{[^}]*white-space:\s*pre-line/,
    '多行文本必须有 white-space: pre-line，否则换行会被折叠',
  );
});

// ---------------------------------------------------------------- 文件组成

test('入口与基础设施文件齐备（docs/webui.md 的文件组成）', () => {
  // 重构后前端是一组扁平模块：入口 main.js，基础设施 backend/dom/task，
  // 纯函数层在 pure/，视图在 view-*.js。
  for (const name of [
    'index.html',
    'style.css',
    'ksu.js',
    'main.js',
    'backend.js',
    'dom.js',
    'task.js',
  ]) {
    assert.ok(existsSync(join(WEBUI, name)), `缺少 ${name}`);
  }
});

test('入口必须是 webroot/index.html', () => {
  // KernelSU 常量 MODULE_WEB_DIR = "webroot"。
  assert.ok(existsSync(join(WEBUI, 'index.html')));
});

test('index.html 必须加载 main.js 作为入口', () => {
  // 若 HTML 指向不存在的脚本，模块加载会 404——界面永远停在「正在连接后端…」，
  // 而所有源码断言都照过（它们扫的是 webui/ 下的文件，不是 HTML 的引用）。
  assert.match(
    html,
    /<script[^>]*type="module"[^>]*src="main\.js"/,
    'index.html 必须以 module 方式加载 main.js',
  );
  assert.ok(existsSync(join(WEBUI, 'main.js')), 'main.js 必须存在');
});

test('模块必须直接放在 webui/ 顶层的纯函数层位置（扁平布局约束）', () => {
  // **打包脚本约束**：`webui_sources()` 只非递归扫描 webui/ 顶层文件，外加
  // 显式登记的 pure/ 子目录（`WEBUI_SUBDIRS`）。因此除 pure/ 之外再出现任何
  // 子目录，里面的模块**不会进包**——本机测试用的是源目录，一切正常，只有在
  // 设备上才会「模块不存在」白屏。这条断言把该失败模式提前到本机。
  const dirs = readdirSync(WEBUI, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();
  assert.deepEqual(
    dirs,
    ['pure', 'tests'],
    'webui/ 下只允许 pure/ 与 tests/ 两个子目录（其它子目录里的模块不会被打包）',
  );
});

// ---------------------------------------------------------------- 零构建约束

test('不引用任何外部 CDN 资源', () => {
  const sources = { 'index.html': html, 'style.css': css, ...Object.fromEntries(modules) };
  for (const [name, text] of Object.entries(sources)) {
    // 允许 http(s) 出现在注释/文档字符串中的说明，但不得出现在 src/href/import 里。
    const active = text
      .split('\n')
      .filter((line) => !/^\s*(\/\/|\*|<!--)/.test(line))
      .join('\n');
    assert.doesNotMatch(
      active,
      /(?:src|href)\s*=\s*["']https?:\/\//i,
      `${name} 引用了外部资源`,
    );
    assert.doesNotMatch(
      active,
      /@import\s+(?:url\()?["']?https?:/i,
      `${name} 通过 CSS @import 引用外部资源`,
    );
    assert.doesNotMatch(
      active,
      /from\s+["']https?:\/\//i,
      `${name} 从外部 URL 导入模块`,
    );
  }
});

test('不使用打包器产物特征（无 node_modules / 压缩包引用）', () => {
  for (const [name, text] of [['index.html', html], ...modules]) {
    assert.doesNotMatch(text, /node_modules/, `${name} 出现了 node_modules`);
    assert.doesNotMatch(text, /\.min\.js/, `${name} 引用了压缩产物`);
  }
});

test('每个模块的相对导入目标都必须真实存在（含 pure/ 下的相对解析）', () => {
  // **逐个模块**检查：模块之间互相 import，任何一个路径写错（例如 pure/ 下写
  // `./bytes.js` 却应是 `./pure/bytes.js`）都会让整条模块图加载失败 → 界面停在
  // 「正在连接后端…」。
  let total = 0;
  for (const [name, source] of modules) {
    const specs = [...source.matchAll(/from\s+['"]([^'"]+)['"]/g)].map((m) => m[1]);
    // 动态 import 也要在盘上存在。
    specs.push(...[...source.matchAll(/import\(\s*['"]([^'"]+)['"]\s*\)/g)].map((m) => m[1]));
    for (const spec of specs) {
      total += 1;
      assert.match(
        spec,
        /^\.{1,2}\//,
        `${name} 的导入必须是相对路径（./ 或 ../），实际为 ${spec}`,
      );
      const target = resolve(dirname(absPath(name)), spec);
      assert.ok(
        target.startsWith(WEBUI + sep) || target === WEBUI,
        `${name} 的导入 ${spec} 解析到了 webui/ 之外：${target}`,
      );
      assert.ok(existsSync(target), `${name} 的导入目标不存在：${spec}`);
    }
  }
  assert.ok(total > 0, '应至少存在一个模块导入');
});

// ---------------------------------------------------------------- 六视图

test('六个视图齐备且 tab 与 panel 一一对应', () => {
  const expected = ['mount', 'create', 'import', 'images', 'edit', 'settings'];

  const tabs = [...html.matchAll(/id="tab-([a-z]+)"/g)].map((m) => m[1]);
  const panels = [...html.matchAll(/id="panel-([a-z]+)"/g)].map((m) => m[1]);
  const controls = [...html.matchAll(/aria-controls="panel-([a-z]+)"/g)].map((m) => m[1]);

  assert.deepEqual(tabs.sort(), [...expected].sort(), 'tab 集合不符');
  assert.deepEqual(panels.sort(), [...expected].sort(), 'panel 集合不符');
  // aria-controls 必须指向真实存在的 panel（无障碍要求）。
  assert.deepEqual(controls.sort(), [...expected].sort(), 'aria-controls 集合不符');
});

test('每个 tabpanel 都有 role 与 aria-labelledby', () => {
  for (const name of ['mount', 'create', 'import', 'images', 'edit', 'settings']) {
    const panelRe = new RegExp(
      `<section role="tabpanel"[^>]*id="panel-${name}"[^>]*aria-labelledby="tab-${name}"`,
    );
    assert.match(html, panelRe, `panel-${name} 缺少 role 或 aria-labelledby`);
  }
});

test('六个视图在前端都有对应的刷新处理', () => {
  // 刷新分支由 task.js 的 refreshForTab 集中持有，视图代码则在各自的
  // view-*.js 里，因此「某处做了某事」要在全模块范围内断言。
  for (const name of ['mount', 'create', 'import', 'images', 'edit', 'settings']) {
    assert.match(allJs, new RegExp(`'${name}'`), `前端未处理视图 ${name}`);
  }
  // 视图模块本身也在（防止删模块后仅靠字符串仍然通过）。
  for (const name of ['mount', 'create', 'import', 'images', 'edit', 'settings']) {
    assert.ok(
      modules.has(`view-${name}.js`),
      `缺少 view-${name}.js`,
    );
  }
});

// ---------------------------------------------------------------- 三类异常兜底

test('前端消费 parseExecResult 的三类异常', () => {
  // 异常处理集中在 pure/channel.js 的 parseExecResult 中，通道层（backend.js）
  // 必须消费其结果。
  assert.match(allJs, /parseExecResult/);
  // 且必须有三类可见提示的处理（错误面板 + 原始输出面板）。
  assert.match(html, /id="error-panel"/);
  assert.match(html, /id="error-message"/);
  assert.match(html, /id="error-detail"/);
});

test('错误面板默认隐藏且不是仅靠颜色传达', () => {
  assert.match(html, /id="error-panel"[^>]*hidden/);
  // 状态标签必须带文字（CSS 中用 .tag + 文案）。
  assert.match(css, /\.tag/);
  assert.match(allJs, /textContent\s*=\s*'已生效'/);
});

// ---------------------------------------------------------------- 硬约束解释

test('UI 解释了硬约束而非只报错', () => {
  // 1. 改一个既有 LUN 不会断开 USB（内核允许在绑定状态下改），但**新增** LUN
  //    必须重建配置，因此会短暂断开重连。UI 要把这个区别说清楚，否则用户会
  //    以为「改一下也会掉盘」或「新增不会掉盘」。
  assert.match(html, /强制弹出/);
  assert.match(html, /不会断开 USB/);
  assert.match(html, /短暂断开/);

  // 2. 槽位模型：弹出只让槽位变空闲（参数保留），删除才让序号消失。
  assert.match(html, /槽位/);
  assert.match(html, /变空闲/);
  assert.match(html, /删除槽位/);
  assert.match(html, /全部卸载/);

  // 3. 双写会损坏文件系统，因此必须先卸载 USB 才能本地挂载。
  //
  // 文案由维护者精简过（不再解释"同一镜像会被两边写入"的机理），但**要求本身**
  // 必须在界面上可见——这是数据安全互斥约束，不能只靠后端报错。
  assert.match(html, /必须先卸载 USB/, '必须先卸载 USB 这条约束必须写在界面上');
});

test('挂载页给出挂载路径与访问建议，不承诺第三方文件管理器可见', () => {
  // 可见性取决于该 app 自身的 mount namespace，界面**不得承诺**自动可见；
  // 但必须给出确切路径与可操作的建议（用支持 root 权限的文件管理器）。
  assert.match(html, /挂载点位于/);
  assert.match(html, /\/data\/adb\/gadget-disk\/mnt\//);
  assert.match(html, /root 权限的文件管理器/);
});

test('稀疏文件行为有说明', () => {
  assert.match(html, /稀疏文件/);
});

// ---------------------------------------------------------------- 无障碍与尺寸

test('触控目标不小于 44px', () => {
  assert.match(css, /--touch:\s*44px/);
  // 主要交互元素都引用该变量。
  assert.match(css, /min-height:\s*var\(--touch\)/);
});

test('适配安全区（insets）', () => {
  assert.match(css, /env\(safe-area-inset-top\)/);
  assert.match(css, /env\(safe-area-inset-bottom\)/);
  assert.match(html, /viewport-fit=cover/);
});

// ---------------------------------------------------------------- ksu.js 契约

test('ksu.js 永不 reject（避免未捕获拒绝导致白屏）', async () => {
  // 会话中不存在 window：exec 应 resolve 一个 errno=-1 的结果而非 reject。
  const module = await import('../ksu.js');
  const result = await module.exec('anything');
  assert.equal(result.errno, -1);
  assert.match(result.stderr, /ksu API unavailable/);
});

test('ksu.js 使用 KernelSU 的回调式 exec 签名', () => {
  // 实测签名：ksu.exec(cmd, optionsJson, callbackName)，回调参数 (errno, stdout, stderr)。
  assert.match(ksuJs, /ksu\.exec\(/);
  assert.match(ksuJs, /JSON\.stringify/);
  assert.match(ksuJs, /callbackName/);
});

test('ksu.js 在回调触发后清理 window 上的临时函数', () => {
  // 防泄漏：每次调用都应在 settle 时删除回调。
  assert.match(ksuJs, /delete window\[callbackName\]/);
});

// ---------------------------------------------------------------- 二进制路径

test('前端优先使用安装后的扁平 bin/gadgetdisk', () => {
  // 二进制路径探测现在住在 backend.js（纯函数层 paths.js 只提供 joinPath）。
  const source = moduleSource('backend.js');
  // `customize.sh` 安装时把本机架构的二进制移到扁平的 `bin/gadgetdisk`，
  // 因此运行时首选它就是扁平路径。仍保留 `bin/<abi>/` 作为兼容项
  // （升级后未重装时旧布局还在）。
  assert.match(source, /bin\/gadgetdisk/, '应使用扁平路径');
  assert.match(source, /resolveBin/, '应有二进制路径探测函数');
  assert.match(source, /\[ -x /, '探测应检查可执行位而不只是存在');
  // 首选必须是扁平路径，而不是先遍历 ABI 目录。
  const flat = source.indexOf("joinPath(MODDIR, 'bin/gadgetdisk')");
  const abi = source.indexOf('bin/${abi}/gadgetdisk');
  assert.ok(flat > 0 && abi > 0, '两种布局都应出现在候选列表里');
  assert.ok(flat < abi, '扁平路径必须优先于 ABI 目录兼容项');
});

test('前端支持多槽位编辑器', () => {
  // 槽位编辑在 view-mount.js，常量校验在 pure/task.js。
  assert.match(allJs, /addDeviceRow/, '应有槽位行编辑器');
  assert.match(allJs, /collectDevices/, '应能从多行收集设备列表');
  assert.match(allJs, /doUnmountLun/, '应有弹出槽位');
  assert.match(allJs, /doDeleteSlot/, '应有删除槽位');
  assert.match(allJs, /renderSlots/, '应按后端状态重建槽位');
  assert.match(allJs, /MAX_LUNS/, '应遵守内核 LUN 上限');
  assert.match(allJs, /INQUIRY_STRING_MAX/, '应校验 INQUIRY 长度');
  // 旧的单镜像输入框已移除。
  assert.doesNotMatch(allJs, /\$\('mount-image'\)/, '不应再引用已删除的 mount-image');
  assert.doesNotMatch(html, /id="mount-image"/, 'index.html 不应再有单镜像输入框');
});

test('挂载页的镜像是下拉框而不是自由文本', () => {
  // 用户要求：WebUI 只允许选择镜像目录里面的镜像。自由文本输入做不到这一点
  // （用户能填任意路径），因此必须是 <select>，选项来自 GET /api/v1/images。
  assert.match(allJs, /buildImageOptions/, '应构造镜像选项');
  assert.match(allJs, /createElement\('select'\)/, '镜像字段应使用 select');
  assert.match(allJs, /availableImages/, '应缓存镜像列表');
  // 镜像字段必须是 select，不能是 input。
  assert.match(
    moduleSource('view-mount.js'),
    /const image = document\.createElement\('select'\)/,
    '镜像字段必须是 select',
  );
});

test('槽位行区分已挂载 / 空闲 / 未创建，并反映可删性', () => {
  const source = moduleSource('view-mount.js');
  assert.match(source, /describeSlot/, '应复用纯函数的槽位判定');
  assert.match(source, /state\.kind === 'mounted'/, '已挂载应显示弹出');
  assert.match(source, /state\.kind === 'idle'/, '空闲应可删除或说明为何不能删');
  assert.match(source, /state\.deletable/, '应依据后端的 deletable 判断');
  // lun.0 不能删时要把原因写在界面上，而不是让用户找不到按钮。
  assert.match(source, /只能弹出不能删除/, '应解释 lun.0 为何不能删');
});

test('前端与 index.html 都有 USB 身份配置界面', () => {
  for (const id of ['cfg-vid', 'cfg-pid', 'cfg-manufacturer', 'cfg-product', 'cfg-serial']) {
    assert.match(html, new RegExp(`id="${id}"`), `index.html 应有 ${id} 输入框`);
  }
  // 身份配置的处理函数现在住在 view-settings.js。
  const source = moduleSource('view-settings.js');
  assert.match(source, /saveIdentityConfig/, '应有保存身份的处理函数');
  assert.match(source, /loadIdentityConfig/, '应能读取当前身份');
});

// ---------------------------------------------------------------- 镜像 SE 标签

test('设置页可以查看与编辑镜像 SELinux 标签', () => {
  // 输入框、保存、恢复默认三件套。
  assert.match(html, /id="cfg-image-context"/, 'index.html 应有镜像上下文输入框');
  assert.match(
    html,
    /id="btn-image-context-save"[^>]*data-role="mutation"/,
    '保存按钮必须标记为变更类（离线时由状态机禁用）',
  );
  assert.match(
    html,
    /id="btn-image-context-reset"[^>]*data-role="mutation"/,
    '恢复默认同样是变更类操作',
  );
  assert.match(html, /id="cfg-image-context-note"/, '应有说明/状态元素');

  const source = moduleSource('view-settings.js');
  assert.match(source, /loadImageContextConfig/, '必须能读取当前标签');
  assert.match(source, /saveImageContextConfig/, '必须能保存标签');
  // 走独立的 op（不能复用身份的 config-set：那有覆盖身份的风险）。
  assert.match(source, /'config-security'/, '读取应走 config-security');
  assert.match(source, /'config-security-set'/, '写入应走 config-security-set');
  // 恢复默认必须是显式 reset，而不是「提交一个空输入框」。
  assert.match(source, /reset: true/, '恢复默认应发显式 reset');

  // main.js 必须把两个按钮接上（否则点了没反应，而只查 id 的断言不会发现）。
  const main = moduleSource('main.js');
  assert.match(main, /btn-image-context-save/, '保存按钮必须绑定事件');
  assert.match(main, /btn-image-context-reset/, '恢复默认按钮必须绑定事件');
});

test('镜像标签卡片说明作用范围与生效时机，而不是只给一个输入框', () => {
  // 文案由维护者精简过（不再解释内核线程/静默丢弃/读不出内容的机理），但三条
  // **可操作事实**必须留在界面上：作用范围、默认值、何时生效。缺任何一条，
  // 用户都不知道改这个框会发生什么。
  assert.match(html, /下一次挂载/, '必须说明改动何时生效');
  assert.match(html, /media_rw_data_file/, '必须给出默认值');
  assert.match(html, /挂载或格式化之前/, '必须说明模块在何时修正标签');
  // 说明必须紧邻输入框：用户不会去别的卡片里找原因。
  // 截出整张卡片（从标题到下一张卡片），而不是从输入框往后——说明在输入框**之前**。
  const cardStart = html.indexOf('镜像安全上下文');
  const cardEnd = html.indexOf('<div class="card">', cardStart);
  const card = html.slice(cardStart, cardEnd > 0 ? cardEnd : undefined);
  assert.ok(cardStart > 0 && card.length > 0, '应能找到镜像标签卡片');
  assert.match(card, /images\//, '必须说明只作用于 images/ 下的镜像');
});

test('本地编辑视图在挂载后显示安全上下文修正提示', () => {
  // 标签改不动时挂载可能仍然「成功」，但内核读不到内容——后果必须当面说清。
  assert.match(html, /id="loop-context-note"/, 'index.html 应有上下文提示元素');
  const source = moduleSource('view-edit.js');
  assert.match(source, /renderLoopContextNote/, '应有渲染上下文提示的函数');
  assert.match(source, /warnings/, '必须消费后端返回的 warnings');
  // 提示里要给出**可操作**的下一步，而不是只说失败。
  assert.match(source, /设置与诊断/, '提示应指向可修改标签的位置');
});

test('身份界面说明 VID/PID，并讲清生效时机', () => {
  // VID/PID 是缩写，必须解释给用户，而不是只给两个十六进制输入框。
  assert.match(html, /Vendor ID/, '应解释 VID 的含义');
  assert.match(html, /Product ID/, '应解释 PID 的含义');
  assert.match(html, /0x0000/, '应给出 VID/PID 的取值范围');

  // 保存身份**不断开 USB**，因此必须告诉用户「下次连接才生效」——否则用户会
  // 以为保存失败。
  assert.match(html, /重新枚举|重新连接/, '应说明身份在下次连接时才生效');

  // 真机观察：断开 USB 后 init 会重置 VID/PID。这会让用户以为设置丢了，
  // 因此必须写在界面上；界面据此建议「在断开 USB 的情况下保存」。
  assert.match(html, /init/, '应说明断开后 init 会重置 VID/PID');
  assert.match(html, /断开 ?USB/, '应给出「断开 USB 时保存」这一可操作建议');
});

test('前端显示「上次导出未兑现」并允许恢复', () => {
  // 未兑现提示在 view-mount.js。
  const source = moduleSource('view-mount.js');
  assert.match(source, /renderPendingIntent/, '应渲染未兑现提示');
  assert.match(source, /doResumeIntent/, '应能从记录恢复');
  assert.match(source, /doClearIntent/, '应能清除记录');
  assert.match(html, /pending-intent/, 'index.html 应有该区块');
});

test('前端不再直接使用未探测的 BIN 常量', () => {
  // 每次 exec 都必须先 resolveBin()，否则探测形同虚设。
  const execCalls = [...allJs.matchAll(/shellQuote\(BIN\)/g)];
  assert.equal(
    execCalls.length,
    0,
    '不应存在未经 resolveBin() 的 BIN 用法',
  );
});

// ---------------------------------------------------------------- 后端通道

test('前端不得把 exec 的结果当字符串处理', () => {
  // `ksu.exec` 永不 reject，且结果**永远是对象** {errno, stdout, stderr}。
  // 对它调字符串方法（如 `probe.trim()`）会抛 TypeError → `resolveBin()` 变成
  // 被拒绝的 Promise → `init()` 没有 `.catch()` → 状态行永远停在「正在连接
  // 后端…」、按钮全部静默失效。
  assert.doesNotMatch(allJs, /probe\.trim/, '不得再对 exec 结果调字符串方法');
  assert.match(allJs, /execProbeSucceeded/, '必须经纯函数判定 exec 结果');
  // 判定必须同时看 errno 与 stdout，且不能出现「把结果当字符串」的其它写法。
  assert.doesNotMatch(allJs, /await exec\([^)]*\)[^;]*\.trim\(/s);
});

test('前端走 IPv4 字面量而不是回环主机名', () => {
  // 实测：回环主机名解析到 IPv6 ::1，而 serve 只绑 IPv4 回环 → Failed to fetch。
  assert.doesNotMatch(allJs, /localhost/, '不得使用回环主机名');
  assert.match(allJs, /restUrl|REST_HOST/, 'REST 地址必须由纯函数给出');
});

test('前端有未捕获错误与未处理拒绝的全局兜底', () => {
  // 再出现一次「静默冻结」是可接受的失败模式吗？不是，所以必须有全局兜底。
  // 监听器住在 dom.js（模块顶层即注册）。
  const source = moduleSource('dom.js');
  assert.match(source, /addEventListener\(\s*'unhandledrejection'/);
  assert.match(source, /addEventListener\(\s*'error'/);
  // 首次加载的刷新必须各自兜底（并发发起，拒绝不会被同步 try/catch 捕获）。
  // 兜底助手是 dom.js 的 guard()，调用点在入口 main.js。
  assert.match(source, /export function guard\(/, '应有 guard 兜底助手');
  const entry = moduleSource('main.js');
  assert.match(entry, /guard\(\s*refreshStatus\(\)/);
  assert.match(entry, /guard\(\s*refreshAvailableSpace\(\)/);
  assert.match(entry, /guard\(\s*refreshLoop\(\)/);
  assert.match(entry, /guard\(\s*refreshCapabilities\(\)/);
});

test('前端保留 CLI 回退路径（REST 不可用时界面仍可用）', () => {
  // 通道层 backend.js 负责回退，参数编译在 pure/channel.js。
  const source = moduleSource('backend.js');
  assert.match(allJs, /buildCliArgs/, '回退路径必须复用同一份字段定义');
  assert.match(source, /runCli/, '必须有 CLI 回退实现');
  // 探测 api.json 必须用同源相对 URL 且禁用缓存。
  assert.match(source, /fetch\(\s*API_JSON_URL/);
  assert.match(source, /cache:\s*'no-store'/);
});

test('前端按「有没有 job_id」区分导入的两种响应形状', () => {
  // REST 通道：`serve` 会继续活着，收尾在后台线程里跑，响应带 `job_id`，
  // 需要轮询进度。
  // 降级通道：CLI 是一次性进程，跑完才返回，响应是**终态**且不带 `job_id`
  // （那个 id 指向的注册表随进程消失）。
  //
  // 曾无条件 `pollJob()`，于是在降级通道下界面会拿一个必然查不到的 id 去轮询，
  // 导入明明成功了却报错。
  //
  const source = moduleSource('view-import.js');
  const arm = source.match(/importState\.jobId\s*=[\s\S]*?\n\s*\}\n\s*\}\s*,\n\s*\{\s*scope:\s*'import'\s*\}/);
  assert.ok(arm, 'doImport 必须有明确的分支处理');
  // `job_id` 缺失必须落成 null。允许两种写法：直接取字段，或先判 `commit.data`
  // 存在再取（后者是后加的负载结构校验，见下一条测试）。
  assert.match(
    arm[0],
    /result\.data\.job_id\s*\|\|\s*null|commit\.data\.job_id\s*\|\|\s*null|\(commit\.data\s*&&\s*commit\.data\.job_id\)\s*\|\|\s*null/,
    'job_id 缺失必须落成 null',
  );
  assert.match(arm[0], /if\s*\(importState\.jobId\)[\s\S]*?pollJob\(\)/, '有 job_id 才轮询');
  assert.match(arm[0], /else[\s\S]*?settleJob\(/, '没有 job_id 时按终态收尾');
});

test('callRestRaw 必须返回规整后的结果，不能是原始 exec 形状', () => {
  // 实测缺陷：上传**成功**却报「后端返回了无法识别的失败结果」，
  // detail 里是 `{"errno":0,"stdout":"{\"bytes_done\":2303445}","stderr":""}`。
  //
  // 根因：`callRestRaw` 直接返回 `restResultToExecResult(...)` 的**原始 exec 形状**，
  // 而调用方 `failed(result)` 是按 `{ok, ...}` 判断成败的 —— `result.ok` 为
  // `undefined`（falsy），于是**成功被当成失败**。服务端其实已收下全部字节。
  //
  // 因此它必须像 `callBackend` 一样经过 `parseExecResult` 规整。
  const backend = moduleSource('backend.js');
  const fn = backend.match(/export async function callRestRaw\([\s\S]*?\n\}/);
  assert.ok(fn, 'callRestRaw 应可定位');
  assert.match(
    fn[0],
    /parseExecResult\(\s*restResultToExecResult\(/,
    'callRestRaw 必须把 HTTP 结果经 parseExecResult 规整后再返回',
  );
  // 不得再直接把原始 exec 形状交给调用方。
  assert.doesNotMatch(
    fn[0],
    /return\s+restResultToExecResult\(/,
    '不得直接返回原始 exec 形状（ok 为 undefined，会被当成失败）',
  );
});

test('失败结果必须带可读文案，不得显示「未知错误」', () => {
  // 实测缺陷：上传时报「出错了 / 未知错误」，控制台无输出（报告者原话）。
  // 根因是 `failed(result)` 直接透传后端结果，而 `showError` 在 `message` 为空时
  // 只回一句通用的「未知错误」——用户拿不到任何排查线索，我们自己也定位不了。
  //
  // 现在 `failed` 必须为缺失的 message 兜底，并把整个结果序列化进 detail。
  const dom = moduleSource('dom.js');
  const fn = dom.match(/export function failed\(result\)[\s\S]*?\n\}/);
  assert.ok(fn, 'failed() 应可定位');
  assert.match(
    fn[0],
    /message:\s*result\.message\s*\|\||result\.message\s*\?\?/,
    'failed() 必须为缺失的 message 兜底',
  );
  assert.match(fn[0], /JSON\.stringify\(result\)/, 'failed() 必须把结果序列化进 detail');
  // 不允许再出现「直接透传」的写法。
  assert.doesNotMatch(fn[0], /showError\(result\);/, '不得直接透传 result（会显示未知错误）');
});

test('上传响应缺少 upload_id 时给出明确原因，而不是 TypeError', () => {
  // 同一次缺陷的另一半：`begin.data.upload_id` 在 `ok:true` 但负载结构不符时
  // 会抛 TypeError，经 runTask 包装后只剩没有上下文的「导入失败」。
  const src = moduleSource('view-import.js');
  assert.match(src, /begin\.data\s*&&\s*begin\.data\.upload_id/, '取 upload_id 前必须判 data 存在');
  assert.match(src, /后端未返回上传会话 ID/, '缺少 upload_id 时必须给出明确提示');
  assert.match(src, /commit\.data\s*&&\s*commit\.data\.job_id/, '取 job_id 前必须判 data 存在');
});

test('pollJob 只轮询 REST 通道，且无 job 时立即返回', () => {
  // `job` 已从 CLI 删除（注册表是进程内内存），因此 `buildCliArgs` 对它返回
  // null——轮询只可能走 REST。
  const source = moduleSource('view-import.js');
  assert.match(source, /async function pollJob\(\)\s*\{\s*\n\s*if\s*\(!importState\.jobId\)\s*return;/);
  // 渲染与收尾必须抽成共用函数，否则轮询路径与一次性路径的文案会漂移。
  assert.match(source, /function renderJobStatus\(/, '进度渲染必须抽成共用函数');
  assert.match(source, /async function settleJob\(/, '终态收尾必须抽成共用函数');
  // 参数编译侧也要继续拒绝 `job` 的 CLI 形态。
  assert.match(moduleSource('pure/channel.js'), /case\s*'job':\s*\n\s*return null;/);
});

test('前端按需拉起 serve 时必须 setsid 且重定向三个标准流', () => {
  // 实测：KernelSU 会在 exec 的 shell 返回后回收进程组，普通 & 会被 SIGKILL；
  // 不重定向标准流则本次 exec 会一直等管道关闭而不返回。
  // 只看真正的命令字面量（`const spawn = ...`），注释里也提到 setsid，不能误判。
  const spawn = moduleSource('backend.js').match(/const spawn =[\s\S]*?&\s*`;/);
  assert.ok(spawn, '必须有 setsid 拉起的 serve 命令');
  assert.match(spawn[0], /setsid/);
  assert.match(spawn[0], />>\s*\$\{shellQuote\(SERVE_LOG\)\}/, 'stdout 必须重定向到日志');
  assert.match(spawn[0], /2>&1/, 'stderr 必须重定向');
  assert.match(spawn[0], /<\s*\/dev\/null/, 'stdin 必须断开');
});

test('不得调用未定义的标识符（未定义引用会让整页白屏）', () => {
  // 实测缺陷：清理「排查提示」卡片时顺手精简了 view-settings.js 的 import 块，
  // 把 `$` 一并删掉，而该文件有十几处 `$('id')` 调用。症状是**一打开设置页就报**
  // 「ReferenceError: $ is not defined」——而且只有那一个视图坏掉，其余正常。
  //
  // 为什么之前的检查漏掉了：ES 模块的**具名导入检查**（另一个测试）只验证
  // 「导入的名字确实被目标模块导出」，**不验证「用到的名字是否导入了」**；
  // `node --check` 只查语法，也发现不了。而按 `\b` 做词边界的正则匹配对 `$`
  // 无效（`$` 不是单词字符），于是漏报。
  //
  // 这里改为：收集每个模块**被调用**的标识符，与「已声明/已导入/浏览器全局」比对。
  const GLOBALS = new Set([
    'window', 'document', 'console', 'localStorage', 'fetch', 'setTimeout',
    'clearTimeout', 'setInterval', 'clearInterval', 'Promise', 'JSON', 'Object',
    'Array', 'Number', 'String', 'Boolean', 'Math', 'Date', 'Error', 'TypeError',
    'RangeError', 'RegExp', 'Map', 'Set', 'Symbol', 'BigInt', 'FileReader', 'File',
    'Blob', 'FormData', 'URL', 'AbortController', 'performance', 'queueMicrotask',
    'isNaN', 'parseInt', 'parseFloat', 'isFinite', 'undefined', 'NaN', 'Infinity',
    'globalThis', 'structuredClone', 'TextEncoder', 'TextDecoder', 'atob', 'btoa',
    'crypto', 'navigator', 'location', 'requestAnimationFrame',
    'encodeURIComponent', 'decodeURIComponent',
  ]);
  const KEYWORDS = new Set(['if', 'for', 'while', 'switch', 'catch', 'return',
    'typeof', 'new', 'function', 'await', 'else', 'do', 'in', 'of', 'case',
    'throw', 'delete', 'void', 'yield', 'super', 'this']);

  const offenders = [];
  for (const name of moduleNames) {
    const src = moduleSource(name);
    const declared = new Set();
    for (const m of src.matchAll(/import\s*\{([^}]*)\}\s*from/g)) {
      for (let n of m[1].split(',')) {
        n = n.trim().split(/\s+as\s+/).pop().trim();
        if (n) declared.add(n);
      }
    }
    for (const m of src.matchAll(/import\s+([A-Za-z_$][\w$]*)\s+from/g)) declared.add(m[1]);
    for (const m of src.matchAll(/(?:^|\n)\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)/g)) declared.add(m[1]);
    for (const m of src.matchAll(/(?:^|\n)\s*(?:export\s+)?(?:const|let|var|class)\s+([A-Za-z_$][\w$]*)/g)) declared.add(m[1]);

    // 去掉注释、字符串与**正则字面量**，避免把说明文字里的 `foo(` 当代码。
    //
    // 正则字面量必须单独剥掉：`/^Type (0x[0-9A-F]{2})$/` 里的 `Type (` 会被
    // 当成一次函数调用，于是本测试报出一个并不存在的 `Type()` 缺陷（实测踩到）。
    // 判据是「前一个非空白字符不是值或标识符」——那正是正则与除法的分界。
    const stripped = src
      .replace(/\/\*[\s\S]*?\*\//g, ' ')
      .replace(/\/\/[^\n]*/g, ' ')
      .replace(/'(?:\\.|[^'\\])*'/g, "''")
      .replace(/"(?:\\.|[^"\\])*"/g, '""')
      .replace(/`(?:\\.|[^`\\])*`/g, '``')
      .replace(/(^|[^.\w$)\]}])?\/(?:\\.|\[(?:\\.|[^\]\\])*\]|[^/\\\n])+\/[gimsuy]*/g, '$1 ');

    for (const m of stripped.matchAll(/(^|[^.\w$])([A-Za-z_$][\w$]*)\s*\(/g)) {
      const called = m[2];
      if (declared.has(called) || GLOBALS.has(called) || KEYWORDS.has(called)) continue;
      const esc = called.replace(/\$/g, '\\$');
      const isLocal = new RegExp(
        '(?:function|const|let|var|class)\\s+' + esc + '\\b',
      ).test(stripped);
      const looksLikeParam = new RegExp(
        '\\([^)]*\\b' + esc + '\\b[^)]*\\)\\s*(?:=>|\\{)',
      ).test(stripped);
      if (!isLocal && !looksLikeParam) offenders.push(`${name}: ${called}()`);
    }
  }

  assert.deepEqual(
    offenders,
    [],
    `以下模块调用了未定义/未导入的标识符（会在运行时报 ReferenceError，且只有那个视图坏掉）：\n  ${offenders.join('\n  ')}`,
  );
});

test('排查提示卡片已移除（用户要求：没有实际用途）', () => {
  // 该卡片曾展示 `dmesg | grep avc` 与「后端原始状态输出」按钮。
  // 用户判定其无用并移除；此处钉住移除结果，避免被无意恢复。
  //
  // 注意区分：**能力探测**与**关于**两张卡片仍在（它们有实际信息），
  // 被删的只有「排查提示」这一张。
  assert.doesNotMatch(html, /id="diag-raw"/, 'diag-raw 应已移除');
  assert.doesNotMatch(html, /id="btn-diag-status"/, '诊断按钮应已移除');
  assert.doesNotMatch(html, /排查提示/, '排查提示卡片应已移除');
  assert.doesNotMatch(allJs, /showRawStatus/, 'showRawStatus 应已移除');

  // 保留的卡片必须还在。
  assert.match(html, /id="caps-list"/, '能力探测应保留');
  assert.match(html, /id="diag-moddir"/, '关于（后端路径）应保留');
});

// ---------------------------------------------------------------- 任务反馈（Task 1）

test('每个视图都有 role="status" 的行内状态元素', () => {
  // 无障碍要求：进行中的文案必须是 live region，屏幕阅读器才能播报。
  const statuses = [...html.matchAll(/role="status"/g)];
  assert.ok(statuses.length >= 1, '至少需要一个 role="status" 元素');
  // 变更类操作分布在各视图，因此每个视图都应有自己的状态行。
  // （loop 视图的状态行沿用 loop-task-status；settings 页另有 config-task-status。）
  for (const id of [
    'mount-task-status',
    'create-task-status',
    'import-task-status',
    'images-task-status',
    'loop-task-status',
    'settings-task-status',
  ]) {
    assert.match(html, new RegExp(`id="${id}"[^>]*role="status"`), `缺少 ${id}`);
  }
});

test('前端用 runTask 包装变更类操作且清理发生在 finally 中', () => {
  // runTask 现在住在 task.js。
  const source = moduleSource('task.js');
  assert.match(source, /async function runTask\(/, '必须有 runTask 助手');
  // 变更类操作全部经 runTask（分布在各 view-*.js 中）。
  const taskCalls = [...allJs.matchAll(/await runTask\(/g)];
  assert.ok(taskCalls.length >= 7, `runTask 调用点应有 7 个以上，实际 ${taskCalls.length}`);
  for (const label of ['挂载', '卸载', '创建镜像', '删除', '导入', '挂载到本地']) {
    assert.match(allJs, new RegExp(`runTask\\(\\s*'${label}'`), `缺少 ${label} 的 runTask`);
  }
  // 忙碌态清理必须在 finally 里：resolve / reject / 同步抛错三条路径都要清干净。
  const body = functionBody(source, 'runTask');
  assert.match(body, /finally\s*\{/, 'runTask 必须在 finally 中清理');
  assert.match(body, /clearInterval\(ticker\)/, 'finally 中必须清掉计时器');
  assert.match(body, /cleanup\(\)/, 'finally 中必须清掉忙碌态');
});

test('前端同步进入忙碌态（禁用按钮 + aria-busy）', () => {
  const source = moduleSource('task.js');
  const busy = functionBody(source, 'enterBusy');
  assert.match(busy, /button\.disabled = true/);
  assert.match(busy, /aria-busy/);
  // 进入忙碌态必须在 await 之前发生（runTask 里先 enterBusy 再 await fn()）。
  const runTask = functionBody(source, 'runTask');
  assert.ok(
    runTask.indexOf('enterBusy') < runTask.indexOf('await fn()'),
    'enterBusy 必须先于 await fn()',
  );
  // 200ms 的计时刷新。
  assert.match(source, /TASK_TICK_MS\s*=\s*200/);
  // doCreate 的可用空间查询也必须落在忙碌态内（否则点击后仍有一段无反馈窗口）。
  const doCreate = functionBody(moduleSource('view-create.js'), 'doCreate');
  assert.match(doCreate, /queryAvailableSpace\(\)/);
  assert.ok(
    doCreate.indexOf('runTask') < doCreate.indexOf('await queryAvailableSpace()'),
    'doCreate 的 df 查询必须发生在 runTask 之内',
  );
});

test('刷新操作有可见反馈但不禁用按钮', () => {
  const source = moduleSource('task.js');
  assert.match(source, /正在同步状态…/);
  const refresh = functionBody(source, 'runRefresh');
  assert.doesNotMatch(refresh, /disabled/, '刷新不得禁用按钮（离线时它是探针）');
  // 每次刷新都经 runRefresh（刷新函数现在分布在各 view-*.js 中）。
  for (const [module, fn] of [
    ['view-mount.js', 'refreshStatus'],
    ['view-images.js', 'refreshImages'],
    ['view-edit.js', 'refreshLoop'],
    ['view-settings.js', 'refreshCapabilities'],
  ]) {
    const body = functionBody(moduleSource(module), fn);
    assert.match(body, /runRefresh\(/, `${fn} 必须经 runRefresh 显示反馈`);
  }
});

// ---------------------------------------------------------------- 后端状态机（Task 2）

test('index.html 有持久的离线横幅与重连按钮', () => {
  assert.match(html, /id="offline-banner"/);
  assert.match(html, /id="btn-reconnect"/);
  // 横幅默认隐藏，但**不能**靠自动隐藏：只有恢复才消失（无 data-auto-hide 之类）。
  assert.match(html, /id="offline-banner"[^>]*hidden/);
  // 必须报告具体原因与「正在重试…」指示。
  assert.match(html, /id="offline-reason"/);
  assert.match(html, /id="offline-retrying"/);
  assert.match(html, /正在重试…/);
});

test('前端有后端状态机且只在 callBackend 出口更新', () => {
  // 状态机整体搬到 backend.js。
  const source = moduleSource('backend.js');
  assert.match(source, /backendState\s*=\s*\{\s*status:\s*'unknown'/);
  assert.match(source, /function renderBackendState\(/);
  assert.match(source, /function reconnect\(/);
  assert.match(source, /classifyBackendFailure/);
  assert.match(source, /shouldBlockActions/);
  assert.match(source, /nextReconnectDelay/);
  // 单飞：手动点击与自动重试共用同一个 Promise，避免拉起两个 serve。
  assert.match(source, /reconnectInFlight/);
  // 入口必须渲染一次状态：首屏即离线时横幅要出现。
  // 重构后 init() 在 main.js（原来在 app.js）。
  const init = functionBody(moduleSource('main.js'), 'init');
  assert.match(init, /renderBackendState\(\)/, 'init 必须调用 renderBackendState');
});

test('离线时禁用全部变更按钮而刷新按钮保持可用', () => {
  // 变更按钮用 data-role="mutation" 标记，renderBackendState 按状态统一设置；
  // 这样动态生成的按钮（镜像列表的「删除」）也能被覆盖。
  const source = moduleSource('backend.js');
  assert.match(source, /data-role="mutation"/);
  assert.match(source, /querySelectorAll\('\[data-role="mutation"\]'\)/);
  // 变更按钮集中在 TASK_SCOPES.buttons 中（enterBusy 用它限定作用范围）。
  const scopes = moduleSource('task.js').match(/const TASK_SCOPES = \{[\s\S]*?\n\};/);
  assert.ok(scopes, '必须有 TASK_SCOPES');
  for (const id of [
    'btn-mount',
    'btn-unmount',
    'btn-create',
    'btn-import',
    'btn-loop-attach',
    'btn-loop-detach',
  ]) {
    assert.match(scopes[0], new RegExp(`'${id}'`), `变更按钮 ${id} 必须被状态机管理`);
    assert.match(html, new RegExp(`id="${id}"[^>]*data-role="mutation"`), `${id} 缺少标记`);
  }
  // 刷新按钮不得出现在禁用集合里。
  for (const id of [
    'btn-refresh-mount',
    'btn-images-refresh',
    'btn-loop-refresh',
    'btn-caps-refresh',
  ]) {
    assert.doesNotMatch(scopes[0], new RegExp(`'${id}'`), `刷新按钮 ${id} 不应被禁用`);
  }
});

test('「已重新连接」只在曾离线后恢复时提示', () => {
  const source = moduleSource('backend.js');
  assert.match(source, /wasOffline/);
  assert.match(source, /toast\('已重新连接'\)/);
});

// ---------------------------------------------------------------- 分区选择（Task 3）

test('index.html 有分区下拉框与「读取分区」按钮', () => {
  assert.match(html, /id="loop-partition-select"/);
  assert.match(html, /<select[^>]*id="loop-partition-select"/);
  assert.match(html, /id="btn-loop-partitions"/);
  assert.match(html, /读取分区/);
  // 旧的裸数字输入必须移除，且不得留下对它的引用（死元素 / 空引用都会失效）。
  assert.doesNotMatch(html, /id="loop-partition"/);
  assert.doesNotMatch(allJs, /\$\('loop-partition'\)/);
});

test('前端在选择镜像与点击按钮时读取分区表', () => {
  // 分区读取在 view-edit.js；事件接线在入口 main.js。
  const source = moduleSource('view-edit.js');
  assert.match(allJs, /op:\s*'image-partitions'/);
  assert.match(source, /function loadPartitions\(/);
  assert.match(allJs, /buildPartitionOptions/);
  assert.match(allJs, /formatPartitionScan/);
  const entry = moduleSource('main.js');
  // 镜像选择是下拉框：一次变更就是一次原子选择，用 `change` 立即读取，
  // **不再需要**输入框时代的 400ms 防抖。
  assert.match(entry, /'loop-image'\)\.addEventListener\('change', \(\) => loadPartitions\(\)\)/);
  assert.match(entry, /'btn-loop-partitions'\)\.addEventListener\('click'/);
  assert.doesNotMatch(source, /schedulePartitionScan|PARTITION_SCAN_DEBOUNCE_MS/);
  // 无分区表时不阻塞用户。
  assert.match(allJs, /未能读取分区表，将作为整盘挂载/);
});

test('本地编辑页的镜像是下拉框而不是自由文本', () => {
  // 用户要求：本地编辑也不能手填路径，只能从镜像目录里选——与挂载页一致。
  // 自由文本输入做不到这一点（能填任意路径，也能填不存在的文件）。
  assert.match(
    html,
    /<select id="loop-image"><\/select>/,
    '#loop-image 必须是 select',
  );
  assert.doesNotMatch(html, /<input[^>]*id="loop-image"/, '#loop-image 不得是文本输入');
  const source = moduleSource('view-edit.js');
  // 复用挂载页同一套选项构造：两页的「能选到什么」必须一致。
  assert.match(source, /buildImageOptions/, '应复用 buildImageOptions');
  assert.match(source, /function renderLoopImageOptions\(/);
  assert.match(source, /function refreshLoopImages\(/);
  assert.match(allJs, /op:\s*'list'/);
});

test('镜像下拉框在任何入口进来时都已填充（不依赖先访问挂载页）', () => {
  // 用户可能直接点开本地编辑页，此时挂载页的 refreshStatus 还没跑过。
  // 若下拉框只靠挂载页填充，用户会看到一个空列表并以为「没有镜像」。
  const entry = moduleSource('main.js');
  assert.match(entry, /guard\(refreshLoopImages\(\), '本地编辑镜像列表'\)/);
  const task = moduleSource('task.js');
  assert.match(
    task,
    /name === 'edit'\)\s*\{[\s\S]*?refreshLoopImages\(\)/,
    '切到本地编辑页时应刷新镜像下拉框',
  );
});

test('镜像管理页不再有「用于挂载」按钮（冗余入口，已移除）', () => {
  // 该按钮的效果等价于「切到挂载页 → 在下拉框里选这个镜像」，而下拉框的选项
  // 本来就来自同一份镜像列表。删掉不损失能力，还消除了一个跨视图副作用：
  // 点按钮会悄悄改掉另一个页面的表单状态。
  assert.doesNotMatch(allJs, /用于挂载/);
  assert.doesNotMatch(allJs, /fillFirstDeviceImage/);
  const images = moduleSource('view-images.js');
  assert.match(images, /textContent = '删除'/, '删除按钮必须保留');
});

test('doAttachLoop 把下拉框的值映射为 partition 字段', () => {
  const source = moduleSource('view-edit.js');
  const attach = functionBody(source, 'doAttachLoop');
  assert.match(attach, /selectedPartition\(\)/);
  assert.match(attach, /partition,/);
  // `''` → null（整盘），否则是数字序号。
  const picker = functionBody(source, 'selectedPartition');
  assert.match(picker, /return null/);
  assert.match(picker, /Number\(value\)/);
});

test('前端仍然不含 localhost（新增代码同样受约束）', () => {
  assert.doesNotMatch(allJs, /localhost/);
  assert.doesNotMatch(allJs, /0\.0\.0\.0/);
});

// ---------------------------------------------------------------- 语法有效性

test('webroot 的 JS 必须能被解析（语法错误会让整个界面卡死）', () => {
  // 重复的顶层函数声明是 **SyntaxError**，会让整个 ES 模块加载失败——症状是页面
  // 永远停在「正在连接后端…」，因为 `init()` 从未执行。
  //
  // 这类缺陷**任何结构断言都发现不了**（那些正则仍然全部匹配），只有真正解析
  // 才能抓到。模块化后必须**逐个模块**检查：任何一个模块解析失败，整条 import
  // 图都会失败。
  //
  // 注意：`node --check <file>` 按 CommonJS 解析，遇到 `import` 会直接报错，
  // 因此要用 `--input-type=module` 从 stdin 喂源码（文件扩展名 .js 在无
  // package.json 时不被当作 ESM）。
  assert.ok(moduleNames.length >= 10, `应收集到全部模块，实际 ${moduleNames.length}`);
  for (const name of moduleNames) {
    const result = spawnSync(
      process.execPath,
      ['--input-type=module', '--check'],
      { input: modules.get(name), encoding: 'utf8' },
    );
    assert.equal(
      result.status,
      0,
      `${name} 存在语法错误（会让整个界面卡死）：\n${result.stderr}`,
    );
  }
});

test('每个模块都不重复声明顶层函数', () => {
  // 直接钉住「重复声明」这一具体形态，给出比「语法错误」更可读的失败信息。
  // 重复声明只在**同一个模块内**才是 SyntaxError，故按模块分别统计。
  const duplicates = [];
  for (const [name, source] of modules) {
    const names = [...source.matchAll(/^function\s+([A-Za-z0-9_$]+)\s*\(/gm)].map((m) => m[1]);
    const seen = new Map();
    for (const fn of names) {
      seen.set(fn, (seen.get(fn) ?? 0) + 1);
      if (seen.get(fn) === 2) duplicates.push(`${name}: ${fn}`);
    }
  }
  assert.deepEqual(duplicates, [], `重复声明的顶层函数：${duplicates.join('、')}`);
});

test('每个模块的具名 import 都必须被目标模块真实导出（链接期错误 = 白屏）', () => {
  // 导入一个目标模块不导出的名字是 **ES 模块的链接期错误**，不是语法错误——
  // 于是整个模块图加载失败、界面永远停在「正在连接后端…」，而 `node --check`
  // 与「能被解析」的断言都照样通过。
  //
  // 视图模块从 backend/task/pure 各处导入，任何一处改名（`export function foo`
  // → `export function bar`）而忘改调用方，都会让**整页**白屏。因此逐个模块
  // 比对「导入的名字 ⊆ 目标模块导出的名字」。
  //
  // 解析范围：只取 `import ... from '...'` 语句（`import.meta` / 动态 import
  // 不涉及具名绑定）。
  const exportRe = /export\s+(?:(?:const|let|var|function|class)\s+([A-Za-z_$][\w$]*)|(?:async\s+)?function\s+([A-Za-z_$][\w$]*)|default\s+(?:function|class)?\s*([A-Za-z_$][\w$]*)?|([A-Za-z_$][\w$]*))/g;
  const namedExportRe = /export\s*\{([^}]*)\}/g;

  /** 收集一个模块导出的全部名字。 */
  function exportedNames(source) {
    const names = new Set();
    let m;
    while ((m = exportRe.exec(source)) !== null) {
      const name = m[1] || m[2] || m[4];
      if (name) names.add(name);
      // `export default function name() {}` 的具名部分只在 default 里，不进名字表。
    }
    while ((m = namedExportRe.exec(source)) !== null) {
      for (const clause of m[1].split(',')) {
        const text = clause.trim();
        if (!text) continue;
        // `export { a as b }`：对外可见的名字是 `b`。
        const asMatch = text.match(/\bas\s+([A-Za-z_$][\w$]*)$/);
        names.add(asMatch ? asMatch[1] : text);
      }
    }
    return names;
  }

  const mismatch = [];
  let checked = 0;
  for (const [name, source] of modules) {
    // `import ... from '...'`；跳过动态 import。
    const importRe = /import\s+(?!\()([\s\S]*?)\s+from\s+['"]([^'"]+)['"]/g;
    let m;
    while ((m = importRe.exec(source)) !== null) {
      const [, clause, spec] = m;
      // 只查具名导入：`import { a, b as c } from ...`。
      const block = clause.match(/^\{([\s\S]*)\}$/) || clause.match(/\{([\s\S]*?)\}/);
      if (!block) continue; // 默认导入 / 命名空间导入（`import * as x`）。
      const target = resolve(dirname(absPath(name)), spec);
      if (!existsSync(target)) continue; // 目标缺失由「导入目标必须存在」那条断言负责。
      const relativeTarget = relative(WEBUI, target).split(sep).join('/');
      const targetSource = modules.get(relativeTarget);
      if (targetSource === undefined) continue;

      const available = exportedNames(targetSource);
      for (const raw of block[1].split(',')) {
        const text = raw.replace(/\/\/.*$/gm, '').trim();
        if (!text) continue;
        // `x as y`：被导入的**原名**是 `x`。
        const imported = (text.split(/\s+as\s+/)[0] || '').trim();
        if (!/^[A-Za-z_$][\w$]*$/.test(imported)) continue;
        checked += 1;
        if (!available.has(imported)) {
          mismatch.push(`${name} 从 ${relativeTarget} 导入了未导出的 ${imported}`);
        }
      }
    }
  }

  assert.ok(checked > 0, '应至少检查到一个具名导入');
  assert.deepEqual(
    mismatch,
    [],
    'ES 模块链接期错误会导致整个前端加载失败：页面永远停在「正在连接后端…」（白屏）。\n' +
      mismatch.join('\n'),
  );
});

test('每个模块都能被 Node 实际导入（覆盖文本比对查不到的加载期失败）', async () => {
  // 上面的比对是文本级的；这里再做一次**真实导入**，覆盖「导出存在但加载时
  // 才失败」之类的情况（例如静态初始化抛错）。视图模块引用 window/document，
  // 因此只对**不依赖 DOM** 的模块做真实导入：
  //   - ksu.js / pure/*  —— 纯逻辑；
  //   - dom.js / backend.js / task.js / view-*.js / main.js —— 顶层触碰 window、
  //     在 Node 里必然抛错（浏览器里正常），由文本断言与 `--check` 覆盖语法。
  const browserFree = moduleNames.filter((name) => name.startsWith('pure/'));
  browserFree.push('ksu.js');
  for (const name of browserFree) {
    const mod = await import(absPath(name));
    assert.ok(
      Object.keys(mod).length > 0,
      `${name} 应能成功导入并导出符号`,
    );
  }
  // 前端整体至少要有模块被真实导入过，避免列表被清空后静默跳过。
  assert.ok(browserFree.length >= 6, '应至少真实导入纯函数层');
});

// ---------------------------------------------------------------- 创建镜像视图

test('创建视图改名为「创建镜像」', () => {
  // 需求：创建磁盘 → 创建镜像。视图 id 保持不变（KernelSuite 的 tab 接线依赖它）。
  assert.match(html, /id="tab-create"[^>]*>创建镜像</);
  assert.match(html, /<h2>创建镜像<\/h2>/);
  assert.match(html, /id="btn-create"[^>]*>创建镜像</);
  // 不应再出现旧措辞。
  assert.doesNotMatch(html, /创建虚拟磁盘/);
});

test('创建视图有文件系统选择与格式化来源说明', () => {
  assert.match(html, /id="create-filesystem"/);
  for (const fs of ['fat32', 'exfat', 'ext4']) {
    assert.match(html, new RegExp(`<option value="${fs}"`), `缺少文件系统选项 ${fs}`);
  }
  // 探测结果必须在界面上如实展示（需求：显示可用的 mkfs）。
  assert.match(html, /id="create-fs-source"/);
});

test('创建视图有分区编辑器（仅 gpt/mbr 显示）', () => {
  assert.match(html, /id="create-partitions-section"/);
  assert.match(html, /id="create-partitions"/);
  assert.match(html, /id="btn-add-partition"/);
  // 同名冲突提示元素。
  assert.match(html, /id="create-name-conflict"/);
});

test('前端按布局显隐分区编辑器并驱动同名预检', () => {
  // 分区编辑器整块在 view-create.js。
  const source = moduleSource('view-create.js');
  assert.match(source, /layoutSupportsPartitions\(/);
  assert.match(source, /renderPartitions\(/);
  // 同名预检必须接在文件名输入上，让用户在点击前就看到冲突。
  // 事件接线点在入口 main.js。
  assert.match(moduleSource('main.js'), /create-name'\)\.addEventListener\('input', checkNameConflict\)/);
  // MBR 下分区名**整个字段不渲染**（无该字段，不能静默丢弃，也不该用禁用控件
  // 假装可以填）。见「MBR 下不渲染名称字段」那条测试。
  assert.match(source, /layoutSupportsPartitionNames\(/);
});

test('MBR 下不渲染名称字段，GPT 下才渲染', () => {
  // MBR 下分区名**整个字段不渲染**：渲染一个 `disabled` 的输入框会让人以为
  // 自己可以填、只是被挡住了。与「容器行不渲染类型选择器」同一条原则。
  const source = moduleSource('view-create.js');
  const nameBlock = source.match(/if \(namesAllowed\) \{[\s\S]*?partition-name-\$\{index\}[\s\S]*?\n    \}/);
  assert.ok(nameBlock, '名称字段必须包在 if (namesAllowed) 里');

  // 不得再有"渲染后禁用"的写法。
  assert.doesNotMatch(source, /nameInput\.disabled = true/, '不应渲染禁用的名称输入框');
  assert.doesNotMatch(source, /MBR 无分区名/, '不应再出现占位符说法');
  assert.doesNotMatch(source, /nameInput\.placeholder/, '名称输入框不该有占位符');
});

test('归属选择器有三项：主分区 / 逻辑分区 / 扩展分区容器', () => {
  // 「1 主 + 1 扩展」要求用户能显式声明一个空容器，因此下拉必须有第三项。
  const source = moduleSource('view-create.js');
  assert.match(source, /partition-kind-\$\{index\}/);
  assert.match(source, /value: 'primary', label: '主分区'/);
  assert.match(source, /value: 'logical', label: '逻辑分区'/);
  assert.match(source, /value: 'extended', label: '扩展分区（容器）'/);
});

test('容器行不渲染类型与文件系统选择器', () => {
  // 容器没有数据区：渲染成禁用控件比不渲染更糟（用户以为自己可以填）。
  // 断言字段构造处的容器分支里没有建这两个控件。
  //
  // 注意 `if (kind === 'extended')` 在渲染函数里出现两次（一次决定标题文案，
  // 一次决定字段）。这里锚定**带 `partition-container-hint` 的那一个**，
  // 否则会匹配到标题分支而测不到真正的字段逻辑。
  const source = moduleSource('view-create.js');
  const hintAt = source.indexOf('partition-container-hint');
  assert.ok(hintAt > 0, '容器行应给出说明文字');

  const branchStart = source.lastIndexOf("if (kind === 'extended') {", hintAt);
  assert.ok(branchStart > 0, '应能找到字段构造处的容器分支');

  const branchEnd = source.indexOf('} else {', hintAt);
  assert.ok(branchEnd > branchStart);
  const branch = source.slice(branchStart, branchEnd);

  assert.doesNotMatch(branch, /partition-mbrtype-/, '容器行不渲染 MBR 类型');
  assert.doesNotMatch(branch, /partition-fs-/, '容器行不渲染文件系统');
  assert.match(branch, /partition-container-hint/, '容器行应给出说明文字');
  // 容器的字节由后端写死，用户不能选类型。
  assert.match(branch, /0x05/, '说明文字要点出容器类型字节');
});

test('扩展容器不显示成「分区 0」', () => {
  // 后端把空容器读回来时 index 为 0（它不占内核序号）。标题里显示数字会让
  // 用户以为存在一个序号 0 的设备。
  const source = moduleSource('view-create.js');
  assert.match(source, /partition-heading-extended/);
  assert.match(source, /扩展分区容器（不占序号/);
  assert.doesNotMatch(source, /分区 \$\{ordinal\}（容器/);
});

test('逻辑分区渲染在缩进分组里（表达位于扩展分区内部）', () => {
  assert.match(moduleSource('view-create.js'), /partition-logical-group/);
  assert.match(css, /\.partition-logical-group\s*\{/);
  assert.match(css, /\.partition-heading-extended\s*\{/);
});

test('渲染顺序：主分区 → 扩展容器 → 逻辑分区', () => {
  // 顺序与内核槽位一致（主分区占 1–4、容器紧随其后），逻辑分区归到容器之后。
  const source = moduleSource('view-create.js');
  const orderStart = source.indexOf('const order = [];');
  assert.ok(orderStart > 0, '应有构造渲染顺序的 `order` 数组');
  // 到 `order.forEach` 为止就是构造顺序的那几段。
  const orderEnd = source.indexOf('order.forEach((index)', orderStart);
  assert.ok(orderEnd > orderStart);
  const body = source.slice(orderStart, orderEnd);

  const primaryPush = body.indexOf("=== 'primary'");
  const extendedPush = body.indexOf("=== 'extended'");
  const logicalPush = body.indexOf("=== 'logical'");
  assert.ok(primaryPush > 0, '应有主分区那一段');
  assert.ok(extendedPush > 0, '应有扩展容器那一段');
  assert.ok(logicalPush > 0, '应有逻辑分区那一段');
  assert.ok(primaryPush < extendedPush, '主分区在前');
  assert.ok(extendedPush < logicalPush, '容器在逻辑分区之前');
});

test('DOM id 必须用原始行下标，不能用渲染序号', () => {
  // **这是渲染重排后最容易出的错**：`readPartitionRow` / `syncPartitionRows`
  // 都按行在 `createPartitions` 里的**原始下标**读写。若 id 改用渲染序号，
  // 一次重绘后每一行的输入都会串到别的行上（改容量会把类型也改掉）。
  //
  // 断言方式：把**所有** `id = \`partition-...-${...}\`` 赋值抓出来，逐个检查
  // 插值表达式就是这个 `index`。只 grep 字面量 `partition-size-${index}` 是不够的
  // ——实测过：把其中一处改成 `${order.indexOf(index)}` 时那种断言照样通过。
  const source = moduleSource('view-create.js');
  const assignments = [...source.matchAll(/\.id = `(partition-[a-z]+)-\$\{([^}]*)\}`/g)];
  assert.ok(assignments.length >= 4, `应找到多个 id 赋值，实际 ${assignments.length}`);

  for (const [, name, expr] of assignments) {
    assert.equal(
      expr.trim(),
      'index',
      `${name} 的 id 必须用原始下标 index，实际插值：${expr}`,
    );
  }

  // 反过来也要成立：渲染序号的变量（order / ordinal）不得出现在任何 id 里。
  assert.doesNotMatch(source, /\.id = `partition-[a-z]+-\$\{[^}]*order[^}]*\}`/);
  assert.doesNotMatch(source, /\.id = `partition-[a-z]+-\$\{[^}]*ordinal[^}]*\}`/);
});

test('readPartitionRow 的 kind 回落值必须是该行原有的 kind', () => {
  // 容器行与逻辑分区行不渲染类型/文件系统控件，`syncPartitionRows` 每次重绘前
  // 逐行读取。若读不到就把 kind 硬写成 'primary'，用户改一个容量、这一行就
  // 悄悄变回主分区——真实的静默错。
  const readFn = functionBody(moduleSource('view-create.js'), 'readPartitionRow');
  assert.match(
    readFn,
    /fallbackKind/,
    'kind 的回落值必须来自该行原有值，不能硬写 primary',
  );
  assert.match(readFn, /kind: kind \? kind\.value : fallbackKind/);
  assert.doesNotMatch(
    readFn,
    /kind: kind \? kind\.value : 'primary'/,
    '不得硬写 primary',
  );
});

test('前端不再把卷标当作“仅 CLI 生效”', () => {
  // 历史缺陷：REST 契约曾不含 label。现在两条通道都会发送它。
  assert.doesNotMatch(allJs, /只有 CLI 回退路径会用到/);
});

// ---------------------------------------------------------------- 字段名一致性

test('分区行的读写字段名必须一致', () => {
  // 截图缺陷的根因：renderPartitions 读 entry.sizeText，而 readPartitionRow
  // 写入的对象只有 sizeBytes —— 任何一次重绘后容量框就渲染成 `undefined`。
  // 零构建约束下没有类型系统，只能靠文本断言把两侧钉在一起。
  const source = moduleSource('view-create.js');
  const body = functionBody(source, 'readPartitionRow');

  for (const field of ['sizeText', 'gptType', 'gptTypeCustom', 'mbrType', 'mbrTypeCustom', 'name', 'filesystem']) {
    assert.match(
      body,
      new RegExp(`\\b${field}:`),
      `readPartitionRow 必须产出 ${field}（与 defaultPartitionRow 一致）`,
    );
    assert.match(
      source,
      new RegExp(`entry\\.${field}\\b`),
      `renderPartitions 应读取 entry.${field}`,
    );
  }

  // 不应再出现旧的字段名（`sizeBytes` 是旧形状，已由 `sizeText` 取代）。
  assert.doesNotMatch(body, /sizeBytes:/, 'readPartitionRow 不应产出 sizeBytes');
});

test('创建视图的分区编辑器含文件系统与自定义类型控件', () => {
  const source = moduleSource('view-create.js');
  // 每分区可选文件系统（含"不格式化"）。
  assert.match(source, /partition-fs-\$\{index\}/);
  assert.match(source, /describeFilesystem\('none'\)/);
  // 自定义 GPT GUID 与 MBR 类型字节输入框。
  assert.match(source, /partition-gptguid-\$\{index\}/);
  assert.match(source, /partition-mbrbyte-\$\{index\}/);
  // 类型按布局切换。
  assert.match(source, /partitionTypePresets\(layout\)/);
});

test('分区编辑器用 .field 单元而非裸 flex 子元素（宽屏布局修复）', () => {
  // 宽屏错位的根因：单层 flex + flex-wrap 让标签按可用宽度重排，
  // 「类型」标签被夹在两个控件之间。改成 .field 单元后标签必定在自己控件上方。
  const source = moduleSource('view-create.js');
  assert.match(source, /className = 'field'/);
  assert.match(source, /className = 'partition-fields'/);
  assert.match(css, /\.partition-fields\s*\{[^}]*display:\s*grid/);
  assert.match(css, /\.field\s*\{[^}]*flex-direction:\s*column/);
});

// ---------------------------------------------------------------- 分块上传（views 3）

test('分块上传四个 op 齐备，且导入视图只走字节流', () => {
  // 上传协议：begin → 多次 chunk（原始字节）→ commit；失败/取消走 abort。
  // 只有 REST 能承载（ksu.exec 无法向子进程写 stdin）。
  for (const op of ['upload-begin', 'upload-chunk', 'upload-commit', 'upload-abort']) {
    assert.match(
      allJs,
      new RegExp(`['"\`]${op}['"\`]|op:\\s*'${op}'|case\\s+'${op}'`),
      `缺少分块上传 op：${op}`,
    );
  }

  const source = moduleSource('view-import.js');
  // 进度与分块计算必须在导入视图里（用户可见的进度来自它）。
  assert.match(source, /export function chunkCount\(/, 'view-import.js 应定义 chunkCount');
  assert.match(source, /export function uploadProgress\(/, 'view-import.js 应定义 uploadProgress');
});

test('上传必须按块读取（file.slice），不得整文件 readAsArrayBuffer', () => {
  // 内存峰值约束：`readAsArrayBuffer(file)` 会把整个镜像读进内存，大镜像直接
  // 崩掉 WebView。必须 `file.slice(start, end)` 后只读这一块。
  const source = moduleSource('view-import.js');
  assert.match(source, /readAsArrayBuffer\(\s*file\.slice\(/, '必须按块 slice 读取');
  assert.match(source, /\.slice\(/, '应使用 File.slice 分块');

  // 注释里**故意**写着 `readAsArrayBuffer(file)`（说明为什么不能这么写），
  // 断言只看真正的代码，否则会被自己的解释性注释绊倒。
  const code = source.replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, '');

  // 明确禁止整文件读取：`readAsArrayBuffer(<不是 slice 的表达式>)`。
  const calls = [...code.matchAll(/readAsArrayBuffer\s*\(([^)]*)\)/g)].map((m) => m[1].trim());
  assert.ok(calls.length > 0, '应至少调用一次 readAsArrayBuffer');
  for (const arg of calls) {
    assert.match(
      arg,
      /\.slice\s*\(/,
      `readAsArrayBuffer 只能读取 slice 出来的块，实际参数：${arg}`,
    );
  }
  assert.doesNotMatch(
    code,
    /readAsArrayBuffer\s*\(\s*file\s*\)/,
    '不得整文件 readAsArrayBuffer(file)',
  );
});

test('上传失败必须清理服务端暂存（否则 serve 永不空闲退出）', () => {
  // 遗留的 tmp/*.part 与 running job 会让 `serve` 永不空闲退出——这违反
  // 「按需进程模型」。因此 chunk 失败、REST 不可用、以及中途抛错三条路径都要
  // 尽力 abort。
  const source = moduleSource('view-import.js');
  const aborts = [...source.matchAll(/abortUpload\(/g)];
  assert.ok(aborts.length >= 3, `abortUpload 调用点应有 3 处以上，实际 ${aborts.length}`);
  assert.match(source, /op:\s*'upload-abort'/, 'abort 必须调用 upload-abort');
});
