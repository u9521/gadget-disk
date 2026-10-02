# Agent Note: 清理整数截断转换、死代码与未使用依赖

Status: implemented

## Problem

`Cargo.toml` 与 `docs/build-and-release.md` 长期把
`clippy::cast_possible_truncation` / `cast_sign_loss` 列为「暂不启用但有价值的规则」，
理由是「镜像布局与容量计算里的整数截断是真实风险来源，值得逐个复核」。这条待办一直
悬着，副作用是：

1. **风险来源没有被真正复核过**。文档写了「~30 处」，但没人知道哪些是真实隐患、
   哪些只是 clippy 看不穿常量。
2. **数字会漂移**。实际清点是 **36 处**（28 截断 + 8 符号丢失），与文档的「~30」对不上。
3. **同类问题还有未清理的角落**。顺带排查发现两处**未使用的依赖声明**与两个**死方法**，
   都是历史演进留下的、无人察觉的残留。

复核后最重要的发现是：**这 36 处没有一处是真实隐患**。它们全部属于「类型上可证安全，
但 clippy 看不见」——例如 `const SECTOR_BYTES: u64 = 512` 被 `as usize`、
`size_of::<T>() as socklen_t`（C 结构体大小必然远小于 `socklen_t` 上限）、
以及测试里拿已知落在目标范围内的常量做断言。也就是说，这条待办的**价值不在
「消除隐患」而在「把真实风险纳入门禁」**：清完之后把它设为阻塞，日后新的整数转换
就必须显式处理，而不是靠 `as` 静默截断。

## Proposal

四步清理，全部为**行为等价**改写，不改任何运行时语义：

1. **`SECTOR_BYTES as usize`（9 处，`partitions.rs`）**：新增
   `pub const SECTOR_BYTES_USIZE: usize = 512;`，一处常量消除全部 9 处转换。
   **关键**：必须写字面量 `512`，不能写 `SECTOR_BYTES as usize` —— 后者只是把告警
   换个位置，在 32 位目标上照样触发（实测踩到过）。
2. **`size_of::<T>() as socklen_t` / `AF_UNIX as sa_family_t`**：改用
   `try_from(...).expect("... 远小于 socklen_t 上限")`。
3. **已判非负后的 `as usize` / `as u32`**：改用 `usize::try_from(n).expect("已检查 >= 0")`。
4. **比较运算中的截断**：提升到更宽类型再比较，例如
   `volume.total_clusters >= MIN_FAT32_CLUSTERS as u32` →
   `u64::from(volume.total_clusters) >= MIN_FAT32_CLUSTERS`。

清零后把两条 lint 设为 **`deny`**，并从「刻意暂不启用」表移入策略表。

**顺带清理**（同类残留，一并处理）：
- 删除 `RealConfigFs::default_root` 与 `RealConfigFs::verify_configfs` —— 两者**零调用点**；
  后者还是 `paths::verify_configfs` 的重复实现（同样的 statfs 逻辑，而
  `ensure_configfs` 实际调用的是 `paths::` 那个）。
- 删除两个**已声明但源码零引用**的依赖：`anyhow`（cli）、`serde`（gdd）。

## Alternatives considered

- **保留 `as` 并给每处加 `#[allow(..., reason = "...")]`**：否决。本仓库**全仓零 allow**，
  引入 36 条豁免会开一个坏头，而且 `#[allow]` 是「关掉检查」，
  `try_from(...).expect(...)` 是「把不可能失败写成可执行断言」——后者在 review 时
  能直接看到理由，前者只能看到一个理由字符串。同理，`docs/build-and-release.md` 里
  已把这一写法固化为约定。
- **只清理生产代码、放过测试代码里的转换**：否决。测试里同样存在真实风险的可能
  （断言写错类型会掩盖回归），而且区分两者会让 lint 无法统一设为阻塞，
  等于把待办继续挂着。
- **把 `clippy::pedantic` 一并转阻塞**：否决。实测 587 条，绝大多数与正确性无关
  （缺 `# Errors` 文档 137 条、建议 `#[must_use]` 127 条），一次性阻塞会让任何改动
  都落不了地。保留 `gd-check --pedantic` 作为非阻塞的渐进改善工具。
- **保留 `RealConfigFs::verify_configfs` 作为「更面向对象」的备选 API**：否决。
  它没有任何调用点，且与 `paths::` 版本逻辑重复；两份实现并存意味着日后修一处忘一处
  （这里恰好是安全关键路径：判断 `/config` 真的是 configfs，判错会写坏普通文件系统）。
- **用 `#[allow(unused)]` 保留死方法**：否决。死代码就是死代码，删掉最清晰；
  真需要时可从版本历史取回。

## Acceptance criteria

- `cargo clippy --workspace --all-targets -- -W clippy::cast_possible_truncation -W clippy::cast_sign_loss`
  输出 **0 行**（此前 36 行）。
- `cargo clippy --workspace --all-targets -- -D warnings` **0 告警**。
- `cargo test --workspace -- --test-threads=1` **440 个测试全通过**，exit 0 ——
  这是「所有转换改写均行为等价」的回归证据。
- `cargo check --workspace --all-targets` 在删除死方法与未使用依赖后通过。
- `cargo tree -p gadgetdisk-cli` 不含 `anyhow`；`cargo tree -p gadgetdisk-gdd -i serde`
  只剩经 `gadgetdisk-proto` / `gadgetdisk-usb` 的**传递**路径，无直接声明。
- `uv run gd-check` 六项全绿；治理门禁通过。
- **反证**：临时把一处 `SECTOR_BYTES_USIZE` 改回 `SECTOR_BYTES as usize` 后，
  `cargo clippy` 必须报 `error: casting u64 to usize may truncate the value` 并编译失败；
  恢复后归零。用于确认新 `deny` 真的生效。

## Risks

- **`SECTOR_BYTES` 与 `SECTOR_BYTES_USIZE` 形成两处可漂移的常量**。缓解：紧邻定义并
  在注释中写明二者必须一致；512 字节扇区是
  [镜像格式](../../../../docs/disk-image-format.md) 记录的既定事实，实际不会变。
  这是「消除 9 处转换」与「单一常量」之间的取舍，选择了前者。
- **新 `deny` 会增加日后写法成本**：任何新的整数转换都必须显式处理。这是把
  「真实风险来源」纳入强制门禁的**预期代价**，与此前把
  `undocumented_unsafe_blocks` 转 `deny` 是同一逻辑。
- **`try_from(...).expect(...)` 在理论上会 panic**，而 `as` 不会。但这正是意图：
  若某处假设被推翻，应当**立即失败**而不是静默截断出一个错误的镜像布局。
  所有 `expect` 的 message 都写明了「为什么不可能失败」。
- **`gadgetdisk-gdd` 的 `logging` 测试存在既有的并行竞争缺陷**（共用进程级
  `static SINK`，临时目录按 PID 命名）。本次修改了 `logging.rs` 的转换写法，
  已确认**不改变其测试语义**；但判定回归时仍应以 `--test-threads=1` 的串行结果为准。
  该缺陷已记入 [路线图已知缺陷](../../../../docs/roadmap.md)。
- **死代码扫描方法**：本次用「定义行 vs 调用点」逐个核对，**不是**启发式扫描。
  启发式扫描会把「仅在本文件内使用」的正常项（如 `constant_time_eq`、`gdd_healthy`、
  `check_available_space`）大量误报为死代码——已确认这些在同文件内都有真实调用。
