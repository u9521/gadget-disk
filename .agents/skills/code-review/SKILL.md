---
name: code-review
description: 按本仓库的 AGENTS.md 约定、Agent Notes 决策与架构不变量进行结构化代码审查。审查 Rust 代码、WebUI、脚本或文档改动时使用。
---

# 代码审查

目标：以仓库既有约定与**已记录的决策**为准绳审查改动，而不是以个人偏好。审查是只读的。

## 准备

1. 读根 [AGENTS.md](../../../AGENTS.md) 了解架构与约定。
2. 读 [.agents/notes/README.md](../../notes/README.md) 与相关 Note —— 尤其是 `implemented/` 与 `archived/`，避免把已否决的方案当成缺陷提出。
3. 明确改动涉及的 crate 与依赖关系（见 [架构](../../../docs/architecture.md)）。

## 检查清单

### 架构不变量

- [ ] configfs 里 **mass_storage 部分只有 `gdd` 写**；CLI 的就地操作（loop、mount(2)、
      镜像文件、身份属性）不得越界去写 `functions/mass_storage.*`。
- [ ] `gdd` 不持有业务状态（不读写 `state.json`/`gadget.json`/`offsets.json`）；
      这条由 `crates/gadgetdisk-gdd/tests/scope.rs` 的源码扫描强制。
- [ ] 同一镜像不会同时作为 gadget LUN 与 loop 附件，也不会作为两个 LUN 的后端；
      判据取**内核真值**（`lun.N/file`、`/sys/block/loopN/loop/backing_file`），
      不靠进程内状态。
- [ ] 重新挂到 gadget 前完成 `sync` → `umount` → `losetup -d` 并校验。
- [ ] 没有引入 TCP 监听或抽象套接字（两者均已被否决，理由见相关 Note）。
- [ ] 跨进程的内核操作经 `run/ops.lock` 的 `flock` 串行化；并发请求返回 `busy`。

### 安全

- [ ] socket 目录为 `0700 root:root`；`SO_PEERCRED` uid 校验存在且拒绝路径有测试。
- [ ] 不重新引入 `sepolicy.rule` 或独立 SELinux 域（已明确不做，理由见
      [Android 集成](../../../docs/android-integration.md)）。
- [ ] 镜像上下文只改 `images/` 下的文件；目录外**只警告不改**。
- [ ] 不用 `lun.N/ro` 判断写入是否可用（写入被拒时它仍回显 `0`）。
- [ ] 文件路径来自外部输入时做了校验（防路径穿越，尤其是在镜像目录之外写入的场景）。
- [ ] 协议长度字段有上限校验，避免内存放大。

### 数据安全

- [ ] configfs 写入顺序正确，`file` 属性**最后**写。
- [ ] `cdrom`/`ro` 在 `file` 之前写入。
- [ ] 涉及 configfs 改写时先备份 gadget 状态（`run/gadget-backup.json`）并按安全次序还原。
- [ ] FAT32 格式化显式指定 `FatType::Fat32`（否则会静默产生 FAT16）。
- [ ] GPT 分区显式传对齐 `Some(2048)`（否则起点落在 LBA 34）。
- [ ] 目录遍历跳过 `.` 与 `..`（避免循环引用引发栈溢出）。
- [ ] 导入写临时文件后原子改名，避免半成品被当作完整镜像。
- [ ] 容量校验含 FAT32 下限与可用空间预检。

### 测试

- [ ] 单测就近内联；集成测试为**单一入口**（每 crate 至多一个 `tests/integration.rs`）。
- [ ] 纯逻辑落在 `gadgetdisk-core` / `gadgetdisk-proto`，可在主机测试。
- [ ] 特权 IO 经 trait 抽象，操作**顺序**有主机侧断言。
- [ ] 未引入需要 Android 目标或真机才能跑的单元测试。

### 文档与治理

- [ ] 关键架构、接口或行为变更配有 Agent Note，且与代码同一提交。
- [ ] 未验证的结论显式标注为**待验证假设**，未伪装为已证事实。
- [ ] 事实只有一处归属：新事实写入其归属文档，其他位置只放链接。
- [ ] 移动/归档 Note 后，入链已更新。
- [ ] 改动 `AGENTS.md` 后词数仍在预算内。

### 一致性与可移植性

- [ ] 未硬编码本机路径或宿主细节（SDK/NDK 绝对路径、路径翻译命令、固定的 prebuilt 目录名）。
      工具链位置只经 `ANDROID_HOME` / `ANDROID_NDK_HOME` 注入，未引入其他环境变量入口。
- [ ] 新增 crate 时同步更新 `scripts/test/cli.py` 的差分映射表。
- [ ] 错误码使用协议中定义的稳定字符串，未临时发明新码。
- [ ] 新增/删除文件后，`docs/**` 的路径描述与 `.gitignore` 保持一致。

## 输出格式

对每个问题给出：

1. **位置**：文件与行号。
2. **问题**：具体缺陷或约定偏离。
3. **依据**：指向 AGENTS.md 条款、Agent Note 或文档章节；若无依据则是个人偏好，应标注为建议而非问题。
4. **建议**：最小可行修改。

按严重性排序：数据损坏/安全 > 违反架构不变量 > 测试缺口 > 文档缺失 > 风格建议。

## 架构既定约束与禁区（已决议方案，禁止重复提案）

- **禁止引入抽象套接字**：缺少文件系统权限保护，非 root 应用可直接枚举。
- **禁止恢复 `client` 子命令层级**：冗余抽象已废除，保持 CLI 扁平化。
- **禁止添加 `sepolicy.rule` 或划分独立域**：IPC 边界由 `0700` 目录权限与 `SO_PEERCRED` 保障，独立域会引发连锁 AVC 拒绝并恶化兼容性。
- **禁止将 `webui/` 挪回模块模板目录**：前端源码保持独立解耦，以便整体热替换。
- **禁止引入 vold 挂载方案**：需修改 vendor 分区 fstab，不具备通用模块化分发条件。
- **禁止自研用户态文件系统驱动**：统一依赖内核原生文件系统驱动挂载。
- **禁止拆分多个独立 `tests/*.rs`**：避免集成测试二进制过多膨胀编译与链接耗时。
