# 需求与范围

## 目标

提供一个 KernelSU / APatch 模块，基于 Linux USB Gadget (ConfigFS) 将 Android 设备模拟为 USB 大容量存储设备 (USB Mass Storage / UMS)，支持 U 盘与 CD-ROM 模式，并在模块内嵌 WebUI 中完成磁盘镜像的创建、导入、挂载与本地维护。

## 目标用户

- 已获取 root 权限（KernelSU 或 APatch）并熟悉模块安装的 Android 用户。
- 典型应用场景：移动存储挂载、ISO 镜像装机引导、免 PC 设备端镜像维护。

## 核心用例

1. **通用大容量存储**：将 `.img` 镜像挂载为读写 USB 块设备，供 Host 端识别为常规物理磁盘。
2. **只读保护分发**：以写保护模式挂载，防止 Host 端意外篡改镜像数据。
3. **系统引导介质**：将 `.iso` 镜像挂载为虚拟 CD-ROM，用于装机启动或系统救援。
4. **镜像创建**：在设备端生成带 GPT/MBR 分区表的磁盘镜像，可配置多分区与 FAT32/exFAT/ext4 文件系统。
5. **本地镜像导入**：将设备现有存储目录中的镜像文件安全导入模块工作区。
6. **设备本地维护**：通过 loop 设备将镜像挂载至本地目录，支持在移动端文件管理器中编辑，卸载后重新导出为 USB 设备。

## MVP 范围

**USB 设备侧**

- 设备模式：可读写 (rw)、只读 (ro)、CD-ROM / ISO (cdrom)。
- **多 LUN 支持**：上限 8 个（`gadgetdisk_usb::MAX_LUNS`）；每个 LUN 可独立配置设备模式与 SCSI INQUIRY 标识。
- configfs 实现：含 UDC 拓扑协调与**开机对账**（取代脆弱的脚本启动保护，详见 [Android 集成](android-integration.md)）。
- USB 设备身份定制：支持配置 `idVendor` / `idProduct` / 制造商 / 产品名 / 序列号（持久化于 `config/gadget.json`）。

**磁盘镜像**

- 布局支持：`raw`（无分区表）、`gpt`（多分区，≤128）、`mbr`（主分区 ≤4；支持 1 个扩展分区容器与最多 64 个逻辑分区）。
  每个分区可独立指定容量、类型与名称（名称仅 GPT 有效）。
- 文件系统：FAT32（模块自带 `mkfs.vfat` 独立工具）、exFAT、ext4（后两者使用系统 `mkfs`）。
- 容量限制：**下限按每个分区所选的文件系统判定**——FAT32 33 MiB、exFAT 1 MiB（待验证假设）、ext4 2 MiB，
  不格式化的分区只需 1 MiB；**不设镜像整体最小容量**，默认镜像 4 GiB。
- **同名不覆盖**：目标已存在时拒绝创建并回 `already_exists`。

**WebUI**

- 纯静态资源（零构建、原生 HTML5/ES6），离线可用。
- 核心视图：挂载状态监控、创建镜像、镜像导入、镜像管理、本地 loop 编辑挂载、设置与诊断。
- 创建视图支持分区编辑（能力受布局限制），并展示探测到的格式化工具来源。

**设备本地挂载**

- 内核 loop 块设备 + 原生文件系统驱动；统一走 `lo_offset` 分区偏移挂载
  （不依赖内核派生分区子设备，见 [本地 loop 挂载](ondevice-loop-mount.md)）。

**工程与治理**

- 7 个 crate 的 workspace 架构，分层测试策略，uv 管理的 Python 工具链，Agent Notes 与文档治理门禁。

## 非目标

明确**不做**的事项（防止需求范围蔓延；若有调整需新建 Agent Note）：

| 非目标 | 原因 |
|---|---|
| **自研用户态文件系统驱动** | 实现复杂度与数据损坏风险过高；磁盘编辑统一调用内核原生文件系统驱动完成（详见 [本地挂载](ondevice-loop-mount.md)） |
| **系统文件管理器全自动可见** | 取决于第三方应用所属的 mount namespace，超出模块底层控制范围（详见 [可见性与 vold](mount-visibility-and-vold.md)） |
| **vold 深度集成** | 需侵入式修改 vendor 分区 fstab，不具备模块化分发条件；列入后置探索项 |
| **`nofua` 用户开关** | 虽能提升写入吞吐，但显著增加异常断电导致数据丢失的风险；现阶段仅探测能力，不暴露开关 |
| **分区表动态编辑工具** | 可在**创建时**配置多分区（GPT ≤128、MBR 主分区 ≤4、扩展/逻辑分区 ≤64，逐个指定容量/类型/名称），但**不提供**对已有镜像增删分区的工具——那需要原地重整分区表与文件系统，风险远高于新建 |
| ~~**多 LUN 支持**~~ | **已支持**：上限 8 个 LUN，各 LUN 映射为独立 SCSI 逻辑单元并享有独立参数 |
| ~~**多分区创建**~~ | **已支持**：`create` 接受 `partitions` 列表，见 [磁盘镜像格式](disk-image-format.md)。原「MVP 聚焦单分区」的限制已解除 |
| ~~**exFAT / ext4 镜像创建**~~ | **已支持**：改用系统 `mkfs`（`mkfs.exfat`、`mke2fs`）后，原否决理由「缺乏无依赖的纯 Rust 写入库」不再成立。**FAT32 由模块自带的独立工具 `bin/mkfs.vfat`（基于 `fatfs` 编译）格式化**——AVD 实测设备上不存在系统级 `mkfs.vfat` |
| **配套 Android 原生 APK** | 架构严格约束为纯 WebUI，避免引入重型应用依赖 |
| **armv7 (32 位) ABI** | 工具链不设支持；基线仅提供 arm64-v8a 与 x86_64 二进制 |
| **Magisk 原生适配** | Magisk 缺失原生 WebUI 容器，需引入 APK 或常驻 HTTP 服务，与极简架构冲突 |

### 已撤销的非目标

以下两项**原列为非目标，经实测论证后纳入范围**，保留推演结论以防重复评估：

| 原非目标 | 原否决理由 | 现结论 |
|---|---|---|
| **REST API** | 「`ksu.exec` + Unix socket 已提供安全的等价能力」 | **已纳入**。`ksu.exec` 每次调用均需 fork 进程并依赖常驻后台；采用本地回环 REST + 按需进程模型后，交互响应大幅提升且开机零常驻进程。 |
| **网络直传导入** | 「WebView 存在 mixed content 拦截风险；开启 TCP 端口扩大攻击面」 | 前半句经实测不成立（见 [协议](protocol.md)）；后半句成立（回环端口对全设备 UID 开放，UID 2000 可直连）。因此 REST 接口强制配置 Bearer Token 鉴权。 |

注：受 Android WebView 架构限制，前端无法获取宿主文件系统路径，镜像导入由前端经系统文件选择器读取二进制数据并通过 REST 通道分块流式上传（详见 [上传与导入](image-upload-and-import.md)）。

## 约束

**平台约束**

- 操作系统：Android 12 及以上。
- Root 环境：KernelSU 为主适配目标；APatch 尽力兼容（**待验证假设**，见 [路线图](roadmap.md)）。
- 内核能力：依赖内核 `mass_storage` function、configfs 与 loop 驱动支持；运行时动态探测。

**技术契约**

- WebUI 运行于 `https://mui.kernelsu.org` 虚拟源，入口固定为 `webroot/index.html`。
- 后端通信以 127.0.0.1 本地回环 REST API 为主通道（附带 Bearer Token 鉴权），以 Unix 域套接字为底层特权与降级通道。
- 模块自完备性：核心功能依赖静态编译二进制，不依赖设备第三方工具链（系统 toybox/busybox 仅用于诊断兜底）。

**开源许可**

- 遵循 **GPL-3.0-only**。

**构建环境**

- NDK 路径与 prebuilt 目录动态探测，禁止硬编码宿主路径；工具链位置**只**经 `ANDROID_HOME` 与 `ANDROID_NDK_HOME` 注入，不读 `local.properties`，也不提供链接器覆盖机制；NDK 必须与构建宿主同平台。
- 构建使用用户默认的 `CARGO_HOME`（`~/.cargo`），不在仓库内自建工具链缓存。
- 详见 [构建与发布](build-and-release.md)。
