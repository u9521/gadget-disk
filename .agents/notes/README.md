# Agent Notes 治理契约

Agent Note 记录架构决策、设计权衡、备选方案及生命周期状态，作为代码实现之外的设计依据。

## 目录即状态

状态由**路径**编码，不由正文声明决定；两者必须一致，由门禁脚本校验。

| 目录 | 含义 | 可否修改正文 |
|---|---|---|
| `proposed/` | 已提出、待实现或待决策 | 可自由修订 |
| `implemented/` | 已落地，正文描述**当前事实** | 重构时**原地更新**，保持与代码一致 |
| `rejected/` | 评估后明确否决 | 冻结，仅追加「为何重开」的链接 |
| `archived/` | 已过时或被取代 | 冻结，只读 |

## 目录即分类

`feature` · `bug-fix` · `simplification` · `architecture` · `process` · `testing`

## 命名与头部格式

路径：`.agents/notes/<status>/<category>/<yyyy-mm-dd>-<kebab-title>.md`

正文必须以 H1 开头，且格式严格为：

```markdown
# Agent Note: <标题>

Status: <proposed|implemented|rejected|archived>
```

`Status:` 必须与所在目录一致。

## 必需章节

所有 Note 必须包含以下五个二级标题，顺序不限，标题文字须完全一致：

- `## Problem` — 要解决的问题与约束
- `## Proposal` — 提议的方案
- `## Alternatives considered` — 考虑过的其他方案与否决理由
- `## Acceptance criteria` — 可验证的完成标准
- `## Risks` — 风险、未知项、待验证假设

## 强制要求

1. **关键架构、接口或行为变更必须新增或更新一篇 Note**，与代码同次提交。
2. **未验证的结论必须显式标注**为「待验证假设」，不得表述为已证事实。证据来源应可追溯（实测、源码、真机日志）。
3. **内容必须反映实现现状**：代码变更导致 Note 失真时，必须同步就地更新。
4. **被取代的 Note 不得删除**，移入 `archived/` 并更新所有入链。
5. 正文使用中文，标识符、协议字段、命令、路径保持原样。
