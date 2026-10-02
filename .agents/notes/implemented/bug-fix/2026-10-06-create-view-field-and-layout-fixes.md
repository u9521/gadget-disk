# Agent Note: 创建镜像视图的三处缺陷修复与宽屏布局

Status: implemented

## Problem

用户在使用创建镜像视图时报告了两个现象，排查后是**三个独立缺陷**：

### 1. 容量输入框显示 `undefined`

`renderPartitions` 读 `entry.sizeText`，而 `readPartitionRow` 写入的行对象只有
`sizeBytes`/`type`/`name`。任何一次编辑或重绘后，容量框就渲染成字符串 `undefined`。

根因是**同一份状态有两套字段名**，而零构建约束下没有类型系统能发现它。

### 2. 容量 `0`（占满剩余空间）无法回填

`parseSizeInput` 显式拒绝 `value <= 0`（镜像容量必须为正），而分区容量的 `0` 是
**合法**语义。两者共用同一个函数，于是 `0` 解析为 `null`，再经调用方的 `?? -1`
造出一个非法的哨兵值，最终表现为「分区 1 容量为空、分区 2 容量为 0」这类中间态。

### 3. 宽屏下标签与控件错位

`.partition-row` 是单层 flex + `flex-wrap`，所有子元素都 `flex: 1 1 30%`。宽屏下
浏览器按可用宽度重新排列，「类型」这类标签被夹在两个控件之间（用户截图可见），
读者完全分不清标签属于哪个控件。

## Proposal

### 1. 收敛为单一状态形状

行对象统一为
`{ sizeText, gptType, gptTypeCustom, mbrType, mbrTypeCustom, name, filesystem }`。

**容量以用户输入的文本为准**（`sizeText`），不在中间层转成字节数——避免
「解析失败 → 哨兵值 → 再次解析」这条已经出错的链路。解析只在提交时做一次。

### 2. `parsePartitionSize` 与 `parseSizeInput` 分开

两者共用同一套单位后缀语法，**只有「是否接受 0」这一条不同**。共用它是本缺陷的
直接成因，故拆成两个函数，并在各自的文档注释里写明差异。

### 3. 布局改成「标签 + 控件」的字段单元 + CSS Grid

每个分区是两行结构：标题行 + 字段网格。网格用
`grid-template-columns: repeat(auto-fit, minmax(150px, 1fr))`，宽屏一行排开多个字段、
窄屏自动堆叠。每个字段是 `.field`（`flex-direction: column`），**标签与控件处在
同一个单元里**，因此标签必定在自己控件上方，与可用宽度无关。

### 4. 类型解析改成三态（顺带修掉的同类缺陷）

实现过程中发现 `gptTypeWire`/`mbrTypeWire` 用 `null` 同时表示「没填」与「填错」，
于是**默认行（类型留空）被 `validatePartitions` 报成「类型不合法」**——用户一进
创建页就见红。改为 `{kind: 'inherit'|'value'|'invalid'}`：留空放行（用布局默认），
只有真的填错才拦。

## Alternatives considered

**在 `renderPartitions` 里做字段名兼容（`entry.sizeText ?? entry.sizeBytes`）** —
能让界面不复现，但保留了两套名字，下次仍会漂移。改成一侧统一更彻底。

**让 `parseSizeInput` 接受 0** — 会让镜像容量也接受 0（非法），把问题挪到别处。

**给 `.partition-row` 加 `row-gap` 或调 flex 基准宽度** — 治标：flex-wrap 的换行
位置本质上依赖可用宽度，标签与控件的从属关系仍不可靠。

## Acceptance criteria

- `webui/tests/structure.test.mjs` 的
  「回归：分区行的读写字段名必须一致」：断言 `readPartitionRow` 产出的字段集合与
  `renderPartitions` 读取的字段一一对应。零构建下没有类型系统，这条文本断言是
  唯一能防止两套名字再次分叉的手段。
- `webui/tests/logic.test.mjs` 的
  「回归：parsePartitionSize 接受 0 而 parseSizeInput 拒绝」：固化两者语义差异。
- 「回归：validatePartitions 留空类型表示继承默认，不是错误」：默认行必须通过校验。
- 「分区编辑器用 .field 单元而非裸 flex 子元素」：断言 CSS 用 grid + `flex-direction: column`。
- `node --test webui/tests/` 全绿；每个前端模块都能被 Node 解析（既有断言）。

## Risks

- **字段名一致性靠文本断言**：`structure.test.mjs` 用正则匹配源码，能覆盖本次这类
  分叉，但无法覆盖语义层面的一致（例如两侧字段名相同、含义不同）。零构建约束下
  这是可接受的上限。
- **`sizeText` 保存的是原始文本**，因此「`0`」与「`0M`」在状态里不同、解析后相同。
  提交时统一解析，不影响结果。
- **`maxLength = 36` 与后端的 UTF-16 码元计数不完全等价**：`maxlength` 按 UTF-16
  码元计，与后端 `encode_utf16().count()` 一致，故二者契合。
