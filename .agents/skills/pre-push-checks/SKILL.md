---
name: pre-push-checks
description: 在推送或提交前对当前改动运行最小必要的测试、检查与治理门禁。适用于本仓库任何改动完成后。
---

# 提交前检查

目标：用**最小必要**的命令集验证当前改动，避免无谓的全量测试。

## 步骤

### 1. 确认改动范围

```
git status --short
git diff --name-only
```

将改动分类：

| 改动路径 | 影响的验证 |
|---|---|
| `crates/<name>/**` | 该 crate 的测试 |
| `crates/**/Cargo.toml`、根 `Cargo.toml`、`Cargo.lock`、`rustfmt.toml` | **全部** Rust 测试 |
| `webui/**` | WebUI 的 Node 测试（`webui/tests/`） |
| `scripts/**`、`pyproject.toml`、`uv.lock` | 按差分脚本的保守规则 → **全部** Rust 测试 + Python 检查 |
| `docs/**`、`.agents/**` | 仅治理门禁 |
| `module_template/**` | 打包与部署验证 |

### 2. 按范围验证（三级纪律，见 [测试规范](../../../docs/testing.md)）

先快速排错，再精准验证：

```
cargo check -p <crate>                    # 第一反馈
cargo nextest run -p <crate>              # 改动 crate 的测试
```

**不要**在迭代期跑无参数的全量测试。改动跨多个 crate 或影响全局配置时，交给差分脚本：

```
uv run gd-test --dry-run                  # 先看会跑什么
uv run gd-test                            # 按 diff 选测
```

> 若 `cargo-nextest` 不可用，先按官方安装器装预编译二进制（见 `docs/testing.md`
> 「测试运行器」一节）；脚本在缺失时会回退 `cargo test -p <crate>`，并提示失去 `-E` 过滤能力。

### 3. 格式与静态检查（一个入口）

```
uv run gd-check                           # 门禁与静态检查全数通过后方可提交
uv run gd-check --fix                     # 自动修复格式问题
```

`gd-check` 依次跑：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`ruff format --check`、`ruff check`、`pyright`、治理门禁。排版迭代期可用 `--rust-only` /
`--python-only` 只跑一侧。

### 4. 治理门禁

已含在 `gd-check` 内；单独跑：

```
python3 .agents/scripts/gates/verify_agent_gates.py
```

### 5. 提交前全量

仅在准备提交/推送时执行：

```
uv run gd-test --all
```

## Agent Note 同步核查

涉及关键架构设计、技术选型或破坏性变更时，必须同步提交 Agent Note（详见 [.agents/notes/README.md](../../notes/README.md)）：

- 架构决策、权衡与新特性 → 新增 Note 或更新既有 Note；
- 既有实现细节调整导致 Note 失真 → 原地修订 `implemented/` 中的 Note；
- 架构决策推翻或方案替代 → 新建 Note 说明替代方案，原 Note 归档至 `archived/` 并建立双向链接。

## 涉及内核交互的改动

若改动触及 configfs、loop 或 mount，**必须**在提交说明中标注是否已在真机验证。以下内容无法在主机验证（开发机无 root、无物理设备），需列入真机验收：

- configfs 写入顺序与 `file` 属性语义；
- loop ioctl、`mount(2)`、`max_part` 相关回退；
- 镜像的 SELinux 上下文是否允许内核线程**读写**（`dmesg | grep avc`，并以镜像 `md5` 变化为准——**不得**依赖 `lun.N/ro`）。

## 完成标准

- [ ] `cargo check` 通过
- [ ] 受影响的测试通过
- [ ] `uv run gd-check` 全绿
- [ ] 关键架构变更已配 Agent Note
- [ ] 内核相关改动已标注真机验证状态
