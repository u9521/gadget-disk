# Agent Note: configfs 实测语义与内核接缝

Status: implemented
Category: architecture

## Problem

`gadgetdisk-usb` 必须操作真实的 `/config/usb_gadget/g1`，但 configfs 的内核行为存在严格的状态机约束：部分属性写入返回成功但未实际生效，部分属性在 LUN 活跃时直接返回 `EBUSY`。此外，导出前需原子备份 Android 原生 gadget 身份及 UDC 拓扑并在卸载时完整回退；真机环境下 Android 的 `init` 进程高频轮询重配 UDC，符号链接误操作极易引发状态互锁。同时需明确各二进制在 SELinux 策略中的运行域归属。

## Proposal

### 1. 内核访问收敛到 `ConfigFs` trait

抽离 `ConfigFs` 接口，使操作时序与分支覆盖可在开发宿主单元测试中断言：

| 实现 | 用途 |
|---|---|
| `RealConfigFs` | 生产：直接读写 `/config/usb_gadget/g1` |
| `MemConfigFs` | 测试：内存测试替身，模拟内核状态机限制（含覆写不改变已生效绑定、`lun.0` 不可删等） |

同层的 `GadgetTree`（`discover`）负责 gadget/config 目录探测，生产环境基于文件系统探测，测试环境使用内存树。

### 2. 内核实测行为约束

`RealConfigFs` 与 `MemConfigFs` 均须严格满足以下内核行为约束：

1. **写 `file` 真正绑定后备介质**：`file` 必须在设备模式属性配置完成后最后写入。
2. `cdrom`/`ro` 必须在 `file` 之前写入；LUN 已绑定时修改属性返回 `EBUSY`，必须先清空 `file`（`clear_lun`）。
3. 写入不存在的镜像路径时返回失败，且属性保持原值。
4. **覆写已导出 LUN 的 `file` 不会动态切换后端**：内核在写入瞬间已打开并锁定文件，属性读回为新值但已生效的绑定不变，`/sys/block/sda/size` 仍对应旧镜像。更换镜像必须显式执行卸载流程。
5. `os_desc/use`（配套 `qw_sign = MSFT100`）必须在挂载 mass_storage 之前写 `0`，避免 Windows 主机误按 MTP 兼容 ID 匹配驱动；原值随身份备份持久化至 `run/gadget-backup.json`，卸载时在重绑 UDC 之前还原。
6. 创建 config 符号链接禁止使用强制覆盖模式：configfs 对覆盖既有链接的操作返回 `EPERM`。
7. `rmdir` 移除 `lun.N` 目录会导致内核隐式解绑 UDC，后续重新绑定步骤必须校验 `state == configured`。
8. 写入 0 字节为 no-op（返回 0 但内核属性不变）：`RealConfigFs` 对空值写入改写 `"\n"`，避免 `clear_lun` 静默失效引发后续属性写入 `EBUSY`。
9. `lun.0` 由内核随 function 固有创建，`rmdir` 返回 `EPERM`，仅支持弹出/清空；`lun.1+` 支持动态删除（详见 [槽位模型与镜像上下文 Note](2026-10-04-slot-model-and-image-context.md)）。
10. 清空 `UDC` 在已为空时返回 `ENODEV`，作为幂等成功处理。
11. `symlink(2)` 的 target 统一规整为绝对路径再调用，消除对进程当前工作目录 (cwd) 的隐式依赖。

### 3. 身份备份与还原（`IdentityBackup`）

CLI 在修改身份前捕获 Android 原生配置并原子写入 `run/gadget-backup.json`：

- **排除本模块自有链接**：链接名等于 `paths::LINK_NAME` 或目标包含 `paths::FUNCTION_NAME` 的项不写入备份，防止还原阶段引用已被清理的 function 导致 `ENOENT`。
- 链接目标通过 `readlink` 提取真实路径并归一化为相对 gadget 根路径，还原时原样写回，禁止无条件追加 `functions/` 前缀。
- 还原时序固定：清空 `UDC` 断开 → 移除自有符号链接 → 恢复 `os_desc/use` → 恢复 gadget 基础属性 → 恢复字符串描述符 → 重建配置符号链接 → 写回 `UDC`。
- 还原策略遵循 Best-effort：单项失败不阻断后续回退，汇总所有错误统一上报。

### 4. UDC 抢占守卫机制

针对 Android `init` 进程高频重配 USB 行为的防护规则：

- **仅增删本模块自有的符号链接**：保留系统原生链接（`f1..f3`），消除与系统 `init` 脚本的 `ENOENT` 互锁，防止 UDC 卡滞于 `addressed` 状态（Windows 呈现代码 10）。
- 挂载采用紧凑执行段：清空 `UDC` → 建立自有链接 → 写入 `file`，三步之间不插入磁盘或日志 I/O。
- 完成判据以内核真值为准：必须同时满足 `lun.N/file` 非空且 `/sys/class/udc/<udc>/state == configured`。

### 5. 运行域模型：沿用 root 管理器 `su` 域

二进制均运行于 root 管理器的 `su` 域（实测 `u:r:ksu:s0`），**不创建独立 SELinux 域**，模块**不包含 `sepolicy.rule`**：

- IPC 边界由 `0700 root:root` 目录权限与 `SO_PEERCRED UID == 0` 强校验保障，不依赖 SELinux 规则；
- 避免独立域策略膨胀引发系统级连锁 AVC 拒绝；
- 降低跨 root 管理器版本的策略适配成本。

## Alternatives considered

- **写入空字节/ftruncate 清空属性**：否决。configfs 属性基于内核 store 回调，空字节写入无动作，须写入 `"\n"` 触发清理。
- **`chdir` 到 gadget 根以支持相对目标**：否决。进程级全局状态会破坏并发安全性与相对路径解析，绝对路径方案无副作用且可离线单测。
- **依据链接名推导 function**：否决。实测符号链接名与 function 内部命名存在解耦，会导致 `ENOENT`。
- **清空配置目录下全部符号链接（f1..f3）**：否决。删除系统原生链接会破坏 init 状态机，导致 UDC 卡滞于 `addressed` 状态。
- **单方面写 UDC 而不等待 Android 状态机**：部分否决。保留为主流程超时后的兜底逻辑，优先等待系统调度完成自动绑定。
- **挂起 USB HAL 守护进程**：否决。真机环境下重配由 PID 1 (init) 直接执行属性动作触发，无独立 HAL 进程可供拦截。
- **划分独立 SELinux 域**：否决。详见 Proposal 第 5 条。

## Acceptance criteria

1. `cargo nextest run -p gadgetdisk-usb` 全绿通过。
2. 关键时序与语义由主机测试断言：
   - `file_attribute_is_written_after_cdrom_and_ro`：断言 `file` 最后写入；
   - `real_fs_write_semantics_for_clear_and_enodev`：断言空串写入转换为 1 字节换行，且容忍 UDC 的 `ENODEV`；
   - `symlink_normalizes_absolute_target`：断言符号链接行为与进程 cwd 无关；
   - `mem_fs_allows_overwrite_but_binding_is_unchanged`、`mem_fs_rejects_binding_missing_file`：断言覆写不切换介质、无效路径报错并保留旧值；
   - `mem_fs_rejects_deleting_lun0`、`mem_fs_rejects_removing_function_with_bound_lun`：断言 LUN 0 删除保护。
3. 备份/还原逻辑通过完整测试集校验。
4. 抢占守卫通过状态机时序测试断言。
5. 运行域模型验证：无 `runcon` 切换、无 `sepolicy.rule`，socket 目录与 UID 校验生效。
6. 镜像 SELinux 上下文在导出前完成读写修正，校验以校验和为准。
7. 门禁验证通过：`python3 .agents/scripts/gates/verify_agent_gates.py`。

## Risks

- **待验证假设**：上述语义基于 AVD 的 `dummy_udc.0` 与 Android 17 内核实测；真机硬件 USB 控制器的属性写入时序与动态 LUN 表现需进一步复核。
- **待验证假设**：`lun.N/file` 覆写在重新插拔 USB 后是否会改用新文件，当前按“换镜像必须先卸载”保守处理。
- **待验证假设**：`os_desc/use` 与 Windows 代码 10 的因果关系，恢复依赖备份文件完整性。
- **待验证假设**：部分厂商内核若在删除 function 后无法重建，需考虑退化为常驻 function 仅清空 `file` 的策略。
