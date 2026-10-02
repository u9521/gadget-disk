# GadgetDisk

把 Android 手机模拟成 USB 大容量存储设备（UMS），并支持在设备本地挂载镜像直接编辑。

*Turn an Android phone into a USB Mass Storage device, with on-device loop mount for editing images.*

## 这是什么

GadgetDisk 是一个 KernelSU / APatch 模块：手机插到电脑上会以**标准 USB 磁盘**出现，
电脑侧无需安装任何驱动或客户端。镜像本身存在手机里，可以随时通过模块自带的 WebUI
创建、上传、挂载与弹出，也能在手机上直接 loop 挂载后编辑分区内容。


## 主要能力

- **镜像生成**：GPT / MBR / raw 三种布局，支持多分区与 FAT32 / exFAT / ext4 格式化。
- **USB 导出**：多 LUN，各自独立的 `rw` / `ro` / cdrom 模式与 SCSI INQUIRY 标识。
- **本地编辑**：经 loop 设备在手机上挂载镜像，与 USB 导出保持**双向互斥**，不可能同时占用同一镜像。
- **设备身份定制**：可改 `idVendor` / `idProduct` / 厂商 / 产品名 / 序列号，保存后于下次连接生效。
- **自带格式化工具**：随模块分发 `mkfs.vfat`，不依赖设备上是否存在 dosfstools。

## 环境要求

| 项 | 要求 |
|---|---|
| 系统 | Android 12 及以上 |
| Root | KernelSU 为主适配目标，APatch 尽力兼容 |
| 内核 | 需要 `mass_storage` function、configfs 与 loop 驱动支持（运行期动态探测） |

版本约束与非目标见[需求与范围](docs/requirements.md)。

## 安装

1. 从本仓库的 Releases 页面下载 `GadgetDisk-<version>.zip`；
2. 在 KernelSU / APatch 管理器中作为模块安装；
3. **重启设备**后生效——管理器走的是暂存路径，重启前模块处于 pending-update 状态，
   此时 WebUI 会被拒绝打开。详见[构建与发布](docs/build-and-release.md)。

## 构建与开发

脚本是 uv 管理的 Python 包，首次使用先 `uv sync`：

```sh
uv run gd-check        # 格式、静态检查与治理门禁
uv run gd-test --all   # 全量测试：Rust + WebUI
uv run gd-build        # 交叉编译各 ABI（需要 NDK）
uv run gd-package      # 组装模块 ZIP
uv run gd-deploy       # 推送并安装到已连接设备
```

改动期遵循三级测试纪律：`cargo check -p <crate>` → `cargo nextest run -p <crate>` →
提交前 `uv run gd-test --all`。完整契约见[测试规范](docs/testing.md)与[构建与发布](docs/build-and-release.md)。
CI 在推送与 PR 上跑同一组入口（`.github/workflows/ci.yml`），打 `v*` 标签时构建并发布 Release。

## 许可

**GPL-3.0-only**，见 [LICENSE](LICENSE)。

## 文档

[文档索引](docs/README.md)收录全部规格与技术契约；设计决策推演与权衡记录在
[Agent Notes](.agents/notes/README.md)；贡献与改动约定见 [AGENTS.md](AGENTS.md)。
