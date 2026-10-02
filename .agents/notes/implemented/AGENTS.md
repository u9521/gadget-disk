# 在 .agents/notes/implemented 内工作

本目录的 Note 描述**已落地的当前事实**。与 `proposed/` 不同，这里的内容必须与代码保持一致。

## 规则

1. **原地修订**：架构重构或实现细节调整时直接修改既有 Note，保持内容与当前代码严格同步。
2. **不得删除**：若某个决策被完全取代，把文件移到 `../archived/<category>/`，改 `Status: archived`，并在新 Note 中说明取代关系。
3. **保持可验证性**：`## Acceptance criteria` 应描述**当前**可执行的验证方式（测试名、命令、真机步骤），不要保留已失效的验收描述。
4. **链接完整性**：移动文件后，更新仓库内所有指向它的相对链接。
5. **单一事实源**：本目录仅阐述决策演进与权衡理由；接口与协议规格索引 `docs/**`，禁止重复定义。

校验命令：`python3 .agents/scripts/gates/verify_agent_gates.py`
