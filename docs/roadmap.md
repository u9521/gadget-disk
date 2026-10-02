# 路线图

本文件为项目的**当前交付状态、已知缺陷与待验证假设**的统一汇总清单。任何未经硬件实测验证的技术结论均在此处集中维护。

## 当前交付状态

**GA 阶段（M1–M11 全部交付）**：完成基准版本代码收敛，剔除历史过渡逻辑与宿主特定路径，构建验证统一收敛至 uv 工具链（详见 [构建与发布](build-and-release.md)）。

已交付能力：

- **磁盘镜像生成**：支持 GPT / MBR / raw 三种布局与 FAT32 文件系统格式化，严格执行容量与对齐校验。
- **USB 块设备导出**：将镜像挂载为 USB Mass Storage LUN（支持多 LUN、`rw`/`ro`/`cdrom` 模式与自定义 SCSI INQUIRY），Host 端识别为标准物理磁盘。
- **设备本地维护**：通过 loop 设备挂载镜像，实现移动端本地免 PC 编辑；与 USB 导出保持严格**双向互斥**。
- **模块 WebUI**：纯静态资源（零构建 H5），提供六大核心视图；以本地回环 REST 为主通道，以 `ksu.exec` + CLI 为降级通道。
- **USB 描述符定制**：支持配置 `idVendor` / `idProduct` / 制造商 / 产品名 / 序列号，支持 UTF-8 中文字符描述符。
- **槽位状态模型**：一个 LUN 对应一个独立槽位，弹出后介质脱卸但配置保留，支持快速重新绑定或删除。
- **SELinux 标签治理**：挂载前主动校验并修正镜像文件安全上下文，确保内核线程具备双向读写权限。
- **按需进程模型**：开机不常驻任何守护进程，资源按需唤起并具备空闲回收机制。
- **持续集成与发版**：推送/PR 上重跑本地同一组门禁与全量测试（含两个 Android target 的跨目标静态检查），`v*` 标签触发构建并发布 Release（版本号与二进制自报版本由 `build-info.json` 强一致）。见 [构建与发布](build-and-release.md#持续集成)。

架构与运行期布局详见 [架构](architecture.md)；设计推演详见 [.agents/notes](../.agents/notes/README.md)。

## 已知缺陷

| # | 缺陷 | 影响 | 归因 / 规划 |
|---|---|---|---|
| ~~1~~ | ~~`create` 忽略 `--label` 与 `--filesystem` 参数~~ | — | **已修复**：`CreateOptions` 补齐 `label`/`filesystem`/`partitions` 字段，CLI 与 REST 两条通道都会下发。回归由 `create::tests::custom_label_is_written_to_volume` 守住 |
| ~~4~~ | ~~分区小于 64 MiB 时误报「空间不足」~~ | — | **已修复**：`resolve_sizes` 曾把 FAT32 的 64 MiB 下限无条件当成镜像下限并返回 `no_space`（HTTP 507 → 前端「存储空间不足」），无论该分区的文件系统是 `none`/ext4/exFAT。现改为**按每个分区的文件系统**判定（FAT32 33 MiB / exFAT 1 MiB / ext4 2 MiB），沿用 `size_below_minimum` 并带上行号与文件系统。见 [分区容量下限按文件系统判定](../.agents/notes/implemented/bug-fix/2026-10-08-per-filesystem-partition-minimum.md) |
| 2 | `gadgetdisk-loop` 测试用例遗留临时目录 | 宿主 `/tmp` 产生临时文件碎片，不影响运行时功能 | 测试用例缺少 Teardown 自动回收逻辑，待补充自动清理 |
| 3 | `gadgetdisk-gdd` 的 `logging` 测试相互竞争 | `cargo test -p gadgetdisk-gdd --lib` 偶发失败（实测在 `HEAD` 上 8 次中 1 次、在并行负载下更频繁），**而在 `--test-threads=1` 串行执行下稳定通过**；属测试隔离缺陷，不影响运行时功能 | 多个测试共用进程级全局 `static SINK: OnceLock<Mutex<Sink>>`，且临时目录按 `std::process::id()` 命名，同进程内并行测试会互相 `init`/`init(None)` 覆盖。需改为每个测试独立 sink 或强制串行 |

已修复的历史问题（内核线程读写 AVC 拦截、loop 次设备号计算偏差、导入偏移缓存未命中、REST 轮询排队延迟、真机 UDC 抢占互锁、非 ASCII 序列号枚举失败等）的机理与实测依据归档于对应 Agent Note 中。

## 待验证假设

以下结论**未经硬件实测充分确认**，暂作为工作假设推进：

| # | 假设 | 影响 | 验证方式 |
|---|---|---|---|
| 1 | APatch 提供等价于 `ksu.exec` 的 WebUI 注入桥接 | 决定 APatch 环境的兼容性 | 真机安装后观察 WebUI 桥接调用 |
| 2 | ~~root shell 能读取管理器授权的 `content://` URI~~ | — | **已结案（命题已改变）**：上传改为由 WebView 读字节、分块传送，root shell **不参与**读取。经核对 KernelSU 上游源码：`onShowFileChooser` 用 `ACTION_GET_CONTENT`，URI 与路径**都不进入 JS 面**，故「取路径再复制」不可行。见 [上传与导入](image-upload-and-import.md) |
| 3 | 非 KernelSU 管理器的 WebUI 虚拟源亦为 `https://mui.kernelsu.org` | 决定 REST CORS 白名单是否需拓展 | 在对应 WebView 中发起 fetch 并核对 Origin 头 |
| 4 | `serve` 进程 60s 空闲退出阈值符合真实操作习惯 | 过短会导致频繁拉起进程，过长增加常驻时间 | 监控真机连续交互下的进程生命周期 |
| 5 | `serve` 按需拉起链路在各类定制 ROM 下不受权限阻断 | 决定 WebUI 主通道的可用性 | 真机冷启动后验证 WebUI 首屏加载 |
| 6 | 激进的系统低内存清理机制不会误杀进行中的导入任务 | 决定长耗时大镜像导入的稳定性 | 高内存压力下执行大文件导入压测 |
| 7 | `lo_offset` 在各类 OEM 定制内核上均稳定可用 | 决定分区镜像本地挂载的回退通道可靠性 | 在多设备上执行 `losetup -o` 挂载验证 |
| 8 | 全局 mount namespace 挂载在主流第三方文件管理器中穿透可见 | 决定设备端本地编辑的实际易用性 | 验证多款第三方 root 文件管理器中的路径可达性 |
| 9 | `media_rw_data_file` 在 OEM 定制 ROM 上普遍允许 `kernel` 域写入 | 决定默认上下文配置的通用性 | 查询 `/sys/fs/selinux/access` 并校验写入后哈希 |
| 10 | UTF-8 中文制造商/产品名在各类 Host OS 上均能正确解析 | 决定中文描述符的通用性 | 在 Windows/macOS/Linux 设备管理器及 `lsusb` 中核对 |
| 11 | 非 ASCII 序列号导致 Host 端枚举异常的具体机理 | 决定 `serial` ASCII 强制限制的边界条件 | 写入非 ASCII 序列号并分析 PC 端枚举错误及 dmesg |
| 12 | 物理插拔或重新连接后 Host 端会重新枚举字符串描述符 | 决定身份配置在下次连接生效的契约可靠性 | 修改身份后重插 USB 线并比对 Host 端描述符变化 |
| 13 | 真机 ROM 的 `mkfs` 集合与 AVD 一致（有 `mkfs.exfat`/`mke2fs`） | 决定 exFAT/ext4 创建在真机是否可用；FAT32 已改由模块自带 `mkfs.vfat`，不再依赖系统 | 在 arm64 真机上 `find / -name 'mkfs*'` 并与 AVD 结果比对 |
| 14 | APatch / Magisk 的 `resetprop -w` 单参语义与 KernelSU 一致 | 决定 `service.sh` 的启动等待是否可靠 | 在 APatch / Magisk 环境执行 `resetprop -w <不存在的属性>` 并观察是否阻塞 |
| 15 | arm64 真机上 `losetup -o --sizelimit` + 逐个 `mkfs` 的分区格式化链路可用 | 决定**全部**分区格式化（已统一经 loop）在真机是否可用 | 真机上对带 GPT 的镜像格式化多个不同文件系统的分区 |
| 16 | 模块自带的 `bin/mkfs.vfat` 在真机上可执行且 SELinux 上下文正确 | 决定 FAT32 创建在真机是否可用（设备原本没有该工具，须随模块分发） | 真机安装后直接执行 `bin/mkfs.vfat --help` 并创建 FAT32 镜像 |
| 17 | 模块自带的 `mkfs.vfat` 在主流文件管理器的 mount namespace 下不被拦截 | 决定它能否被用户在终端里直接调用 | 在第三方 root 终端中执行并观察 AVC 拒绝 |
| 18 | 真机上按逻辑分区的绝对偏移 `LOOP_SET_STATUS64` 能成功挂载 | 决定 MBR 逻辑分区在设备端本地编辑是否可用 | **AVD 已验证**（见 [MBR 扩展分区](disk-image-format.md#mbr-扩展分区与逻辑分区)）；真机待复核 |
| 19 | WebView 的 `FileReader` 能读出系统文件选择器返回的 `content://` 字节 | 决定分块上传能否工作（是整条上传链路的**唯一前提**） | 真机经 WebUI 选一个大镜像并完成上传；注意这与 #2 是**两个不同命题**（读字节的是 renderer，不是 root shell） |
| 20 | ~~分块上传的吞吐可接受~~ | — | **已实测（AVD）**：450 MiB 约 8.4s；其中读取 `content://` 占 54%（106 MiB/s，系统 provider 决定）、上传 124 MiB/s。分块 32 MiB 为读取吞吐峰值（8/16/64/128 MiB 分别为 46/74/90/29 MiB/s）。真机待复核 |
| 21 | 各 provider 返回的 `File.size` 与实际字节数一致 | 影响进度显示与空间预检的准确度 | 从「文件」「下载」「相册」等不同来源各选一个文件，比对 `File.size` 与实际上传字节数 |
| 22 | 真机内核对「有 `0x05` 扩展分区项、但**没有任何 EBR**」的分区表的行为 | 决定**空扩展分区容器**（预留空间）在设备端是否被正常枚举 | **AVD 已验证创建与读回**（见 [MBR 扩展分区](disk-image-format.md#mbr-扩展分区与逻辑分区)）：`0x05` 项正确写出、起点无 EBR、REST 读回容器与真实分区同时出现；内核枚举与真机待复核 |
| 23 | ~~WebUI 错误面板在**后端返回英文 `message`** 时仍全中文~~ | — | **AVD 已验证**：真实设备后端返回 4 类错误（404/400/`image_not_found`/`size_below_minimum`），传递给**设备运行环境中的** `channel.js` 与 `describe.js`，标题全部为中文且后端原文完整保留在 `detail`。真机待复核 |
| 24 | 运行期输出文案是否需要一条术语词表守护测试 | `--help` 有 `every_help_screen_is_english_and_non_empty` 守，而**运行期文案（日志、错误 JSON 的 `message`、脚本输出）没有**——同构产物的守护存在覆盖不对称 | 观察后续若干次改动是否出现文案语言回退，再决定是否值得引入词表测试 |
| 25 | 真机上 loop 挂载/经 loop 格式化的内核读写者同样受 SELinux 域限制（与 gadget 导出同源） | 决定「挂载 loop 前修正镜像上下文」是否真机必需（当前依据是真机现象报告 + gadget 侧实测类推） | 真机挂载 loop 后 `dmesg \| grep avc` 观察 loop 相关拒绝；用拒绝写入的标签验证写入是否被静默丢弃 |
| 26 | 默认 `media_rw_data_file` 在真机上同时允许 loop 工作线程读写 | 决定默认值能否直接套用到 loop 路径（gadget 侧已在 AVD 实测） | 真机把镜像标为该类型后执行 loop 挂载 + 写入，比对镜像校验和变化 |
| 27 | 33–48 MiB 的**裸 FAT32**（`raw` 布局）在 Linux 上被判为 `vfat` 而非 `vfat32` | 影响 Host 端文件系统类型显示与个别工具的自动挂载决策，不影响读写 | `MKFS.FAT` 判「无分区表」看 `st_size <= 0xFFFFFFFF`，内核/`libblkid` 看簇数 ≥65525——两者判据不同。在 Linux 上对 33 MiB 的 `raw` 镜像执行 `blkid`、`file -s` 并挂载核对；Windows/macOS 行为未验证 |
| 28 | exFAT 的**单分区下限为 1 MiB** | 决定 exFAT 分区的最小可创建容量（当前值是保守猜测） | 宿主没有 `mkfs.exfat`，无法离线实测；在 AVD/真机上对 1 MiB、2 MiB、8 MiB 分区各执行一次 `mkfs.exfat`，记录实际下限并回填常量 |
| 29 | APatch 同样向 `customize.sh` 导出 `$ARCH` / `$IS64BIT` | 决定安装脚本能否去掉 `getprop`/`uname` 兜底。不成立时表现为安装期明确 abort（不会静默装错架构） | 在 APatch 环境安装模块并观察 `ARCH` 取值；KernelSU（`installer.sh`）与 Magisk（`util_functions.sh`）已由源码确认 |
| 30 | CI 的两个 job 在 GitHub 侧可正常跑通 | 首次推送才能确认；失败表现为该 job 红 | 本机无 `act`，仅校验了 YAML 结构与各 action 的输入名。推送一次并观察 Actions 日志 |
| 31 | runner 镜像自带的 NDK 能编出与本地 NDK 同样可用的二进制 | 只影响「产物完全可复现」的强度，不影响正确性 | 本机 NDK 为 29.0.14206865，runner 默认 27.3.13750724；比对两次构建的 ZIP 内容与设备端行为 |
| 32 | `versionCode = git rev-list --count HEAD` 在历史被 squash/rebase 后会**回退** | KernelSU 要求 versionCode 递增，回退可能导致管理器拒绝更新 | 改写历史后重新发版并观察管理器行为；当前 `module.prop` 缺省值与 commit 数恰好一致（均为 1），属巧合而非保证 |

## 后置探索项

| 项 | 状态 | 前置条件 |
|---|---|---|
| **vold 深度集成** | 需侵入式系统修改，**默认不做** | 上游提供通用公共卷注册接口，或出现免改 `/vendor` 的挂载途径（详见 [可见性与 vold](mount-visibility-and-vold.md)） |
| ~~多分区支持与独立文件系统~~ | **已交付** | 见 [磁盘镜像格式](disk-image-format.md)：GPT ≤128 / MBR ≤4 分区，FAT32 + exFAT + ext4 |
| `armv7` (32 位) ABI 支持 | 待评估 | 安装交叉编译 target 并验证 32 位环境二进制行为 |
| Magisk 原生适配 | 待评估 | 需设计免 APK 的 WebUI 替代承载方案 |
| `nofua` 性能开关 | 待评估 | 建立完善的断电数据损坏风险提示与确认交互 |
| ~~`exfat` 镜像创建~~ | **已交付** | 经系统 `mkfs.exfat`（分区经 loop 设备），见 [磁盘镜像格式](disk-image-format.md) |
| clippy `pedantic` 规则治理 | 待评估 | 587 条存量告警，绝大多数与正确性无关（缺 `# Errors` 文档 137 条、建议 `#[must_use]` 127 条）（入口：`uv run gd-check --pedantic`） |
| ~~`create --label` / `--filesystem` 参数生效~~ | **已交付** | 见「已知缺陷」#1（已修复） |

## 开发环境限制说明

- 当前宿主环境**缺少真实物理设备**，移动端行为结论暂标为「待验证假设」，先基于 AVD 环境提供实测基准。
- 当前宿主环境**缺少 root 权限**，特权 loop ioctl 与 `mount(2)` 经由 trait 抽象测试替身覆盖，真实硬件表现列入真机验收规程（见 [测试规范](testing.md)）。
