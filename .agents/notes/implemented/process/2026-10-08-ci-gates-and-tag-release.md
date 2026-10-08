# Agent Note: GitHub CI 门禁与标签发版

Status: implemented

## Problem

仓库此前**完全没有 CI**：门禁、全量测试与跨目标静态检查只靠人记得在本机跑。
具体暴露出的三个缺口：

1. `uv run gd-check` 只跑**宿主** target。而 `docs/build-and-release.md` 的
   「跨目标编译已知问题」一节（`rustix::fs::StatFs::f_type` 在 Linux 是 `c_long`、
   Android 是 `u64`）明确要求改这类代码后手动跑两条 `cargo check --target`——
   没有任何机制保证它真的被执行过。
2. **MSRV 声明不可执行**：`rust-version = "1.99"` 只是 manifest 里的一个字段，
   没有环境用它做过一次构建。
3. 没有产物发布通道：ZIP 只能由作者本机构建后手动上传，而「包内 `module.prop`
   的版本」与「二进制自报版本」的一致性守卫（`build-info.json`）只在本地生效。

约束：仓库已有 5 个 uv 入口与三级测试纪律，CI **不应另立一套检查**；宿主差异
（SDK 路径等）禁止进仓库；`.github/**` 此前不在 `AGENTS.md` 的「改动需确认」清单内。

## Proposal

新增单一 workflow `.github/workflows/ci.yml`，两个 job。

**`gates`**（任意分支推送 / 所有 PR / 手动 / tag 都跑）：按序安装
checkout → Rust `1.99.0`（+ `rustfmt`、`clippy`、两个 Android target）→ rust-cache →
`cargo-nextest` → uv → Node，然后执行 `uv sync --locked` →
`uv run gd-check` → **`cargo build --workspace --bins`** → `uv run gd-test --all` →
`cargo check --workspace --all-targets --target {aarch64,x86_64}-linux-android`。

**`release`**（`startsWith(github.ref, 'refs/tags/v')` 且 `needs.gates`）：
`fetch-depth: 0` 检出 → `version = ${GITHUB_REF_NAME#v}`、
`versionCode = git rev-list --count HEAD` → `gd-build` → `gd-package` →
`gh release create`。两处 fail-closed 断言：检出不得是 shallow（否则 commit 数恒为 1）、
versionCode 必须匹配 `^[1-9][0-9]*$`。

关键取舍（逐条给出实测依据）：

| 决定 | 依据 |
|---|---|
| 跨目标守卫用 `cargo check` 而**不用** `gd-build` | 清空 `ANDROID_HOME`/`ANDROID_NDK_HOME` 后两个 target 均 `Finished`（实测）。因此不需要 NDK，避免为 CI 引入 NDK 版本与安装方式的额外不确定性 |
| 测试前先 `cargo build --workspace --bins` | `nextest` 只构建 test target，而 `gadgetdisk-cli` 的 `loop_adapter` 有 5 个用例经 `testutil::TestMkfsFormatter` **执行** `target/<profile>/mkfsvfat`（刻意保留真实子进程调用链）。首次 CI 运行正是因此红了 5 个用例 |
| 显式装 Rust `1.99.0` | runner 自带 **1.98.1**，低于 MSRV，不装必然失败。钉住后 MSRV 声明变成可执行断言 |
| 所有 action 按 commit SHA 引用 | 标签可被上游移动；SHA 不可。升级是显式的人工提交 |
| `versionCode` 用 commit 数 | 与 `module.prop` 缺省值天然一致（本仓库当前 1 个 commit）；不像 Actions 运行计数器那样在重跑时产生空洞 |
| `uv sync --locked` 而非 `--frozen` | `--locked` 在锁文件过期时**非零退出**，`--frozen` 只警告且退出 0（均已实测） |
| `setup-node` 排在 `gd-check` 之前 | `pyright` 源码里 `USE_NODEJS_WHEEL` 未装时回退全局 node（`_resolve_strategy`，已读源码），CI 必须先有 node |
| 不引入 dependabot/Renovate | 当前无其他自动化需求，引入即多一套需维护的配置 |

`AGENTS.md` 同步加入一行 CI 事实与 `.github/**`、根 `README.md` 到「改动需确认」清单。

## Alternatives considered

- **只做门禁、不做发版**：范围最小，但「包内版本与二进制自报版本一致」这条最有价值
  的守卫仍然只在本地生效；且 tag 发版本来就需要人工本机构建，与「CI 是唯一可信构建者」
  相悖。
- **CI 里跑 `gd-build` 做跨目标守卫**：能额外验证链接，但引入 NDK 依赖（runner 的
  NDK 版本会随镜像漂移）与全量交叉编译时间。链接问题在实践中由 `gd-deploy` 后的真机
  验收覆盖，而**类型层**的问题（`cargo check` 能抓的那类）才是静默漏检的主因。
- **自建 NDK（下载指定版本）**：可复现性更强，但需要额外的缓存、校验与版本升级流程。
  当前收益不足以抵消维护面，记为待验证假设（#31）而不是现在就做。
- **`versionCode` 用 `github.run_number`**：无需 `fetch-depth: 0`，但重跑工作流会产生
  空洞，且与仓库内既有 `versionCode` 语义（KernelSU 的递增整数）只是"碰巧"都递增。
- **`versionCode` 写进 tag（如 `v0.2.0-20`）**：显式可控，但 tag 不再是纯 semver。
- **引用 `dtolnay/rust-toolchain@stable`**：省去手动跟进版本，但会让「MSRV 声明」随
  runner 漂移而失去断言意义。
- **`uv sync --frozen`**：不因缺锁而失败（实测 exit 0 且只警告），不适合当门禁。

## Acceptance criteria

1. `.github/workflows/ci.yml` 通过 YAML 解析（`pyyaml`），两个 job 的步骤与
   `docs/build-and-release.md#持续集成` 的表格逐条对应。
2. 本地复现 CI 命令序列全部通过：`uv sync --locked`、`uv run gd-check`
   （`All 6 checks passed`）、`cargo build --workspace --bins`、`uv run gd-test --all`、
   两条 `cargo check --target`（在**清空** Android 环境变量下通过）。
3. **顺序约束可复现**：在**全新 `CARGO_TARGET_DIR`** 下按上述顺序执行，
   `cargo nextest run --workspace` 报 **644/644 passed**；跳过
   `cargo build --workspace --bins` 则稳定红掉
   `loop_adapter` 的 5 个用例（`bundled mkfsvfat binary not found`）。两步均已实测。
4. release job 的命令序列在本地以真实值预演成功：`gd-build` → `gd-package` 通过
   `build-info.json` 一致性守卫，产出 ZIP 名与包内 `module.prop` 两行一致。
5. `AGENTS.md` 词数仍在 1500 以内（加入 CI 行后为 1459），且治理门禁全体文件 **PASS**。
6. 三条新待验证假设（CI 首次运行、runner NDK 差异、versionCode 回退）已进
   `docs/roadmap.md`。

## Risks

- **首次运行暴露的既有缺陷（根因不在 CI）**：`gadgetdisk-cli` 的 5 个 `loop_adapter`
  用例依赖 `target/<profile>/mkfsvfat`，而 `nextest` / `cargo test` 都不构建 bin target。
  这不是本次引入的——在**干净的 HEAD worktree + 全新 `CARGO_TARGET_DIR`** 上同样复现，
  只是开发机因残留文件而长期显示全绿。本次用 CI 的步骤顺序（先 `cargo build
  --workspace --bins`）兜住；**更彻底的做法是让这些测试不再依赖外部产物**（同文件里的
  `logical_partition_resolves_to_its_absolute_offset` 已用 `NoopFormatter` 示范），
  但那会牺牲 CLI→子进程的集成覆盖，故未在本次改动。记为后续候选。
- **首次运行暴露的第二个既有缺陷：WebUI 测试的 Node 版本依赖**。`scripts/test/cli.py`
  原先执行 `node --test tests/`（**目录**形式）。实测该形式在 **Node 22 与 24 上失败**
  （把目录当模块 `require`，报 `Cannot find module .../webui/tests`，exit 1、0 个测试），
  只有 Node 26+ 才容忍目录参数——而 `setup-node` 钉的是 24，于是**本地（26）全绿、
  CI 必红**。已改为由 Python 枚举 `webui/tests/*.test.mjs` 后传**显式文件路径**：
  该写法在 22/24/26 上均 252/252 通过。**未采用** `--test 'tests/**/*.test.mjs'` 或
  裸 `--test`——两者在**零匹配时静默 `exit 0`**（实测），测试文件被误删会得到一次绿色
  的空跑，与本仓库「绝不静默通过」的立场相悖；现写法零匹配即报错（已负向验证）。
  这条同时说明：`setup-node` 的版本是**被测试到的行为**，不只是"装个 node"。
- **待验证假设 #30**：CI 首次运行未实测——本机无 `act`、无 GitHub 侧环境。已按各 action
  的 `action.yml` 核对了输入名，但运行期行为（缓存命中、`gh` CLI 可用性）只能由首次推送确认。
- **待验证假设 #31**：runner 自带 NDK（27.3.13750724）与本地（29.0.14206865）不同，
  release 产物因此不是逐字节可复现的。已在 job summary 记录 `source.properties` 缓解。
- `gh release create` 依赖 runner 预装 `gh` CLI，未在本机验证；退路是改用
  `softprops/action-gh-release`（需再钉一个 SHA）。
- **`versionCode` 会随历史改写回退**（待验证假设 #32）：本仓库刚做过一次 squash，
  当前 commit 数（1）与 `module.prop` 缺省值一致属巧合。KernelSU 要求递增，回退可能导致
  管理器拒绝更新；本次不承诺单调性。
- 按 SHA 引用意味着 action 升级需要人工跟进；不引入自动升级机器人是**有意的**取舍。
