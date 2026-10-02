# GadgetDisk — Agent 指令

KernelSU / APatch 模块：将手机模拟为 USB 大容量存储 (UMS)，支持通过本地 loop 挂载镜像进行编辑。
本文件为规则入口，**不承载规格细节**；遵循单一事实源原则。

## 项目定位与架构

纯 WebUI 驱动，**无配套 APK**。前端为零构建纯静态 HTML5 页面（源在 `webui/`，打包进 `webroot/`）。**按需进程模型**：开机不常驻任何进程；WebUI 主通道走回环 REST（`gadgetdisk serve`，空闲退出），`ksu.exec` + CLI 为降级通道；`gdd` 仅在**有镜像导出为 USB 设备**时运行。

```
WebUI (H5, webui/ -> 模块 webroot/)
   |  (主) fetch http://127.0.0.1:<port>  Bearer token  ← webroot/api.json
   |  (回退) ksu.exec
   v
gadgetdisk serve (按需, su 域)      gadgetdisk <子命令> (一次性, su 域)
   |  loop ioctl / mount / fs          |  AF_UNIX 路径 socket + SO_PEERCRED
   |  身份写 configfs                   |  仅 mount/unmount/delete-slot/rebind
   |  run/state.json（导出意图）         |
   +-- mount/unmount --> gdd <---------+
                    (仅导出期间存在, 无状态)
                          configfs /config/usb_gadget/g1 -> kernel UDC
```

- 模块 ID `gadget-disk`；安装于 `/data/adb/modules/gadget-disk`；数据在 `/data/adb/gadget-disk/`。
- **`gdd` 仅负责 mass_storage 挂载且无业务状态**（日志仅为诊断输出）；镜像生命周期、身份配置与镜像上下文修正由 CLI 负责。边界由类型与源码扫描测试强制，见 [按需进程模型 Note](.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。
- 7 个 crate 划分与可测性边界见 [架构规范](docs/architecture.md)；关键决策见 [.agents/notes](.agents/notes/README.md)。

## 命令矩阵

`scripts/` 是 **uv 管理的 Python 包**，入口经 `[project.scripts]` 暴露。首次使用先 `uv sync`。

| 用途 | 命令 |
|---|---|
| 快速排错（首选） | `cargo check -p <crate>` |
| 精准测试（改动后） | `cargo nextest run -p <crate>` |
| 精准测试（单个） | `cargo nextest run -p <crate> -E 'test(<name>)'` |
| 差分测试（按 git diff） | `uv run gd-test` |
| 提交前全量测试 | `uv run gd-test --all` |
| **格式与静态检查总闸** | `uv run gd-check`（`--fix` 自动修） |
| 交叉编译（Android） | `uv run gd-build` |
| 打包模块 ZIP | `uv run gd-package` |
| 推送并验证 | `uv run gd-deploy` |
| 治理与文档门禁 | `python3 .agents/scripts/gates/verify_agent_gates.py` |

> 宿主差异（SDK 路径、PyPI 镜像、宿主特定 linker 包装）**不进仓库**，见 [构建与发布](docs/build-and-release.md)。

CI（`.github/workflows/ci.yml`）在推送与 PR 上重跑与本地相同的入口，在 `v*` 标签上构建并发布 Release；契约见[构建与发布](docs/build-and-release.md#持续集成)。

## 分层测试纪律（强制）

三级递进，**禁止迭代期运行无参数全量测试**：

1. `cargo check -p <crate>` — 每次改动的第一反馈。
2. `cargo nextest run -p <crate>` — 改完某 crate 或特定测试后。
3. `uv run gd-test --all` — 仅提交前执行；此时须 `uv run gd-check` 全绿。

约定与理由见 [测试规范](docs/testing.md)。

## 约定与安全规则

- **改动需确认**：`AGENTS.md`、`.agents/**`、`docs/**` 可直接写入；其余（Rust 源码、`Cargo.toml`、`webui/**`、`module_template/**`、`scripts/**`、`.github/**`、根 `README.md`）改动前须取得明确确认。
- **单一事实源**：新增事实写入对应归属文档，其他位置仅放相对链接，禁止重复定义。
- **关键架构、接口或行为变更必须同步新增或更新 Agent Note**，与代码同次提交。
- **未验证即标注**：凡未经真机或实测确认的结论，统一标注为「待验证假设」。已知待验证项见 [路线图](docs/roadmap.md)。
- **安全底线**：
  - 回环 REST **只能**监听 `127.0.0.1`，禁止 `0.0.0.0` 与 IPv6 通配（实测 UID 2000 可连回环端口）；请求须校验 Bearer token，token 仅经 `webroot/api.json`（`0600`）投递；
  - `gdd` 的 socket 目录必须为 `0700 root:root` 并以 `SO_PEERCRED` 校验对端 UID 为 0；**严禁使用抽象套接字**。详见 [协议](docs/protocol.md)。
- **数据安全互斥**：同一镜像**严禁**同时作为 gadget LUN 与 loop 后端，亦禁止绑定多个 LUN；本地编辑挂载前必须先卸载 gadget。判定取**内核实际状态**（`lun.N/file`、`/sys/block/loopN/loop/backing_file`），严禁依赖内存状态。
- **配置归属**：`config/`（用户可配置）存持久意图；`run/`（程序管理）存运行期状态；`logs/`（程序管理）存诊断日志。**无 `state/` 目录**。
- **镜像 SELinux 上下文**：**导出、loop 挂载、经 loop 格式化**三条路径都须在触碰内核前确保标签允许内核线程（`kernel` 域）**读写**（仅读会导致写入被内核静默丢弃）。默认 `u:object_r:media_rw_data_file:s0`；**仅改 `images/` 下文件**，外部仅警告。校验取内核实际状态与写入后镜像校验和，**不得**依赖 `lun.N/ro`。可在 `config/gadget.json` 的 `image_context` 覆盖，WebUI 设置页可编辑。
- **SELinux 域模型**：二进制均运行于 root 管理器的 `su` 域，模块**不带 `sepolicy.rule`** 且不规划独立域；socket 边界由 `0700` 目录与 `SO_PEERCRED` 保障，不依赖 SELinux。
- **设备身份与挂载解耦**：保存 USB 设备身份不会断开 USB 物理连接，亦不重新绑定 UDC（配置于下次连接时生效）；挂载亦不触发重新绑定。提供 `rebind`/`--rebind` 显式重绑命令。字段约束：`manufacturer`/`product` 允许 UTF-8（字节 ≤126）；`serial` 必须为可打印 ASCII（避免 Host 枚举失败）。
- **槽位语义**：一个 LUN 对应一个槽位；`unmount --lun N` 仅置为**空闲**（保留参数），`delete-slot --lun N` 移除序号。`lun.0` 为内核创建，**不可删**。
- **脚本只传参**：`module_template/*.sh` 仅传参，决策由 Rust 端（`gadgetdisk boot`）完成。**严禁重新引入 `post-fs-data.sh`**。
- **运行期单一二进制路径**（`bin/gadgetdisk`、`bin/gdd`）：`customize.sh` 安装期按架构扁平化放置，运行期不做 ABI 探测。
- **不手动设置 `webroot` 权限或 SELinux 上下文**，由安装器统一处理。

## 文档地图

| 文档 | 内容 |
|---|---|
| [需求](docs/requirements.md) | 目标、MVP 范围、**非目标** |
| [架构](docs/architecture.md) | crate 划分、数据流、可测试性边界 |
| [协议](docs/protocol.md) | socket 传输、消息表、槽位语义、REST 路由 |
| [镜像格式](docs/disk-image-format.md) | GPT/MBR/FAT32 生成、对齐、下限、已知陷阱 |
| [WebUI](docs/webui.md) | 页面结构、零构建约束、错误兜底 |
| [上传与导入](docs/image-upload-and-import.md) | 存储访问限制与流式分块上传 |
| [本地挂载](docs/ondevice-loop-mount.md) | loop 挂载、能力探测阶梯、互斥约束 |
| [挂载可见性与 vold](docs/mount-visibility-and-vold.md) | 可见性限制、vold 迁移评估与否决证据 |
| [Android 集成](docs/android-integration.md) | configfs 流程、开机对账、SELinux 与镜像上下文 |
| [测试](docs/testing.md) | 三级验证、目录约定、差分选择 |
| [构建与发布](docs/build-and-release.md) | 脚本入口、工具链探测、代码风格、打包产物 |
| [路线图](docs/roadmap.md) | 当前状态、已知缺陷、待验证假设 |

## 词数预算

本文件 ≤1500 词（中文按字符计），由 `.agents/scripts/gates/verify_agent_gates.py` 强制。
