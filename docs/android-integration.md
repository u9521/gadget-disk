# Android 集成

本文件定义 configfs 操作流程、USB 设备模式、启动保护、SELinux 与生命周期脚本。设计理由见 [架构 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

## configfs 路径

**运行时必须探测，不能硬编码**。AOSP 的 [init.usb.configfs.rc][aosp-rc]
基准仅引用 `usb_gadget/g1` 与 `configs/b.1`，可作为通用兜底；
但实测部分厂商 ROM（如红魔等定制系统）在 `/config/usb_gadget/` 下除 `g1` 外还存在
**vendor 私有的 `g2`**，说明厂商会增删 gadget。严禁硬编码路径，必须在运行时动态匹配。

[aosp-rc]: https://android.googlesource.com/platform/system/core/+/refs/heads/main/rootdir/init.usb.configfs.rc

探测顺序（`gadgetdisk_usb::discover`，理由会记入 `Layout` 并显示在诊断页）：

| 目标 | 顺序 |
|---|---|
| gadget | 绑定到 `sys.usb.controller` 的那个 → 名为 `g1` → 唯一的一个 → **报错** |
| config | 含至少一个符号链接的那个 → 名为 `b.1` → 唯一的一个 → `b.1`（待创建） |

选不出来时**必须报错而不是盲写**——猜错会写坏别的模块/框架正在用的 gadget。

| 项 | 值 |
|---|---|
| Gadget 根 | 探测结果；AOSP 兜底 `/config/usb_gadget/g1` |
| 配置名 | 探测结果；AOSP 兜底 `b.1` |
| Function 名 | `mass_storage.gadget-disk` |
| 配置符号链接名 | `mass_storage.gadget-disk`（**必须**与 function 同名，见下） |
| LUN 数量上限 | `gadgetdisk_usb::MAX_LUNS = 8`（内核 `FSG_MAX_LUNS` 更低时以其错误为准） |
| `inquiry_string` 长度上限 | `28`（内核按 `%-28s` 定宽写入，超长**静默截断**） |
| gadget 字符串上限 | `126`（内核 `usb_string_copy` 的 `USB_MAX_STRING_LEN`） |
| UDC 控制器来源 | 系统属性 `sys.usb.controller` |
| UDC 状态 | `/sys/class/udc/<udc>/state`（`configured` 才表示主机已接受） |

写入前必须用 `fstatfs` 校验 magic 为 **configfs**，避免误写普通文件系统造成数据损坏。

### 为何链接名必须避开 Android 的 `fN`

AOSP 的 `sys.usb.config=none` 动作**只删** `configs/b.1/f1`、`f2`、`f3`（同上引 rc）。
将本模块的配置符号链接命名为 `mass_storage.gadget-disk`（而非 `f1`），可避免其受到 Android
原生 teardown 逻辑的影响——这是下面「清 UDC 后立刻建链接」策略成立的前提，已由 AOSP
源码确证，不再是推断。

## 挂载流程

**与 Android 抢占 UDC 是本流程的核心约束**。真机实测：`init` 监听 `sys.usb.config`，
对此前我们清空 UDC 的动作反应**有延迟**；若在「清 UDC」与「建立我们的配置」之间
留下窗口，Android 会抢先按自己的配置绑定 UDC，结果是 UDC 停在 `addressed`、
主机侧报「代码 10 / 指定不存在的设备」。

**本流程不再暂停 gadget HAL**。原先第 3 步对 `android.hardware.usb.gadget*` 发
`SIGSTOP`，但真机（红魔9 Pro）上根本没有该进程——USB 配置由 `init` 通过
`sys.usb.config` 驱动，而作为 PID 1 的 `init` 进程无法被挂起；且内核 `gadget_dev_desc_UDC_store` 在
gadget 已绑定时写 `UDC` 返回 `EBUSY`，说明绑定状态由**显式写入**驱动，不存在
需要压制的持续抢占者。整套机制已删除，理由见
[Note](../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)。

### 哪些操作需要 UDC 空闲（内核源码依据）

拆开「改 configfs」与「断开 UDC」这两件事，是本流程的关键。核 mainline 源码后：

| 操作 | 要求 UDC 空闲？ | 出处 |
|---|---|---|
| 建配置符号链接 | **是**，已绑定时 `EINVAL` | `configfs.c` `config_usb_cfg_link` |
| `mkdir lun.N`（N≥1） | **是**，function **被配置链接引用**时 `EBUSY` | `f_mass_storage.c` `fsg_lun_make`（`fsg_opts->refcnt`） |
| `rmdir lun.N`（N≥1） | 不检查，但会**隐式解绑** gadget | `fsg_lun_drop` → `unregister_gadget_item` |
| 写 `lun.N/file` | 否（仅 `prevent_medium_removal` 时 `EBUSY`） | `storage_common.c` `fsg_store_file` |
| 写 `cdrom`/`ro` | 否，但要求后端文件**未打开** | `_fsg_store_ro` |
| `lun.N/forced_eject` | 否 | `fsg_store_forced_eject` |
| 写 `lun.N/inquiry_string` | 否 | `fsg_store_inquiry_string` |
| 写 `idVendor`/`idProduct`/`strings/*`/`os_desc` | 否 | `configfs.c` / `usb_string_copy` / `os_desc_use_store` |
| 写 `UDC`（非空） | — 已绑定时返回 `EBUSY` | `gadget_dev_desc_UDC_store` |

**因此：断开 UDC 是「刷新并生效」的手段，不是所有写操作的前提。** 只有「建链接」
与「增删 LUN 目录」需要它。

> **LUN 创建依赖与时序约束**：内核 `fsg_lun_make` 校验的是 function 的引用计数
> （`fsg_opts->refcnt`）。只要 function 仍被配置符号链接引用，`mkdir lun.N` 即返回 `EBUSY`
> （即使 UDC 处于解绑状态）。
>
> 因此新增 LUN 的操作序必须为：**解绑并移除本模块符号链接 → `mkdir lun.N` → 重建符号链接 → 写入 `file`**。
> 本实现严格按此顺序编排（由测试 `creating_a_lun_unlinks_our_link_first` 保证）。

### 归属：谁写哪些 configfs 条目

| 条目 | 归属 |
|---|---|
| `functions/mass_storage.gadget-disk` 及 `lun.N/*`、配置链接、`UDC` | **`gdd`** |
| `idVendor`/`idProduct`/`strings/*`/`os_desc` | **CLI**（`gadgetdisk_usb::identity`） |

`gdd` **不认识**身份属性；CLI 的只读视图**不实现**可写 trait，因此它在类型层面
无法改 LUN。见 [gdd 拆分 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

### `mount`：准备阶段 + 条件性紧凑段

1. **参数校验**（写 configfs 之前）：LUN 序号 `< MAX_LUNS`、同一请求内不得重复、
   `inquiry_string` ≤28。
2. **探测 UDC**：无则返回 `no_udc` 且**不得继续改动 configfs**。
3. **确保 function 存在**（`mkdir` 不要求 UDC 空闲）。
4. **逐个处理请求的 LUN**：
   - **已存在** → `forced_eject(N)` → 写 `cdrom`、`ro`、`inquiry_string`。
     这一步**不碰 UDC**，因此改一个既有 LUN 不会让 USB 链路抖动。
   - **不存在** → 记入待创建列表。
5. **条件性紧凑段**（仅当有待创建 LUN，或我们的链接不存在时）：
   - 写空串到 `UDC`，并**删掉我们自己的符号链接**（把 `refcnt` 降到 0，
     否则 `mkdir lun.N` 报 `EBUSY`）；
   - `mkdir lun.N` 并写好全部参数（`file` 留到最后）；
   - 重建配置目录（若需）与**我们自己的**符号链接；
   - 等 UDC 恢复（3 秒轮询）；超时则自己写 `UDC`。
   - 段内**禁止**任何日志、`/proc` 读取与写盘——时间窗口越短，Android 系统框架并发介入导致竞态冲突的概率越低。
6. **逐个写 `lun.N/file`（最后写）**：内核在写入的瞬间打开并 pin 住后端文件，
   因此 `cdrom`/`ro` 必须先写好。UDC 已绑定时该写入仍合法（`forced_eject` 已清掉
   `prevent_medium_removal`）。
7. **校验**：`UDC` 非空 + 我们的链接存在 + **每个被请求的** `lun.N/file` 非空。
   任一不满足返回 `NotActive`，**不得**报成功。

> **只增删我们自己名字的链接**：绝不遍历删除配置目录下的符号链接，Android 的
> `f1..f3` 属于框架，删掉会让它的 USB 状态与内核不一致，反而更容易被重新抢占。

### 为什么用 `forced_eject` 而不是覆写 `file`

`lun.N/file` 可以被覆写且返回成功，但**已生效的绑定不会改变**（AVD 实测：
guest 侧容量不变）——内核在首次写入时 pin 住了 `filp`。

`lun.N/forced_eject`（write-only，写任意非零字节）先清 `prevent_medium_removal`
再调 `fsg_store_file(..., "")`，**真正关闭**后端文件；之后重写 `file` 才会让内核
重新打开新的后端。因此「换镜像 / 换模式」的第一步必须是强制弹出。

退化路径：`forced_eject` 是较新的属性，老内核可能没有。此时退化为「清空 `file`」，
在主机未锁介质时等效。

### 卸载：按 LUN 与全部是两件事

| 命令 | configfs 效果 | 主机侧 |
|---|---|---|
| `unmount --lun N` | 只 `forced_eject(N)`；LUN 目录、链接、function、UDC **全部保留** | 第 N 个介质消失，其余不变 |
| `unmount`（无 `--lun`） | 断 UDC → 清全部 file → 删链接 → 删 `lun.1+` → 删 function | 设备整体消失 |

按 LUN 卸载后要恢复只需重新写 `file`，不必重配 USB。

### 「设备弹出」后的收尾（不解绑 UDC）

内核在 gadget deactivate（拔线）时会清空**全部** `lun.N/file`，这是**弹出**的内核
真值判据（`is_ejected`：我们的 function 与链接存在，且全部 LUN 的后端都已解绑）。
此时 `gdd` 删掉我们的链接与 function，**不解绑 UDC**——一旦主动解绑，`init` 进程将立即根据
`sys.usb.config` 重新下发系统原生配置，导致状态机出现紊乱。

**判据必须要求「全部」LUN 为空**：单个 LUN 为空是正常的按 LUN 卸载，不得触发清理。

若 configfs 因仍绑定而拒绝删链接/删目录（`EBUSY`/`EINVAL`），**只记日志不报错**：
容忍局部节点残留优于导致整个清理流程报错中断，下次挂载前的准备阶段会再清一次。

**AVD 实测**：删除配置符号链接这一步**本身就会让内核清空 `UDC`**——「删链接」与
「解绑 UDC」在这颗内核上是同一件事。（`rmdir lun.N` 同理，见上面的源码依据表。）
我们的代码在弹出清理里不写 `UDC`（由单测锁定），但无法阻止这个隐式解绑。
真机上「弹出」由拔线触发、gadget 已 deactivate，因此不受影响。

### `effective` 的判据

`status` 的每个 LUN 的 `effective` 取 **`/sys/class/udc/<udc>/state == configured`**
（且该 `lun.N/file` 非空）。只看 `file` 非空会在「主机未接受配置」时谎报成功——真机实测正是
这种情形下 Windows 报「代码 10」。读不到 UDC 名或 `state` 文件时返回 `false`：
`effective` 的语义是「已确认生效」，确认不了就不该声称生效。

### `inquiry_string` 为什么要在写入前校验

内核用 `snprintf(curlun->inquiry_string, sizeof(...), "%-28s", buf)` 写入定长缓冲，
**超长会被静默截断**。因此我们在写入前自行拒绝（`INQUIRY_STRING_MAX = 28`），
而不是让用户「设了却发现没生效」。内核缺该属性（老内核）而用户明确请求了它时
**报错而不是忽略**。

### USB 设备身份

`idVendor`/`idProduct`/`strings/<lang>/{manufacturer,product,serialnumber}` 与
`os_desc/use` 由 **CLI** 写入（`gadgetdisk_usb::identity`），持久配置在
`config/gadget.json`，Android 的原始值备份在 `run/gadget-backup.json`。

要点：

- **字段全部可选**：只写设置过的字段；配置文件不存在时**完全不碰** configfs，
  让 Android 的值继续生效。
- **逐项读回核实**：configfs 接受写入不等于值生效，不一致返回
  `GadgetError::Config`。用户明确设置了身份却静默沿用 Android 的，属于谎报成功。
- **字符串规则按字段区分、长度以 UTF-8 字节计**：
  `manufacturer`/`product` 允许中文（UTF-8 字节数 ≤126），`serial` 额外要求可打印
  ASCII。上限单位是**字节**——内核 `usb_string_copy` 判的是 `strlen`，AVD 实测
  42 个汉字（126 字节）通过、43 个（129 字节）被拒。
  序列号单独保留 ASCII 是因为真机观察：非 ASCII 序列号会导致 Host 端无法识别设备
  （成因见 [路线图](roadmap.md) 的待验证假设）。
  > 字符串描述符经内核转换为 UTF-16LE（`utf8s_to_utf16s`），制造商与产品名完全支持中文输入。详见
  > [UTF-8 身份 Note](../.agents/notes/implemented/feature/2026-10-04-usb-identity-and-multi-lun.md)。
- **应用身份时顺带关掉 `os_desc/use`**：Android 为 MTP 把它置 `1` 并设
  `qw_sign = MSFT100`，保留会让 Windows 按 MTP 的兼容 ID 匹配驱动（「代码 10」
  的候选成因之一）。
- **身份与挂载解耦：保存身份不断开 USB**。身份属性写在 configfs 上，但主机只在
  **重新枚举**时读到它们，因此「保存」的语义是**保存意图、下次连接生效**。
  CLI 不自动重绑，`run_mount` 也不因身份改动强制重绑——提供
  `--rebind` 与 `POST /api/v1/rebind` 作为显式重绑命令。
  > 为什么不自动重绑：用户修改展示用的产品名时不应导致 USB 链路抖动；且
  > 实测断开 USB 后 Android init 会按 `sys.usb.config` 重置 VID/PID，重新绑定的效果将被系统重置所抵消。持久意图以 `config/gadget.json` 为准。
- **保存身份不得清空 `image_context`**：两者同在 `config/gadget.json`，因此「只改
  身份」必须经 `GadgetConfig::store_identity` 做读—改—写，不能整文件覆盖。

## 与 loop 挂载的互斥

同一镜像**绝不可**同时作为 gadget LUN 与 loop 附件，否则双写损坏文件系统。
判据取**内核真值**：`gdd` 拒绝对同一镜像的重复 LUN（同一请求内），CLI 在删除/
导入/本地挂载前读 configfs 判断该镜像是否正被导出。两者都是短命进程、都现读真值，
因此即使它们是按需拉起的，判定也永远正确（见 [本地 loop 挂载](ondevice-loop-mount.md)）。

重新挂到 gadget 前必须完成 `sync` → `umount` → `losetup -d` 并校验释放。

## USB 设备模式

| 模式 | configfs 属性 | 说明 |
|---|---|---|
| `rw` | `ro=0`, `cdrom=0` | 默认，可读写 U 盘 |
| `ro` | `ro=1`, `cdrom=0` | 写保护 |
| `cdrom` | `cdrom=1` | 光驱，建议配合 `.iso` 且只读 |

`nofua`（跳过强制落盘）与 `removable` 作为**能力探测项**：内核缺该属性时静默跳过，不作为错误。本轮不向用户暴露 `nofua` 开关。

## 开机对账（取代启动保护）

**问题**：若在改写 configfs 的中途失败（崩溃、断电、被 kill），会留下半配置状态
——我们的 function/链接还在，但 LUN 没绑好，或 UDC 没绑上。

**机制**：`service.sh` 调用一次 `gadgetdisk boot --data-dir …`，由它读
`run/state.json`（**导出意图**）与 configfs 真值收敛：

| 情况 | 动作 |
|---|---|
| `state.json` 有非空意图 | 丢弃镜像不存在的条目；其余经 UDS 让 `gdd` 重新导出；用返回的内核真值回写意图 |
| 意图里的镜像**全部**不存在 | 清空 `state.json` |
| 无意图，但内核里还有我们的 function/链接 | 经 UDS 全量卸载清理残留 |
| 无意图，也无残留 | 无事可做 |

失败时**保留** `state.json`（用户的「重启后自动恢复」意图不该被一次失败抹掉），
原因写入 `logs/service.log`（超过 256 KiB 即轮转一代，保留 `.1`）。`boot` **只尝试
一次**，不重试——它总是以 0 退出，脚本不需要判据。架构采用声明式意图对账，废弃基于 shell 脚本的熔断计数与 dirty 状态标记。相关推演见
[按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)
与 [内核接缝 Note](../.agents/notes/implemented/architecture/2026-10-04-configfs-and-kernel-seam.md)。

### 数据目录一次性迁移

模块 id 由 `gadgetdisk` 改为 `gadget-disk` 时数据根也随之改名。迁移
（`/data/adb/gadgetdisk` → `/data/adb/gadget-disk`，**仅当新目录不存在**）原本在
`post-fs-data.sh` 里，现在由 `gadgetdisk` 的 `prepare()` 承担——**任何**子命令启动
时都会过一遍，因此用户升级后立刻打开 WebUI 也能看到旧镜像，不必先重启。

## 镜像文件的 SELinux 上下文（内核读镜像的前提）

**问题**：`mass_storage` 由**内核线程**读后备镜像（`f_mass_storage.c` 的
`file-storage`，用 `kernel_read`），而 SELinux 按**该线程的域**判定其读权限。
镜像默认继承 `u:object_r:adb_data_file:s0`（由 root 管理器定义），`kernel` 域对它
没有 `read`：

```text
avc: denied { read } for comm="file-storage"
     scontext=u:r:kernel:s0 tcontext=u:object_r:adb_data_file:s0 tclass=file
```

表现是设备**枚举成功但读不出内容**（`critical medium error` /
`unable to read partition table`）——PC 侧看到一块「无法读取」的磁盘。

### 实测（x86_64 AVD / Android 17）

**判据必须同时覆盖 `read` 与 `write`**，并且要用端到端校验和交叉验证。内核真值
取自 `/sys/fs/selinux/access`；「guest 写入落到镜像」指 guest 侧写入后宿主侧镜像
的 `md5` 是否变化：

| 上下文 | kernel `read` | kernel `write` | guest 写入落到镜像 | 新增 avc |
|---|---|---|---|---|
| `u:object_r:media_rw_data_file:s0` | ALLOW | **ALLOW** | **YES** | **0** |
| `u:object_r:system_file:s0` | ALLOW | **DENY** | **NO**（`md5` 不变） | 10 |
| `u:object_r:vendor_file:s0` | DENY | DENY | 不可用 | — |
| `u:object_r:rootfs:s0` | 无法设置（`chcon` 被拒） | — | — | — |
| `u:object_r:adb_data_file:s0`（继承的默认） | DENY | DENY | 不可用 | — |

> **SELinux 上下文对写入的静默丢弃**：若镜像被标注为 `system_file` 等标签，
> 内核 `fsg_lun_open` 打开失败将静默回退至只读模式。此时 guest 侧虽可正常挂载并执行写入（不报 I/O 错误），
> 但底层变更不会同步至镜像，且 `lun.N/ro` 仍可能汇报 0。因此不能以 `lun.N/ro` 作为可写性依据，
> 必须确保镜像文件具备允许 `kernel` 域读写的上下文（默认 `media_rw_data_file`）。
> 详见 [`system_file` 静默丢写 Note](../.agents/notes/implemented/bug-fix/2026-10-04-selinux-image-context.md)。

另两条实测：上下文**跨重启保持**（未被 `restorecon` 重置）；用
`ksud sepolicy apply 'allow kernel adb_data_file:file { read open }'` 加规则
**无效**（规则被接受，avc 依旧）。因此「改文件标签」是当前唯一可行的手段。

### 实现

**三条路径在触碰内核之前检查并（必要时）修正**，共用同一个实现
（`crates/gadgetdisk-cli/src/image_context.rs`，判定逻辑在 `src/selinux.rs`）：

| 路径 | 内核侧读写者 | 入口 | 修正时机 |
|---|---|---|---|
| 导出为 USB 设备（gadget LUN） | `file-storage` 内核线程 | `run_mount` / `serve::gdd_op` | 写 `lun.N/file` **之前** |
| 本地挂载（loop） | loop 工作线程 | `run_attach_loop` / `serve::loop_attach` | `LOOP_SET_FD` **之前** |
| 经 loop 的格式化（`mkfs`） | loop 工作线程 | `MkfsFormatter::format` | 第一个 `losetup` **之前** |

> **本地 loop 路径同样受限（真机报告）**：loop 的后备镜像由内核工作线程读写，
> SELinux 按**该线程的域**判定权限，与“谁发起了挂载”无关。因此标签不许可时
> loop 挂载同样会失败或写入被静默丢弃。该结论的**真机独立确认**列为待验证假设
> （见 [路线图](roadmap.md)）。

判定规则：

1. 读 `security.selinux` xattr；读不到（非 SELinux 设备）→ 静默通过；
2. **不在 `images/` 下** → 只输出告警，**不修改**（用户的文件可能被其他系统策略依赖）；
3. 在 `images/` 下且上下文已是目标值 → 无需操作；
4. 在 `images/` 下且不匹配 → 修正为目标值并告警；修正失败亦仅告警，**不阻断**挂载或格式化
   （挂载操作本身仍具备部分可用性，例如仅供主机端识别块设备存在）。

**只读（`--mode ro`）挂载同样应用目标标签**：不区分 ro/rw 方可保障“先只读挂载、
随后改读写”流程无缝衔接且无需二次修改标签。这是一条明确的架构取舍。

目标上下文默认 `u:object_r:media_rw_data_file:s0`（实测 `read` 与 `write` 均允许），
可在 `config/gadget.json` 的 `image_context` 字段中配置：CLI 采用
`gadgetdisk config security set --image-context …` / `clear`，
REST 采用 `GET|POST /api/v1/config/security`，
WebUI 位于「设置与诊断 → 镜像安全上下文（SE 标签）」——提供可配置项是考虑到 AVD 的
策略未必与各厂商定制 ROM 完全一致。

两处写入（CLI 与 REST/WebUI）**先校验再落盘**，且落盘一律走
`GadgetConfig::store_image_context`（原子读—改—写）：其与 USB 身份同存于
`config/gadget.json`，整文件覆盖将导致彼此的配置项被静默清除。

**修改安全标签后需重新挂载方可生效**：内核在 `lun.N/file` 写入（或 loop 打开后备文件）
的瞬间按**当时**的安全标签打开并锁定文件句柄，已在导出或挂载中的镜像不会因修改标签而
动态重新校验权限。

**告警传播机制**：CLI 通道同时输出至 `logs/cli.log`、stderr 与 stdout（WebUI 的 CLI
回退通道读取 stdout）；REST 通道输出至 stderr，并将告警封装于响应的 `warnings` 字段
（`POST /api/v1/loop/attach` 与 `POST /api/v1/create`），供前端呈现英文诊断原文。

**取舍**：`media_rw_data_file` 的访问规则比 `adb_data_file` 宽，其他应用对该文件
的访问可能受影响。我们的镜像位于 `0700` 的 `/data/adb` 下、只有 root 能到达，
因此实际影响面很小。

## SELinux

### 运行域：`su` 域，不建独立域

`gadgetdisk` 与 `gdd` 都**不经 `runcon` 切换**，直接运行在 root 管理器授予的
`su` 域（本环境实测为 `u:r:ksu:s0`）。模块脚本（`service.sh` / `uninstall.sh`）
实测同样运行于该域，并已在 init 挂载命名空间内。

模块**不带 `sepolicy.rule`**，也不计划声明独立的 `gadgetdisk_daemon` 域。理由：

1. **socket 的安全边界不由 SELinux 提供**（见下节），独立域不增加实际防护；
2. 完整规则集规模大，调试期容易产生连锁 `avc` 拒绝而影响整机；
3. 两个二进制都在 `su` 域已经具备全部所需能力，声明额外的域只会多一层
   需要与 root 管理器版本对齐的不确定性。

### 访问控制不依赖 SELinux（已实测）

socket 的安全边界由两层独立保证，与运行域无关：

1. **文件系统层**：socket 目录 `0700`、owner `root:root`。无 `x` 权限即无法 `connect()`。
2. **凭据层**：`accept` 后读取 `SO_PEERCRED`，`uid != 0` 立即关闭连接。

AVD 实测：非 root 进程在文件系统层即被挡住；`ksu.exec` 发起的 CLI 在 `gdd` 侧
被记录为 `peer uid=0`，链路正常。REST 侧另有一道 Bearer token 校验（见
[协议](protocol.md)），因为回环端口对全设备开放。

### 内核读写镜像文件**不是** SELinux 规则的事

内核线程（`file-storage`）按自己的域判定能否读写 LUN 的后备镜像。内核线程读写镜像文件受阻并非配置 SELinux 策略规则所能解决的问题
——实测 `ksud sepolicy apply 'allow kernel ...:file { read
open }'` 被接受（`rc=0`）但 `avc` 依旧。实际做法是**改镜像文件自身的上下文**，
由 CLI 在挂载前完成，见上文「镜像上下文」。loop 挂载与经 loop 的格式化同理
（读写者是 loop 工作线程）。

**APatch 的 WebUI 桥接能力未验证**（不涉及 `sepolicy.rule`），列为待验证假设。

## 生命周期脚本

**按需进程模型**：开机不常驻任何进程。`service.sh` 仅调用一次 `gadgetdisk boot`
完成开机对账并立即退出；REST 后端由 WebUI 首次访问时按需拉起，空闲超时自动退出。理由见
[按需进程模型 Note](../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)。

| 脚本 | 阶段 | 职责 |
|---|---|---|
| `service.sh` | late_start service（非阻塞） | 定位二进制 → 建目录并 `chmod 0700 run/` → `resetprop -w sys.boot_completed` 等待启动完成 → 传参给 `gadgetdisk boot` → 退出。**脚本里没有任何判据** |
| `uninstall.sh` | 卸载时（**开机早期，早于 `service.sh`**） | 删 `webroot/api.json` → 调 `gadgetdisk uninstall` 收尾 → 兜底清理 loop 与数据目录。**不终止任何进程**（见下） |
| ~~`post-fs-data.sh`~~ | — | **已删除**。该阶段不做任何事，职责归 `service.sh` + `gadgetdisk boot` |
| `action.sh` | 用户点击 Action 按钮 | 本轮不实现（WebUI 由管理器界面进入，Action 价值有限） |

**等待启动完成：`resetprop -w`（AVD 实测）**

`service.sh` 用 `resetprop -w sys.boot_completed` 阻塞等待，取代原先「每秒
`getprop` 一次、最多 60 次」的轮询：由属性服务的变更通知唤醒，不再空转。

三条实测/源码结论（缺一即会写出错误实现）：

1. **必须用单参形式。** `resetprop -w NAME VALUE` 的语义是「值**不等于** VALUE 就
   立即返回，等于 VALUE 才阻塞等待」，因此 `-w sys.boot_completed 1` 在开机早期
   （属性尚未置 1）会**立刻返回**，完全不等。单参形式 `-w NAME` 才是「等该属性出现」。
   AVD 实测：属性已为 `1` 时 `-w sys.boot_completed 1` 阻塞（超时被杀），
   而 `-w gd.test.prop` 在属性 3 秒后出现时准点返回 0。
2. **不加 `--timeout`。** 那是 KernelSU 对 resetprop 的扩展，Magisk 没有该选项，
   加上会破坏 Magisk 兼容性。
3. **`resetprop` 不在默认 `PATH` 上，但模块脚本能直接调用它。** KernelSU 为模块脚本
   追加了自己的二进制目录（源码 `get_common_script_envs`：`PATH` = 原值 + `BINARY_DIR`，
   而 `BINARY_DIR` = `/data/adb/ksu/bin/`；该目录下有 `resetprop -> ksud` 符号链接）。
   注意：在 `adb shell` 里直接跑 `resetprop` 会报 `inaccessible or not found`，
   那**不代表**模块脚本里也不能用——它的环境不同。

> **待验证假设**：KernelSU 侧已由源码与 AVD 实测确认；APatch 与 Magisk 的
> `resetprop -w` 单参语义按 Magisk 上游实现（`native/src/core/resetprop/cli.rs`）
> 推断一致，尚未在对应环境实测。

该等待是**机械等待**而非判据，因此不违反下文的「脚本只传参」原则：失败/超时的
判断仍留给 `gadgetdisk boot`。

**脚本注意事项**

- 用 `MODDIR=${0%/*}` 获取模块目录，**不要硬编码路径**。
- 模块脚本由 KernelSU 的 busybox `ash` 执行。
- **脚本只传参，决策在 Rust 侧**：不要在 `service.sh` 里加判据。此前旧版本曾在此处解析
  `state/dirty` 标记、累加失败计数并写入 `touch disable` 阻断文件，那是为常驻进程模型服务的，
  现在只会成为第二份会漂移的实现。
- **`pkill -f` 不可用**：toybox 的 `pkill` 没有 `-f`（实测报 `No PATTERN`）。当前
  **没有任何脚本需要按命令行杀进程**（`uninstall.sh` 已不需要，见下条）；若将来需要，
  只能扫 `/proc/*/cmdline` 并注意 `/gdd` 与 `/gadgetdisk` 会互相误伤。
- **`uninstall.sh` 不终止任何进程**：卸载脚本在**开机早期**执行，早于 `service.sh`——
  KernelSU 在 post-fs-data 阶段由 `prune_modules()`（`userspace/ksud/src/module.rs`，
  经 `init_event.rs` 调用）执行它，Magisk 由 daemon 的 `remove_modules()`
  （`native/src/core/daemon.rs` → `module.rs`）执行。那时 `gadgetdisk`/`gdd` 不可能在跑
  （`gdd` 只在有镜像导出期间存在，而导出状态是 `service.sh` → `gadgetdisk boot` 恢复的），
  因此扫 `/proc` 杀进程既无对象，又会带来误伤他人进程的风险。凭据文件
  （`webroot/api.json`）仍要兜底清理：用户可能在模块被移除前手动拉起过 `serve`。
- **`boot` 总是以 0 退出**（失败不该被当成进程崩溃）。真正的结果体现在
  `run/state.json` 是否被兑现与 `logs/service.log` 的记录上，脚本不做判据。
- 不需要 `setsid` / 退避重启 / 服务锁：那些是为「常驻 daemon」服务的，按需进程模型下无需长期维持守护进程的存活状态。
- `/data/adb` 的遍历权限可能被 root 管理器重置，需在启动脚本中确保可到达模块目录。

## 能力探测（运行时）

模块启动与首次操作时探测并缓存，经 `CapabilitiesResponse` 暴露：

- `mass_storage` function 是否可创建（部分 OEM 内核不支持）；
- 可用 LUN 数量上限；
- `max_part`、`vfat`/`exfat` 支持（见 [本地 loop 挂载](ondevice-loop-mount.md)）；
- `nofua` / `removable` 属性是否存在；
- LUN 数量上限与 `inquiry_string` 长度上限（经 `Capabilities` 回 `max_luns` 与 `inquiry_string_max`）；
- SELinux 是否 enforcing。

**USB 未连接 / 无 UDC 时**：允许只完成配置，但状态必须标记为 `effective=false`（「已配置但未生效」），不得误报成功。
