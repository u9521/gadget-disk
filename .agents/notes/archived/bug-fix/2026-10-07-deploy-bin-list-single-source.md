# Agent Note: 部署校验的二进制名单改为单一事实源

Status: archived

> **归档原因**：本 Note 记录的修复对象是 `gd-deploy` 安装后的结构校验函数。该函数已整体
> 移除——`ksud` 把模块暂存到 `modules_update/`，重启前那里的内容并不是用户实际运行的
> 东西，校验它既可能通过也不代表部署有效，故改为「推送并安装后由用户检查实际结果」。
> 本文保留以记录当时的判断：**假失败比没有检查更糟**（它训练读者忽略部署输出），以及
> 期望集合必须取自打包侧单一事实源而非硬编码。该原则在 `gd-package` 中仍然适用。

## Problem

`gd-deploy` 在安装后做结构校验，其中一条断言设备上模块 `bin/` 的内容：

```python
if bin_list == "gadgetdisk,gdd":
    checks.append("PASS …")
else:
    checks.append(f"FAIL bin/ 内容不符（期望 gadgetdisk,gdd，实际 {bin_list}）")
```

这个名单是**硬编码**的。`mkfs.vfat` 随模块分发之后（见
[自带 mkfs.vfat 与 loop 格式化](../../implemented/feature/2026-10-06-bundled-mkfs-vfat-and-loop-formatting.md)），
`bin/` 实际含三个二进制，于是该断言**每次都报 FAIL**：

```
FAIL bin/ 内容不符（期望 gadgetdisk,gdd，实际 gadgetdisk,gdd,mkfs.vfat）
```

同一份文件里还有两处按名硬编码 `("gadgetdisk", "gdd")`：可执行性探测循环，以及
marker → 文案映射表。三处都要跟着打包名单走，但只有一处会报错，另两处是**静默**
地不再覆盖新二进制（`mkfs.vfat` 缺少可执行位时没人会发现）。

**假失败比没有检查更糟**：它训练读者忽略 `gd-deploy` 的输出，而这条输出恰恰是
「装到设备上的东西是否可用」的唯一凭据。当时已连续多轮部署都带着这条 FAIL，实际
验证完全成功——这正是假信号开始起作用的表现。

## Proposal

期望集合改为取自打包侧的既有常量 `scripts/lib/common.py::PACKAGED_BIN_NAMES`
（由 `BIN_RENAMES` 派生，是「模块 `bin/` 下应该有哪些文件」的**单一事实源**）：

- 可执行性探测循环 → 遍历 `PACKAGED_BIN_NAMES`；
- marker → 文案映射 → 由 `PACKAGED_BIN_NAMES` 生成；
- `bin/` 集合比对 → 与 `sorted(PACKAGED_BIN_NAMES)` 比**集合**而非字符串。

比对从「字符串相等」改为「集合相等」，并把差异说清楚：

| 情况 | 消息 |
|---|---|
| 完全一致 | `PASS bin/ 为扁平布局且含 gadgetdisk 与 gdd 与 mkfs.vfat` |
| 缺文件 | `FAIL …；缺少 mkfs.vfat` |
| 多文件 | `FAIL …；多出 extra.bin` |
| 空目录 | `FAIL bin/ 为空（期望 …）` |

`BIN_LIST` 由设备端 `ls | tr '\n' ','` 产生，顺序取决于文件系统，因此**必须排序后
比较**——直接比字符串会在顺序变化时产生第二条假失败。

## Alternatives considered

**A. 只把硬编码字面量改成 `"gadgetdisk,gdd,mkfs.vfat"`。**
否决：这仍然是第二份名单。下次增删二进制（例如将来加 exFAT 工具）会再犯同一个错误，
而且是同一个「静默漂移」形态。

**B. 放宽为「至少含 gadgetdisk 与 gdd」，不检查多余文件。**
否决：`bin/` 里出现非预期文件往往意味着打包逻辑出错（例如 ABI 子目录没被扁平化、
或残留了中间产物）。这类问题值得报出来，而不是放过。

**C. 完全删掉这条检查。**
否决：它能抓到真问题——手工解包路径下 `customize.sh` 不执行，扁平化与 `chmod 0755`
都由部署脚本补做；若那一步漏了某个二进制，只有这条检查能发现。

**D. 从设备读 `module.prop` 或 `customize.sh` 反推名单。**
否决：`module.prop` 不含二进制清单；`customize.sh` 执行后已被删除（它不在名单里正是
因为如此）。打包侧的常量才是权威。

## Acceptance criteria

- `uv run gd-deploy` 在设备上**18 项全 PASS、0 FAIL**（此前恒有 1 项假 FAIL）；
- 名单只存在于 `PACKAGED_BIN_NAMES`：`grep -n "'gadgetdisk'\|gadgetdisk,gdd" scripts/deploy/cli.py`
  只应命中解释性注释，不含逻辑；
- 新增/删除 `BIN_RENAMES` 条目时，`gd-deploy` 的三处检查自动跟随，无需改动
  `scripts/deploy/cli.py`；
- 失败路径可区分「缺少」「多出」「空目录」三种情况并指名具体文件（已用合成输入
  逐分支核对）；
- `uv run gd-check` 6 项全绿（含 ruff format/check 与 pyright）。

## Risks

- **集合比对比字符串比对更严格**：若某 ABI 的构建产物名与 `BIN_RENAMES` 不一致，
  现在会明确报「多出」而不是恰好漏过。这是**期望的行为**（早先的字符串比对同样会
  失败，只是消息不如现在清楚），但值得记录：这条检查的职责是「设备内容 == 打包
  契约」，任何一侧偏离都应报出来。
- **`PACKAGED_BIN_NAMES` 与 `customize.sh` 的扁平化逻辑仍是两处**。前者是 Python
  常量、后者是安装期 shell。若将来只改其中一处，`gd-deploy` 会报 FAIL（在手工解包
  路径下）——即失败是**响亮的**，不是静默的。未做的是让两者共用同一份生成物，属于
  后续可选的收敛项。
- 本条修复**未改变**任何部署行为（推送、安装、重启语义均不变），只影响校验输出的
  正确性。设备侧无需为此重启。
