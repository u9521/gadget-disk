# Agent Note: Rust 工具链升级到 1.99

Status: implemented

## Problem

根 `Cargo.toml` 声明 `rust-version = "1.85"`，但**这个下限已经不成立了**：

1. `gpt = "4.1.0"` 的传递依赖 `uuid@1.27.0` 要求 `rustc 1.89.0`。在 `1.85.1` 上
   构建直接失败：

   ```
   error: rustc 1.85.1 is not supported by the following package:
     uuid@1.27.0 requires rustc 1.89.0
   ```

   已在干净 `HEAD`（`git worktree`，排除本次改动干扰）上复现，属既有问题。

2. 更隐蔽的是：`1.85` 曾被文档表述为「edition 2024 的硬性下限」，于是它看起来像
   一个有依据的选择。但"edition 要求的最低版本"与"**依赖图 + lint 配置**实际要求的
   最低版本"是两件事。声明得比实际低，等于把失败推迟到某个随机的编译错误上——
   而 `rust-version` 的作用恰恰是让 cargo **提前**拒绝不满足的工具链。

3. 新的 clippy 把若干规则纳入了默认 `warn`：`collapsible_if`、
   `manual_is_multiple_of`、`chunks_exact_to_as_chunks`。本仓库把
   `-D warnings` 设为阻塞，因此这些规则在 `1.99` 上共产生 **37 条**新告警，
   全部来自既有代码。

## Proposal

### 1. 把 `rust-version` 提到**实际验证过的**版本

```toml
[workspace.package]
edition = "2024"
rust-version = "1.99"
```

选择"当前实际验证过的 stable"而不是"刚好能编过的最低版本"：后者需要逐个依赖回溯
测试，收益仅为兼容更旧的工具链——而本仓库**不提交 `rust-toolchain.toml`、不在 CI 里
钉死工具链**，构建机用最新 stable 即可。声明的价值在于"用旧工具链会响亮地失败"，
现在确实如此（实测 `1.85.1` 报 `requires rustc 1.99`）。

### 2. 收敛新版 clippy 的新增规则

`cargo clippy --workspace --all-targets --fix` 自动改写了 37 处，全部是等价变换：

| 规则 | 改写 | 例 |
|---|---|---|
| `collapsible_if` | 嵌套 `if` → `if ... && let ...` / `if ... && cond` | `if let Some(p) = path.parent() { if !p.is_empty() { … } }` → `if let Some(p) = path.parent() && !p.is_empty() { … }` |
| `manual_is_multiple_of` | `x % n != 0` → `!x.is_multiple_of(n)` | `table.first_lba % ALIGNMENT_SECTORS != 0` |
| `chunks_exact_to_as_chunks` | `slice.chunks_exact(N)` → `slice.as_chunks::<N>().0`（常量 N） | GPT 分区项按 128 字节切分 |

随后 `cargo fmt --all`：新版 rustfmt 会重排 `if let ... &&` 这类新语法的折行。

**这些改写不改变行为**，由既有的 644 项测试（含 GPT/MBR 解析的往返用例）覆盖。

### 3. 文档与注释同步

- `docs/build-and-release.md`：MSRV 表改为 `1.99`，并把"为什么是这个版本"写成
  依赖图 + lint 的实际要求，附上升级工具链的必做项（`fmt` + `clippy`）。
- 删掉两处已失效的 MSRV 论证：`configfs.rs` 里"用 `EBUSY` 而非
  `ErrorKind::ResourceBusy` 是为了不抬高 MSRV"（该 API 到 1.83 就稳定了，
  现在 MSRV 远高于它；保留"`EBUSY` 更贴近内核实返回的 errno"这个仍然成立的理由），
  以及 `gadgetdisk-proto` 里"MSRV 1.85 上合法"的表述。

## Alternatives considered

**A. 把 `rust-version` 直接删掉。**
cargo 就不再做版本检查，`1.85.1` 上的失败会退化成一个"某个依赖要求更高版本"的错误，
而不是 manifest 层面的明确拒绝。丢失的正是 `rust-version` 唯一的用处。

**B. 把 `rust-version` 设成 `1.89`（`uuid` 的要求），工具链留在 `1.97`。**
能满足依赖，但 `1.99` 的 clippy 新规则照样会触发（那是**构建机**的 clippy 版本决定的，
与 `rust-version` 无关）。声明 `1.89` 而实际用 `1.99` 构建，等于又回到"声明低于实际"。

**C. 把新增的 clippy 规则加进 `allow` 列表。**
本仓库的既定策略是 `-D warnings` 基线为零（见 `docs/build-and-release.md` 的
clippy 分级策略），`--pedantic` 才是非阻塞参考项。为省 37 处等价改写而开口子，
会让"零告警"这个信号贬值。而且这些改写确实让代码更短（嵌套层级少一层）。

**D. 降级依赖（`cargo update uuid --precise 1.16` 之类）让 `1.85` 重新可用。**
需要把传递依赖钉在一个更旧的版本上，且每次 `cargo update` 都可能被顶回去。
用"钉住传递依赖"来维持一个没人用的旧 MSRV，是把复杂度花在错的地方。

**E. 保留 `1.85` 声明，只在文档里注明"实际需要 1.89+"。**
文档与 manifest 不一致时，manifest 才是机器读的那一份；这种不一致迟早会让某人
在旧工具链上浪费半天。

## Acceptance criteria

- `Cargo.toml` 的 `rust-version = "1.99"`；`cargo +1.85.1 check -q -p gadgetdisk-cli`
  **必须失败**并报 `requires rustc 1.99`（证明声明是有效的，而不是装饰）。
- `cargo fmt --all --check` 通过（含新版 rustfmt 对 `if let ... &&` 的折行）。
- `cargo clippy --workspace --all-targets --message-format=short` 输出中
  `warning:` / `error:` 计数为 **0**（`-D warnings` 基线仍为零）。
- `cargo nextest run --workspace`：644 项全通过——自动改写不改变行为。
- `uv run gd-check` 六项全绿；`uv run gd-test --all` 含 WebUI 的 Node 测试全绿。
- `uv run gd-build` 在 `1.99` 上仍能交叉编译两个 Android ABI（libc/rustix 的
  `--all-targets` 与 Android target 都要过）。

## Risks

- **依赖图可能在下次 `cargo update` 时再次抬高要求**（`uuid` 这类传递依赖不受本
  仓库控制）。症状是 `cargo check` 明确报"某包要求更高 rustc"，处理方式是把
  `rust-version` 跟着提上去——本 Note 的流程（提版本 → `fmt` → `clippy` → 全量测试）
  可以照搬。
- **`as_chunks` 要求块大小是编译期常量**：改写后的
  `raw.as_chunks::<{ GPT_ENTRY_MIN_LEN as usize }>()` 依赖 `GPT_ENTRY_MIN_LEN` 是
  `const`。若某天它变成运行期值，这个改写不成立（编译期就会报错，不会静默出错）。
- **不再支持 `1.85`–`1.98` 的工具链**。这对本仓库的实际影响很小（不提交
  `rust-toolchain.toml`、无 CI 钉版本、开发机就是最新 stable），但如果有下游构建
  环境停留在 `1.97` 之类，会需要升级一次。
- **自动改写过的 37 处需要人工复核语义**。已逐处确认是等价变换，并由全量测试覆盖；
  但 `identity.rs` 的 12 处涉及字段校验的分支合并（`if name == "serial" && let Some(bad) = ...`），
  是本批改动里最需要留意语义的一处——它由
  `gadgetdisk-usb` 的序列号 ASCII 校验测试守护。
