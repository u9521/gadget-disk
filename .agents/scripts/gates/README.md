# 治理门禁

零依赖（仅 Python 3 标准库）的仓库治理校验脚本。

## 用法

```sh
python3 .agents/scripts/gates/verify_agent_gates.py        # 校验
python3 .agents/scripts/gates/verify_agent_gates.py -v     # 同时列出通过项
python3 .agents/scripts/gates/verify_agent_gates.py --root /path/to/repo
```

退出码：`0` 全部通过；`1` 存在违规（违规项按文件分组打印）。

## 校验内容

| # | 规则 | 依据 |
|---|---|---|
| 1 | Agent Note 的 H1 必须为 `# Agent Note: <标题>`，且含 `Status: <状态>` | [.agents/notes/README.md](../../notes/README.md) |
| 2 | `Status:` 必须与所在目录（`proposed/` `implemented/` `rejected/` `archived/`）一致 | 同上 |
| 3 | Note 文件名必须为 `yyyy-mm-dd-kebab-title.md`，位于 `<status>/<category>/` 下 | 同上 |
| 4 | Note 必须含五个必需章节：`## Problem`、`## Proposal`、`## Alternatives considered`、`## Acceptance criteria`、`## Risks` | 同上 |
| 5 | `AGENTS.md` 词数 ≤ 1500（中文按字符、ASCII 按词计） | [AGENTS.md](../../../AGENTS.md) |
| 6 | 所有 Markdown 相对链接可解析；带锚点的链接其锚点存在于目标文件 | 本文件 |

## 实现说明

- **代码块被忽略**：围栏代码块（` ``` `/`~~~`）内的内容不参与标题与链接解析，避免文档中的示例链接造成误报。
- **锚点近似 GitHub 规则**：转小写、去除标点、空格转连字符。若未来出现复杂标题（含 emoji 或公式）导致误报，应调整 `slugify()` 而非删除锚点校验。
- **豁免文件**：`.agents/notes/` 下的 `README.md` 与 `AGENTS.md` 是治理文件而非 Agent Note，不参与 Note 格式校验。
- **词数口径**：中文每字计 1，ASCII 每词计 1。这是为中文正文设计的口径；纯英文文档按此口径接近常规词数。

## 何时运行

- 提交前（见 [pre-push-checks 技能](../../skills/pre-push-checks/SKILL.md)）
- CI 中作为独立门禁

改动文档或 Agent Note 后务必本地跑一次 —— 移动或归档 Note 时最容易遗漏入链更新。
