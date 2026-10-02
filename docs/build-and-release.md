# 构建与发布

本文件是**脚本入口、工具链探测、代码风格与打包产物**的唯一事实归属。
测试选择策略见[测试规范](testing.md)，crate 划分见[架构规范](architecture.md)，
尚未在真机或实测确认的结论见[路线图](roadmap.md)的待验证假设汇总。

## 脚本入口

构建脚本已封装为 **uv 管理的 Python 包**（由 `pyproject.toml` 定义，通过 `uv.lock` 锁定
依赖版本），入口经 `[project.scripts]` 暴露。所有子命令均通过 `uv run <subcommand>` 调用，
禁止直接执行脚本文件。首次使用请先执行 `uv sync`（初始化 `.venv/` 并安装开发依赖）。

| 入口 | 实现 | 职责 |
|---|---|---|
| `uv run gd-build` | `scripts/build/cli.py` | 交叉编译各 ABI 的 `gadgetdisk` 与 `gdd`，暂存到 `target/dist/bin/<abi>/` |
| `uv run gd-package` | `scripts/package/cli.py` | 组装模块 ZIP → `target/dist/GadgetDisk-<version>.zip` |
| `uv run gd-test` | `scripts/test/cli.py` | 差分 / 全量测试，策略与目录约定见[测试规范](testing.md) |
| `uv run gd-check` | `scripts/check/cli.py` | **格式与静态检查总闸**，见下「代码风格与静态检查」 |
| `uv run gd-deploy` | `scripts/deploy/cli.py` | 推送并安装（含暂存路径提示），见「发布流程」 |

公共参数（完整列表以 `--help` 为准）：

| 入口 | 参数 |
|---|---|
| `gd-build` | `--abi <abi>`（可重复，缺省全部）、`--debug`（缺省 release）、`--no-stage`、`--version <v>`、`--version-code <n>` |
| `gd-package` | `--abi <abi>`（可重复，缺省取 `target/dist/bin/` 下已构建的全部）、`--out <path>`、`--version <v>`、`--version-code <n>` |
| `gd-test` | `--all`、`--base <ref>`、`--dry-run` |
| `gd-check` | `--fix`、`--rust-only`、`--python-only`、`--pedantic`、`--quiet`、`--no-gates` |
| `gd-deploy` | `--zip <path>`、`--reboot`、`--no-install` |

包布局：`scripts/__init__.py`、`scripts/lib/common.py`（探测与命令执行的共用实现）、
以及 `scripts/{build,package,test,check,deploy}/` 下各自的 `__init__.py` + `cli.py`。
`gd-package` 只把 `scripts/` 装进 wheel（`only-include = ["scripts"]`），仓库其余部分是
Rust 与文档，不属于这个 Python 项目。

## 工具链探测

探测逻辑集中在 `scripts/lib/common.py`。**仓库内不含任何机器特定路径**：只使用公认的
环境变量与标准 SDK 布局，失败时列出已尝试的每个位置并报错，**绝不静默使用猜测值**。

### Rust 版本

| 项 | 值 |
|---|---|
| `edition` | `2024` |
| MSRV（`rust-version`） | `1.99` |

`rust-version` 声明的是**最低可编译版本**，不是构建机上装的版本。cargo 会据此拒绝
更旧的工具链（实测在 `1.85.1` 上加载 manifest 直接失败：
`gadgetdisk-cli@0.1.0 requires rustc 1.99`），因此这个字段必须反映**代码实际需要的**
下限，而不能留一个更旧的数字——声明得比实际低，等于把「用旧工具链构建」的失败
推迟到某个随机的编译错误上。

**为什么是 1.99**：`1.85` 曾是「edition 2024 的硬性下限」，但依赖图随后抬高了要求——
`gpt` 的传递依赖 `uuid@1.27.0` 需要 `1.89`，`clippy` 在 `1.99` 上又把
`collapsible_if` / `manual_is_multiple_of` / `chunks_exact_to_as_chunks` 纳入了
`-D warnings` 的判定范围。与其声明一个已经不能构建的旧下限，不如把下限提到**当前
实际验证过的版本**：仓库已完成一次 `1.85 → 1.99` 的升级（全量重排 + 新 lint 收敛）。

仓库**不提交 `rust-toolchain.toml`**，构建机用任意 `>= 1.99` 的 stable 即可；
CI 显式安装 `1.99.0`（runner 自带的 stable 低于本仓库 MSRV），并同时装齐两个
Android target，因此「MSRV 声明」在 CI 上是可执行的断言，而代码中**不得**依赖高于
1.99 才稳定的 API。

> **升级工具链时的必做项**：`cargo fmt --all`（新版 rustfmt 的排版规则会变）与
> `cargo clippy --workspace --all-targets`（新版 clippy 会新增默认 warn 级规则，
> 而本仓库把它们全部设为阻塞）。两者都会在 `uv run gd-check` 里暴露。

### NDK

| 优先级 | 来源 |
|---|---|
| 1 | `ANDROID_NDK_HOME`（存在且是目录） |
| 2 | `ANDROID_HOME` 本身，或其下的 `ndk/`（按「本目录含 `toolchains/`」或「取最新版本子目录」判定） |

**只认这两个变量**：不读 `ANDROID_NDK_ROOT` / `NDK_HOME` / `ANDROID_SDK_ROOT` 等别名，
也不读 `local.properties`。探测入口收敛到一处，避免「没设环境变量时到底用了哪个」的歧义。

**NDK 必须与构建宿主同平台**：Linux 宿主使用 Linux 版 NDK，其
`toolchains/llvm/prebuilt/<host>/bin/clang` 是原生可执行文件，直接解析 rustc 传入的
Linux 路径。在 Linux 宿主上指向 Windows 版 NDK（其 clang 是调用 `clang.exe` 的包装脚本）
会让链接阶段报 `no such file or directory`——**本仓库不做路径形式转换来兼容这种用法**，
也没有链接器覆盖机制；正确做法是换成同平台的 NDK。

**NDK 宿主目录动态探测**：针对官方 NDK 中随宿主架构变动的
`toolchains/llvm/prebuilt/<host>/` 路径，探测逻辑通过动态扫描 `prebuilt/*` 目录并
校验 clang 编译器有效性完成匹配，避免静态枚举无法覆盖边缘平台。

### adb

| 优先级 | 来源 |
|---|---|
| 1 | `ADB` → `ANDROID_ADB`（值必须是存在的文件） |
| 2 | `PATH` |
| 3 | `$ANDROID_HOME/platform-tools/adb` |

多设备选择用 `ANDROID_SERIAL`（由 adb 自身识别）。`gd-deploy` 在检测到多台处于
`device` 状态的设备时直接报错并提示设置该变量，不猜目标。

### 环境变量契约

| 变量 | 用途 |
|---|---|
| `ANDROID_NDK_HOME` | NDK 根目录（最高优先级） |
| `ANDROID_HOME` | Android SDK 根目录（据此推导 NDK 与 platform-tools） |
| `ADB` / `ANDROID_ADB` | adb 可执行文件 |
| `ANDROID_SERIAL` | 多设备时选择目标 |
| `CARGO_HOME` | **不读也不改写**，一律使用用户默认的 `~/.cargo`，使依赖缓存与已安装的 cargo 子命令在多个检出间共享 |
| `UV_DEFAULT_INDEX` / `UV_CACHE_DIR` | uv 的索引与缓存目录；等价的持久写法是本机 `uv.toml` |

## 宿主中立性

代码库保持宿主中立性：**仓库不提交宿主绝对路径**，工具链位置一律经上述环境变量注入。

### 不提供链接器覆盖机制

曾有一个 `GD_LINKER_WRAPPER_<TARGET>` 扩展点，用于在「Linux 宿主 + Windows 版 NDK」
这种跨平台混用场景下注入路径翻译包装器。**该机制已移除**：

- 它的适用场景本身就是错误配置——NDK 应与构建宿主同平台；
- 换用同平台 NDK 后，clang 直接解析 Linux 路径，包装器不再有任何用途；
- 保留它会让「构建失败」多出一条与真实原因无关的排查分支。

因此 linker 恒为探测到的 NDK 自带的 `<arch>-linux-android30-clang`，`gd-build` 不读任何
链接器覆盖变量。理由与备选方案见
[移除 linker 包装器 Note](../.agents/notes/implemented/simplification/2026-10-05-drop-linker-wrapper-and-pin-ndk-env.md)。

### `.cargo/config.toml` 不再跟踪

`.cargo/config.toml` 已从 git 移除（磁盘上可能仍存在），该单文件由 `.gitignore` 忽略。
linker 与 `RUSTFLAGS` 一律由 `gd-build` 在构建时通过环境变量
注入（`CARGO_TARGET_<TARGET>_LINKER`、`CC_<target>`、`AR_<target>`、`RUSTFLAGS`），
因此**没有任何本机绝对路径需要写进仓库**。

### 忽略清单

**单一事实源是提交的 `.gitignore`**：忽略规则一律写在那里，`.git/info/exclude`（本机私有、
不提交）保持为空，避免「换机器或清空该文件后构建产物被误提交」。

`.gitignore` 覆盖的条目：

| 类别 | 条目 |
|---|---|
| Rust 构建与打包产物 | `/target/`、`**/target/`（打包产物在 `target/dist/`，随 target 一并忽略） |
| Python 环境与检查缓存 | `/.venv/`、`/.uv-cache/`、`/.ruff_cache/`、`/.pyright/` |
| 本机差异 | `/.cargo/config.toml`、`/local.properties`、`/local/`、`/uv.toml` |
| 验证与临时产物 | `/.probe/`、`/.tmp/`、`*.img`、`__pycache__/`、`*.pyc` |

> 不再有仓库内的 `CARGO_HOME`（`.cargo-home/`）与本地工具目录（`.tools/`）：前者已改为
> 使用用户默认的 `~/.cargo`，后者随 `cargo-nextest` 改走官方安装器而删除。
>
> `local.properties` 与 `local/` 曾用于本机 Gradle SDK 指向与链接器包装器，两者都已废弃
> （脚本不再读取前者，后者随 `GD_LINKER_WRAPPER_<TARGET>` 一并删除），但保留在忽略清单中
> 以免旧检出误提交。

### `uv.lock` 与镜像

| 项 | 规则 |
|---|---|
| `uv.lock` 的 registry | **入库的那一份必须是 canonical 的 `https://pypi.org/simple`**，提交前核对 |
| 本机镜像 | 写在**不提交**的 `uv.toml` 里：`cache-dir` 指定可写缓存（某些机器上 `$HOME/.cache/uv` 不可写），`[[index]]` 指定 PyPI 镜像并置 `default = true` |
| 为什么不冲突 | `uv` 在 sync 时以**已配置的 index 覆盖**锁文件记录的 registry，因此把锁钉在 pypi.org 不会让镜像用户失败；两者可以同时成立 |

> **核对命令**：`grep -o 'https://[^"]*' uv.lock | sed 's|\(https://[^/]*\).*|\1|' | sort -u`
> 应当只输出 `https://pypi.org`。若在配了镜像的机器上重新生成过锁文件，它会带上镜像
> 主机名——**入库前必须改回 canonical 源**，否则没有该镜像的机器无法复现依赖。

环境变量 `UV_DEFAULT_INDEX` / `UV_CACHE_DIR` 是 `uv.toml` 的等价临时写法。

## 代码风格与静态检查

`uv run gd-check` 是**唯一的格式与静态检查总闸**：一次跑完全部检查，任一失败即非零退出。
检查在开始前一次性规划好，失败时**不提前返回**，以便一轮看到所有问题。

| 顺序 | 检查 | 命令 |
|---|---|---|
| 1 | Rust 格式 | `cargo fmt --all --check` |
| 2 | Rust 静态检查 | `cargo clippy --workspace --all-targets -- -D warnings` |
| 3 | Python 格式 | `uv run ruff format --check scripts .agents/scripts` |
| 4 | Python 静态检查 | `uv run ruff check scripts .agents/scripts` |
| 5 | Python 类型检查 | `uv run pyright scripts .agents/scripts` |
| 6 | 治理与文档门禁 | `python3 .agents/scripts/gates/verify_agent_gates.py` |

`--fix` 就地修复可自动修复项（`cargo fmt --all`、`ruff format`、`ruff check --fix`），
此时**不跑门禁**，修完须重新运行 `uv run gd-check` 复核。`--rust-only` / `--python-only`
各自只跑一半（两者不可同时使用）；`--quiet` 让 clippy 用 `--message-format=short`；
`--no-gates` 跳过第 6 项。

### rustfmt

`rustfmt.toml` 只设三项，判据都是「对现有代码**零 diff**」（`cargo fmt --all --check`
在本配置下不产生任何改动）：

| 配置 | 值 | 理由 |
|---|---|---|
| `edition` | `"2024"` | 与 Cargo workspace 一致；显式写出可让单独运行 rustfmt（不经 cargo）时行为一致 |
| `max_width` | `100` | 与 ruff 的 `line-length` 一致，避免两种语言在编辑器里折行位置不同 |
| `newline_style` | `"Unix"` | 源码用 LF；显式声明可避免 Windows 检出（`core.autocrlf`）被改成 CRLF 后 `--check` 误报。用 `"Unix"` 而非 `"Native"`，因为后者跟随构建宿主，会让同一份代码在不同平台得出不同的 `--check` 结论 |

> **`edition` 会连带决定 `style_edition`**（未显式设置时跟随 `edition`）。从 2021 升到
> 2024 时，排版规则随之变化：`use` 分组改按 ASCII 序、`assert!`/`vec![]` 折行位置变化、
> `return` 表达式在分支末尾补分号。本仓库已按 2024 风格全量重排；日后若需恢复旧排版，
> 应显式写 `style_edition = "2021"`，而不是回退 `edition`。

### Cargo workspace lint 策略

根 `Cargo.toml` 的 `[workspace.lints.rust]` / `[workspace.lints.clippy]` 是唯一出处，
每个 crate 通过 `[lints] workspace = true` 继承。

**判据：本节声明的策略必须当前零告警。** 因为 `gd-check` 用 `-D warnings` 跑 clippy，
任何一条 `warn` 级规则写进来都等于立刻让所有改动无法通过检查。因此这里只放两类：
`deny`（提交后一定出问题）与**显式** `allow`（写明「有意不启用」，避免读者以为是遗漏）。

| 规则 | 级别 | 说明 |
|---|---|---|
| `unsafe_code` | `allow` | 内核交互（configfs / loop ioctl / mount）必然需要 `unsafe`，实测当前 41 处；设为 `warn` 只会产生持续噪音而无法收敛。可强制的替代目标是「每处 `unsafe` 都带 `SAFETY:` 注释」，由下一条 `undocumented_unsafe_blocks` 强制 |
| `clippy::undocumented_unsafe_blocks` | `deny` | 每处 `unsafe` 必须写 `SAFETY:` 注释说明「为何安全」。当前 0 处遗漏 |
| `clippy::cast_possible_truncation` | `deny` | 整数截断。镜像布局与容量计算里的这类转换是**真实风险来源**，强制显式处理（`try_from` + 说明性 `expect`，或提升到更宽类型比较），不允许 `as` 静默截断。当前 0 处 |
| `clippy::cast_sign_loss` | `deny` | 无符号/有符号转换丢符号，同上。当前 0 处 |
| `clippy::dbg_macro` | `deny` | 调试宏不得进入提交 |
| `clippy::todo` | `deny` | 未完成标记不得进入提交 |
| `clippy::unimplemented` | `deny` | 同上 |
| `clippy::allow_attributes_without_reason` | `deny` | `#[allow(...)]` 必须写明理由 |

**刻意暂不启用**（在 `Cargo.toml` 中注明为后续专项，若立即设为阻断级别会导致当前改动无法推进合入）：

| 候选 | 当前命中 | 为什么值得做但暂缓 |
|---|---|---|
| `clippy::pedantic` | 587 | `gd-check --pedantic` 会追加 `-W clippy::pedantic`（非阻塞）。主要为「函数返回 `Result` 缺少 `# Errors` 文档」137 条、「方法建议加 `#[must_use]`」127 条、「文档缺反引号」89 条，大部分建议与正确性无关 |

> **整数转换的写法约定**：本仓库**不使用 `#[allow(...)]` 豁免**（全仓零 allow）。
> 遇到「clippy 看不见、但人知道安全」的转换时，统一写法是：
>
> ```rust
> // 已确认 n >= 0，故转换必然成功。
> let n = usize::try_from(n).expect("n 已检查 >= 0");
> ```
>
> 或提升到更宽的类型再比较（`u64::from(x) >= CONST`，而不是 `x >= CONST as u32`）。
> 二者都把「不可能失败」写成可执行的断言，而不是让 `as` 静默截断。

### 优先用 `rustix` 而非裸 `libc`

系统调用一律**优先使用 `rustix` 的安全封装**（`statfs`/`statvfs`/`chmod`/`mknodat`/
`flock`/`mount`/`lgetxattr` 等），只有在 rustix 无对应 API、或其封装会掩盖本项目刻意
显式化的内核语义时才回退到裸 `libc`（如 `LOOP_SET_STATUS64` 的裸 ioctl、
`SO_PEERCRED` 的凭据校验）。

用 rustix 的收益不只是「少一处 `unsafe`」：它还免去手工 `CString` 构造、手工
`mem::zeroed()`、手工 errno 检查与整数转换，并让错误类型统一可转 `std::io::Error`
（errno 保留）。

> **跨目标编译已知问题（实测记录）**：`rustix::fs::StatFs::f_type` 的类型是平台相关的
> `FsWord` —— Linux（`linux_raw` 后端）为 `c_long`，**Android（`libc` 后端）为 `u64`**，
> 部分平台为 `u32`。因此：
> - `i64::from(st.f_type)` —— Android 上编译失败；
> - `st.f_type.try_into()` —— 宿主上被 `clippy::useless_conversion` 拦下；
> - `st.f_type as u64` —— 被已设为 `deny` 的 `clippy::cast_sign_loss` 拦下。
>
> 可用写法是**统一经 `i128` 中转**：`i128::from(st.f_type) == i128::from(MAGIC)`。
> `i64`/`u64`/`u32` 三种形态都能无损转入，且在宿主与两个 Android 目标上均零告警。
>
> 该问题无法通过 `uv run gd-check` 直接检出（它只跑宿主 target）。改动这类平台相关代码后，
> 必须手动跑跨目标编译：
>
> ```sh
> cargo check --workspace --all-targets --target aarch64-linux-android
> cargo check --workspace --all-targets --target x86_64-linux-android
> ```

`--pedantic` 模式下 clippy **不再传 `-D warnings`**，改为 `-W clippy::pedantic`，即它是
非阻塞的渐进改善工具，不是提交门禁。

### ruff 与 pyright

配置都在 `pyproject.toml`。

| 项 | 值 | 理由 |
|---|---|---|
| `line-length` | `100` | 与 rustfmt `max_width` 一致 |
| `target-version` | `py311` | 与 `requires-python = ">=3.11"` 一致 |
| `select` | `E` `F` `W` `I` `N` `UP` `B` `PLE` `PLW` `RUF` | 错误/未使用/导入排序/命名/语法现代化/bugbear 真实缺陷模式/pylint 错误与警告/ruff 自有规则 |
| `ignore` | `RUF001` `RUF002` `RUF003` | 正文与注释用中文全角标点（`：`、`（`、`，`）是既定风格，这三条把它们报成「ambiguous unicode」，对本仓库是纯噪音 |
| pyright `typeCheckingMode` | `standard` | 只要求标准严格度 |
| pyright `include` | `["scripts"]` | 与 `gd-check` 传入的路径一致；类型检查覆盖 `scripts` 与 `.agents/scripts` |

**刻意不选 `PLR`**（pylint 重构建议：分支/语句过多、magic value）：那些阈值对「把一件事
写完整」的构建脚本是反向激励——这类脚本本来就要顺序做二十来件事，拆成小块只会
让「哪一步失败了」更难读。

开发依赖**版本刻意钉死**（`ruff==0.16.2`、`pyright==1.1.409`）：格式与类型检查必须在所有
机器上给出同一份结论，否则「本地通过、别人不通过」会变成常态。Python 侧一律以
`uv run ruff` / `uv run pyright` 调用，工具由 `uv.lock` 决定。

## 支持的目标 ABI

| ABI | Rust target | 支持 | 说明 |
|---|---|---|---|
| `arm64-v8a` | `aarch64-linux-android` | ✔ | 真机主力 |
| `x86_64` | `x86_64-linux-android` | ✔ | 模拟器；静态链接需额外的 builtins archive（见下节） |
| `armeabi-v7a` | `armv7-linux-androideabi` | ✘ | Rust target 未安装（需 `rustup target add`），`customize.sh` 明确报错中止 |

Android API level 为 **30**（`scripts/lib/common.py` 的 `ANDROID_API`），与 NDK clang
wrapper 的 `-android30` 后缀一致。新增 ABI 时必须同时更新 `ABI_TARGETS` 与
`customize.sh` 的架构探测分支，否则两层判定会不一致。

## 交叉编译要点

`gd-build` 对每个目标注入以下环境（不写任何配置文件）：

| 变量 | 值 |
|---|---|
| `CARGO_TARGET_<TARGET>_LINKER` | NDK 的 `<arch>-linux-android30-clang` |
| `CC_<target>` | 同上，便于依赖的构建脚本（如 `cc` crate）使用同一编译器 |
| `AR_<target>` | `<ndk>/toolchains/llvm/prebuilt/<host>/bin/llvm-ar` |
| `RUSTFLAGS` | `-C target-feature=+crt-static`（静态链接，避免依赖设备上的 libc 版本） |

`CARGO_HOME` **不在**注入之列：一律沿用用户默认的 `~/.cargo`。

**`x86_64-linux-android` 额外需要 `libclang_rt.builtins-x86_64-android.a`**（已实测）：
缺少它会出现 `undefined symbol: __cpu_model`。脚本在
`<ndk>/toolchains/llvm/prebuilt/<host>/lib/clang/*/lib/linux/` 下按文件名 glob 找到该
archive，找不到就报错并指出预期位置，而不是让链接阶段报一个指不到原因的错误。
`aarch64-linux-android` 不需要它。

构建产物先落到 `target/<target>/<profile>/`，再由 `gd-build` 复制到
`target/dist/bin/<abi>/`（权限 0755）；`--no-stage` 只验证能否编译。

## 模块包结构

产物：`target/dist/GadgetDisk-<version>.zip`（`<version>` 取自 `module_template/module.prop`）。

```
module.prop
customize.sh
service.sh
uninstall.sh
bin/arm64-v8a/gadgetdisk
bin/arm64-v8a/gdd
bin/arm64-v8a/mkfs.vfat
bin/x86_64/gadgetdisk
bin/x86_64/gdd
bin/x86_64/mkfs.vfat
webroot/index.html
webroot/main.js
webroot/backend.js
webroot/dom.js
webroot/task.js
webroot/ksu.js
webroot/view-*.js
webroot/pure/*.js
webroot/style.css
```

**规则**

| 规则 | 说明 |
|---|---|
| WebUI 来源 | 顶层 `webui/`（自包含的一层：`index.html`、`style.css`、`ksu.js`、`main.js`、`backend.js`、`dom.js`、`task.js`、`view-*.js`，以及 `pure/` 子目录），打包时复制为包内 `webroot/*`；更换前端架构只需替换该目录 |
| 必需文件 | 模板侧 `module.prop`、`customize.sh`、`service.sh`、`uninstall.sh`；WebUI 侧核心资源文件及 `pure/` 子目录。缺少任一必要文件均会导致打包**失败** |
| 版本来源 | `module_template/module.prop` 提供**缺省值**；`gd-build` / `gd-package` 的 `--version` / `--version-code` 可覆盖。两个入口必须传同一个值——二进制自报的版本要与包内 `module.prop` 一致（见下「版本号如何对齐」） |
| 权限 | 二进制与 `customize.sh` / `service.sh` / `uninstall.sh` 为 `0755`；其余 `0644`。ZIP 不保留 Unix 权限位，因此写入时显式设置，并在打包后**重新打开 ZIP 校验** |
| 可复现 | 固定 ZIP 时间戳（1980-01-01），相同输入产出逐字节一致的文件 |
| 只打包齐备的 ABI | 只写入 `target/dist/bin/<abi>/` 下 `gadgetdisk` 与 `gdd` **都在**的 ABI；显式 `--abi` 指定的 ABI 缺件则直接报错 |

**不得出现在包内**

| 内容 | 原因 |
|---|---|
| `sepolicy.rule` | 已删除：两个二进制都跑在 root 管理器的 `su` 域，不规划独立域，socket 的边界由 `0700` 目录与 `SO_PEERCRED` 提供 |
| `*.apk` | 项目前提是纯 WebUI，无配套应用 |
| `webroot/api.json` | `serve` 的运行期产物（临时端口 + 短时效 token）；打进包等于发布一个已过期的 token，且让 WebUI 去连不存在的端口。打包时会从 `webui/` 删除该残留文件 |
| `webroot/tests/` | WebUI 自己的测试属于仓库，不属于模块 |
| `logs/`、`run/`、`config/` | 运行期产物，由程序在设备上创建 |
| `post-fs-data.sh` | 该阶段已删除（职责归 `service.sh` + `gadgetdisk boot`），重新引入会让「决策只在 Rust 侧」静默失效 |

**安装期扁平化（关键）**

包内按 `bin/<abi>/` 分目录，因为同一个 ZIP 要能装到 arm64 真机与 x86_64 模拟器；
`customize.sh` 使用安装器已经算好的 `$ARCH`（`arm64` / `x64`）后，把匹配的那三个
二进制**移动**到扁平位置并删除其余 ABI 目录。

> `$ARCH` / `$IS64BIT` 由安装器在 `source customize.sh` **之前**设置
> （KernelSU `installer.sh` 与 Magisk `util_functions.sh` 的 `api_level_arch_detect`），
> 因此脚本不再自己 `getprop ro.product.cpu.abi` 或 `uname -m` 兜底：同一件事算两遍，
> 两份结果可能不一致，那时以哪份为准就成了隐式约定。**APatch 是否同样导出这两个
> 变量未经核对**，属待验证假设；不成立时表现为安装期明确 abort，而不是静默装错架构。

**运行期只有一条二进制路径**（`bin/gadgetdisk`、`bin/gdd`、`bin/mkfs.vfat`）。这不是
风格问题：WebUI 读不到 `ro.product.cpu.abi`，只能按已知目录逐个探测，猜错会让所有命令
失败，且症状是「后端不可达」，完全指不到真正的原因。安装期已经知道架构，没有理由把
这个不确定性留到运行期。理由另见[架构规范](architecture.md)。

**三个二进制必须一起发布**：

- `gadgetdisk` 需要导出时通过**同目录**找到 `gdd` 拉起它；
- `gadgetdisk` 创建镜像时需要自带的 `mkfs.vfat`（设备上通常没有 dosfstools）。

只带 `gadgetdisk` 的包会在第一次挂载时报「找不到 gdd」、在第一次创建 FAT32 镜像时
报「未找到 fat32 的 mkfs 工具」。

**Cargo 产物名与包内名不同（`mkfs.vfat`）**：Cargo **不允许二进制名含 `.`**，因此
crate `gadgetdisk-mkfsvfat` 的产物叫 `mkfsvfat`；`scripts/lib/common.py` 的
`BIN_RENAMES` 在暂存阶段把它重命名为 dosfstools 惯用的 `mkfs.vfat`。构建、打包与
`customize.sh` 的校验都读同一份映射，避免三处各写一份名字。

**不预设 `webroot/` 的权限与 SELinux 上下文**：安装器会自动处理。

## `module.prop` 字段

字段定义在 `module_template/module.prop`，它是**版本号的缺省来源**（不是唯一来源：
`gd-build` / `gd-package` 可用 `--version` / `--version-code` 覆盖，见下）。

| 字段 | 必需 | 说明 |
|---|---|---|
| `id` | ✔ | 模块 id，决定安装目录 `/data/adb/modules/<id>` 与暂存目录 `/data/adb/modules_update/<id>` |
| `name` | ✔ | 显示名，同时用作 ZIP 文件名前缀 |
| `version` | ✔ | 语义化版本，出现在 ZIP 文件名与部署输出中；可被 `--version` 覆盖 |
| `versionCode` | ✔ | 递增整数（脚本校验必须是纯数字）；可被 `--version-code` 覆盖 |
| `author` | ✘ | 作者 |
| `description` | ✘ | 一句话描述 |

打包时只把 ZIP 内 `module.prop` 的 `version` / `versionCode` 两行替换为本次发布的
版本，其余行（含 `description`）逐字保留；仓库里的模板文件**保持缺省值不变**
（它是缺省来源，不是产物）。

## 版本号如何注入与对齐

三个二进制的版本号由构建环境注入，而不是各写各的常量：

| 环节 | 机制 |
|---|---|
| Rust 常量 | `gadgetdisk-proto::VERSION` 与 `gadgetdisk_mkfsvfat::VERSION` 都是 `match option_env!("GD_VERSION") { Some(v) => v, None => env!("CARGO_PKG_VERSION") }` |
| 编译期注入 | `gd-build` 把 `--version`（缺省取 `module.prop`）写进 `GD_VERSION` 环境变量 |
| 自报 | `gadgetdisk --version`、`gdd --version`、`mkfs.vfat -V` 都输出该值 |
| 打包期写包 | `gd-package` 把版本写进 ZIP 内 `module.prop` |
| 一致性守卫 | `gd-build` 在 `target/dist/build-info.json` 里按 ABI 记录版本；`gd-package` 比对，不一致**拒绝打包** |

**为什么需要 `build-info.json`**：Android 目标的 ELF **不能在宿主执行**，因此打包时
无法"运行二进制问它版本"。若两个入口各传各的版本，就会产出"包内版本与二进制自报
版本不一致"的 ZIP，而这种不一致在设备上只能靠人肉发现（版本号写错不会让任何命令报错）。
因此构建时记下事实、打包时核对，是唯一可靠且可被测试覆盖的做法。

`option_env!` 会被 cargo 记录为**环境依赖**，改 `GD_VERSION` 必然触发重编，不会留下
旧版本号的产物。未注入时回落到 Cargo 包版本，`cargo run` / `cargo test` / IDE 的行为
与历史一致。

## 发布流程

按顺序执行；前三步任何一步失败都不应进入下一步。

| 步骤 | 命令 | 说明 |
|---|---|---|
| 1 | `uv run gd-check` | 格式 + 静态检查 + 治理门禁全绿 |
| 2 | `uv run gd-test --all` | 全量测试（含 WebUI 的 Node 测试） |
| 3 | `uv run gd-build` | 交叉编译全部支持的 ABI，落 `target/dist/bin/<abi>/`，并写 `build-info.json` |
| 4 | `uv run gd-package` | 产出 `target/dist/GadgetDisk-<version>.zip` 并自校验结构 |
| 5 | `uv run gd-deploy` | 推送并安装；**默认不重启**，`--reboot` 显式请求 |

`gd-deploy` 的安装优先级为 `ksud module install` → `magisk --install-module` →
手工解包到 `/data/adb/modules/<id>`（模块目录是两者的**事实标准**）；手工路径会在设备上
补做 `customize.sh` 的等价动作（扁平化、`chmod 0755`），因为脚本不会被执行。可用性取决于
用户的 root 管理器。

**`gd-deploy` 不做安装后验证。** 它只推送、安装，并如实报告是否走了暂存路径。理由：
在目标设备上验证模块产物的可用性，必须在**重启之后、在真实使用路径里**进行（通过 WebUI
实际操作一遍）。而 `ksud` 走的是暂存路径，脚本能触及的只有尚未生效的
`modules_update/<id>/`——在那里跑冒烟测试得到的绿色结果**并不代表最终生效版本功能完备**，
反而会给人以「已验证」的错觉。真机验收清单见[测试规范](testing.md)。

### 安装后模块处于 pending-update：WebUI 打不开（已实测）

`ksud module install` **不直接改写** `modules/<id>/`，而是解到
`modules_update/<id>/` 并写 `modules/<id>/update` 标记，等**下次开机**才提升为生效版本。

在提升之前，KernelSU 会**拒绝打开该模块的 WebUI**（toast：`Module <name> is disabled,
updating, or pending removal`），`WebUIActivity` 在 `onLaunchFailed()` 里 `finish()`，
页面根本不加载。该异常在前端常表现为 WebUI 无法启动、开发者工具无法连接套接字或 JavaScript 脚本未执行等假象。
排查时应优先确认模块是否处于待生效（pending-update）状态，避免误判为前端自身缺陷。

三种处理方式：

- 重启设备（`uv run gd-deploy --reboot`）——最干净；
- 直接删掉 `modules/<id>/update` 标记（等价于立即提升）；
- 想**同时**验证「当前生效版本」与「暂存版本」时，两处 `webroot/` 都要推同一份文件。

`gd-deploy` 会**如实区分**这两种状态并在暂存时提示需要重启。调试 WebUI 时若改的是
生效目录而暂存目录还是旧文件，重启后会被旧版本覆盖。

## 持续集成

`.github/workflows/ci.yml` 是**唯一**的 CI 定义，它不在 GitHub 上重新发明一套检查，
而是重跑与本地**完全相同的入口**。触发条件为：任意分支推送、所有 PR、手动触发，
以及 `v*` 标签推送。

### `gates` job

| 步骤 | 命令 / action | 说明 |
|---|---|---|
| 1 | `actions/checkout` | 全量检出 |
| 2 | `dtolnay/rust-toolchain`（`1.99.0`） | 必须显式安装：runner 自带 stable 低于本仓库 MSRV；同时装 `rustfmt`、`clippy` 与两个 Android target |
| 3 | `Swatinem/rust-cache` | 缓存 cargo 产物 |
| 4 | `taiki-e/install-action`（`cargo-nextest`） | 官方预编译二进制；缺它 `gd-test` 会回退 `cargo test` 并失去 `-E` 过滤 |
| 5 | `astral-sh/setup-uv` | 版本与本地一致，并启用缓存 |
| 6 | `actions/setup-node` | 必须在 `gd-check` **之前**：`pyright` 未随附 nodejs wheel 时回退到全局 `node` |
| 7 | `uv sync --locked` | 锁文件过期即**非零退出**（`--frozen` 只警告，故不用于 CI） |
| 8 | `uv run gd-check` | 格式 + 静态检查 + 治理门禁（与本地同一入口） |
| 9 | `uv run gd-test --all` | 全量 Rust + WebUI 测试 |
| 10 | `cargo check --workspace --all-targets --target <t>` | 两个 Android target 各一次 |

第 10 步对应的正是「跨目标编译已知问题」要求手动执行的那两条命令（见上）。它**只做类型
检查，因此不需要 NDK**——实测在清空 `ANDROID_HOME` / `ANDROID_NDK_HOME` 后两个目标均能
`Finished`。这一步填的是 `gd-check` 只跑宿主 target 留下的盲区。

CI 里**不写任何宿主绝对路径**，NDK 沿用 runner 镜像自带的版本；每次运行会把 `rustc -Vv`、
`uv`/`node` 版本与 NDK 的 `source.properties` 写进 job summary，便于追溯产物来源。

### `release` job

仅 `v*` 标签触发，且必须 `gates` 通过。**版本号只解析一次、两个入口复用同一个值**：

| 项 | 取值 |
|---|---|
| `version` | 标签去掉前缀 `v`（`v0.2.0` → `0.2.0`） |
| `versionCode` | `git rev-list --count HEAD`（仓库 commit 总数） |

`versionCode` 用 commit 数量：它与 `module_template/module.prop` 里的缺省值天然一致
（本仓库当前只有 1 个 commit，缺省即 `versionCode=1`），无需在 tag 里重复编码，也无需
依赖 Actions 的运行计数器（后者重跑会产生空洞）。两条 fail-closed 断言：
`git rev-parse --is-shallow-repository` 必须为 `false`（否则 commit 数恒为 1），
且解析结果必须匹配 `^[1-9][0-9]*$`。

> **改写历史会让 `versionCode` 回退**，而 KernelSU 要求它递增。本次发布流程不承诺单调性，
> 见「待验证假设」。

随后依次执行 `gd-build` → `gd-package` → `gh release create`。发布产物与本地流程第 3–4 步
完全相同：`gd-package` 会回读 `target/dist/build-info.json` 核对每个 ABI 的构建版本，
因此「包内版本 ≠ 二进制自报版本」的 ZIP **不可能**从 CI 发出去。重复推送同一个标签会让
`gh release create` 直接失败（符合预期，不做静默覆盖）。

### 第三方 action 的版本钉法

所有 action 一律按 **commit SHA** 引用，行尾注释写明对应版本（如
`actions/checkout@3d3c42e5… # v7.0.1`）。标签可被上游移动，SHA 不可。升级方式是手动改
SHA + 注释，本仓库**不引入** dependabot / Renovate（当前无其他自动化需求，引入即多一套
需要维护的配置）。

## 待验证假设

| # | 假设 | 影响 | 验证方式 |
|---|---|---|---|
| 1 | 有多个 `prebuilt/*` 子目录都含 clang wrapper 时，「排序后第一个」总是本机可用的那个 | 选错会让所有链接失败 | 构造双 prebuilt 目录的 NDK 验证 |
| 2 | `ksud module install` 与 `magisk --install-module` 的可用性随 root 管理器而异；两者都没有时手工解包可用 | 部署方式不同，验证路径也不同（手工解包无 `customize.sh` 保护） | 在 KernelSU / APatch / Magisk 各装一次 |
| 3 | `armeabi-v7a`（`armv7-linux-androideabi`）的构建可用 | 该 Rust target 未安装，未做任何实测；`customize.sh` 目前直接拒绝该架构 | `rustup target add` 后跑 `gd-build --abi`（需先加入 `ABI_TARGETS`） |
| 4 | 构建宿主只需 Linux/macOS 与 Windows 两类；扫描 prebuilt 的判据（存在 `*-linux-android*-clang`）对其他宿主布局同样成立 | 未知布局的 NDK 会报「未找到含 clang wrapper 的 prebuilt 目录」 | 在非常见宿主布局的 NDK 上运行 `gd-build` |
| 5 | 同平台 NDK 的 clang 能直接解析 rustc 传入的全部路径参数，**包括带 C/汇编依赖的 crate**（如 `ring`、`aws-lc-sys`） | 这类 crate 可能自行调用编译器并传入不兼容路径；移除 linker 覆盖机制后失去了兜底手段 | 在 Linux 宿主上编译一个带 `ring` 的临时 crate |
| 6 | CI 的两个 job 在 GitHub 侧可正常跑通（本机无 `act`，仅校验了 YAML 结构与各 action 的输入名） | 首次推送才能确认；失败表现为该 job 红 | 推送一次并观察 Actions 日志 |
| 7 | runner 镜像自带的 NDK 能编出与本地 NDK 同样可用的二进制（本机为 29.0，runner 默认 27.3） | 只影响「产物完全可复现」的强度，不影响正确性 | 比对两次构建的 ZIP 内容与设备端行为 |
| 8 | `versionCode = git rev-list --count HEAD` 在历史被 squash/rebase 后会**回退** | KernelSU 要求 versionCode 递增；回退可能导致管理器拒绝更新 | 改写历史后重新发版并观察管理器行为 |
