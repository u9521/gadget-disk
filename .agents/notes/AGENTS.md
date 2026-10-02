# 在 .agents/notes 内工作

本目录是决策记录区。在此工作的规则：

1. **先读契约**：[README.md](README.md) 定义了状态编码、分类、命名与必需章节。
2. **状态与路径必须一致**：在 `proposed/` 起草时写 `Status: proposed`；落地后**移动文件**到 `implemented/` 并同步改 `Status:`。
3. **不覆盖历史**：不要重写 `rejected/` 与 `archived/` 的正文。若要推翻旧决策，新建一篇 Note，并在双方正文中互链。
4. **结论可追溯**：技术断言须提供实测数据、内核源码路径或真机日志；未经验证项一律归入 `## Risks` 并标注「待验证假设」。
5. **单主题原则**：一篇 Note 仅记录单项正交决策；关联变更拆分归档并通过相对链接索引。
6. **保持链接有效**：移动或归档文件后，更新全仓库指向它的相对链接。

校验命令：`python3 .agents/scripts/gates/verify_agent_gates.py`
