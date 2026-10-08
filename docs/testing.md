# 测试规范

本文件定义测试的组织方式与执行纪律。

## 目标

建立细粒度增量验证机制，降低全量回归延迟，实现高频提交门禁闭环。

## 分层验证策略（强制）

三级递进，**禁止在迭代期运行无参数的全量测试**：

| 层次 | 命令 | 时机 | 说明 |
|---|---|---|---|
| 1. 快速排错 | `cargo check -p <crate>` | 每次改动的第一反馈 | 不做代码生成，秒级响应 |
| 2. 精准验证 | `cargo nextest run -p <crate>` | 改完某 crate 后 | 单 crate 回归 |
| 2b. 单个测试 | `cargo nextest run -p <crate> -E 'test(<name>)'` | 调试指定失败用例 | 依赖 nextest 表达式过滤 |
| 3. 提交前全量 | `uv run gd-test --all` | **仅**提交前执行 | 全量回归 |

## 测试目录约定

### 单元测试：就近内联

放置在各 crate 源文件底部的 `#[cfg(test)] mod tests` 中，以便直接访问私有项。

### 集成测试：单一入口

每个 crate **至多一个** `crates/<crate>/tests/integration.rs`，内部用 `mod` 划分子模块。

**设计依据**：`tests/` 目录下的每个 `.rs` 文件都会生成独立的测试二进制，多文件会线性增加编译链接时间与产物体积。随着 crate 代码规模增长，若单个集成测试文件过大，应通过内部子模块进行组织，而非退化为多个独立的测试入口二进制文件。

### 平台特权 I/O 解耦

核心业务逻辑与数据解析下沉至 `gadgetdisk-core` 与 `gadgetdisk-proto`，与系统特权 I/O（configfs / ioctl / mount）彻底隔离。上述模块必须支持在开发宿主环境离线运行单元测试，严禁依赖 Android 目标架构或真机环境。

## 特权 I/O 的测试策略

`gadgetdisk-usb` 与 `gadgetdisk-loop` 将内核操作抽象为 trait：

- 生产实现：操作真实 `/config/usb_gadget/g1` 与 `/dev/loop*`；
- 测试实现：内存结构或临时目录测试替身。

确保高风险的**操作时序**可在开发机断言：

| 断言 | 归属 |
|---|---|
| configfs 的 `file` 属性在 `cdrom`/`ro` **之后**写入 | `gadgetdisk-usb` |
| 镜像切换必须先清除旧后端或走重建流程以防元数据失效 | `gadgetdisk-usb` |
| 释放顺序为 `sync` → `umount` → `losetup -d` | `gadgetdisk-loop` |
| 内存挂载记录与内核真值的对账是双向的（陈旧记录不误报 `busy`；仅存于挂载表的记录不误删） | `gadgetdisk-loop` |
| 分区镜像挂载只分配一次 loop 设备（无 partscan 预尝试） | `gadgetdisk-loop` |
| `max_part` 取值不改变挂载调用序列 | `gadgetdisk-loop` |
| 同一镜像不得同时进入 gadget 与 loop 状态 | `gadgetdisk-gdd` |
| `SO_PEERCRED` UID ≠ 0 时立即断开连接 | `gadgetdisk-gdd` |

## 测试运行器：`cargo-nextest`

**安装指引**：用官方安装器装预编译二进制（缺省落在 `~/.cargo/bin`，本就在 `PATH` 上）：

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://get.nexte.st/latest/linux | sh
# 或已有 cargo-binstall：
cargo binstall cargo-nextest
```

**不要**用 `cargo install cargo-nextest`：源码编译会拉入 `aws-lc-sys`，其构建需要 cmake，
在无 cmake 的机器上不可用。本仓库**不自建**仓库内工具目录，安装位置完全交给官方安装器。

`uv run gd-test`（`scripts/test/cli.py`）在 nextest 缺失时：

1. 给出上述官方安装指引；
2. **安全回退到 `cargo test`**，并提示失去 `-E` 过滤能力。

## 差分按需测试（`uv run gd-test`）

**选择规则**

1. 计算改动文件集合：`git diff --name-only <base>...HEAD` 加上未暂存的工作区改动。
2. 映射 `crates/<name>/` 前缀至对应的 crate。
3. 仅对命中的 crate 执行 `cargo nextest run -p <crate>`。
4. 仅改动 `docs/**`、`.agents/**`、`webui/**` 时**跳过 Rust 测试**；`webui/**` 会触发 Node 测试。
5. **映射未命中时保守回退全量测试**（例如改动根 `Cargo.toml`、`Cargo.lock`、`rustfmt.toml`、`pyproject.toml`、`uv.lock`、`scripts/**`）。

**参数表**

| 参数 | 行为 |
|---|---|
| （缺省） | `--changed`：按上述规则选测 |
| `--all` | 全量测试（提交前使用） |
| `--base <ref>` | 指定比较基线 |
| `--dry-run` | 仅打印将执行的命令而不实际运行 |

## 格式与静态检查

```bash
uv run gd-check
uv run gd-check --fix    # 应用自动修复
```

检查管道依次执行：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`ruff format --check`、`ruff check`、`pyright` 与治理门禁。

## 前端测试（`webui/tests/`）

在改动命中 `webui/` 时由测试脚本自动调度；缺失 `node` 时输出告警并跳过。

**调用形式**：由 `scripts/test/cli.py` 枚举 `webui/tests/*.test.mjs`，把**显式文件路径**
交给 `node --test`。不要改回目录形式（`node --test tests/`）：它在 **Node 22 与 24 上
会把目录当模块 `require`**，报 `Cannot find module .../webui/tests` 并失败，只有 Node 26+
才容忍目录参数——而 CI 钉的是 Node 24，症状是「本地全绿、CI 红」。也不要改用
`--test 'tests/**/*.test.mjs'` 或裸 `--test`：两者在**零匹配时静默 `exit 0`**，
测试文件被误删或改名会得到一次绿色的空跑；上述枚举方式则直接报错。

**测试覆盖矩阵**

| 测试文件 | 覆盖内容 |
|---|---|
| `webui/tests/logic.test.mjs` | 纯函数层（`webui/pure/*`）：`ksu.exec` 输出解析、探测判定归一化、REST/CLI 调用映射（含分块上传四个 op）、容量校验与对齐、错误码文案转换（含**错误码优先于后端 `message`** 的回归）、安全路径拼接 |
| `webui/tests/structure.test.mjs` | 六大视图齐备、零构建（无外链 CDN）、触控区域 ≥44px、安全区适配、硬约束文案完整性、**对全量模块做「具名 import ⊆ 目标模块 export」的链接期校验**、**禁止调用未定义标识符**、模块必须平铺（仅 `pure/` 可为子目录）、**镜像只能从下拉框选择**（挂载页与本地编辑页都是 `<select>`）、禁止使用 `localhost` |

> 静态结构测试只断言**当前必须成立的性质**，不保留「某次删除已完成」的恒真断言
> （例如「某文件已不存在」）——那类断言永远不会失败，只会增加维护面。真正需要守住
> 的是**边界**：后端不暴露任意目录枚举（`rest.rs` 的路由表断言）、前端不回归自由
> 文本输入（下拉框断言）、模块图完整可加载（导入与导出比对）。
>
> 「未定义标识符」等守卫是**负向验证过**的：故意重新引入缺陷时它们必须失败。该检查
> 会先剥掉注释、字符串与**正则字面量**再匹配调用点——漏掉正则会让 `/^Type (0x…)$/`
> 被误报成 `Type()`。

**语言边界的守护测试**

| 测试 | 位置 | 守住什么 |
|---|---|---|
| `every_help_screen_is_english_and_non_empty` | `crates/gadgetdisk-cli/src/main.rs` | 全部子命令的 `--help` 无中文且**每项都有说明**。子命令清单从 clap 自身枚举，新增子命令自动纳入 |

> 这条测试的存在理由：clap derive 把 `///` doc comment 直接渲染成 help，因此
> 「注释用中文」与「输出用英文」在同一语法位置冲突。漏写显式 `help` 属性**不会
> 编译失败**，只会让那一项静默变空或漏出中文——只有渲染出来才看得见。详见
> [文档规范](README.md#语言边界操作者输出用英文)。

**分工约束**：`webui/pure/*` 保持纯函数设计，禁止访问 DOM、`window` 或浏览器特有 API，保证可在 Node.js 中离线运行单元测试；DOM 渲染与原生交互收敛于 `main.js` / `backend.js` / `view-*.js`。

**布局约束（由测试用例保障）**：打包脚本只扫描 `webui/` **顶层**文件，外加显式登记的 `pure/` 子目录。因此模块必须平铺，新增层级要同步更新 `scripts/package/cli.py` 的 `WEBUI_SUBDIRS`，否则**本机测试全绿、打包却丢文件**（症状只在设备上出现）。

**REST 传输测试归属**

| 层级 | 文件 | 覆盖内容 |
|---|---|---|
| HTTP 解析/序列化 | `crates/gadgetdisk-cli/src/http.rs` | 请求行、头部解析、定长 Body、拒绝 chunked、超限流保护、畸形请求兜底、响应头（CORS / Connection: close） |
| 路由与鉴权 | `crates/gadgetdisk-cli/src/rest.rs` | 路径与方法匹配（404/405 语义）、OPTIONS 免鉴权、Bearer Token 常数时间比较、错误码映射、`api.json` 原子写入与 0600 权限 |
| 空闲回收 | `crates/gadgetdisk-gdd/src/server.rs` | 空闲退出判定（有活跃挂载时永不退出）、超时返回、退出时清理 socket |
| 跨进程锁 | `crates/gadgetdisk-gdd/src/oplock.rs` | 并发持锁互斥校验、锁释放可重入性、父目录容错 |
| 资源互斥 | `crates/gadgetdisk-gdd/src/service.rs` | 针对已被 loop 占用的镜像返回 `image_in_use`，以内核真值为准 |
| 只读系统工具 | `crates/gadgetdisk-core/src/fsinfo.rs` | `available_bytes` 回显、`nearest_existing_ancestor` 归属判定 |

## 治理与门禁

```bash
python3 .agents/scripts/gates/verify_agent_gates.py
```

校验项：Agent Note 头部格式与状态、必需 5 个章节完备性、`AGENTS.md` 词数预算（≤1500）、Markdown 相对链接与锚点完整性。

## 持续集成

`.github/workflows/ci.yml` 在推送与 PR 上重跑与本地**相同的入口**：`uv run gd-check`、
`uv run gd-test --all`，外加两个 Android target 的 `cargo check`（不需要 NDK）。打 `v*`
标签时额外构建并发布 Release。job 表、版本号解析与 action 版本钉法见
[构建与发布](build-and-release.md#持续集成)。

## 真机验收规程（特权内核行为）

以下项目须在真实 Android 目标设备上验证：

1. 镜像挂载后 Host 端正确识别为 USB 磁盘且支持读写。
2. 只读模式下 Host 端写入被明确拦截。
3. CD-ROM 模式被 Host 端识别为虚拟光驱。
4. 卸载操作后 Host 端感知设备正常断开。
5. `lo_offset` 分区偏移挂载在 `raw` / `gpt` / `mbr` 三种布局下均可用；loop 设备无残留。
6. 本地挂载点在第三方 root 文件管理器中的路径可达性。
7. SELinux 日志中无未预期的 `avc` 拒绝记录（`dmesg | grep avc`）。
8. 系统重启后 `gadgetdisk boot` 能根据 `run/state.json` 自动收敛对账。
9. **loop 路径的镜像上下文**：把镜像标签临时改成不允许内核线程读写的类型后本地
   挂载，观察是否失败或写入被静默丢弃；再恢复默认标签重新挂载，确认可读写
   （写入后镜像校验和**必须**变化）。对应 [路线图](roadmap.md) 待验证假设 #25 / #26。
10. **小分区的文件系统下限**：33 MiB 的 FAT32 分区（`raw` 与有分区表两种布局各一次）
    能创建成功且 Host 端可读——`mkfs.vfat -v` 应报 `total_clusters >= 65525`，Host 端
    `fsck.vfat` 与挂载读文件均正常；32 MiB 及其以下应被明确拒绝。
    对应 [路线图](roadmap.md) 待验证假设 #27 / #28。
11. **版本号一致性**：设备上执行 `bin/gadgetdisk --version` 与 `bin/gdd --version`、
    `bin/mkfs.vfat -V`，三者应与模块详情页显示的版本号（即包内 `module.prop`）**完全一致**。
    宿主机上 `uv run gd-package` 已用 `target/dist/build-info.json` 守住这一点，
    但清单只记录构建时的意图，真机核对的是实际发布的产物。
12. **安装脚本的架构变量**：在 KernelSU 与 APatch 上各装一次，确认 `customize.sh`
    能从 `$ARCH` 正确取出 ABI（失败会明确 abort，不会静默装错）。
    对应 [路线图](roadmap.md) 待验证假设 #29。
13. **卸载脚本不依赖杀进程**：模块处于已卸载（`remove` 标记）状态重启后，
    确认卸载收尾正常执行且 `/data/adb/gadget-disk` 与模块的 loop 附件均被清理。
    对应 [Android 集成](android-integration.md) 的执行时序结论。

### 真机 UDC 状态核验（排查 Host 端代码 10 故障）

详细推演见 [真机 UDC 抢占 Note](../.agents/notes/implemented/architecture/2026-10-04-real-device-udc-contention.md)，核对项目以内核实际状态为准：

| # | 检查项 | 期望状态 |
|---|---|---|
| 1 | `cat /sys/class/udc/$(cat /config/usb_gadget/g1/UDC)/state` | `configured`（非 `addressed`） |
| 2 | `ls /config/usb_gadget/g1/configs/b.1/` | 同时存在 `mass_storage.gadget-disk` 与系统原生 `f1..f3` |
| 3 | `cat .../functions/mass_storage.gadget-disk/lun.0/file` | 指向目标后备镜像路径 |
| 4 | `cat /config/usb_gadget/g1/os_desc/use` | 挂载期间为 `0`；卸载后恢复系统原值 |
| 5 | `status` 查询响应中的 `effective` | 与 UDC state configured 保持一致 |
| 6 | 清空全部 `lun.N/file` 后的状态 | 模块符号链接与 function 完整回收 |
| 7 | 全量卸载后拓扑 | `os_desc/use` 恢复；`configs/b.1` 移除本模块链接 |
| 8 | `dmesg \| grep -iE "usb_gadget\|sys.usb.config"` | 分析 Android init 重配触发器时序 |

### 多 LUN 与设备身份核验

1. 挂载多个 LUN 时，各 `lun.N/inquiry_string` 与读写/只读属性各自独立生效。
2. 更新已有 LUN 时，UDC 保持连接不断开；新增 LUN 拓扑时按需短暂重连。
3. 单个 LUN 卸载仅清空对应 `lun.N/file`，目录与配置参数均予以保留（空闲槽位）。
4. 更新 USB VID/PID/产品名后，新描述符持久化保存且于下次连接生效。
5. 系统重启后，`gadgetdisk boot` 正确读取 `run/state.json` 并重新挂载各 LUN。

### USB 读写端到端完整性验收

设备挂载验收必须同时包含 Host 端的读操作与写操作校验，严禁以单一的 USB 设备枚举或内核属性节点作为通过判据：

| 标识 | 校验项目（Host 端执行） | 预期结果 |
|---|---|---|
| R1 | 磁盘分区表读取（`fdisk -l` / `parted -l`） | 成功解析分区表，无 I/O 错误 |
| R2 | 文件系统挂载并读取现有文件 | 内容与写入镜像时完全一致 |
| R3 | 内核审计监控（`dmesg \| grep -i avc`） | 无 `file-storage` 相关的 AVC 拒绝 |
| R4 | 数据写入验证：在 Host 端创建文件并执行 `sync` + `umount` | 校验设备端镜像文件 `md5sum` **必须改变** |

*注：若 SELinux 权限受限，内核写操作可能被静默抑制而不返回 I/O 错误（此时 `lun.N/ro` 仍显示为 0）。后备镜像校验和变化是确认为真实写入的唯一可信证据。*
