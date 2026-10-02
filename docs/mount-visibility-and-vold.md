# 挂载可见性与 vold 评估

本文件回答两个问题：本地挂载的**可见性**边界，以及能否**改用 vold** 以获得更好的三方可见性。结论依据为 Android `system/vold` 源码查证。

## 一、可见性：只承诺「路径可访问」

Android 各进程的 **mount namespace 不统一**。root 建立的挂载是否被某个文件管理器看到，取决于**该 app 进程所处的 mount namespace**，而非挂载本身是否成功。

**本模块的立场**

- 挂载执行进程（`gadgetdisk serve` 或 CLI）默认以**加入 init 全局 mount namespace** 的方式挂载（由 root 管理器保障，等价于 `su -M`），以最大化跨进程可见性。
- 提供「私有 namespace」配置项作为备选。
- **只保证「挂载点在指定路径可访问」**，UI 提示使用支持 root 浏览的文件管理器。
- **不承诺系统文件管理器自动可见** —— 取决于目标应用所属命名空间，不在本模块底层控制范围。

**待验证假设**：切换至全局 mount namespace 后在各厂商定制 ROM 下对第三方文件管理器的实际穿透可见性，需在不同真机环境下进一步验证。

## 二、vold 迁移：当前不可行

### 四条否决证据

| 候选路径 | 否决依据（源码查证） |
|---|---|
| `persist.sys.virtual_disk` 调试特性 | `VolumeManager::updateVirtualDisk()`：镜像路径**硬编码**为 `/data/misc/vold/virtual_disk`，容量固定 `kSizeVirtualDisk = 536870912`（512 MiB），由 `Loop::createImageFile()` **自行创建**；构造的 `Disk` 带 `Flags::kAdoptable \| Flags::kSd`。`kAdoptable` 意味着挂载时会**格式化并加密，清除原有数据**。**缺乏向外部预制镜像提供动态挂载的接口** |
| fstab `vold_managed` DiskSource | `VolumeManager::mDiskSources` **仅**由 `main.cpp::process_config()` 从 `ReadDefaultFstab()` 读取的 `/vendor/etc/fstab.*` 条目填充（条件 `entry.fs_mgr_flags.vold_managed`）。`handleBlockEvent()` 用条目的 `blk_device` 作为 `sysPattern` 去匹配 uevent 的 `DEVPATH`；且仅接受 `DEVTYPE == "disk"`。本模块创建的 loop 设备**无法匹配**这些模式。要匹配必须修改 `/vendor` |
| `IVold.createStubVolume(sourcePath, mountPath, fsType, …)` | AIDL 签名看似正是「把任意路径挂成卷」，但 `model/StubVolume.cpp` 的 `doMount()` 实现仅为：调用 `listener->onVolumeMetadataChanged()`、`setInternalPath(mSourcePath)`、`setPath(mMountPath)`，随后 `return OK` —— **不执行任何实际挂载**。该接口仅作为应用程序存储存根的元数据外壳 |
| `IVold.mountFstab(blkDevice, mountPoint, …)` | 面向 fstab 的内部接口，非「挂载任意镜像」的通用 API；不提供卷注册与向应用暴露的能力 |

### 关键区分：受阻的是可见性，不是挂载能力

本模块的 loop 实现与 **vold 使用同一内核机制**：

| 操作 | vold `Loop::create()` | 本模块 `gadgetdisk-loop` |
|---|---|---|
| 取空闲设备 | `LOOP_CTL_GET_FREE` | 同 |
| 绑定后端文件 | `LOOP_SET_FD` | 同 |
| 设置偏移/标志 | `LOOP_SET_STATUS64` | 同 |

因此**放弃 vold 不损失底层块设备挂载能力**。限制本质在于 Android 上层存储栈：第三方应用对卷的感知依赖于 vold 卷注册表及 MediaProvider/FUSE 堆栈，而该机制对非标准外部镜像完全封闭。

### 结论与路线图定位

- **MVP**：使用自有 `gadgetdisk-loop` + 全局 namespace 挂载。不依赖 fstab、不修改系统分区、不受 ROM 差异影响。
- **vold 迁移**：列入[路线图](roadmap.md)的**后置探索项**，标注「需系统级修改，默认不做」。保留本文档证据以避免重复调研。
- **仅限调试的旁路**（**不得用于用户镜像**）：`persist.sys.virtual_disk` 可用于观察 vold 如何向系统暴露卷。注意它会清除该 512 MiB 虚拟盘的数据，且与用户可能已有的虚拟盘冲突。

## 三、重新评估的触发条件

若出现以下任一情况，应新建 Agent Note 重新评估，并在本文档追加链接（不修改本结论）：

- 上游为 vold 增加「从任意镜像创建公共卷」的公开接口；
- 出现不修改 `/vendor` 即可被 vold 接受的 Disk 注册途径；
- 有厂商 ROM 提供了等价的稳定挂载暴露机制。

## 四、降级与兜底方案

任何 loop 能力不可用的设备上，模块仍可通过 **gadget 方向**工作：把镜像挂载为 USB 设备，在电脑上编辑。UI 在 loop 不可用时应明确引导该路径，而非仅报错。
