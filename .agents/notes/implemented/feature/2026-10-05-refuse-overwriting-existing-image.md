# Agent Note: 创建镜像时同名不覆盖

Status: implemented

## Problem

原实现的 `create_image` 会用 `std::fs::File::create` **直接覆盖**同名文件，且既有测试
`crates/gadgetdisk-core/src/create.rs` 的 `recreating_over_existing_file_replaces_it`
正是**断言这一行为**（「重建同名文件即替换它」）。

这在 CLI 场景下或许可以辩解为「重建」，但在 WebUI 下是数据丢失：用户手滑用了已有
镜像名（例如默认值 `disk.img`），一次点击就抹掉几十 GiB 的镜像与其中全部数据，
且**没有任何提示**。镜像动辄数 GiB、创建耗时数十秒，用户很难立刻察觉被覆盖。

需求明确要求：**创建时探测镜像目录有没有同名镜像，有的话在 WebUI 阻止创建**。

## Proposal

**两层防护，后端为权威判定。**

1. **后端硬拒绝**（权威）：`create::ensure_target_free(path)` 在**任何写入之前**检查
   目标是否存在，命中即返回新增的 `CoreError::AlreadyExists`；该错误映射为协议错误码
   `already_exists`、HTTP `409`。这样经 CLI、curl 或其他任何调用方都无法绕过。
2. **WebUI 预检**（快速反馈）：`pure/task.js` 的纯函数 `imageNameExists` 比对已知镜像名，
   命中则显示提示并**禁用创建按钮**。预检不是唯一防线，只是让用户不必等一次往返。

配套决定：

- **错误码语义**：`409` 而非 `400`。同名冲突是「与当前资源状态冲突」（换个名字即可
  成功），不是「请求本身不合法」。前端据此给出「改名或先删除」的具体指引。
- **既有测试语义反转**：`recreating_over_existing_file_replaces_it` 改为
  `refuses_to_overwrite_existing_file`，且断言**原文件逐字节未被修改**——
  拒绝创建不能有任何副作用。这是本决策的核心回归保护。
- **用 `symlink_metadata` 而非 `exists()`**：前者能区分「不存在」与「无法访问」。
  其他 I/O 错误（如父目录无权限）**如实上报**，不得当成「不存在」而放行——
  那会让后续写入失败，错误信息离真正原因更远。
- **不提供 `--force` / 覆盖开关**：覆盖是破坏性操作，若要支持应由用户显式**先删除**
  （`delete` 已有占用检查与错误提示），而不是在创建路径上加一个容易误点的开关。

## Alternatives considered

**仅在 WebUI 阻止，后端保持覆盖** — 防护不完整：CLI 与 REST 直连仍可覆盖，而本模块
的 CLI 是**并列的一等入口**（见「按需进程模型」），不能被当成「内部实现细节」。

**前端询问「是否覆盖」，用户确认后后端执行覆盖** — 增加一个破坏性路径，却没有解决
「默认值 `disk.img` 容易撞名」的根本问题；且需要在协议上加确认语义。删除后重建是
同样方便、语义更清晰的路径。

**自动改名（`disk-1.img`）** — 静默改变用户指定的文件名，比覆盖更容易造成困惑：
用户以为创建的是 `disk.img`，实际拿到的是别的名字。

**返回 `400 invalid_argument`** — 会让前端无法区分「名字写法错误」与「名字被占用」，
而这两种情况的用户动作完全不同。

## Acceptance criteria

- `cargo nextest run -p gadgetdisk-core -E 'test(refuses_to_overwrite_existing_file)'`
  通过，且断言原文件内容逐字节不变。
- `refuses_even_for_zero_length_existing_file` 覆盖空文件同样算「已存在」。
- `ensure_target_free_accepts_missing_path` 覆盖未命中路径放行、命中路径报
  `AlreadyExists`。
- REST 层：`serve.rs` 的 create 预检返回 `already_exists`；`status_for` 把它映射为 409。
- WebUI：`node --test webui/tests/` 中 `imageNameExists` 的大小写不敏感用例通过；
  `index.html` 含 `create-name-conflict` 元素，`view-create.js` 把预检接在文件名输入上。
- 失败路径不残留半成品：`failure_after_creation_still_removes_half_product`。

## Risks

- **大小写比较取更严格的一侧**：`imageNameExists` 按不区分大小写比较，而底层文件系统
  可能大小写敏感（`Disk.img` 与 `disk.img` 在 Android 上通常是两个文件）。这意味着
  UI 会**多**拦下一些本可创建的请求。这是有意的取舍：镜像名大小写混淆是常见的用户
  失误，多拦一次的成本（改个名）远低于误判放行后两个近似同名镜像并存。
  后端 `ensure_target_free` 用的是文件系统真实语义，因此不区分大小写只是 UI 侧更严。
- **预检存在竞态**：列表刷新后、创建前若有其他入口创建了同名文件，预检会放行而
  后端拒绝。此时用户看到的是后端返回的 `already_exists`——两层防护的兜底正是为此。
- **`run/offsets.json` 的旧缓存**：同名镜像被删除后重建时，若旧偏移缓存残留会指向
  错误位置。`delete` 路径已调用 `Offsets::remove`，创建路径也会 `Offsets::set` 覆盖，
  故不受影响；但这条依赖值得在改动 `delete` 时留意。
