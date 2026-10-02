# Agent Note: GPL-3.0-only 许可与仓库 README

Status: implemented

## Problem

仓库声明了 GPL-3.0-only，却**没有许可文本本体**，也没有面向访客的入口文档：

1. 根 `Cargo.toml` 的 `license = "GPL-3.0-only"`、7 个 crate 经 `workspace = true`
   继承，`docs/requirements.md` 也写了「遵循 GPL-3.0-only」——但仓库里没有 `LICENSE`
   文件。对 GPL 这类**要求随分发提供完整文本**的许可来说，仅在元数据里写 SPDX 标识
   并不构成合规的分发形态。
2. 没有任何 `README.md`：一个首次来到仓库的人只能看到 `AGENTS.md`（写给 AI/贡献者的
   规则入口）与 `docs/**`（规格细节）。缺少一份"这是什么、怎么装、怎么构建"的简短入口。
3. 顺带确认了一条隐含边界：仓库的「零冗余注释」风格下，**是否要给每个源文件加 SPDX
   头**需要一次明确决策，否则每次新增文件都会重新讨论。

## Proposal

**许可**：新增仓库根 `LICENSE`，内容为 **GNU GPL v3 全文逐字**（gnu.org
`gpl-3.0.txt`：674 行 / 35149 字节 / md5 `1ebbd3e34237af26da5dc08a4e440464`），
不加改写、不加补充条款、不做版权人署名变体。SPDX 标识统一为 `GPL-3.0-only`
（与 `Cargo.toml` 一致：**不含** `or-later`）。

**不加逐文件许可头**：`crates/**`、`webui/**`、`scripts/**`、`module_template/**`
一律不加 SPDX 注释块。理由：① 根 `LICENSE` + 每个 crate 的 `license` 字段已经构成
完整的机器可读声明（`cargo metadata` 可查询）；② 会给约 100 个文件各加两行噪音，
与本仓库"注释只写理由"的风格冲突；③ 会让本次改动触发全部 crate 重编译与全量检查。

**README**（仓库根，中文，简短）：只做**入口**，不复制 `docs/**` 的规格。六节：
定位（含一行英文 tagline，对应 `module.prop` 的双语 description）→ 能力要点（6 条，
每条一行）→ 环境要求（表格）→ 安装（三步，含 pending-update 提醒）→ 构建与开发
（5 个 uv 入口 + 三级测试纪律 + CI）→ 许可与文档索引。凡超出"一句话"的规则一律外链。

`AGENTS.md` 的「改动需确认」清单补入根 `README.md`（与 `.github/**` 同次）。

## Alternatives considered

- **只写 `LICENSE` 引用（如 `GPL-3.0-only` 的 SPDX 短声明 + 链接）**：GPL 明确要求
  随程序提供许可副本；只给链接不符合"随分发提供"的要求，对模块 ZIP 这类二进制分发
  尤其不成立。
- **给全部源文件加 SPDX 头**：机器可读性更强，且部分组织（如 REUSE）以此为准。
  否决理由见上——本仓库的所有权与许可事实已由 `LICENSE` + `Cargo.toml` 表达，
  逐个文件重复同一行不增加信息量。
- **把 README 写成完整文档（能力、架构、协议、排错全都有）**：会与 `docs/**` 形成
  第二个事实源，违反单一事实源原则，且必然随时间失真。
- **不写 README，把 `AGENTS.md` 当入口**：`AGENTS.md` 面向的是"改动本仓库的人"，
  内容以规则与门禁为主；对只想安装使用的访客噪音过大。
- **README 用英文**：文档正文的语言边界已定为中文（见 [文档规范](../../../../docs/README.md#语言边界操作者输出用英文)），
  README 属文档正文；英文只保留一行 tagline 以利于搜索。

## Acceptance criteria

1. `md5sum LICENSE` = `1ebbd3e34237af26da5dc08a4e440464`，与 gnu.org 的
   `gpl-3.0.txt` 逐字节一致（674 行 / 35149 字节）。
2. `cargo metadata --no-deps --format-version 1` 中每个包的 `license` 字段均为
   `GPL-3.0-only`；仓库内不存在 `LICENSE` 之外的许可文本副本。
3. `grep -rn SPDX crates webui scripts module_template` 无命中（负向断言"没有逐文件头"）。
4. `README.md` 通过治理门禁的链接与锚点校验（它是被门禁校验的首个根级文档）；
   `python3 .agents/scripts/gates/verify_agent_gates.py` 全体文件 **PASS**。
5. `AGENTS.md` 词数 ≤1500（现 1459）且「改动需确认」清单含根 `README.md`。

## Risks

- **版权人未在 `LICENSE` 内署名**：GPL 全文本身不含版权行，本仓库也没有单独的
  `COPYING` 头或 `NOTICE`。分发者若需明确著作权归属，应依赖 git 提交历史与
  `module.prop` 的 `author` 字段。是否补一份带年份与署名的版权声明**待定**——
  当前不做，因为它与"全文逐字"的校验方式（md5 比对）互斥。
- README 的"能力要点"与 `docs/**` 存在**表述层面**的重复。已通过"每条一行、细节外链"
  约束规模，但仍需在功能变更时一并检查，否则会缓慢失真。这是可读性与单一事实源之间的
  有意折中。
- 待验证假设：GPL-3.0-only 与仓库依赖图的许可兼容性**未做全量审计**。
  `gpt` / `fatfs` / `fscommon` / `clap` / `serde` / `rustix` 等主流 crate 通常为
  MIT/Apache-2.0（与 GPL-3.0 兼容），但未逐个核对，也未引入 `cargo-deny` 之类的
  自动审计工具。
