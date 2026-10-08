// logic.test.mjs —— 纯函数层（webui/pure/）的 Node 原生测试。
//
// 零构建约束下没有前端测试框架（docs/testing.md），故用 Node 内置 test runner：
//
//   uv run gd-test --all
//
// 只测纯函数，不涉及 DOM。

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  ALIGNMENT_BYTES,
  DEFAULT_IMAGE_BYTES,
  FILESYSTEM_MIN_BYTES,
  alignUp,
  formatBytes,
  minPartitionBytes,
  parsePartitionSize,
  parseSizeInput,
  sizeNote,
  validateSize,
} from '../pure/bytes.js';
import {
  baseName,
  joinPath,
  parentPath,
  safeImageName,
  shellQuote,
} from '../pure/paths.js';
import {
  describeFilesystem,
  describeGptPartitionType,
  describeInUse,
  describeLayout,
  describeMbrPartitionType,
  describeMode,
  describePartitionType,
  messageForCode,
  modeWarning,
  truncate,
} from '../pure/describe.js';
import {
  CUSTOM_TYPE_VALUE,
  GPT_PARTITION_TYPES,
  MAX_PARTITIONS,
  MBR_MAX_EXTENDED,
  MBR_MAX_LOGICAL,
  MBR_MAX_PRIMARY,
  MBR_PARTITION_TYPES,
  buildPartitionOptions,
  customTypeWire,
  defaultPartitionType,
  describePartition,
  formatPartitionScan,
  layoutSupportsPartitionNames,
  layoutSupportsPartitions,
  mbrSlotUsage,
  normalizePartitions,
  partitionKernelIndex,
  resolveGptType,
  resolveMbrType,
  validatePartitions,
} from '../pure/partitions.js';
import {
  EXIT_CODE_UNREACHABLE,
  REST_HOST,
  buildCliArgs,
  buildRestCall,
  classifyBackendFailure,
  execProbeSucceeded,
  nextReconnectDelay,
  normalizeDevices,
  parseApiInfo,
  parseExecResult,
  restResultToExecResult,
  restUrl,
  shouldBlockActions,
  tryExtractJson,
} from '../pure/channel.js';
import {
  INQUIRY_STRING_MAX,
  MAX_LUNS,
  RECONNECT_BASE_MS,
  RECONNECT_MAX_MS,
  SLOW_TASK_THRESHOLD_MS,
  buildImageOptions,
  describeFormattingSource,
  describeSlot,
  describeTaskElapsed,
  hasControlChar,
  imageNameExists,
  isPrintableAscii,
  mergeSlotRows,
  normalizeIdentity,
  normalizeImageContext,
  shouldWarnSlow,
  taskProgressLabel,
  utf8ByteLength,
  validateIdentityField,
  validateImageContext,
} from '../pure/task.js';

// ---------------------------------------------------------------- 常量一致性

test('常量与 Rust 侧及文档一致', () => {
  // 下限是**按分区文件系统**的，不是镜像级属性：同一个镜像里三种文件系统各按
  // 各的门槛判。FAT32 33 MiB 来自实测簇数边界（34077184 字节 = 65525 簇）。
  assert.deepEqual(FILESYSTEM_MIN_BYTES, {
    fat32: 33 * 1024 * 1024,
    exfat: 1024 * 1024,
    ext4: 2 * 1024 * 1024,
  });
  assert.equal(minPartitionBytes('fat32'), 33 * 1024 * 1024);
  assert.equal(minPartitionBytes('ext4'), 2 * 1024 * 1024);
  // 不格式化与未知类型没有下限（`null`，不是 0）。
  assert.equal(minPartitionBytes('none'), null);
  assert.equal(minPartitionBytes('nonsense'), null);
  assert.equal(minPartitionBytes(undefined), null);

  assert.equal(DEFAULT_IMAGE_BYTES, 4 * 1024 * 1024 * 1024);
  assert.equal(ALIGNMENT_BYTES, 1024 * 1024);
});

// ---------------------------------------------------------------- parseExecResult
// 三类异常必须有明确且可区分的处理（docs/webui.md：缺一即可能白屏）。

test('解析：成功路径返回数据', () => {
  const result = parseExecResult({
    errno: 0,
    stdout: '{"udc":"dummy_udc.0","devices":[]}',
    stderr: '',
  });
  assert.equal(result.ok, true);
  assert.equal(result.data.udc, 'dummy_udc.0');
});

test('解析异常一：命令失败（errno 非零）', () => {
  const result = parseExecResult({ errno: 4, stdout: '', stderr: 'boom' });
  assert.equal(result.ok, false);
  assert.equal(result.kind, 'command_failed');
  assert.match(result.message, /exit code 4/);
  // stderr 必须被保留以供排查。
  assert.equal(result.detail, 'boom');
});

test('解析异常一（变体）：命令失败但 stdout 含后端 JSON 错误', () => {
  const result = parseExecResult({
    errno: 4,
    stdout: '{"error":"image_not_found","message":"image not found: /x.img"}',
    stderr: '',
  });
  assert.equal(result.ok, false);
  assert.equal(result.kind, 'backend_error');
  assert.equal(result.code, 'image_not_found');
  // **错误码优先**：后端 message 是英文的，不能直接上屏污染中文界面。
  assert.equal(result.message, messageForCode('image_not_found'));
  assert.doesNotMatch(result.message, /image not found/, '后端英文 message 不得占标题位');
  // 但后端原文必须保留在 detail 里，否则排查时没有线索。
  assert.match(result.detail, /image not found/);
});

test('回归：后端 message 为英文时，界面文案仍取本地中文映射', () => {
  // 英文 message + 已知错误码 → 标题必须是中文。
  for (const [code, message] of [
    ['image_in_use', 'image is in use by the gadget'],
    ['no_space', 'not enough space: need 100 bytes'],
    ['already_exists', 'destination already exists: a.img'],
  ]) {
    const result = parseExecResult({
      errno: 4,
      stdout: JSON.stringify({ error: code, message }),
      stderr: '',
    });
    assert.equal(result.message, messageForCode(code), `code=${code}`);
    assert.doesNotMatch(result.message, /[a-z]{4,} [a-z]{4,}/i, `code=${code} 漏出了英文原文`);
  }
});

test('回归：REST 错误的 detail 必须同时带状态码与后端原文', () => {
  // 实测缺陷：REST 通道下错误 JSON 放在 stdout，pickDetail 于是去取「另一条流」
  // stderr——那里面只有 `HTTP 400`。结果 detail 退化成一行状态码，后端唯一的
  // 诊断线索（英文 message）被丢掉。
  //
  // 现在后端 message 尤其重要：本地文案只说"分区容量低于该文件系统的下限"（不带数字，
  // 因为下限随文件系统而变），**具体是哪个分区、哪个文件系统、差多少**只在 message 里。
  const backendMessage =
    'partition 1 (fat32) is 1024 bytes, which is below the fat32 minimum of 34603008 bytes';
  const result = parseExecResult(
    restResultToExecResult(400, JSON.stringify({ error: 'size_below_minimum', message: backendMessage })),
  );
  assert.equal(result.ok, false);
  assert.equal(result.code, 'size_below_minimum');
  // 标题是本地中文文案，不含英文原文。
  assert.equal(result.message, messageForCode('size_below_minimum'));
  assert.doesNotMatch(result.message, /bytes is below/);
  // detail 必须两者兼有：状态码 + 后端原文。
  assert.match(result.detail, /HTTP 400/);
  assert.match(result.detail, /34603008/);
  // 行号与文件系统也必须透传到用户眼前——否则多分区镜像里无从定位。
  assert.match(result.detail, /partition 1 \(fat32\)/);
});

test('CLI 通道的 detail 不重复后端 message', () => {
  // CLI 通道下错误 JSON 就在 stdout/stderr 之一，context 与 message 是同一段文本，
  // 不应被拼接两次。
  const result = parseExecResult({
    errno: 4,
    stdout: '',
    stderr: '{"error":"no_space","message":"not enough space: need 100 bytes"}',
  });
  assert.equal(result.message, messageForCode('no_space'));
  const occurrences = result.detail.split('not enough space').length - 1;
  assert.equal(occurrences, 1, `后端原文不该重复：${result.detail}`);
});

test('未知错误码时才用后端 message 兜底（并保留错误码）', () => {
  const result = parseExecResult({
    errno: 4,
    stdout: '{"error":"some_future_code","message":"brand new failure mode"}',
    stderr: '',
  });
  assert.equal(result.code, 'some_future_code');
  assert.match(result.message, /brand new failure mode/);
  assert.match(result.message, /some_future_code/, '兜底文案仍要带上错误码');
});

test('解析异常二：空输出', () => {
  for (const stdout of ['', '   ', '\n\n', '\t ']) {
    const result = parseExecResult({ errno: 0, stdout, stderr: '' });
    assert.equal(result.ok, false, `stdout=${JSON.stringify(stdout)}`);
    assert.equal(result.kind, 'empty_output');
    assert.match(result.message, /did not respond/);
  }
});

test('解析异常三：JSON 解析失败', () => {
  const result = parseExecResult({ errno: 0, stdout: 'not json at all', stderr: '' });
  assert.equal(result.ok, false);
  assert.equal(result.kind, 'bad_json');
  assert.match(result.message, /not valid JSON/);
  // 必须原样展示（截断后的）stdout。
  assert.match(result.detail, /not json at all/);
});

test('解析：容忍 stdout 尾随换行与前后空白', () => {
  const result = parseExecResult({
    errno: 0,
    stdout: '  \n {"udc":null,"devices":[]}\n\n  ',
    stderr: '',
  });
  assert.equal(result.ok, true);
  assert.equal(result.data.udc, null);
});

test('解析：从混入 shell 警告的输出中提取 JSON', () => {
  const result = parseExecResult({
    errno: 0,
    stdout: 'sh: some warning\n{"images":[]}\ntrailing noise',
    stderr: '',
  });
  assert.equal(result.ok, true);
  assert.deepEqual(result.data.images, []);
});

test('解析：嵌套对象提取不被内层括号截断', () => {
  const result = parseExecResult({
    errno: 0,
    stdout: 'warn\n{"a":{"b":[1,2,{"c":3}]},"d":"}"}\nend',
    stderr: '',
  });
  assert.equal(result.ok, true);
  assert.deepEqual(result.data, { a: { b: [1, 2, { c: 3 }] }, d: '}' });
});

test('解析：字符串内的花括号不影响配对', () => {
  const extracted = tryExtractJson('x {"msg":"contains { brace }"} y');
  assert.deepEqual(extracted, { msg: 'contains { brace }' });
});

test('解析：非对象结果（errno=-1 且无输出）给出可读错误', () => {
  const result = parseExecResult({ errno: -1, stdout: '', stderr: 'ksu 不可用' });
  assert.equal(result.ok, false);
  assert.equal(result.kind, 'command_failed');
  assert.match(result.detail, /ksu 不可用/);
});

test('解析：缺字段的结果不抛异常', () => {
  const result = parseExecResult({});
  assert.equal(result.ok, false);
  assert.equal(result.kind, 'command_failed');
});

test('解析：bad_json 的 detail 被截断但保留长度提示', () => {
  const long = `x${'y'.repeat(1000)}`;
  const result = parseExecResult({ errno: 0, stdout: long, stderr: '' });
  assert.equal(result.kind, 'bad_json');
  assert.match(result.detail, /已截断/);
  assert.ok(result.detail.length < long.length);
});

// ---------------------------------------------------------------- 容量

test('容量：镜像容量本身没有下限（小镜像不再被容量层拒绝）', () => {
  // 本次修复的核心：早先 64 MiB 是"镜像下限"，于是 64 MiB 镜像里放一个占满剩余的
  // 分区会被报成空间不足。现在容量层只拒绝 0/负数与非有限值——真正要拦的
  // 「分区在其文件系统下太小」由 validatePartitions 带行号报出。
  for (const ok of [1, 1024 * 1024, 33 * 1024 * 1024 - 1]) {
    assert.equal(validateSize(ok, Infinity).ok, true, `容量 ${ok} 应被接受`);
  }
});

test('容量：恰好 1 MiB 被接受且不必再对齐', () => {
  const result = validateSize(ALIGNMENT_BYTES, Infinity);
  assert.equal(result.ok, true);
  assert.equal(result.alignedBytes, ALIGNMENT_BYTES);
});

test('容量：向上对齐到 1 MiB', () => {
  const result = validateSize(ALIGNMENT_BYTES + 1, Infinity);
  assert.equal(result.ok, true);
  assert.equal(result.alignedBytes % ALIGNMENT_BYTES, 0);
  assert.equal(result.alignedBytes, ALIGNMENT_BYTES * 2);
});

test('容量：可用空间不足时拒绝并给出数值', () => {
  const result = validateSize(DEFAULT_IMAGE_BYTES, 1024 * 1024);
  assert.equal(result.ok, false);
  assert.match(result.message, /空间不足/);
  assert.match(result.message, /需要/);
  assert.match(result.message, /可用/);
});

test('容量：非法输入被拒绝', () => {
  for (const bad of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, null, undefined]) {
    const result = validateSize(bad, Infinity);
    assert.equal(result.ok, false, `输入 ${bad}`);
  }
});

test('容量：可用空间未知（Infinity）时不拦截', () => {
  const result = validateSize(DEFAULT_IMAGE_BYTES, Number.POSITIVE_INFINITY);
  assert.equal(result.ok, true);
});

test('alignUp：边界与零对齐', () => {
  assert.equal(alignUp(0, 1024), 0);
  assert.equal(alignUp(1024, 1024), 1024);
  assert.equal(alignUp(1025, 1024), 2048);
  assert.equal(alignUp(1234, 0), 1234);
});

// ---------------------------------------------------------------- 格式化

test('formatBytes：常见尺度', () => {
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(1024), '1 KiB');
  assert.equal(formatBytes(64 * 1024 * 1024), '64 MiB');
  assert.equal(formatBytes(4 * 1024 * 1024 * 1024), '4 GiB');
  assert.equal(formatBytes(Number.NaN), '—');
});

test('parseSizeInput：解析用户输入', () => {
  assert.equal(parseSizeInput('64M'), 64 * 1024 * 1024);
  assert.equal(parseSizeInput('4G'), 4 * 1024 ** 3);
  assert.equal(parseSizeInput('512k'), 512 * 1024);
  assert.equal(parseSizeInput('1024'), 1024);
  assert.equal(parseSizeInput('1.5G'), Math.round(1.5 * 1024 ** 3));
  assert.equal(parseSizeInput('1GiB'), 1024 ** 3);
});

test('parseSizeInput：非法输入返回 null', () => {
  for (const bad of ['', 'abc', '-1M', '0', '1X', null, undefined, 'M']) {
    assert.equal(parseSizeInput(bad), null, `输入 ${JSON.stringify(bad)}`);
  }
});

test('truncate：短串原样返回，长串带提示', () => {
  assert.equal(truncate('abc', 10), 'abc');
  assert.match(truncate('a'.repeat(20), 5), /已截断，共 20 字符/);
  assert.equal(truncate(null, 5), '');
});

// ---------------------------------------------------------------- 文案

test('messageForCode：已定义错误码返回专门文案', () => {
  assert.match(messageForCode('busy'), /稍后重试/);
  assert.match(messageForCode('image_in_use'), /先卸载/);
  assert.match(messageForCode('no_udc'), /USB/);
  // loop 不可用时必须引导替代路径，而非只报错。
  assert.match(messageForCode('loop_unsupported'), /USB 挂载/);
});

test('messageForCode：未知错误码有兜底且带上原码', () => {
  assert.match(messageForCode('some_new_code'), /未知错误.*some_new_code/);
  assert.match(messageForCode(undefined), /未知错误/);
});

test('所有协议错误码都有文案', () => {
  // 与 docs/protocol.md 错误码表对齐；缺一个就会在 UI 上显示兜底文案。
  const documented = [
    'busy',
    'image_not_found',
    'image_in_use',
    'not_regular_file',
    'unsupported_layout',
    'no_udc',
    'mass_storage_unsupported',
    'loop_unsupported',
    'filesystem_unsupported',
    'size_below_minimum',
    'no_space',
    'permission_denied',
    'invalid_argument',
  ];
  for (const code of documented) {
    assert.doesNotMatch(messageForCode(code), /未知错误/, `缺少 ${code} 的文案`);
  }
});

test('describe*：覆盖全部取值', () => {
  assert.match(describeLayout('gpt'), /GPT/);
  assert.match(describeLayout('raw'), /无分区表/);
  assert.match(describeMode('cdrom'), /光驱/);
  assert.match(describeInUse('importing'), /导入中/);
  // 未知值原样返回，不崩。
  assert.equal(describeInUse('weird'), 'weird');
});

test('modeWarning：解释硬约束而非只报错', () => {
  assert.match(modeWarning('cdrom'), /只读/);
  assert.match(modeWarning('ro'), /无法写入/);
  assert.equal(modeWarning('rw'), null);
});

test('sizeNote：只解释稀疏文件，不再有"低于下限"分支', () => {
  // 镜像容量没有下限，因此这里**不能**再出现"至少使用 64 MiB"这类说法——
  // 那正是把 64 MiB 镜像误报成空间不足的根源。
  assert.match(sizeNote(DEFAULT_IMAGE_BYTES), /稀疏文件/);
  assert.equal(sizeNote(1024), sizeNote(DEFAULT_IMAGE_BYTES));
  assert.equal(sizeNote(Number.NaN), null);
});

// ---------------------------------------------------------------- 路径

test('joinPath：处理多余与缺失斜杠', () => {
  assert.equal(joinPath('/a/b', 'c.img'), '/a/b/c.img');
  assert.equal(joinPath('/a/b/', 'c.img'), '/a/b/c.img');
  assert.equal(joinPath('/a/b', '/c.img'), '/a/b/c.img');
  assert.equal(joinPath('', 'c.img'), 'c.img');
  assert.equal(joinPath('/a/b', ''), '/a/b');
});

test('parentPath：根目录与普通路径', () => {
  assert.equal(parentPath('/'), '/');
  assert.equal(parentPath('/a'), '/');
  assert.equal(parentPath('/a/b/c'), '/a/b');
  assert.equal(parentPath('/a/b/'), '/a');
  assert.equal(parentPath(''), '/');
});

test('baseName：取最后一段', () => {
  assert.equal(baseName('/a/b/c.img'), 'c.img');
  assert.equal(baseName('/a/b/'), 'b');
  assert.equal(baseName('c.img'), 'c.img');
  assert.equal(baseName('/'), '');
  assert.equal(baseName(''), '');
});

test('safeImageName：清洗非法字符', () => {
  assert.equal(safeImageName('my disk.img'), 'my_disk.img');
  assert.equal(safeImageName('a/b.img'), 'a_b.img');
  assert.equal(safeImageName('plain'), 'plain.img');
  assert.equal(safeImageName('  '), 'image.img');
  assert.equal(safeImageName('..'), 'image.img');
  assert.equal(safeImageName('../evil.img'), 'evil.img');
  assert.equal(safeImageName(null), null);
});

test('safeImageName：结果始终可用于路径拼接', () => {
  const samples = ['../../etc/passwd', 'a b c.img', '.hidden', '///', 'ok.img'];
  for (const sample of samples) {
    const name = safeImageName(sample);
    assert.ok(name, `${sample} 应产出名字`);
    assert.ok(!name.includes('/'), `${name} 不得含路径分隔符`);
    assert.ok(name !== '.' && name !== '..', `${name} 不得是伪条目`);
  }
});

// ---------------------------------------------------------------- execProbeSucceeded
//
// 回归：`ksu.exec` **永不 reject 且结果永远是对象**。曾经把它当字符串调 `.trim()`，
// 抛出的 TypeError 让 resolveBin() 变成被拒绝的 Promise，而 init() 没有 .catch()，
// 结果状态行永远停在「正在连接后端…」、所有按钮静默失效。

test('execProbeSucceeded：errno=0 且 stdout 为 yes 才算成功', () => {
  assert.equal(execProbeSucceeded({ errno: 0, stdout: 'yes\n', stderr: '' }), true);
  assert.equal(execProbeSucceeded({ errno: 0, stdout: '  yes  ', stderr: '' }), true);
});

test('execProbeSucceeded：errno 非零即失败', () => {
  assert.equal(execProbeSucceeded({ errno: 1, stdout: 'yes\n', stderr: '' }), false);
  // ksu 不可用时 exec 解析为 errno=-1，即便 stdout 恰好是 yes 也不算成功。
  assert.equal(execProbeSucceeded({ errno: -1, stdout: 'yes\n', stderr: '' }), false);
});

test('execProbeSucceeded：空 stdout 或空结果即失败', () => {
  assert.equal(execProbeSucceeded({ errno: 0, stdout: '', stderr: '' }), false);
  assert.equal(execProbeSucceeded({ errno: 0, stdout: '   \n', stderr: '' }), false);
  assert.equal(execProbeSucceeded({ errno: 0, stdout: 'no\n', stderr: '' }), false);
  assert.equal(execProbeSucceeded({}), false);
  assert.equal(execProbeSucceeded(null), false);
});

test('execProbeSucceeded：字符串输入必须返回 false 而不是抛异常', () => {
  // 这正是原缺陷：把 exec 的结果当字符串用。
  for (const bad of ['yes\n', 'yes', '', 'no']) {
    assert.equal(execProbeSucceeded(bad), false, `输入 ${JSON.stringify(bad)}`);
  }
});

// ---------------------------------------------------------------- parseApiInfo

test('parseApiInfo：合法内容返回 {port, token}', () => {
  assert.deepEqual(parseApiInfo('{"port":39481,"token":"abc123"}'), {
    port: 39481,
    token: 'abc123',
  });
  // 容忍前后空白（文件可能带尾随换行）。
  assert.deepEqual(parseApiInfo('\n {"port":1,"token":"t"} \n'), { port: 1, token: 't' });
});

test('parseApiInfo：缺少 token 或端口不合法都返回 null', () => {
  assert.equal(parseApiInfo('{"port":39481}'), null, '缺 token');
  assert.equal(parseApiInfo('{"token":"t"}'), null, '缺 port');
  assert.equal(parseApiInfo('{"port":39481,"token":""}'), null, '空 token');
  assert.equal(parseApiInfo('{"port":39481,"token":123}'), null, 'token 非字符串');
});

test('parseApiInfo：非整数或越界端口返回 null', () => {
  for (const port of [0, 70000, -1, 'abc', '39481', 39481.5, null, true]) {
    const text = JSON.stringify({ port, token: 't' });
    assert.equal(parseApiInfo(text), null, `端口 ${JSON.stringify(port)}`);
  }
});

test('parseApiInfo：畸形 JSON 与非对象内容返回 null', () => {
  for (const bad of ['', '   ', '{"port":', 'not json', 'null', '[]', '"str"', undefined]) {
    assert.equal(parseApiInfo(bad), null, `输入 ${JSON.stringify(bad)}`);
  }
});

// ---------------------------------------------------------------- buildRestCall

test('buildRestCall：无参数操作映射到 GET 路径', () => {
  assert.deepEqual(buildRestCall({ op: 'status' }), {
    method: 'GET',
    path: '/api/v1/status',
    body: null,
  });
  assert.equal(buildRestCall({ op: 'list' }).path, '/api/v1/images');
  assert.equal(buildRestCall({ op: 'list-loop' }).path, '/api/v1/loop');
  assert.equal(buildRestCall({ op: 'capabilities' }).path, '/api/v1/capabilities');
});

test('buildRestCall：job 映射到 /jobs/<id>', () => {
  const call = buildRestCall({ op: 'job', jobId: 'job-1' });
  assert.deepEqual(call, { method: 'GET', path: '/api/v1/jobs/job-1', body: null });
  // id 必须编码：不能让其中的 `/` 改变路径结构。
  assert.equal(buildRestCall({ op: 'job', jobId: 'a/b' }).path, '/api/v1/jobs/a%2Fb');
  assert.equal(buildRestCall({ op: 'job' }), null);
});

test('buildRestCall：df 把路径放进查询串并编码', () => {
  const call = buildRestCall({ op: 'df', path: '/data/adb/gadget-disk' });
  assert.equal(call.method, 'GET');
  assert.equal(call.path, '/api/v1/tool/df?path=%2Fdata%2Fadb%2Fgadget-disk');
  assert.equal(call.body, null);

  // 空格与 `&` 等必须编码，否则会被当成参数分隔符。
  assert.match(buildRestCall({ op: 'df', path: '/sdcard/My Docs/x' }).path, /%20/);
  assert.match(buildRestCall({ op: 'df', path: '/a&b' }).path, /%26/);

  assert.equal(buildRestCall({ op: 'df' }), null, 'df 缺 path 应返回 null');
  assert.equal(buildRestCall({ op: 'df', path: '' }), null);
});

test('回归：已删除的 ls / stat op 必须返回 null，而不是拼出打不通的路径', () => {
  // `ls` / `stat` 只为已删除的内置路径浏览器服务：后端 `Tool` 枚举只剩 `Df`，
  // `route(Method::Get, "/api/v1/tool/ls")` 返回 `NotFound`，CLI 也没有这两个
  // 子命令。曾经这里保留着分支，会拼出一条必然 404 的路径——比返回 null 更糟，
  // 因为调用方拿到的失败原因会指向「后端出错」而不是「这个功能不存在」。
  for (const op of ['ls', 'stat']) {
    assert.equal(buildRestCall({ op, path: '/sdcard' }), null, `REST 不应再支持 ${op}`);
    assert.equal(buildCliArgs({ op, path: '/sdcard' }), null, `CLI 不应再支持 ${op}`);
    assert.equal(buildRestCall({ op }), null);
    assert.equal(buildCliArgs({ op }), null);
  }
});

test('buildRestCall：create 发送完整契约（含卷标与分区）', () => {
  const call = buildRestCall({
    op: 'create',
    path: '/data/adb/gadget-disk/images/a.img',
    sizeBytes: 67108864,
    layout: 'gpt',
    filesystem: 'ext4',
    label: 'GADGETDISK',
    partitions: [
      {
        sizeBytes: 33554432,
        gptType: 'gpt:linux_filesystem',
        mbrType: '',
        name: 'ROOT',
        filesystem: 'ext4',
      },
      { sizeBytes: 0, gptType: 'gpt:microsoft_basic', mbrType: '', name: '', filesystem: 'none' },
    ],
  });
  assert.equal(call.method, 'POST');
  assert.equal(call.path, '/api/v1/create');
  assert.deepEqual(JSON.parse(call.body), {
    path: '/data/adb/gadget-disk/images/a.img',
    size_bytes: 67108864,
    layout: 'gpt',
    filesystem: 'ext4',
    volume_label: 'GADGETDISK',
    partitions: [
      {
        size_bytes: 33554432,
        gpt_type: 'gpt:linux_filesystem',
        name: 'ROOT',
        filesystem: 'ext4',
      },
      // 未给出的 name 不下发；`filesystem: none` 必须**原样下发**，
      // 否则后端会把它当成"未指定"而套上全局默认（真实踩过的坑）。
      { size_bytes: 0, gpt_type: 'gpt:microsoft_basic', filesystem: 'none' },
    ],
  });

  // 缺 layout 时按服务端缺省 GPT；缺 filesystem 时按 FAT32。
  const minimal = JSON.parse(buildRestCall({ op: 'create', path: '/a.img', sizeBytes: 1 }).body);
  assert.equal(minimal.layout, 'gpt');
  assert.equal(minimal.filesystem, 'fat32');
  // 没有分区时不发送 partitions（后端按单分区占满）。
  assert.equal(minimal.partitions, undefined);
  assert.equal(minimal.volume_label, undefined);

  // 缺 path 或容量非数字 → null（不能猜一个容量出来）。
  assert.equal(buildRestCall({ op: 'create', path: '/a.img' }), null);
  assert.equal(buildRestCall({ op: 'create', sizeBytes: 1 }), null);
  assert.equal(buildRestCall({ op: 'create', path: '/a.img', sizeBytes: '64M' }), null);
});

test('buildRestCall：delete', () => {
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'delete', path: '/x.img' }).body), {
    path: '/x.img',
  });
  assert.equal(buildRestCall({ op: 'delete' }), null);
});

test('buildRestCall：分块上传的三个 JSON 端点', () => {
  // begin：带声明大小时下发 size_bytes（仅用于进度与空间预检）。
  const begin = buildRestCall({ op: 'upload-begin', destName: 'b.img', sizeBytes: 4096 });
  assert.equal(begin.method, 'POST');
  assert.equal(begin.path, '/api/v1/upload/begin');
  assert.deepEqual(JSON.parse(begin.body), { dest_name: 'b.img', size_bytes: 4096 });
  assert.equal(buildRestCall({ op: 'upload-begin' }), null);

  // commit / abort 都用 upload_id。
  const commit = buildRestCall({ op: 'upload-commit', uploadId: 'u-1' });
  assert.equal(commit.path, '/api/v1/upload/commit');
  assert.deepEqual(JSON.parse(commit.body), { upload_id: 'u-1' });
  assert.equal(buildRestCall({ op: 'upload-commit' }), null);

  const abort = buildRestCall({ op: 'upload-abort', uploadId: 'u-1' });
  assert.equal(abort.path, '/api/v1/upload/abort');
  assert.deepEqual(JSON.parse(abort.body), { upload_id: 'u-1' });
  assert.equal(buildRestCall({ op: 'upload-abort' }), null);
});

test('buildRestCall：upload-chunk 不在这里（它发原始字节）', () => {
  // 块体是二进制，不能走 `post`（那会 JSON 序列化）。它由 backend.js 的
  // callRestRaw 直接发，因此这里必须返回 null，避免有人误加一条 JSON 映射。
  assert.equal(buildRestCall({ op: 'upload-chunk', uploadId: 'u-1', offset: 0 }), null);
});

test('buildRestCall：begin 的 size_bytes 为 0/缺失时不发送', () => {
  // provider 可能给 0；发送 0 会让服务端把「未知大小」当成「空间预检要求 0 字节」，
  // 因此这种情况干脆不发该字段。
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'upload-begin', destName: 'x' }).body), {
    dest_name: 'x',
  });
  assert.deepEqual(
    JSON.parse(buildRestCall({ op: 'upload-begin', destName: 'x', sizeBytes: 0 }).body),
    { dest_name: 'x' },
  );
});

test('buildRestCall：mount 发送多 LUN 的 MountRequest', () => {
  const call = buildRestCall({
    op: 'mount',
    devices: [
      { image_path: '/data/x.img', mode: 'cdrom' },
      { image_path: '/data/y.img', mode: 'rw', lun: 1, inquiry_string: 'DISK' },
    ],
  });
  assert.equal(call.path, '/api/v1/mount');
  assert.deepEqual(JSON.parse(call.body), {
    devices: [
      { image_path: '/data/x.img', mode: 'cdrom' },
      { image_path: '/data/y.img', mode: 'rw', lun: 1, inquiry_string: 'DISK' },
    ],
  });
});

test('buildRestCall：mount 的缺省与校验', () => {
  // mode 缺省 rw。
  const single = JSON.parse(
    buildRestCall({ op: 'mount', devices: [{ image_path: '/x.img' }] }).body,
  );
  assert.equal(single.devices[0].mode, 'rw');

  // 无设备 / 空路径 / 非法 mode → 不发送。
  assert.equal(buildRestCall({ op: 'mount', devices: [] }), null);
  assert.equal(buildRestCall({ op: 'mount' }), null);
  assert.equal(buildRestCall({ op: 'mount', devices: [{ image_path: '' }] }), null);
  assert.equal(
    buildRestCall({ op: 'mount', devices: [{ image_path: '/x.img', mode: 'nope' }] }),
    null,
  );

  // 同一镜像不得出现在两个 LUN（内核侧也会拒，这里提前拦）。
  assert.equal(
    buildRestCall({
      op: 'mount',
      devices: [{ image_path: '/x.img' }, { image_path: '/x.img' }],
    }),
    null,
  );
});

test('buildRestCall：mount 的 lun 与 inquiry 边界', () => {
  const ok = (device) => buildRestCall({ op: 'mount', devices: [device] });
  // lun 必须是 0..MAX_LUNS-1 的整数。
  assert.equal(ok({ image_path: '/x.img', lun: 0 }).path, '/api/v1/mount');
  assert.equal(ok({ image_path: '/x.img', lun: MAX_LUNS - 1 }).path, '/api/v1/mount');
  assert.equal(ok({ image_path: '/x.img', lun: MAX_LUNS }), null);
  assert.equal(ok({ image_path: '/x.img', lun: -1 }), null);
  assert.equal(ok({ image_path: '/x.img', lun: 'x' }), null);

  // inquiry_string 上限 28（内核 "%-28s" 定长，超长静默截断）。
  assert.equal(ok({ image_path: '/x.img', inquiry_string: 'a'.repeat(28) }).path, '/api/v1/mount');
  assert.equal(ok({ image_path: '/x.img', inquiry_string: 'a'.repeat(29) }), null);
  // 空串等价于「不设置」，不写进请求体。
  const body = JSON.parse(ok({ image_path: '/x.img', inquiry_string: '' }).body);
  assert.equal('inquiry_string' in body.devices[0], false);
});

test('buildRestCall：rebind 只在显式要求时出现', () => {
  const plain = JSON.parse(buildRestCall({ op: 'mount', devices: [{ image_path: '/x.img' }] }).body);
  assert.equal('rebind' in plain, false);

  const rebind = JSON.parse(
    buildRestCall({ op: 'mount', devices: [{ image_path: '/x.img' }], rebind: true }).body,
  );
  assert.equal(rebind.rebind, true);
});

test('buildRestCall：config 的读写与校验', () => {
  assert.equal(buildRestCall({ op: 'config' }).path, '/api/v1/config');

  const saved = buildRestCall({
    op: 'config-set',
    identity: {
      id_vendor: 0x18d1,
      id_product: 0x4ee7,
      manufacturer: 'GadgetDisk',
      product: 'GD Storage',
      serial: 'ABC123',
    },
  });
  assert.equal(saved.path, '/api/v1/config');
  assert.deepEqual(JSON.parse(saved.body), {
    id_vendor: 0x18d1,
    id_product: 0x4ee7,
    manufacturer: 'GadgetDisk',
    product: 'GD Storage',
    serial: 'ABC123',
  });

  // 空身份没有意义。
  assert.equal(buildRestCall({ op: 'config-set', identity: {} }), null);
  // VID/PID 越界。
  assert.equal(buildRestCall({ op: 'config-set', identity: { id_vendor: 0x10000 } }), null);
  assert.equal(buildRestCall({ op: 'config-set', identity: { id_vendor: -1 } }), null);
  // 超长（127 个 ASCII 字符 = 127 字节 > 126）。
  assert.equal(
    buildRestCall({ op: 'config-set', identity: { product: 'a'.repeat(127) } }),
    null,
  );
  // 只设一个字段是合法的（只改产品名不该顺带改 VID）。
  assert.equal(
    buildRestCall({ op: 'config-set', identity: { product: 'X' } }).path,
    '/api/v1/config',
  );
});

test('身份：制造商与产品名允许中文（UTF-8）', () => {
  // 内核经 utf8s_to_utf16s 正确转 UTF-16LE，因此中文产品名必须能设进去。
  // 回归：M10 曾对全部字段拒绝非 ASCII，中文产品名根本设不了。
  const call = buildRestCall({ op: 'config-set', identity: { product: '磁盘' } });
  assert.equal(call.path, '/api/v1/config');
  assert.deepEqual(JSON.parse(call.body), { product: '磁盘' });

  const both = buildRestCall({
    op: 'config-set',
    identity: { manufacturer: '中文制造商', product: '移动存储设备' },
  });
  assert.deepEqual(JSON.parse(both.body), {
    manufacturer: '中文制造商',
    product: '移动存储设备',
  });

  // CLI 回退通道同样要带上中文（shellQuote 负责转义）。
  assert.equal(
    buildCliArgs({ op: 'config-set', identity: { product: '磁盘' } }),
    "config set --product '磁盘'",
  );
});

test('身份：序列号仍只接受可打印 ASCII', () => {
  // 真机实测：非 ASCII 序列号会让电脑连不上设备，因此这是**唯一的字段级例外**。
  assert.equal(buildRestCall({ op: 'config-set', identity: { serial: '序列号' } }), null);
  assert.equal(buildRestCall({ op: 'config-set', identity: { serial: 'ABC123' } }).path, '/api/v1/config');
  // 中文产品名可以，但同一份身份里的中文序列号必须被拒——防止「统一规则」把它一起去掉。
  assert.equal(
    buildRestCall({ op: 'config-set', identity: { product: '磁盘', serial: '序号' } }),
    null,
  );
});

test('身份：长度按 UTF-8 字节算，不按字符数', () => {
  // 42 个汉字 = 126 字节（正好在上限）→ 通过。
  const atLimit = '磁'.repeat(42);
  assert.equal(utf8ByteLength(atLimit), 126);
  assert.equal(buildRestCall({ op: 'config-set', identity: { product: atLimit } }).path, '/api/v1/config');
  // 43 个 = 129 字节 → 拒绝。AVD 实测内核正是在这条边界上返回 rc=1。
  assert.equal(buildRestCall({ op: 'config-set', identity: { product: '磁'.repeat(43) } }), null);
  // 127 个 ASCII 字符同样是 127 字节 → 拒绝。
  assert.equal(buildRestCall({ op: 'config-set', identity: { product: 'a'.repeat(127) } }), null);
  // 边界值本身要通过（防止 off-by-one 把 126 也拒了）。
  assert.equal(buildRestCall({ op: 'config-set', identity: { product: 'a'.repeat(126) } }).path, '/api/v1/config');
});

test('身份：控制字符被拒（内核会剥换行、在 NUL 处截断）', () => {
  // 内嵌控制字符必须被拒：内核 usb_string_copy 会剥掉**尾部**换行、在 NUL 处
  // 截断 C 串，两者都会让写入值与读回值不一致而报「身份无法应用」。
  for (const bad of ['a\nb', 'a\tb', 'a\u0000b', 'a\u007fb']) {
    assert.equal(
      buildRestCall({ op: 'config-set', identity: { product: bad } }),
      null,
      `${JSON.stringify(bad)} 应被拒`,
    );
  }
  // 但**首尾**空白由表单/`stringField` 先 trim 掉，因此 `'a\n'` 等价于 `'a'`，
  // 是合法的——这不是漏网，而是与输入框的行为一致。
  assert.deepEqual(
    JSON.parse(buildRestCall({ op: 'config-set', identity: { product: 'a\n' } }).body),
    { product: 'a' },
  );

  assert.ok(hasControlChar('a\nb'));
  assert.ok(!hasControlChar('中文 abc'));
});

// ---------------------------------------------------------------- 镜像 SE 标签

test('validateImageContext：拒绝显然写错的上下文（与后端逐条对齐）', () => {
  // 合法值（含两侧空白：由 normalize 先 trim）。
  assert.equal(validateImageContext('u:object_r:media_rw_data_file:s0'), null);
  assert.equal(validateImageContext('  u:object_r:vendor_file:s0  '), null);
  assert.equal(validateImageContext('u:object_r:media_rw_data_file:s0:c0.c1023'), null);

  // 空 / 纯空白。
  assert.match(validateImageContext(''), /不能为空/);
  assert.match(validateImageContext('   '), /不能为空/);
  // 缺 `:`。
  assert.match(validateImageContext('media_rw_data_file'), /u:object_r:/);
  // 内部空白（含制表符与换行）：xattr 会原样写入，内核随后拒绝。
  assert.match(validateImageContext('u:object_r:media rw:s0'), /空白/);
  assert.match(validateImageContext('u:object_r:a\tb:s0'), /空白/);
  assert.match(validateImageContext('a:b\nc:d'), /空白/);
  // 超长。
  assert.match(validateImageContext(`u:object_r:${'x'.repeat(300)}:s0`), /超过/);
});

test('normalizeImageContext：合法才返回 trim 后的值，否则 null', () => {
  assert.equal(
    normalizeImageContext(' u:object_r:media_rw_data_file:s0 '),
    'u:object_r:media_rw_data_file:s0',
  );
  assert.equal(normalizeImageContext('nope'), null);
  assert.equal(normalizeImageContext(''), null);
  assert.equal(normalizeImageContext(undefined), null);
  assert.equal(normalizeImageContext(42), null);
});

test('buildRestCall：config-security 的读写与校验', () => {
  // 读：独立端点（不与身份共用）。
  assert.deepEqual(buildRestCall({ op: 'config-security' }), {
    method: 'GET',
    path: '/api/v1/config/security',
    body: null,
  });

  // 写：合法值进 `image_context`，且只发这一个字段（不得顺带发身份字段，
  // 否则「保存标签」会变成一次身份写入）。
  const saved = buildRestCall({
    op: 'config-security-set',
    imageContext: 'u:object_r:vendor_file:s0',
  });
  assert.equal(saved.path, '/api/v1/config/security');
  assert.deepEqual(JSON.parse(saved.body), { image_context: 'u:object_r:vendor_file:s0' });

  // 恢复默认走显式的 `reset`，而不是「空字符串」——两者对用户的含义不同。
  const reset = buildRestCall({ op: 'config-security-set', reset: true });
  assert.deepEqual(JSON.parse(reset.body), { reset: true });

  // 非法值根本不发送（后端也会拒，但不该让它走一趟往返再报错）。
  assert.equal(buildRestCall({ op: 'config-security-set', imageContext: 'nope' }), null);
  assert.equal(buildRestCall({ op: 'config-security-set', imageContext: '' }), null);
  assert.equal(buildRestCall({ op: 'config-security-set' }), null);
});

test('buildCliArgs：config-security 映射到 config security 子命令', () => {
  assert.equal(buildCliArgs({ op: 'config-security' }), 'config security get');
  assert.equal(
    buildCliArgs({ op: 'config-security-set', imageContext: 'u:object_r:vendor_file:s0' }),
    "config security set --image-context 'u:object_r:vendor_file:s0'",
  );
  assert.equal(buildCliArgs({ op: 'config-security-set', reset: true }), 'config security clear');
  assert.equal(buildCliArgs({ op: 'config-security-set', imageContext: 'nope' }), null);
  // **关键**：绝不映射到 `config set`（那是身份，会把标签写丢）。
  assert.doesNotMatch(
    buildCliArgs({ op: 'config-security-set', imageContext: 'u:object_r:x:s0' }),
    /^config set /,
  );
});

test('utf8ByteLength / isPrintableAscii 的边界', () => {
  assert.equal(utf8ByteLength(''), 0);
  assert.equal(utf8ByteLength('abc'), 3);
  assert.equal(utf8ByteLength('中'), 3);
  assert.equal(utf8ByteLength('é'), 2); // 2 字节
  assert.equal(utf8ByteLength('😀'), 4); // 代理对必须算 4，而不是 2×2 或 2
  assert.equal(utf8ByteLength('a中😀'), 1 + 3 + 4);

  assert.ok(isPrintableAscii('ABC 123 ~'));
  assert.ok(!isPrintableAscii('中'));
  assert.ok(!isPrintableAscii('a\nb'));
  assert.ok(!isPrintableAscii('')); // 空串不是「可打印 ASCII」

  // 校验函数按字段分流：同一段文本，产品名过、序列号不过。
  assert.equal(validateIdentityField('product', '磁盘'), null);
  assert.match(validateIdentityField('serial', '磁盘'), /ASCII/);
  assert.match(validateIdentityField('product', '磁'.repeat(43)), /字节/);
  assert.match(validateIdentityField('product', 'a\nb'), /控制字符/);
});

test('buildRestCall：delete-slot 需要整数 lun', () => {
  const call = buildRestCall({ op: 'delete-slot', lun: 2 });
  assert.equal(call.path, '/api/v1/slot/delete');
  assert.deepEqual(JSON.parse(call.body), { lun: 2 });
  // 非整数不发送（避免把坏值交给后端）。
  assert.equal(buildRestCall({ op: 'delete-slot', lun: 'x' }), null);
  assert.equal(buildRestCall({ op: 'delete-slot' }), null);
});

test('buildCliArgs：delete-slot', () => {
  assert.equal(buildCliArgs({ op: 'delete-slot', lun: 3 }), 'delete-slot --lun 3');
  assert.equal(buildCliArgs({ op: 'delete-slot', lun: 'x' }), null);
});

test('describeSlot 区分已挂载 / 空闲 / 未创建', () => {
  // 已挂载：来自后端，attached 为真。
  assert.deepEqual(describeSlot({ attached: true, deletable: false }), {
    kind: 'mounted',
    deletable: false,
    label: '已挂载',
  });
  // 空闲：来自后端（有 deletable 字段）但 attached 为假 —— LUN 目录还在。
  assert.deepEqual(describeSlot({ attached: false, deletable: true }), {
    kind: 'idle',
    deletable: true,
    label: '空闲',
  });
  // 未创建：本地新加的行没有 deletable 字段。
  assert.deepEqual(describeSlot({}), { kind: 'new', deletable: false, label: '未创建' });
  assert.deepEqual(describeSlot(undefined), {
    kind: 'new',
    deletable: false,
    label: '未创建',
  });
});

test('buildImageOptions 只列出给定镜像并保留占位项', () => {
  const options = buildImageOptions(
    [
      { path: '/data/adb/gadget-disk/images/a.img', size_bytes: 8388608 },
      { path: '/data/adb/gadget-disk/images/b.img', size_bytes: 1048576 },
    ],
    '',
  );
  assert.equal(options.length, 3);
  assert.deepEqual(options[0], { value: '', label: '（未选择）' });
  assert.equal(options[1].value, '/data/adb/gadget-disk/images/a.img');
  assert.match(options[1].label, /^a\.img（/);

  // 无镜像时只有占位项（不报错）。
  assert.deepEqual(buildImageOptions([], ''), [{ value: '', label: '（未选择）' }]);
  assert.deepEqual(buildImageOptions(undefined, undefined), [
    { value: '', label: '（未选择）' },
  ]);
});

test('buildImageOptions 在镜像不在列表时插入「文件不存在」项', () => {
  // 场景：intent 里记的镜像已被删。必须让用户看见，而不是静默清空。
  const options = buildImageOptions(
    [{ path: '/data/adb/gadget-disk/images/a.img', size_bytes: 8388608 }],
    '/data/adb/gadget-disk/images/gone.img',
  );
  const last = options[options.length - 1];
  assert.equal(last.value, '/data/adb/gadget-disk/images/gone.img');
  assert.equal(last.missing, true);
  assert.match(last.label, /文件不存在/);
});

test('buildImageOptions 去重且忽略空路径', () => {
  const options = buildImageOptions(
    [
      { path: '/x/a.img' },
      { path: '/x/a.img' },
      { path: '' },
      null,
      { path: '/x/b.img' },
    ],
    '',
  );
  const values = options.map((o) => o.value);
  assert.deepEqual(values, ['', '/x/a.img', '/x/b.img']);
});

test('mergeSlotRows 把后端槽位置前、本地新行置后', () => {
  const rows = mergeSlotRows(
    [
      { index: 1, image_path: '/x/b.img', attached: true, deletable: true },
      { index: 0, image_path: '', attached: false, deletable: false },
    ],
    [{ image_path: '/x/c.img' }],
  );
  assert.equal(rows.length, 3);
  // 后端按序号升序。
  assert.equal(rows[0].index, 0);
  assert.equal(rows[1].index, 1);
  // 本地行在最后且被标记。
  assert.equal(rows[2].local, true);
  assert.equal(rows[0].local, false);
});

test('buildRestCall：unmount 的 lun 缺省表示全部', () => {
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'unmount' }).body), {});
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'unmount', lun: 0 }).body), { lun: 0 });
  // 非法 lun 不发送，等价于「全部卸载」而不是发一个坏值。
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'unmount', lun: 'x' }).body), {});
});

test('buildRestCall：attach-loop 的只读与分区', () => {
  const call = buildRestCall({ op: 'attach-loop', image: '/data/x.img' });
  assert.equal(call.path, '/api/v1/loop/attach');
  assert.deepEqual(JSON.parse(call.body), {
    image: '/data/x.img',
    read_only: false,
    partition_index: null,
  });

  const ro = JSON.parse(
    buildRestCall({ op: 'attach-loop', image: '/x.img', readOnly: true, partition: 1 }).body,
  );
  assert.deepEqual(ro, { image: '/x.img', read_only: true, partition_index: 1 });

  // mode=ro 与 read_only=true 等价，两条都表达只读。
  assert.equal(
    JSON.parse(buildRestCall({ op: 'attach-loop', image: '/x.img', mode: 'ro' }).body).read_only,
    true,
  );
  assert.equal(buildRestCall({ op: 'attach-loop', mode: 'rw' }), null);
});

test('buildRestCall：detach-loop 至少要有 image 或 loop_dev', () => {
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'detach-loop', image: '/x.img' }).body), {
    image: '/x.img',
    loop_dev: null,
  });
  assert.deepEqual(JSON.parse(buildRestCall({ op: 'detach-loop', loopDev: 'loop7' }).body), {
    image: null,
    loop_dev: 'loop7',
  });
  assert.equal(buildRestCall({ op: 'detach-loop' }), null);
});

test('buildRestCall：未知 op 与非对象输入返回 null', () => {
  for (const bad of [{ op: 'nope' }, {}, null, undefined, 'status', 42]) {
    assert.equal(buildRestCall(bad), null, `输入 ${JSON.stringify(bad)}`);
  }
});

// ---------------------------------------------------------------- buildCliArgs（回退路径）

test('buildCliArgs：与 REST 共用同一份字段定义', () => {
  // 回退路径必须仍然可用：这是 REST 不可用时的安全网。
  assert.equal(buildCliArgs({ op: 'status' }), 'status');
  assert.equal(buildCliArgs({ op: 'list-loop' }), 'list-loop');
  assert.equal(buildCliArgs({ op: 'df', path: '/data' }), "df '/data'");
  assert.equal(buildCliArgs({ op: 'unmount' }), 'unmount');
  assert.equal(buildCliArgs({ op: 'unmount', lun: 2 }), 'unmount --lun 2');
});

test('buildCliArgs：路径被单引号转义（含单引号本身）', () => {
  assert.equal(buildCliArgs({ op: 'delete', path: '/a b/c.img' }), "delete '/a b/c.img'");
  // 含单引号的路径必须转义，否则会破坏命令。
  assert.equal(buildCliArgs({ op: 'delete', path: "/a'b.img" }), `delete '/a'\\''b.img'`);
});

test('buildCliArgs：create 携带卷标、文件系统与分区', () => {
  const args = buildCliArgs({
    op: 'create',
    path: '/x.img',
    sizeBytes: 67108864,
    layout: 'mbr',
    filesystem: 'exfat',
    label: 'MY VOL',
    partitions: [
      {
        sizeBytes: 33554432,
        gptType: 'gpt:microsoft_basic',
        mbrType: 'mbr:ntfs_exfat',
        name: 'DATA',
        filesystem: 'exfat',
      },
    ],
  });
  // `/` 分隔：类型线格式自带 `gpt:`/`mbr:` 前缀，冒号不能既当分隔符又当前缀。
  // MBR 布局下只下发 MBR 类型，GPT 段留空。
  assert.equal(
    args,
    "create '/x.img' --size 67108864 --layout 'mbr' --filesystem 'exfat' " +
      "--label 'MY VOL' --partition '33554432//mbr:ntfs_exfat/DATA/exfat/'",
  );
  // 未给卷标/文件系统时让 CLI 用自身的缺省值。
  const minimal = buildCliArgs({ op: 'create', path: '/x.img', sizeBytes: 1 });
  assert.doesNotMatch(minimal, /--label/);
  assert.doesNotMatch(minimal, /--filesystem/);
  assert.doesNotMatch(minimal, /--partition/);
});

test('buildCliArgs：空段必须保留占位', () => {
  // 空段表示"用默认值"，省略它会让后面的段整体前移、语义错位。
  const args = buildCliArgs({
    op: 'create',
    path: '/x.img',
    sizeBytes: 1024,
    layout: 'gpt',
    partitions: [{ sizeBytes: 512, gptType: '', mbrType: '', name: 'BOOT', filesystem: '' }],
  });
  // 6 段（第 6 段是归属，空 = 主分区）。
  assert.match(args, /--partition '512\/\/\/BOOT\/\/'/);
});

test('buildCliArgs：分块上传没有 CLI 等价物', () => {
  // 上传只有 REST 通道：`ksu.exec` 无法向子进程写 stdin，一次性进程也承载不了
  // 「多个 chunk + 一个 commit」这组跨请求状态。必须返回 null，让调用方走
  // 「该操作没有回退通道」分支给出可读提示，而不是拼一条必然失败的命令。
  for (const op of ['upload-begin', 'upload-chunk', 'upload-commit', 'upload-abort']) {
    assert.equal(buildCliArgs({ op, uploadId: 'u-1', destName: 'x.img', offset: 0 }), null);
  }
});

test('buildCliArgs：mount / loop', () => {
  assert.equal(
    buildCliArgs({ op: 'mount', devices: [{ image_path: '/x.img', mode: 'ro' }] }),
    "mount '/x.img' --mode 'ro'",
  );
  // 多设备：位置参数是镜像，其余选项**按下标对齐**（与 CLI 契约一致）。
  assert.equal(
    buildCliArgs({
      op: 'mount',
      devices: [
        { image_path: '/a.img', mode: 'rw' },
        { image_path: '/b.iso', mode: 'cdrom', lun: 1, inquiry_string: 'ISO' },
      ],
    }),
    "mount '/a.img' '/b.iso' --mode 'rw' --mode 'cdrom' --lun 1 --inquiry 'ISO'",
  );
  assert.equal(buildCliArgs({ op: 'mount', devices: [] }), null);
  assert.equal(
    buildCliArgs({ op: 'attach-loop', image: '/x.img', mode: 'rw', partition: 1, readOnly: true }),
    "attach-loop '/x.img' --mode 'ro' --partition 1 --read-only",
  );
  assert.equal(
    buildCliArgs({ op: 'detach-loop', loopDev: 'loop7' }),
    "detach-loop --loop-dev 'loop7'",
  );
  // 缺必填字段时返回 null，由调用方给出可读错误。
  assert.equal(buildCliArgs({ op: 'attach-loop' }), null);
  assert.equal(buildCliArgs({ op: 'detach-loop' }), null);
  assert.equal(buildCliArgs({ op: 'unknown' }), null);
});

// ---------------------------------------------------------------- shellQuote

test('shellQuote：单引号被正确转义', () => {
  assert.equal(shellQuote('/a b.img'), "'/a b.img'");
  assert.equal(shellQuote("a'b"), `'a'\\''b'`);
});

// ---------------------------------------------------------------- restResultToExecResult

test('REST 结果转换：2xx 有响应体即成功，body 进 stdout', () => {
  const result = restResultToExecResult(200, '{"udc":null,"devices":[]}');
  assert.equal(result.errno, 0);
  assert.equal(result.stdout, '{"udc":null,"devices":[]}');
  assert.equal(result.stderr, '');
  // 交给既有的 parseExecResult 即可得到数据。
  assert.equal(parseExecResult(result).ok, true);
});

test('REST 结果转换：错误状态码用非零 errno 且保留响应体', () => {
  for (const status of [400, 401, 404, 409, 500, 507]) {
    const body = '{"error":"image_in_use","message":"image is in use"}';
    const result = restResultToExecResult(status, body);
    assert.notEqual(result.errno, 0, `HTTP ${status}`);
    assert.equal(result.errno, 4, `HTTP ${status} 应使用 ExitCode::Server`);
    // 响应体必须原样保留在 stdout：parseExecResult 从中还原 error/message。
    assert.equal(result.stdout, body);
    const parsed = parseExecResult(result);
    assert.equal(parsed.ok, false);
    assert.equal(parsed.kind, 'backend_error');
    assert.equal(parsed.code, 'image_in_use');
    assert.equal(parsed.message, messageForCode('image_in_use'));
  }
});

test('REST 结果转换：2xx 但空响应体不算成功', () => {
  for (const empty of ['', '   ', '\n']) {
    const result = restResultToExecResult(200, empty);
    assert.notEqual(result.errno, 0, JSON.stringify(empty));
    // 失败原因要可读：不能只留一个「exit code 4」让用户猜。
    const parsed = parseExecResult(result);
    assert.equal(parsed.ok, false);
    assert.match(parsed.detail, /empty body/);
  }
});

test('REST 结果转换：网络层失败用 errno 3', () => {
  const result = restResultToExecResult(0, '');
  assert.equal(result.errno, 3);
  assert.equal(result.errno, EXIT_CODE_UNREACHABLE);
  const parsed = parseExecResult(result);
  assert.equal(parsed.ok, false);
  assert.equal(parsed.kind, 'command_failed');
  assert.match(parsed.detail, /REST backend/);
});

test('REST 结果转换：缺参数不抛异常', () => {
  assert.equal(restResultToExecResult(undefined, undefined).errno, 3);
  assert.equal(restResultToExecResult(500, undefined).errno, 4);
});

// ---------------------------------------------------------------- REST 地址

test('REST 地址使用 IPv4 字面量而不是回环主机名', () => {
  // 实测：主机名会解析到 IPv6 ::1，而 serve 只绑定 IPv4 回环 → Failed to fetch。
  assert.equal(REST_HOST, '127.0.0.1');
  assert.equal(restUrl({ port: 39481 }, '/api/v1/status'), 'http://127.0.0.1:39481/api/v1/status');
  assert.equal(restUrl({ port: 8080 }, '/x'), 'http://127.0.0.1:8080/x');
});

// ---------------------------------------------------------------- 任务反馈（Task 1）
//
// 操作本身是毫秒级的（mount≈17ms、attach-loop≈115ms、create 64MiB≈158ms），
// 缺的是**反馈**：await 期间界面必须显示「已用多久」，否则用户会重复点击。

test('describeTaskElapsed：1 秒以下保留一位小数', () => {
  assert.equal(describeTaskElapsed(0), '0.0s');
  assert.equal(describeTaskElapsed(400), '0.4s');
  assert.equal(describeTaskElapsed(17), '0.0s');
  assert.equal(describeTaskElapsed(999), '1.0s');
});

test('describeTaskElapsed：1 秒及以上用整秒', () => {
  assert.equal(describeTaskElapsed(1000), '1s');
  assert.equal(describeTaskElapsed(3200), '3s');
  assert.equal(describeTaskElapsed(65000), '65s');
});

test('describeTaskElapsed：负数与非数字都夹到 0，不产出 NaN', () => {
  for (const bad of [-1, -1000, Number.NaN, Number.POSITIVE_INFINITY, null, undefined, 'x']) {
    assert.equal(describeTaskElapsed(bad), '0.0s', `输入 ${String(bad)}`);
  }
});

test('shouldWarnSlow：阈值默认 3000ms 且严格大于才算慢', () => {
  assert.equal(SLOW_TASK_THRESHOLD_MS, 3000);
  assert.equal(shouldWarnSlow(0), false);
  assert.equal(shouldWarnSlow(3000), false, '恰好等于阈值不算慢');
  assert.equal(shouldWarnSlow(3001), true);
  assert.equal(shouldWarnSlow(5000), true);
  // 显式阈值与非法输入。
  assert.equal(shouldWarnSlow(100, 50), true);
  assert.equal(shouldWarnSlow(100, 500), false);
  assert.equal(shouldWarnSlow(Number.NaN), false);
});

test('taskProgressLabel：包含动作名与已用时间', () => {
  assert.equal(taskProgressLabel('挂载', 400), '正在挂载…（已用 0.4s）');
  assert.equal(taskProgressLabel('创建镜像', 0), '正在创建镜像…（已用 0.0s）');
  // 空/非字符串动作名有兜底，不产出「正在…」这种空动作。
  assert.equal(taskProgressLabel('', 0), '正在处理…（已用 0.0s）');
  assert.equal(taskProgressLabel(null, 0), '正在处理…（已用 0.0s）');
});

test('taskProgressLabel：慢任务追加「请勿关闭页面」提示', () => {
  const fast = taskProgressLabel('挂载', 2999);
  assert.doesNotMatch(fast, /请勿关闭页面/);
  const slow = taskProgressLabel('挂载', 3001);
  assert.match(slow, /正在挂载…/);
  assert.match(slow, /已用 3s/);
  assert.match(slow, /仍在进行，请勿关闭页面/);
});

// ---------------------------------------------------------------- 后端状态机（Task 2）

test('classifyBackendFailure：只有通道级失败算离线', () => {
  assert.equal(classifyBackendFailure({ kind: 'unreachable' }), 'offline');
  assert.equal(classifyBackendFailure({ code: 'gdd_unreachable' }), 'offline');
  // 业务错误说明后端活着并正常答复：绝不能据此暂停界面。
  for (const code of ['image_in_use', 'no_space', 'busy', 'invalid_argument', 'permission_denied']) {
    assert.equal(classifyBackendFailure({ kind: 'backend_error', code }), 'online', code);
  }
  // 其它失败类型（空输出 / 坏 JSON / 命令失败）不是通道级。
  for (const kind of ['empty_output', 'bad_json', 'command_failed', 'internal']) {
    assert.equal(classifyBackendFailure({ kind }), 'online', kind);
  }
});

test('classifyBackendFailure：CLI 退出码 3 / -1 也算离线（实测回归）', () => {
  // 回归：这两条正是「CLI 回退也走不通」时的真实形态。早期分类器只认
  // kind/code，于是两条通道全断时界面仍显示「在线」——离线横幅永不出现，
  // 变更按钮停在「正在…」。实测于 AVD。
  assert.equal(classifyBackendFailure({ kind: 'command_failed', errno: 3 }), 'offline');
  assert.equal(classifyBackendFailure({ kind: 'command_failed', errno: -1 }), 'offline');
  assert.equal(classifyBackendFailure({ kind: 'backend_error', code: 'gdd_unreachable', errno: 3 }), 'offline');
  // 其它非零退出码（用法错误 2 / 服务端业务错误 4）不算通道级。
  assert.equal(classifyBackendFailure({ kind: 'command_failed', errno: 2 }), 'online');
  assert.equal(classifyBackendFailure({ kind: 'command_failed', errno: 4 }), 'online');
});

test('解析：CLI 的 gdd_unreachable 写在 stderr 也必须被识别（实测回归）', () => {
  // 回归：CLI 的错误输出走 stderr（output::JsonOutput 的 to_stderr = true）。
  // 早期 parseExecResult 只解析 stdout，于是通道级错误码被降级成 command_failed，
  // 离线状态机永远不触发。
  const result = parseExecResult({
    errno: 3,
    stdout: '',
    stderr: '{"error":"gdd_unreachable","message":"cannot connect to gdd (/x.sock): Connection refused"}',
  });
  assert.equal(result.ok, false);
  assert.equal(result.kind, 'backend_error');
  assert.equal(result.code, 'gdd_unreachable');
  // 标题取本地中文映射；后端英文原文进 detail。
  assert.equal(result.message, messageForCode('gdd_unreachable'));
  assert.match(result.detail, /cannot connect to gdd/);
  // 端到端：解析结果必须被判为离线。
  assert.equal(classifyBackendFailure(result), 'offline');
});

test('解析：stderr 混有 shell 警告时仍能提取错误 JSON', () => {
  const result = parseExecResult({
    errno: 3,
    stdout: '',
    stderr: 'sh: some warning\n{"error":"gdd_unreachable","message":"无法连接"}\ntrailing',
  });
  assert.equal(result.code, 'gdd_unreachable');
  assert.equal(classifyBackendFailure(result), 'offline');
});

test('解析：stdout 优先于 stderr（成功路径的数据只在 stdout）', () => {
  const result = parseExecResult({
    errno: 4,
    stdout: '{"error":"image_not_found","message":"镜像不存在"}',
    stderr: '{"error":"gdd_unreachable","message":"干扰"}',
  });
  assert.equal(result.code, 'image_not_found');
  // 业务错误不得被 stderr 的通道级错误覆盖成离线。
  assert.equal(classifyBackendFailure(result), 'online');
});

test('解析：命令失败时保留 errno 供分类器使用', () => {
  const unreachable = parseExecResult({ errno: 3, stdout: '', stderr: 'Connection refused' });
  assert.equal(unreachable.errno, 3);
  assert.equal(classifyBackendFailure(unreachable), 'offline');

  const ksuGone = parseExecResult({ errno: -1, stdout: '', stderr: 'ksu 不可用' });
  assert.equal(ksuGone.errno, -1);
  assert.equal(classifyBackendFailure(ksuGone), 'offline');

  const business = parseExecResult({ errno: 4, stdout: '', stderr: 'boom' });
  assert.equal(business.errno, 4);
  assert.equal(classifyBackendFailure(business), 'online');
});

test('classifyBackendFailure：非法输入不抛异常且不算离线', () => {
  for (const bad of [null, undefined, 'offline', 42, [], { code: null }]) {
    assert.equal(classifyBackendFailure(bad), 'online', JSON.stringify(bad));
  }
});

test('shouldBlockActions：只有 offline 才暂停操作', () => {
  assert.equal(shouldBlockActions('offline'), true);
  assert.equal(shouldBlockActions('online'), false);
  assert.equal(shouldBlockActions('unknown'), false);
  assert.equal(shouldBlockActions(''), false);
  assert.equal(shouldBlockActions(undefined), false);
});

test('nextReconnectDelay：指数退避且封顶', () => {
  assert.equal(RECONNECT_BASE_MS, 5000);
  assert.equal(RECONNECT_MAX_MS, 30000);
  // failures<=0 视为第一次。
  assert.equal(nextReconnectDelay(0), RECONNECT_BASE_MS);
  assert.equal(nextReconnectDelay(-3), RECONNECT_BASE_MS);
  assert.equal(nextReconnectDelay(1), 5000);
  assert.equal(nextReconnectDelay(2), 10000);
  assert.equal(nextReconnectDelay(3), 20000);
  // 封顶：绝不超过 max。
  assert.equal(nextReconnectDelay(4), 30000);
  assert.equal(nextReconnectDelay(100), 30000);
});

test('nextReconnectDelay：非法输入与自定义参数', () => {
  assert.equal(nextReconnectDelay(Number.NaN), RECONNECT_BASE_MS);
  assert.equal(nextReconnectDelay(2, 100, 500), 200);
  assert.equal(nextReconnectDelay(9, 100, 500), 500);
  // 非法 base/max 回落到默认值，不产出 NaN 延迟（NaN 传给 setTimeout 会立刻触发，
  // 等于把退避变成忙等重试风暴）。
  assert.equal(nextReconnectDelay(1, 0, 0), RECONNECT_BASE_MS);
  assert.equal(nextReconnectDelay(1, Number.NaN, Number.NaN), RECONNECT_BASE_MS);
  assert.ok(Number.isFinite(nextReconnectDelay(50)));
});

// ---------------------------------------------------------------- 分区（Task 3）

test('describePartition：序号 · 容量 · 类型', () => {
  assert.equal(
    describePartition({ index: 1, size_bytes: 66043392, type_label: '基本数据分区' }),
    '分区 1 · 63 MiB · 基本数据分区',
  );
  // 缺字段时降级但仍有可读文案。
  assert.equal(describePartition({ index: 2 }), '分区 2');
  assert.equal(describePartition({ index: 2, size_bytes: 1024 }), '分区 2 · 1 KiB');
  assert.equal(describePartition(null), '分区');
  assert.equal(describePartition({}), '分区 ?');
});

test('buildPartitionOptions：整盘恒在首位，有分区表时默认选中 default_index', () => {
  const scan = {
    layout: 'gpt',
    default_index: 1,
    partitions: [
      { index: 1, size_bytes: 66043392, type_label: '基本数据分区' },
      { index: 3, size_bytes: 1024 * 1024, type_label: 'EFI 系统分区' },
    ],
  };
  const options = buildPartitionOptions(scan);
  assert.equal(options.length, 3);
  assert.deepEqual(options[0], { value: '', label: '整盘（无分区表）', selected: false });
  assert.equal(options[1].value, '1');
  assert.equal(options[1].selected, true);
  assert.equal(options[2].value, '3');
  assert.equal(options[2].selected, false);
});

test('buildPartitionOptions：无分区表时只有整盘且默认选中', () => {
  for (const scan of [{ layout: 'raw', partitions: [], default_index: null }, {}, null, undefined]) {
    const options = buildPartitionOptions(scan);
    assert.equal(options.length, 1, JSON.stringify(scan));
    assert.deepEqual(options[0], { value: '', label: '整盘（无分区表）', selected: true });
  }
});

test('buildPartitionOptions：default_index 缺失或不在列表里时仍有一个选中项', () => {
  const noDefault = buildPartitionOptions({
    partitions: [{ index: 1, size_bytes: 1024, type_label: 'x' }],
    default_index: null,
  });
  assert.equal(noDefault.filter((o) => o.selected).length, 1);
  assert.equal(noDefault[0].selected, true);

  const dangling = buildPartitionOptions({
    partitions: [{ index: 1, size_bytes: 1024, type_label: 'x' }],
    default_index: 9,
  });
  assert.equal(dangling.filter((o) => o.selected).length, 1);
});

test('buildPartitionOptions：跳过没有整数序号的条目', () => {
  const options = buildPartitionOptions({
    partitions: [{ index: 'x' }, { index: 2, size_bytes: 1024, type_label: 'y' }, null],
    default_index: 2,
  });
  assert.deepEqual(
    options.map((o) => o.value),
    ['', '2'],
  );
});

test('formatPartitionScan：GPT/MBR 摘要与无分区表', () => {
  assert.equal(
    formatPartitionScan({ layout: 'gpt', partitions: [{ index: 1 }] }),
    'GPT · 1 个分区',
  );
  assert.equal(
    formatPartitionScan({ layout: 'mbr', partitions: [{ index: 1 }, { index: 2 }] }),
    'MBR · 2 个分区',
  );
  assert.equal(formatPartitionScan({ layout: 'raw', partitions: [] }), '无分区表（整盘）');
  assert.equal(formatPartitionScan({ layout: 'gpt', partitions: [] }), '无分区表（整盘）');
  assert.equal(formatPartitionScan(null), '无分区表（整盘）');
});

test('buildRestCall：image-partitions 映射到带编码查询串的 GET', () => {
  const call = buildRestCall({
    op: 'image-partitions',
    path: '/data/adb/gadget-disk/images/p.img',
  });
  assert.deepEqual(call, {
    method: 'GET',
    path: '/api/v1/image/partitions?path=%2Fdata%2Fadb%2Fgadget-disk%2Fimages%2Fp.img',
    body: null,
  });
  // 中文目录名与空格必须编码。
  assert.match(buildRestCall({ op: 'image-partitions', path: '/a b/中文.img' }).path, /%20/);
  // 缺 path → null。
  assert.equal(buildRestCall({ op: 'image-partitions' }), null);
  assert.equal(buildRestCall({ op: 'image-partitions', path: '' }), null);
});

test('buildCliArgs：只有 REST 通道的操作必须返回 null', () => {
  // 返回 null 而不是拼一条必然「参数不合法」的命令；app.js 据此给出
  // 「该操作需要 REST 后端」。
  //
  // `image-partitions`：`gadgetdisk` 没有该子命令。
  assert.equal(buildCliArgs({ op: 'image-partitions', path: '/a.img' }), null);
  // `job`：job 注册表是 `serve` 进程内的内存，CLI 是一次性进程——既没有
  // `job` 子命令（已删除），也不可能查到别的进程登记的 job。
  assert.equal(buildCliArgs({ op: 'job', jobId: 'j1' }), null);
});


// ---------------------------------------------------------------- 分区与文件系统

/** 构造一个合法分区行（测试用）。 */
function prow(over = {}) {
  return {
    sizeBytes: 1e6,
    gptType: 'gpt:microsoft_basic',
    gptTypeCustom: '',
    mbrType: 'mbr:fat32_lba',
    mbrTypeCustom: '',
    name: '',
    filesystem: '',
    ...over,
  };
}

/**
 * 一个扩展分区**容器**行。
 *
 * 容器没有数据区，因此默认不带文件系统（`none`）——这与界面的行为一致：
 * 容器行上不渲染文件系统选择器。
 */
function ecrow(over = {}) {
  return prow({ kind: 'extended', filesystem: 'none', ...over });
}

test('布局能力：只有 gpt/mbr 支持分区，只有 gpt 支持分区名', () => {
  assert.equal(layoutSupportsPartitions('gpt'), true);
  assert.equal(layoutSupportsPartitions('mbr'), true);
  assert.equal(layoutSupportsPartitions('raw'), false);

  // MBR 没有分区名字段——UI 据此禁用输入，而不是静默丢弃。
  assert.equal(layoutSupportsPartitionNames('gpt'), true);
  assert.equal(layoutSupportsPartitionNames('mbr'), false);
  assert.equal(layoutSupportsPartitionNames('raw'), false);
});

test('分区数上限与后端一致', () => {
  // 这几个值必须与 Rust 侧 partspec::max_total_partitions 相同。
  // MBR 给的是**总数**上界：3 主 + 最多 64 逻辑（逻辑分区共用一个扩展容器槽位）。
  assert.equal(MAX_PARTITIONS.gpt, 128);
  assert.equal(MAX_PARTITIONS.mbr, MBR_MAX_PRIMARY - 1 + 64);
  assert.equal(MAX_PARTITIONS.raw, 1);
  // 主分区槽位数是独立常量：UI 靠它判断"再加一个就必须是逻辑分区了"。
  assert.equal(MBR_MAX_PRIMARY, 4);
  // 扩展容器最多一个（首扇区里只有一个这样的项可写）。
  assert.equal(MBR_MAX_EXTENDED, 1);
});

test('describeFilesystem / describePartitionType 给出可读文案', () => {
  assert.match(describeFilesystem('fat32'), /FAT32/);
  assert.match(describeFilesystem('exfat'), /exFAT/);
  assert.match(describeFilesystem('ext4'), /ext4/);
  assert.equal(describeFilesystem('none'), '不格式化');
  assert.equal(describeFilesystem('nope'), '未知文件系统');

  assert.equal(describeGptPartitionType('gpt:linux_filesystem'), 'Linux filesystem');
  assert.equal(describeGptPartitionType('gpt:efi_system'), 'EFI System');
  assert.equal(describeMbrPartitionType('mbr:fat32_lba'), 'FAT32 (LBA)');
  assert.equal(describeMbrPartitionType('mbr:linux'), 'Linux');

  // 按布局分发。
  assert.equal(
    describePartitionType('mbr', 'gpt:linux_filesystem', 'mbr:linux'),
    'Linux',
  );
  assert.equal(
    describePartitionType('gpt', 'gpt:linux_filesystem', 'mbr:linux'),
    'Linux filesystem',
  );
});

test('defaultPartitionType 按布局分域', () => {
  // 两套类型空间独立：同一个文件系统在 GPT 与 MBR 下的默认值不同。
  assert.equal(defaultPartitionType('gpt', 'fat32'), 'gpt:microsoft_basic');
  assert.equal(defaultPartitionType('gpt', 'ext4'), 'gpt:linux_filesystem');
  assert.equal(defaultPartitionType('gpt', 'exfat'), 'gpt:microsoft_basic');

  assert.equal(defaultPartitionType('mbr', 'fat32'), 'mbr:fat32_lba');
  assert.equal(defaultPartitionType('mbr', 'ext4'), 'mbr:linux');
  assert.equal(defaultPartitionType('mbr', 'exfat'), 'mbr:ntfs_exfat');
});

test('预设类型列表按布局切换且各自非空', () => {
  assert.ok(GPT_PARTITION_TYPES.length > 0);
  assert.ok(MBR_PARTITION_TYPES.length > 0);
  // 两套集合不重叠：它们属于不同的类型空间。
  for (const t of GPT_PARTITION_TYPES) assert.ok(t.startsWith('gpt:'), t);
  for (const t of MBR_PARTITION_TYPES) assert.ok(t.startsWith('mbr:'), t);
});

test('validatePartitions：raw 布局不需要分区', () => {
  assert.equal(validatePartitions([], { layout: 'raw', imageBytes: 1e9 }).ok, true);
});

test('validatePartitions：至少一个分区', () => {
  const result = validatePartitions([], { layout: 'gpt', imageBytes: 1e9 });
  assert.equal(result.ok, false);
  assert.match(result.message, /至少需要一个分区/);
});

test('mbrSlotUsage：主分区各占一槽，逻辑分区共用一个扩展容器槽位', () => {
  // 4 主：用满 4 个槽位。
  const fourPrimary = mbrSlotUsage([prow(), prow(), prow(), prow()]);
  assert.equal(fourPrimary.primaries, 4);
  assert.equal(fourPrimary.logicals, 0);
  assert.equal(fourPrimary.neededSlots, 4);
  assert.equal(fourPrimary.usesExtended, false);
  assert.equal(fourPrimary.freeSlots, 0);

  // 3 主 + 3 逻辑：逻辑分区共用一个容器槽位，仍是 4。
  const mixed = mbrSlotUsage([
    prow(),
    prow(),
    prow(),
    prow({ kind: 'logical' }),
    prow({ kind: 'logical' }),
    prow({ kind: 'logical' }),
  ]);
  assert.equal(mixed.primaries, 3);
  assert.equal(mixed.logicals, 3);
  assert.equal(mixed.neededSlots, 4, '逻辑分区共同占一个槽位');
  assert.equal(mixed.usesExtended, true);

  // 1 主 + 2 逻辑：只用了 2 个槽位。
  const small = mbrSlotUsage([prow(), prow({ kind: 'logical' }), prow({ kind: 'logical' })]);
  assert.equal(small.neededSlots, 2);
  assert.equal(small.freeSlots, 2);

  // 缺省 kind 视为主分区（老数据形状）。
  assert.equal(mbrSlotUsage([{ sizeBytes: 1 }]).primaries, 1);
  // 非数组容错。
  assert.equal(mbrSlotUsage(null).neededSlots, 0);
});

test('mbrSlotUsage：扩展容器占一个槽位，且不算分区', () => {
  // 「1 主 + 1 空容器」：2 个槽位，但只有 1 个分区。
  const withEmpty = mbrSlotUsage([prow(), ecrow()]);
  assert.equal(withEmpty.primaries, 1);
  assert.equal(withEmpty.extendeds, 1);
  assert.equal(withEmpty.logicals, 0);
  assert.equal(withEmpty.usesExtended, true, '空容器同样占槽位');
  assert.equal(withEmpty.neededSlots, 2);
  assert.equal(withEmpty.hasEmptyExtended, true, '容器里还没有逻辑分区');
  assert.equal(withEmpty.countsAsPartition, 1, '容器不是分区');

  // 容器 + 逻辑分区：容器不重复计数（两者是同一个槽位的两种来源）。
  const withLogical = mbrSlotUsage([prow(), ecrow(), prow({ kind: 'logical' })]);
  assert.equal(withLogical.extendeds, 1);
  assert.equal(withLogical.logicals, 1);
  assert.equal(withLogical.neededSlots, 2, '显式容器与逻辑分区共用同一个槽位');
  assert.equal(withLogical.hasEmptyExtended, false);
  assert.equal(withLogical.countsAsPartition, 2);

  // 「3 主 + 1 空容器」正好用满 4 个槽位。
  const full = mbrSlotUsage([prow(), prow(), prow(), ecrow()]);
  assert.equal(full.neededSlots, 4);
  assert.equal(full.freeSlots, 0);
  assert.equal(full.countsAsPartition, 3, '分区数只有 3');

  // 一个容器都没有时，行为与改动前逐字段一致。
  const plain = mbrSlotUsage([prow(), prow()]);
  assert.equal(plain.extendeds, 0);
  assert.equal(plain.usesExtended, false);
  assert.equal(plain.hasEmptyExtended, false);
});

test('validatePartitions：1 主 + 1 空扩展容器合法', () => {
  const result = validatePartitions([prow(), ecrow()], { layout: 'mbr', imageBytes: 1e9 });
  assert.equal(result.ok, true, result.message);
});

test('validatePartitions：4 主 + 1 容器被拒（需要 5 个槽位）', () => {
  const result = validatePartitions([prow(), prow(), prow(), prow(), ecrow()], {
    layout: 'mbr',
    imageBytes: 1e9,
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /4 个分区项/);
  // 报错必须说清容器占了一个槽位，否则用户不知道为什么要改归属。
  assert.match(result.detail, /扩展分区容器/);
});

test('validatePartitions：3 主 + 1 空容器正好用满 4 个槽位', () => {
  const result = validatePartitions([prow(), prow(), prow(), ecrow()], {
    layout: 'mbr',
    imageBytes: 1e9,
  });
  assert.equal(result.ok, true, result.message);
});

test('validatePartitions：只能有一个扩展容器', () => {
  const result = validatePartitions([prow(), ecrow(), ecrow()], {
    layout: 'mbr',
    imageBytes: 1e9,
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /只能有一个扩展分区容器/);
});

test('validatePartitions：容器不能被格式化', () => {
  const result = validatePartitions([prow(), ecrow({ filesystem: 'fat32' })], {
    layout: 'mbr',
    imageBytes: 1e9,
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /不能被格式化/);
  assert.match(result.detail, /没有数据区/);
});

test('validatePartitions：容器不计入分区总数', () => {
  // 3 主 + 1 容器 + 64 逻辑 = 67 个分区，正好是 MAX_PARTITIONS.mbr 的上界。
  // 若把容器也算进去就会变成 68 而被错误拦下。
  const rows = [prow(), prow(), prow(), ecrow()];
  for (let i = 0; i < 64; i += 1) rows.push(prow({ kind: 'logical' }));
  const result = validatePartitions(rows, { layout: 'mbr', imageBytes: 1e12 });
  assert.equal(result.ok, true, result.message);
  assert.equal(mbrSlotUsage(rows).countsAsPartition, MAX_PARTITIONS.mbr);
});

test('validatePartitions：只有容器的表被拒（Host 上挂不上任何东西）', () => {
  const result = validatePartitions([ecrow()], { layout: 'mbr', imageBytes: 1e9 });
  assert.equal(result.ok, false);
  assert.match(result.message, /至少要有一个主分区或逻辑分区/);
});

test('validatePartitions：GPT 下不能有扩展容器', () => {
  const result = validatePartitions([prow(), ecrow()], { layout: 'gpt', imageBytes: 1e9 });
  assert.equal(result.ok, false, 'GPT 没有扩展分区机制');
});

// ---------------------------------------------------------------- 分区下限（按文件系统）

test('validatePartitions：下限按每个分区自己的文件系统判定', () => {
  // 同一个 1 MiB：exFAT 合法，FAT32 非法（33 MiB）。早先的实现一律按 FAT32 的
  // 64 MiB 判，连 exFAT 分区也会被拦下。
  const exfat = prow({ sizeBytes: 1024 * 1024, filesystem: 'exfat' });
  assert.equal(
    validatePartitions([exfat], { layout: 'gpt', imageBytes: 1e9, filesystem: 'fat32' }).ok,
    true,
  );

  const fat32 = prow({ sizeBytes: 1024 * 1024, filesystem: 'fat32' });
  const result = validatePartitions([fat32], {
    layout: 'gpt',
    imageBytes: 1e9,
    filesystem: 'fat32',
  });
  assert.equal(result.ok, false);
  // 文案必须**指到行**并说清下限与文件系统，而不是笼统的"空间不足"。
  assert.match(result.message, /第 1 个分区/);
  assert.match(result.message, /fat32/);
  assert.match(result.detail, /33 MiB/);
  assert.match(result.detail, /1 MiB/);
});

test('validatePartitions：留空文件系统时用全局默认判下限', () => {
  // 1 MiB 分区 + 全局默认 exFAT → 通过；全局默认 FAT32 → 被拒。
  const inherited = prow({ sizeBytes: 1024 * 1024, filesystem: '' });
  assert.equal(
    validatePartitions([inherited], { layout: 'gpt', imageBytes: 1e9, filesystem: 'exfat' }).ok,
    true,
  );
  assert.equal(
    validatePartitions([inherited], { layout: 'gpt', imageBytes: 1e9, filesystem: 'fat32' }).ok,
    false,
  );
});

test('validatePartitions：不格式化的分区没有下限', () => {
  // 4 KiB 的裸分区（只写分区表）不该被文件系统下限拦下——它不建文件系统。
  const bare = prow({ sizeBytes: 4096, filesystem: 'none' });
  assert.equal(
    validatePartitions([bare], { layout: 'gpt', imageBytes: 1e9, filesystem: 'fat32' }).ok,
    true,
  );
});

test('validatePartitions：ext4 下限 2 MiB 与实测一致', () => {
  const tooSmall = prow({ sizeBytes: 1024 * 1024, filesystem: 'ext4' });
  const result = validatePartitions([tooSmall], {
    layout: 'gpt',
    imageBytes: 1e9,
    filesystem: 'fat32',
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /ext4/);
  assert.match(result.detail, /2 MiB/);

  const atFloor = prow({ sizeBytes: 2 * 1024 * 1024, filesystem: 'ext4' });
  assert.equal(
    validatePartitions([atFloor], { layout: 'gpt', imageBytes: 1e9, filesystem: 'fat32' }).ok,
    true,
  );
});

test('validatePartitions：行号指向真正违规的那一行', () => {
  const rows = [
    prow({ sizeBytes: 64 * 1024 * 1024, filesystem: 'ext4' }),
    prow({ sizeBytes: 2 * 1024 * 1024, filesystem: 'fat32' }),
  ];
  const result = validatePartitions(rows, {
    layout: 'gpt',
    imageBytes: 1e9,
    filesystem: 'fat32',
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /第 2 个分区/);
});

test('validatePartitions：「占满剩余」的那一行也按下限判定', () => {
  // 64 MiB 镜像 + 一个 40 MiB 的固定 FAT32 分区 + 一个占满剩余的分区：
  // 剩余约 24 MiB，低于 FAT32 的 33 MiB → 必须在这一行报错。
  // 早先这里被报成 no_space（"存储空间不足"），完全指不到原因。
  const rows = [
    prow({ sizeBytes: 40 * 1024 * 1024, filesystem: 'fat32' }),
    prow({ sizeBytes: 0, filesystem: 'fat32' }),
  ];
  const result = validatePartitions(rows, {
    layout: 'gpt',
    imageBytes: 64 * 1024 * 1024,
    filesystem: 'fat32',
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /第 2 个分区/);
  assert.match(result.message, /低于 fat32 下限/);

  // 换成 exFAT 就合法（1 MiB 下限），同样的容量不该被拦。
  const exfatRows = [
    prow({ sizeBytes: 40 * 1024 * 1024, filesystem: 'exfat' }),
    prow({ sizeBytes: 0, filesystem: 'exfat' }),
  ];
  assert.equal(
    validatePartitions(exfatRows, {
      layout: 'gpt',
      imageBytes: 64 * 1024 * 1024,
      filesystem: 'fat32',
    }).ok,
    true,
  );
});

test('validatePartitions：64 MiB 镜像里一个占满剩余的 FAT32 分区现在合法', () => {
  // 本次修复的直接回归：旧实现把它报成 no_space。
  const rows = [prow({ sizeBytes: 0, filesystem: 'fat32' })];
  const result = validatePartitions(rows, {
    layout: 'gpt',
    imageBytes: 64 * 1024 * 1024,
    filesystem: 'fat32',
  });
  assert.equal(result.ok, true, result.message || '');
  assert.equal(result.detail, undefined);
});

test('normalizePartitions：容器下发 kind=extended 且强制不格式化', () => {
  const out = normalizePartitions([prow(), ecrow({ filesystem: 'fat32' })], 'mbr');
  // 主分区不发 kind（老请求形状逐字节不变是回归底线）。
  assert.equal('kind' in out[0], false);
  assert.equal(out[1].kind, 'extended');
  // 即便界面残留了一个文件系统值，也不能真的下发。
  assert.equal(out[1].filesystem, 'none');

  // 逻辑分区照旧只发 kind=logical。
  const logical = normalizePartitions([prow({ kind: 'logical' })], 'mbr');
  assert.equal(logical[0].kind, 'logical');

  // GPT 下不下发 kind（后端会拒绝，前端也不该制造这种请求）。
  const gpt = normalizePartitions([prow({ kind: 'extended' })], 'gpt');
  assert.equal('kind' in gpt[0], false);
});

test('partitionKernelIndex：扩展容器不占序号，返回 null', () => {
  const list = [
    prow(),                      // 行0 主 -> 1
    ecrow(),                     // 行1 容器 -> 无序号
    prow({ kind: 'logical' }),   // 行2 逻辑 -> 5
    prow(),                      // 行3 主 -> 2
  ];
  assert.equal(partitionKernelIndex(list, 0, 'mbr'), 1);
  assert.equal(partitionKernelIndex(list, 1, 'mbr'), null, '容器没有内核序号');
  assert.equal(partitionKernelIndex(list, 2, 'mbr'), 5);
  assert.equal(partitionKernelIndex(list, 3, 'mbr'), 2, '容器不参与主分区计数');

  // 容器排在逻辑分区之前/之后都不影响逻辑分区的编号。
  const containerFirst = [ecrow(), prow(), prow({ kind: 'logical' })];
  assert.equal(partitionKernelIndex(containerFirst, 0, 'mbr'), null);
  assert.equal(partitionKernelIndex(containerFirst, 1, 'mbr'), 1);
  assert.equal(partitionKernelIndex(containerFirst, 2, 'mbr'), 5);
});

test('describePartition：扩展容器不显示成「分区 0」', () => {
  // 后端把容器读回来时 index 为 0（不占序号）。显示成「分区 0」会让用户
  // 以为存在一个序号为 0 的设备。
  const container = describePartition({ index: 0, size_bytes: 32 * 1024 * 1024 });
  assert.match(container, /扩展分区容器/);
  assert.doesNotMatch(container, /分区 0/);

  // 带 kind 时同样识别。
  const byKind = describePartition({ index: 0, kind: 'extended', size_bytes: 1024 });
  assert.match(byKind, /扩展分区容器/);

  // 容量照常显示（去掉多余的 .0）。
  assert.match(container, /32 MiB/);
});

test('validatePartitions 用 mbrSlotUsage 判定，与提示文案同源', () => {
  // 4 主 + 1 逻辑：槽位 5 > 4，必须拒绝。
  const bad = mbrSlotUsage([prow(), prow(), prow(), prow(), prow({ kind: 'logical' })]);
  assert.equal(bad.neededSlots, 5);
  const result = validatePartitions(
    [prow(), prow(), prow(), prow(), prow({ kind: 'logical' })],
    { layout: 'mbr', imageBytes: 1e9 },
  );
  assert.equal(result.ok, false);
  assert.match(result.message, /4 个分区项/);

  // 3 主 + 2 逻辑：槽位 4，合法。
  assert.equal(
    validatePartitions(
      [prow(), prow(), prow(), prow({ kind: 'logical' }), prow({ kind: 'logical' })],
      { layout: 'mbr', imageBytes: 1e9 },
    ).ok,
    true,
  );
});

test('partitionKernelIndex：MBR 主分区 1 起，逻辑分区从 5 起', () => {
  const list = [
    prow(),                      // 行0 主 -> 1
    prow({ kind: 'logical' }),   // 行1 逻辑 -> 5
    prow(),                      // 行2 主 -> 2
    prow({ kind: 'logical' }),   // 行3 逻辑 -> 6
    prow({ kind: 'logical' }),   // 行4 逻辑 -> 7
  ];
  assert.equal(partitionKernelIndex(list, 0, 'mbr'), 1);
  assert.equal(partitionKernelIndex(list, 1, 'mbr'), 5);
  assert.equal(partitionKernelIndex(list, 2, 'mbr'), 2);
  assert.equal(partitionKernelIndex(list, 3, 'mbr'), 6);
  assert.equal(partitionKernelIndex(list, 4, 'mbr'), 7);

  // 逻辑分区在前、主分区在后：序号仍按各自的计数走，与后端排序一致。
  const reversed = [prow({ kind: 'logical' }), prow()];
  assert.equal(partitionKernelIndex(reversed, 0, 'mbr'), 5);
  assert.equal(partitionKernelIndex(reversed, 1, 'mbr'), 1);

  // 全部逻辑：从 5 连续递增。
  const allLogical = [prow({ kind: 'logical' }), prow({ kind: 'logical' })];
  assert.equal(partitionKernelIndex(allLogical, 0, 'mbr'), 5);
  assert.equal(partitionKernelIndex(allLogical, 1, 'mbr'), 6);

  // GPT/raw：序号即出现顺序，与 kind 无关。
  assert.equal(partitionKernelIndex(list, 0, 'gpt'), 1);
  assert.equal(partitionKernelIndex(list, 1, 'gpt'), 2);
});

test('describePartition 标出逻辑分区', () => {
  // 逻辑分区序号从 5 起，不标注会让用户以为中间缺了几个分区。
  const logical = describePartition({ index: 5, size_bytes: 1024, type_label: 'Linux' });
  assert.match(logical, /分区 5（逻辑）/);

  const primary = describePartition({ index: 1, size_bytes: 1024, type_label: 'FAT32 (LBA)' });
  assert.match(primary, /分区 1(?!（逻辑）)/);
  assert.doesNotMatch(primary, /逻辑/);

  // 后端给了 kind 字段时以它为准。
  assert.match(describePartition({ index: 3, kind: 'logical' }), /逻辑/);
});

test('validatePartitions：MBR 每行默认是主分区，槽位规则与后端一致', () => {
  // 4 个主分区合法（正好占满 4 个槽位）。
  const four = Array.from({ length: 4 }, () => prow());
  assert.equal(validatePartitions(four, { layout: 'mbr', imageBytes: 1e9 }).ok, true);

  // 4 主 + 1 逻辑需要 5 个槽位，必须拦下。
  const five = [...four, prow({ kind: 'logical' })];
  const result = validatePartitions(five, { layout: 'mbr', imageBytes: 1e9 });
  assert.equal(result.ok, false);
  assert.match(result.message, /4 个分区项/);
  // 文案要说清"为什么"，而不是只报一个数字上限。
  assert.match(result.detail, /扩展分区容器/);
  assert.match(result.detail, /需要 5 个槽位/);
});

test('validatePartitions：3 主 + 多个逻辑分区合法', () => {
  // 3 主 + 1 扩展容器 = 正好 4 个槽位；逻辑分区可以有很多个。
  const list = [
    prow(),
    prow(),
    prow(),
    ...Array.from({ length: 10 }, () => prow({ kind: 'logical' })),
  ];
  assert.equal(validatePartitions(list, { layout: 'mbr', imageBytes: 1e9 }).ok, true);
});

test('validatePartitions：全部为逻辑分区也合法', () => {
  // 扩展容器占一个槽位，逻辑分区数量不受 4 的限制。
  const list = Array.from({ length: 5 }, () => prow({ kind: 'logical' }));
  assert.equal(validatePartitions(list, { layout: 'mbr', imageBytes: 1e9 }).ok, true);
});

test('validatePartitions：扩展分区不能作为用户可选**类型**', () => {
  // 容器的类型字节恒为 0x05、由后端生成，不是可选的「分区类型」。要表达容器
  // 请改「归属」——报错必须指引到那里，否则用户不知道该怎么办。
  const list = [prow({ mbrType: 'mbr:extended' })];
  const result = validatePartitions(list, { layout: 'mbr', imageBytes: 1e9 });
  assert.equal(result.ok, false);
  assert.match(result.message, /扩展分区/);
  assert.match(result.detail, /归属/);

  // 但「归属 = 扩展分区」的行**是**合法的（下面那条测试覆盖）。
});

test('validatePartitions：MBR 逻辑分区总数上限', () => {
  const list = [
    prow(),
    ...Array.from({ length: 65 }, () => prow({ kind: 'logical' })),
  ];
  const result = validatePartitions(list, { layout: 'mbr', imageBytes: 1e9 });
  assert.equal(result.ok, false);
  // 必须报**逻辑分区**上限，而不是笼统的总数上限：总数上界在"1 主 + 65 逻辑"
  // 这种组合下不是紧的，笼统报法会让用户不知道该减什么。
  assert.match(result.message, /逻辑分区最多 64 个/);
});

test('validatePartitions：最多一个分区占满剩余空间', () => {
  const two = [prow({ sizeBytes: 0 }), prow({ sizeBytes: 0 })];
  const result = validatePartitions(two, { layout: 'gpt', imageBytes: 1e9 });
  assert.equal(result.ok, false);
  assert.match(result.message, /占满剩余空间/);

  // 一个 auto + 一个具体值是合法的。
  const mixed = [prow({ sizeBytes: 1e6 }), prow({ sizeBytes: 0 })];
  assert.equal(validatePartitions(mixed, { layout: 'gpt', imageBytes: 1e9 }).ok, true);
});

test('validatePartitions：总和超容量要拒绝（不静默裁剪）', () => {
  const result = validatePartitions([prow({ sizeBytes: 2e9 })], {
    layout: 'gpt',
    imageBytes: 1e9,
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /超过镜像容量/);
});

test('validatePartitions：容量非法要拒绝', () => {
  const result = validatePartitions([prow({ sizeBytes: -1 })], {
    layout: 'gpt',
    imageBytes: 1e9,
  });
  assert.equal(result.ok, false);
  assert.match(result.message, /容量不合法/);
});

test('validatePartitions：GPT 名称超 36 字符才拒绝', () => {
  const long = [prow({ name: 'x'.repeat(37) })];
  assert.equal(validatePartitions(long, { layout: 'gpt', imageBytes: 1e9 }).ok, false);

  const ok = [prow({ name: 'x'.repeat(36) })];
  assert.equal(validatePartitions(ok, { layout: 'gpt', imageBytes: 1e9 }).ok, true);

  // MBR 不写名字，长名字不应导致失败。
  assert.equal(validatePartitions(long, { layout: 'mbr', imageBytes: 1e9 }).ok, true);
});

test('validatePartitions：留空类型表示继承默认，不是错误', () => {
  // **回归**：解析结果曾是 `string|null`，`null` 同时表示"没填"与"填错"，
  // 于是默认行（类型留空）被报成"类型不合法"，用户一进创建页就见红。
  const empty = validatePartitions([prow({ gptType: '', mbrType: '' })], {
    layout: 'gpt',
    imageBytes: 1e9,
  });
  assert.equal(empty.ok, true, '留空类型应合法（用布局默认值）');

  const emptyMbr = validatePartitions([prow({ gptType: '', mbrType: '' })], {
    layout: 'mbr',
    imageBytes: 1e9,
  });
  assert.equal(emptyMbr.ok, true);

  // 而**真的填错**仍然要拒绝。
  const bad = validatePartitions([prow({ gptType: 'gpt:bogus' })], {
    layout: 'gpt',
    imageBytes: 1e9,
  });
  assert.equal(bad.ok, false);
  assert.match(bad.message, /GPT 类型 GUID 不合法/);

  const badMbr = validatePartitions([prow({ mbrType: 'mbr:bogus' })], {
    layout: 'mbr',
    imageBytes: 1e9,
  });
  assert.equal(badMbr.ok, false);
  assert.match(badMbr.message, /MBR 类型不合法/);
});

test('resolveGptType / resolveMbrType 三态', () => {
  assert.equal(resolveGptType(prow({ gptType: '' })).kind, 'inherit');
  assert.equal(resolveGptType(prow({ gptType: 'gpt:linux_filesystem' })).kind, 'value');
  assert.equal(resolveGptType(prow({ gptType: 'gpt:bogus' })).kind, 'invalid');

  assert.equal(resolveMbrType(prow({ mbrType: '' })).kind, 'inherit');
  assert.equal(resolveMbrType(prow({ mbrType: 'mbr:linux' })).kind, 'value');
  assert.equal(resolveMbrType(prow({ mbrType: 'mbr:bogus' })).kind, 'invalid');
});

test('validatePartitions：自定义类型必须合法', () => {
  // 合法的自定义 GPT GUID。
  const goodGuid = validatePartitions(
    [prow({ gptType: CUSTOM_TYPE_VALUE, gptTypeCustom: '12345678-9ABC-DEF0-1234-56789ABCDEF0' })],
    { layout: 'gpt', imageBytes: 1e9 },
  );
  assert.equal(goodGuid.ok, true);

  // 非法 GUID。
  const badGuid = validatePartitions(
    [prow({ gptType: CUSTOM_TYPE_VALUE, gptTypeCustom: 'nope' })],
    { layout: 'gpt', imageBytes: 1e9 },
  );
  assert.equal(badGuid.ok, false);

  // 合法的自定义 MBR 字节（`0x1A` 与裸 `1a` 都应接受）。
  for (const text of ['0x1A', '1a']) {
    const r = validatePartitions(
      [prow({ mbrType: CUSTOM_TYPE_VALUE, mbrTypeCustom: text })],
      { layout: 'mbr', imageBytes: 1e9 },
    );
    assert.equal(r.ok, true, text);
  }

  const badByte = validatePartitions(
    [prow({ mbrType: CUSTOM_TYPE_VALUE, mbrTypeCustom: 'zz' })],
    { layout: 'mbr', imageBytes: 1e9 },
  );
  assert.equal(badByte.ok, false);
});

test('customTypeWire 规范化自定义类型', () => {
  assert.equal(customTypeWire('gpt', '12345678-9abc-def0-1234-56789abcdef0'),
    'gpt:12345678-9ABC-DEF0-1234-56789ABCDEF0');
  assert.equal(customTypeWire('mbr', '1a'), 'mbr:0x1A');
  assert.equal(customTypeWire('mbr', '0x1A'), 'mbr:0x1A');
  assert.equal(customTypeWire('gpt', 'nope'), null);
  assert.equal(customTypeWire('mbr', 'zzz'), null);
});

test('normalizePartitions 只下发当前布局的类型', () => {
  const rows = [
    prow({ sizeBytes: 1024, gptType: 'gpt:linux_filesystem', mbrType: 'mbr:linux' }),
    prow({
      sizeBytes: 2048,
      gptType: 'gpt:efi_system',
      mbrType: 'mbr:efi_system',
      name: 'BOOT',
      filesystem: 'fat32',
    }),
  ];

  // GPT 布局：只带 gpt_type。
  assert.deepEqual(normalizePartitions(rows, 'gpt'), [
    { size_bytes: 1024, gpt_type: 'gpt:linux_filesystem' },
    {
      size_bytes: 2048,
      gpt_type: 'gpt:efi_system',
      name: 'BOOT',
      filesystem: 'fat32',
    },
  ]);

  // MBR 布局：只带 mbr_type。
  assert.deepEqual(normalizePartitions(rows, 'mbr'), [
    { size_bytes: 1024, mbr_type: 'mbr:linux' },
    { size_bytes: 2048, mbr_type: 'mbr:efi_system', name: 'BOOT', filesystem: 'fat32' },
  ]);

  // 非数组输入按空处理，不抛错。
  assert.deepEqual(normalizePartitions(null, 'gpt'), []);
});

test('normalizePartitions 只在 MBR 下、且仅对逻辑分区下发 kind', () => {
  // 主分区**不下发** `kind`：缺省即主分区，这样老请求的形状逐字节不变。
  const primary = normalizePartitions([prow({ kind: 'primary' })], 'mbr');
  assert.equal(primary[0].kind, undefined);

  // 逻辑分区必须下发，否则后端会按主分区处理。
  const logical = normalizePartitions([prow({ kind: 'logical' })], 'mbr');
  assert.equal(logical[0].kind, 'logical');

  // GPT 下不下发 `kind`（没有这个概念；后端会拒绝 logical）。
  const gpt = normalizePartitions([prow({ kind: 'logical' })], 'gpt');
  assert.equal(gpt[0].kind, undefined);
});

test('buildCliArgs 把逻辑分区写进 --partition 的第 6 段', () => {
  const call = {
    op: 'create',
    path: '/data/adb/gadget-disk/images/x.img',
    sizeBytes: 512 * 1024 * 1024,
    layout: 'mbr',
    partitions: [
      prow({ sizeBytes: 1024, kind: 'primary' }),
      prow({ sizeBytes: 2048, kind: 'logical' }),
    ],
  };
  const args = buildCliArgs(call);
  // 主分区：第 6 段为空（保留占位），末尾不能出现 `logical`。
  assert.match(args, /--partition '1024\/\/mbr:fat32_lba\/\/\/'/);
  // 逻辑分区：第 6 段为 `logical`。
  assert.match(args, /--partition '2048\/\/mbr:fat32_lba\/\/\/logical'/);
});

test('normalizePartitions 保留 filesystem: none', () => {
  // **回归**：`none` 必须原样下发。早先它与"未指定"都是空值，导致请求
  // "不格式化"的分区被后端套上全局默认并真的被格式化了。
  const out = normalizePartitions([prow({ filesystem: 'none' })], 'gpt');
  assert.equal(out[0].filesystem, 'none');

  // 空串表示"继承全局默认"，不下发。
  const inherit = normalizePartitions([prow({ filesystem: '' })], 'gpt');
  assert.equal(inherit[0].filesystem, undefined);
});

test('imageNameExists：同名冲突检测（不区分大小写）', () => {
  const images = [{ name: 'disk.img' }, { name: 'other.img' }];
  assert.equal(imageNameExists('disk.img', images), true);
  assert.equal(imageNameExists('DISK.IMG', images), true);
  assert.equal(imageNameExists('new.img', images), false);
  assert.equal(imageNameExists('', images), false);
  assert.equal(imageNameExists('disk.img', null), false);
});

test('describeFormattingSource：FAT32 说明走内置实现', () => {
  const probes = [
    { filesystem: 'fat32', path: null, note: 'x' },
    { filesystem: 'ext4', path: '/system/bin/mkfs.ext4', note: 'y' },
    { filesystem: 'exfat', path: null, note: '系统中未找到该工具' },
  ];
  // 设备上无 mkfs.vfat 是实测事实，文案必须如实说明走内置实现。
  assert.match(describeFormattingSource(probes, 'fat32'), /内置/);
  assert.match(describeFormattingSource(probes, 'ext4'), /\/system\/bin\/mkfs\.ext4/);
  // 未探测到结果时不谎报，如实说未知。
  assert.match(describeFormattingSource([], 'ext4'), /未知/);
});

test('messageForCode：同名冲突有可操作的中文文案', () => {
  assert.match(messageForCode('already_exists'), /同名镜像已存在/);
});

// ---------------------------------------------------------------- 缺陷回归

test('回归：parsePartitionSize 接受 0 而 parseSizeInput 拒绝', () => {
  // 截图缺陷的根因：分区容量 `0` 表示"占满剩余空间"，但早先它与镜像容量
  // 共用 parseSizeInput，而后者拒绝非正数 → `0` 变成 null → 调用方的
  // `?? -1` 造出哨兵值 → 容量框显示 undefined / 空白。
  assert.equal(parsePartitionSize('0'), 0);
  assert.equal(parsePartitionSize('0M'), 0);
  assert.equal(parseSizeInput('0'), null, '镜像容量必须为正数，0 非法');

  // 正常值两者一致。
  assert.equal(parsePartitionSize('64M'), 64 * 1024 * 1024);
  assert.equal(parseSizeInput('64M'), 64 * 1024 * 1024);

  // 非法值都返回 null（由调用方给出提示）。
  assert.equal(parsePartitionSize('abc'), null);
  assert.equal(parsePartitionSize(''), null);
  assert.equal(parsePartitionSize(null), null);
});
