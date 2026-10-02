# GadgetDisk 文档

本目录收录 GadgetDisk 的**系统架构规范与技术契约**。设计决策推演与方案权衡详见 [.agents/notes](../.agents/notes/README.md)。

## 规范与要求

- 正文使用中文；标识符、协议字段、命令、路径与代码标识保持原样。
- 未经硬件实测验证的结论，统一标注为 **待验证假设**。
- 遵循单一事实源原则，跨模块技术规格统一通过相对链接引用。

## 语言边界：操作者输出用英文

**面向操作者的输出一律英文**；**代码注释与文档正文一律中文**；**WebUI 界面
一律中文**。三者互不重叠。

| 面 | 语言 | 理由 |
|---|---|---|
| CLI `--help`、stdout/stderr、日志 | **英文** | 终端环境、日志聚合与外部工具链的通用语言 |
| CLI / REST 的 `message`、`note` 等 JSON 文本值 | **英文** | 面向 API 使用者；`error` 码才是稳定契约 |
| Python 脚本（`scripts/**`）输出、`module_template/*.sh` 的 `ui_print` | **英文** | 安装器 UI 与构建日志 |
| Rust / Python / Shell 的注释与 doc comment | **中文** | 面向维护者，与文档正文一致 |
| `docs/**`、`.agents/notes/**` 正文 | **中文** | 本仓库的规格载体 |
| WebUI 界面文案（标签、按钮、提示、错误文案） | **中文** | 面向终端用户 |

**两条硬性推论**

1. **错误码优先于后端 `message`。** `message` 是英文的，不能直接进中文界面。
   前端按 `error` 码用自己的映射表生成文案，后端 `message` 只用于未知码兜底与
   `detail` 排查线索（见 [通信协议](protocol.md#http-api)）。
2. **clap 的帮助文案必须写成显式属性。** clap derive 把 `///` doc comment
   直接渲染成 help，因此注释规范（中文）与输出规范（英文）冲突。凡用户可见的
   帮助文本都写成 `#[arg(help = "...")]` / `#[command(long_about = "...")]`，
   中文说明保留为 `//` 注释。漏写**不会编译失败**，只会让该选项的说明变空或漏出
   中文——由 `every_help_screen_is_english_and_non_empty` 测试用例专项保障此项规范。

## 索引

| 文档 | 内容 |
|---|---|
| [需求与范围](requirements.md) | 目标、用户场景、MVP 范围、**非目标**、约束 |
| [架构](architecture.md) | crate 划分、数据流、可测试性边界、运行期布局 |
| [通信协议](protocol.md) | 传输与安全控制、握手、帧格式、完整消息表、job 机制 |
| [磁盘镜像格式](disk-image-format.md) | 布局规范、生成算法、对齐与容量约束、已知陷阱 |
| [WebUI](webui.md) | 前端结构、双通信通道、视图流转、错误处理 |
| [上传与导入](image-upload-and-import.md) | 存储访问限制、WebView 分块流式上传、进度轮询 |
| [本地 loop 挂载](ondevice-loop-mount.md) | 挂载路径、能力探测、互斥约束与释放时序 |
| [挂载可见性与 vold](mount-visibility-and-vold.md) | namespace 限制、vold 集成评估与否决证据 |
| [Android 集成](android-integration.md) | configfs 流程、开机对账、SELinux 上下文、生命周期脚本 |
| [测试规范](testing.md) | 分层测试策略、目录约定、差分选择规则 |
| [构建与发布](build-and-release.md) | 脚本入口、工具链探测、代码风格、ABI 扁平化、打包产物 |
| [路线图](roadmap.md) | 里程碑交付、验收清单、待验证假设汇总 |
