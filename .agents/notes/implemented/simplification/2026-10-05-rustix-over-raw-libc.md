# Agent Note: 用 rustix 消除可避免的 unsafe

Status: implemented

## Problem

本仓库对 `unsafe` 的治理策略是「`unsafe_code = "allow"`，但每处必须带 `SAFETY:`
注释」，由 `clippy::undocumented_unsafe_blocks = "deny"` 强制。这套策略解决了
「有没有写理由」，但没解决另一个问题：**很多 `unsafe` 本来就不该存在**。

排查发现，相当一部分 `unsafe` 只是「手工包装一个 libc 系统调用」的样板代码：

```rust
let c_path = CString::new(path.as_os_str().as_encoded_bytes())?;
// SAFETY: statvfs 只写入我们提供的缓冲区。
let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
// SAFETY: c_path 是有效的 NUL 结尾字符串。
if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
    return Err(Error::Io(std::io::Error::last_os_error()));
}
let available = stat.f_bavail as u64 * stat.f_frsize as u64;
```

这一小段里塞了四类纯手工风险：`CString` 构造（NUL 字节处理）、`mem::zeroed()`
（未初始化内存）、裸指针传递、errno 手工检查，外加两处整数转换。而这些问题
**rustix 全都已经安全地封装好了**——且 `rustix` 本就是本项目的既定依赖
（`gadgetdisk-gdd` 的 `flock`、`gadgetdisk-loop` 的 `mount`/`unmount` 都在用）。

也就是说，这不是引入新范式，而是**把既有范式的覆盖补齐**。

## Proposal

把 5 个文件里共 **13 处**「纯包装 libc」的 `unsafe` 换成 `rustix` 安全封装
（实测：`grep -c unsafe` 在改动前后逐文件比对）：

| 文件 | 原实现 | 换成 | 消除 |
|---|---|---|---|
| `core/fsinfo.rs` | `libc::statvfs` + `zeroed` | `rustix::fs::statvfs` | 2 |
| `core/create.rs` | 同上 | 同上 | 2 |
| `cli/job.rs` | 同上 | 同上 | 2 |
| `usb/paths.rs` | `libc::statfs` + `zeroed` | `rustix::fs::statfs` | 2 |
| `cli/selinux.rs` | `libc::lgetxattr` ×2 + 手写两步缓冲 | `rustix::fs::lgetxattr` | 2 |
| `loop/loopdev.rs` | `libc::mknod` ×2 + `libc::chmod` | `rustix::fs::mknodat` / `chmod` | 3 |

连带收益：`CString` 手工构造、`mem::zeroed()`、errno 手工检查与多处整数转换一并消失。
`gadgetdisk-core` / `-cli` / `-usb` 三个 crate 增加 `rustix` 依赖，工作区 features
补 `alloc`（`lgetxattr` 的 `&mut Vec<u8>` 缓冲需要）。

**系统调用总体策略**：一律优先 rustix 安全封装；仅在 rustix 无对应 API、或其封装会
掩盖本项目刻意显式化的内核语义时才用裸 `libc`。剩余 41 处属于此类
（`LOOP_SET_STATUS64` 等裸 ioctl、`SO_PEERCRED` 凭据校验、`accept`/`poll` 的
`EINTR`/`EAGAIN` 细节、`sockaddr_un` 的定长布局），继续由 `SAFETY:` 注释强制。

## Alternatives considered

- **保持现状（`unsafe` + `SAFETY:` 注释）**：否决。注释能说明「为什么这次是安全的」，
  但无法消除「每次都靠人写对」的负担。这类样板代码是纯机械的，交给库比交给注释可靠。
- **迁移 `gdd/socket.rs` 的 socket 原语**：否决。该文件的 `0700` 目录 + `SO_PEERCRED`
  校验 UID 0 是**安全关键路径**；rustix 的 `accept`/`poll` 封装会丢掉本项目显式处理的
  `EINTR`/`EAGAIN` 分支，收益不抵风险。
- **迁移 `loopdev.rs` 的 ioctl 封装**：否决。`LOOP_SET_STATUS64` 等需要按结构体精确
  布局，rustix 无对应安全封装，硬包一层只会把风险藏得更深。
- **为「无害转换」引入 `#[allow]`**：否决，同前次决策。全仓保持零 `allow`。

## Acceptance criteria

- `unsafe` 代码出现次数 **54 → 41**（在 `HEAD` 与改动后分别用
  `grep -rn unsafe --include=*.rs crates/ | grep -v unsafe_code | grep -vE ':\s*//' | wc -l`
  实测；排除注释行，避免把「提到 unsafe 的说明文字」算进来）。
- `cargo clippy --workspace --all-targets -- -D warnings` **0 告警**
  （`undocumented_unsafe_blocks` 仍为 `deny`，无遗漏注释）。
- **跨目标编译必须通过**（见下「风险」，`gd-check` 抓不到这一项）：
  ```sh
  cargo check --workspace --all-targets --target aarch64-linux-android
  cargo check --workspace --all-targets --target x86_64-linux-android
  ```
- `cargo test --workspace -- --test-threads=1` **440 个测试全通过**，exit 0。
- `uv run gd-check` 六项全绿。
- 行为等价：`statvfs`/`statfs` 的字段值与 `libc` 版逐字段一致（实测 `f_type`、
  `f_frsize`、`f_bavail`、`f_blocks`、`f_bsize` 全部相同）；`mknodat` 的 `EEXIST`
  与 `libc::mknod` 的 `EEXIST` 同为 errno 17（实测比对）。

## Risks

- **`FsWord` 跨目标类型不一致（本轮最大的坑，已实测踩到）**：
  `rustix::fs::StatFs::f_type` 是 `FsWord`，其定义随后端变化——
  Linux `linux_raw` 后端为 `c_long`，**Android `libc` 后端为 `u64`**，部分平台为 `u32`。
  于是三种直觉写法各有各的失败：
  - `i64::from(st.f_type)` → Android 上编译失败（`u64: Into<i64>` 不存在）；
  - `st.f_type.try_into()` → 宿主上被 `clippy::useless_conversion` 拦下；
  - `st.f_type as u64` → 被已设为 `deny` 的 `clippy::cast_sign_loss` 拦下。

  最终采用**统一经 `i128` 中转**：`i128::from(st.f_type) == i128::from(MAGIC)`。
  `i64`/`u64`/`u32` 三种形态都能无损转入，且在宿主与两个 Android 目标上均零告警。

  这个陷阱**不会被 `uv run gd-check` 发现**，因为它只跑宿主 target。
  因此改动平台相关代码后必须手动跑双 Android 目标的 `cargo check`，
  已把该要求写入 [构建与发布](../../../../docs/build-and-release.md)。

- **`rustix::fs::lgetxattr` 不自动扩容**：`&mut Vec<u8>` 用的是 `self.len()`，
  传空 Vec 等价于「传 null/0」。因此**两步式（先问长度、再读）必须保留**；
  收益在于两步都不再有 `unsafe`。
- **`mknodat` 的错误处理语义**：rustix 用 `Errno::EXIST` 而非 `libc::EEXIST`，
  实测两者 errno 均为 17，语义一致。`create_device_node` 的 EEXIST 竞态分支已保留。
- **`chmod` 的失败处理被强化**：原实现忽略 `chmod` 返回值，现在改为向上报错。
  这是**有意的行为收紧**——节点权限收敛到 `0600` 是安全属性，静默失败会让
  loop 设备对非 root 可读写。若这导致真机回归，应重新评估而不是简单忽略。
- **新增依赖面**：三个 crate 新增 `rustix` 直接依赖。实际编译增量近零，因为它们
  本就经 `gadgetdisk-gdd`/`gadgetdisk-loop` 间接依赖它；工作区 features 加 `alloc`。
- **`StatVfs` 未实现 `Debug`**：测试里不能对它 `unwrap()`/`unwrap_err()`，需 `match`。
