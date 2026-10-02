# Agent Note: 移除 linker 包装器扩展点并把 NDK 探测收敛到两个环境变量

Status: implemented

## Problem

构建脚本曾提供 `GD_LINKER_WRAPPER_<TARGET>` 扩展点：设置后取代 NDK 自带 clang 作为
cargo 的 linker，供「Linux 宿主 + Windows 版 NDK」这类跨平台混用场景注入路径翻译包装器。

三个问题：

1. **它在为错误配置兜底**。Windows 版 NDK 的 `aarch64-linux-android30-clang` 是 bash 脚本，
   最终调用 `clang.exe`；后者是原生 Windows 程序，解析不了 rustc 传入的 Linux 路径
   （`/mnt/c/...`、`/home/...`）。实测报错：

   ```
   clang: error: no such file or directory: '.../gdd-...rcgu.o'
   clang: error: no such file or directory: '.../libcompiler_builtins-....rlib'
   ```

   两个路径都真实存在，只是 Windows 侧看不到。正确修法是改用**同平台** NDK，而不是给
   跨平台混用加一层翻译。

2. **多一条与真实原因无关的排查分支**。构建失败时，「包装器没设置 / 设置错 / `wslpath`
   失败」都会被纳入怀疑范围，而根因其实只有「NDK 平台不对」一个。

3. **探测来源过多**。NDK 可来自 `ANDROID_NDK_HOME` / `ANDROID_NDK_ROOT` / `NDK_HOME` /
   `local.properties` 的 `sdk.dir` + `ndk.version` / `ANDROID_HOME` / `ANDROID_SDK_ROOT`
   六个入口，很难回答「当前这次构建究竟用了哪个 NDK」。

## Proposal

1. **删除 `GD_LINKER_WRAPPER_<TARGET>`**：移除 `scripts/lib/common.py` 的
   `linker_wrapper_override()` 与 `scripts/build/cli.py` 的 `linker_for()`，linker 恒为
   探测到的 NDK 自带的 `<arch>-linux-android30-clang`。同时删除本机私有目录 `local/`
   （`make-linker-wrapper.sh` 与生成的包装器）。
2. **NDK/SDK 探测只认两个变量**：`ANDROID_NDK_HOME`（最高优先级）与 `ANDROID_HOME`
   （其下 `ndk/`，或 `ANDROID_HOME` 本身即 NDK 根）。删除 `ANDROID_NDK_ROOT` /
   `NDK_HOME` / `ANDROID_SDK_ROOT` 别名，并**移除 `local.properties` 读取**
   （连带删除 `read_local_properties()` 与 `_ndk_from_local_properties()`）。
3. **把「NDK 必须与宿主同平台」写成显式契约**，写入 `common.py` docstring、失败时的报错
   文案，以及 `docs/architecture.md` / `docs/build-and-release.md`。

不变的部分：`ADB` / `ANDROID_ADB` / `ANDROID_SERIAL` 维持原样；`Ndk` dataclass、prebuilt
动态扫描、`builtins_archive()`（x86_64 的 `libclang_rt.builtins-x86_64-android.a`）、
`CARGO_TARGET_<TARGET>_LINKER` / `CC_<target>` / `AR_<target>` 注入全部保留。

## Alternatives considered

| 方案 | 否决理由 |
|---|---|
| **保留 `GD_LINKER_WRAPPER_<TARGET>`** | 它的唯一用途是让跨平台 NDK 混用「能跑」。同平台 NDK 下 clang 是原生 ELF，直接解析 Linux 路径，机制失去对象；保留即长期维护一条通向错误配置的路径 |
| **保留包装器但默认自动生成** | 会把宿主差异的处置权收进仓库，与「仓库不含宿主细节、不硬编码本机路径」的既定约束冲突；且仍需有人决定 NDK 装在哪 |
| **保留 `ANDROID_NDK_ROOT` / `NDK_HOME` / `ANDROID_SDK_ROOT` 别名** | 别名只是历史习惯，不增加能力。多入口会让「没设 `ANDROID_NDK_HOME` 时到底用了哪个」不可判定，与本次「收敛探测面」的目标相反 |
| **保留 `local.properties` 作为兜底** | 它让构建结果依赖一个**不入库的私有文件**：同一份代码在不同机器上会静默用不同 NDK。显式环境变量可被 `env` 一眼看清，`local.properties` 不行 |
| **不做路径翻译、直接要求用户装 Linux 版 NDK**（即本方案） | 采纳。唯一代价是 Windows 版 NDK 无法在 WSL 复用，而本机已有可用的 Linux 版 NDK |

## Acceptance criteria

1. `grep -rn "GD_LINKER_WRAPPER"` 在仓库（含 `docs/`、`.agents/`）**零命中**。
2. 只设 `ANDROID_HOME` 指向 Linux 版 SDK 时，`uv run gd-build` 产出 arm64-v8a 与 x86_64
   各两个二进制，**不设置任何链接器覆盖变量**。
3. 四个产物均为 `ELF ... statically linked ... for Android 30`（arm64 为 `ARM aarch64`，
   x86_64 为 `x86-64`）。
4. 不设任何相关环境变量时，报错只列出 `ANDROID_NDK_HOME` 与 `ANDROID_HOME` 两个来源，
   并给出「NDK 必须与构建宿主同平台」的提示；磁盘上存在 `local.properties` 也不影响该结论。
5. `uv run gd-check` 全绿（含 ruff / pyright 与治理门禁）。

## Risks

| # | 风险 | 影响 | 状态 |
|---|---|---|---|
| 1 | 同平台 NDK 的 clang 未必能处理**带 C/汇编依赖的 crate**（如 `ring`、`aws-lc-sys`）自行调用编译器时传入的路径 | 这类 crate 可能绕过 `CC_<target>`；移除覆盖机制后失去兜底 | 待验证假设：当前依赖树不含此类 crate，未实测 |
| 2 | 移除 `local.properties` 回退后，惯用 Gradle 约定的使用者会突然构建失败 | 首次失败需手动导出环境变量 | 已缓解：报错明确列出两个可用变量与安装位置提示 |
| 3 | 本机需把 Linux 版 NDK 落到稳定位置（`/opt`），该步骤依赖外部权限 | 环境未配好前无法构建 | 环境配置问题，非代码缺陷；见 `docs/build-and-release.md` |

**实测证据**（2026-10-05，WSL2 Linux 宿主）：同一提交下，指向 Windows 版 NDK 时链接失败并
复现上述 `no such file or directory`；把 `ANDROID_HOME` 指向 Linux 版 SDK
（`prebuilt/linux-x86_64`，`clang-21` 为 ELF）后，两个 ABI 四个二进制全部构建成功。
