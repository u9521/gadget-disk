# Agent Note: 安装与卸载脚本的既有 TODO 收敛

Status: implemented

## Problem

`module_template/customize.sh` 与 `uninstall.sh` 里各留了一条 `todo`，都是"当初
没查清楚就先留着"的悬置判断。两条都会在**安装/卸载这类不可重试的时刻**出错，
因此必须各有明确依据。

**TODO 1（`customize.sh`）：是否该用 KernelSU 提供的架构变量，而不是自己探测？**

脚本原来自己跑 `getprop ro.product.cpu.abi`，失败再 `uname -m` 兜底，最后映射到
`arm64-v8a` / `x86_64`。绕过安装器的代价是**同一件事算两遍**：安装器在此之前已经
算过一次架构，两份结果可能不一致（探测源不同），而"以哪份为准"从未定义。

**TODO 2（`uninstall.sh`）：卸载时要不要先杀掉 `gadgetdisk`/`gdd` 进程？**

脚本用 `kill_our_processes()` 扫 `/proc/*/cmdline`，先 `SIGTERM`、`sleep 1`、
再 `SIGKILL`。但这个循环的**必要性完全取决于执行时序**——若卸载脚本在开机早期跑，
那时两个进程不可能存在，这段代码就是"杀不存在的进程"，还带着误伤他人进程的风险。

## Proposal

### 1. `customize.sh` 直接用安装器算好的 `$ARCH` / `$IS64BIT`

```sh
case "$ARCH" in
  arm64) abi="arm64-v8a" ;;
  x64)   abi="x86_64" ;;
  *)     abi="" ;;
esac
# 取不到就 abort，并把 ARCH/IS64BIT 的实际取值打进安装日志
```

**依据（源码核对）**：KernelSU 的 `userspace/ksud/src/installer.sh` 中，
`install_module()` 先调 `api_level_arch_detect()`（由 `ro.product.cpu.abi` 推出
`ARCH=arm64|x64|arm|x86|riscv64`、`IS64BIT=true|false`），随后才
`. $MODPATH/customize.sh`；Magisk 的 `scripts/util_functions.sh` 同样在
`install_module()` 里先 `api_level_arch_detect` 再 `. $MODPATH/customize.sh`，
变量名与取值集合一致。**`.` 是同一 shell 内的 source**，因此这两个变量在
`customize.sh` 中必然可见。

去掉 `uname -m` 兜底的额外理由：`uname -m` 在 32 位用户态 + 64 位内核的机型上会
报 `aarch64`，据此选 `arm64-v8a` 的二进制会装上无法执行的产物；而安装器的
`IS64BIT` 正是为区分这种情况存在的。**自己探测比安装器更容易错**。

### 2. `uninstall.sh` 删除杀进程循环

**依据（源码核对）**：卸载脚本在**开机早期**执行，早于 `service.sh`——

- KernelSU：`prune_modules()`（`userspace/ksud/src/module.rs`，由
  `init_event.rs` 在 post-fs-data 阶段调用）扫描带 `remove` 标记的模块并
  `exec_script(uninstall.sh, ScriptWait::Forever)`；
- Magisk：daemon 的 `remove_modules()`（`native/src/core/daemon.rs` →
  `module.rs`）在 boot 阶段之前执行。

而 `gdd` **只在有镜像导出为 USB 设备期间存在**，导出状态由
`service.sh` → `gadgetdisk boot` 恢复。既然卸载脚本更早，那时两个进程不可能在跑。

**但凭据文件仍要兜底清理**：`rm -f "$MODDIR/webroot/api.json"`。用户可能在卸载前
手动拉起过 `serve` 并留下 `api.json`，模块目录被删后它不该继续存在。

保留的其余收尾不变：调 `gadgetdisk uninstall`（拆导出、还原身份、清 `run/`）、
`sync` 后按 `backing_file` 只分离本模块 `images/` 下的 loop、`rm -rf "$DATA"`。

### 3. `module.prop` 的 `todo` 由版本注入解决

`version=0.1.0 todo 根据package脚本设置` 与 `versionCode=1 todo 根据package脚本设置`
改为纯值。版本号改由 `gd-build` / `gd-package` 的 `--version` / `--version-code`
注入与写入（见 [二进制版本号注入与构建清单守卫](../feature/2026-10-08-binary-version-injection.md)）。
残留的 `todo` 文本还直接导致 `gd-package` 失败——`versionCode` 的纯数字校验必然不通过
（实测报 `versionCode must be an integer: '1 todo 根据package脚本设置'`）。

## Alternatives considered

**A. `customize.sh` 保留双路探测：优先 `$ARCH`，缺失时回退 `getprop`/`uname`。**
表面上更稳健，实际把"两份结果不一致时用哪份"变成隐式约定，而且掩盖了
"某个管理器不导出 `$ARCH`"这一真实信息——那种情况应当**明确 abort**，
而不是靠兜底悄悄装上一个可能错的架构（32 位用户态 + 64 位内核的机型正是如此）。

**B. `uninstall.sh` 保留杀进程循环，"反正杀了也没坏处"。**
有坏处：`/proc/*/cmdline` 匹配会走遍所有进程，`kill` 的目标集合在开机早期不受控；
`*"/gadgetdisk "*|*"/gadgetdisk"` 这种路径结尾匹配一旦写错就会误伤。
为一个不可能存在的场景保留危险代码，是净负收益。

**C. `uninstall.sh` 改为只发 `SIGTERM`，去掉 `sleep` 与 `SIGKILL`。**
同样是"为不存在的场景写代码"。而且 `sleep 1` 会拖慢卸载流程（在开机关键路径上）。

**D. `uninstall.sh` 完全删掉 `api.json` 清理，交给 `gadgetdisk uninstall`。**
`run_uninstall` 只清 `run/` 下的状态与身份备份，**不碰 `webroot/`**（它属于安装器管理的
区域）。若进程被强杀，`api.json` 里的 token 会在模块目录里留到下一次安装。
保留一行 `rm -f` 成本极低，收益明确。

## Acceptance criteria

- `sh -n module_template/customize.sh` 与 `sh -n module_template/uninstall.sh` 通过。
- `module_template/customize.sh` 中不再出现 `getprop`、`uname`、`ro.product.cpu.abi`；
  只按 `$ARCH` 取值，未知取值走 `abort` 并把 `ARCH`/`IS64BIT` 打进日志。
- `module_template/uninstall.sh` 中不再出现 `kill`、`/proc/`、`sleep`；
  `rm -f "$MODDIR/webroot/api.json"` 与 `gadgetdisk uninstall` 调用保留。
- `grep -rn todo crates/ webui/ scripts/ module_template/` 无结果
  （`Cargo.toml` 里的 `todo = "deny"` 是 lint 配置，不是待办）。
- AVD：安装模块后 `bin/` 扁平化正确（`customize.sh` 取到 ABI）；放置 `remove` 标记后
  重启，卸载收尾正常执行且 `/data/adb/gadget-disk` 与模块的 loop 附件均被清理。
  真机验收第 12 / 13 项（[测试规范](../../../../docs/testing.md)）。

## Risks

- **APatch 是否同样导出 `$ARCH` / `$IS64BIT` 未核对**（KernelSU 与 Magisk 已由源码确认）。
  记为**待验证假设**（[路线图](../../../../docs/roadmap.md) #29）。不成立时的表现是
  安装期明确 `abort` 并打印 `ARCH='unset'`——比静默装错架构好，但会让 APatch 用户
  装不上模块。
- **卸载脚本不再杀进程，依赖"时序结论"**。若某个管理器改变了卸载时机（改到开机后），
  残留的 `serve` 会持有已删除模块目录里的二进制路径。影响面很小（进程空闲即退出，
  且 `gdd` 会因 socket 目录被删而失败），且真机验收第 13 项覆盖该场景。
- **`customize.sh` 与安装器共享 shell 变量**是一种隐式契约。它在 KernelSU 与 Magisk
  的**当前**实现中成立（已核对源码），但对未来的管理器改动没有编译期保护——
  这正是 `abort` 分支存在的意义：契约失效时**响亮地失败**。
