# Agent Note: 二进制版本号注入与构建清单守卫

Status: implemented

## Problem

`module.prop` 的 `version` / `versionCode` 是**发布产物**的字段，而三个二进制
（`gadgetdisk`、`gdd`、`mkfs.vfat`）自己也有版本号。此前两者互不相干：

- 二进制用 clap 的 `#[command(version)]`，即 `CARGO_PKG_VERSION`（工作区的 `0.1.0`）；
- ZIP 里的 `module.prop` 由 `scripts/package/cli.py` 从 `module_template/module.prop` 读。

于是"设置发布版本号"这件事无处可做——改 `Cargo.toml` 会影响所有 crate，改
`module.prop` 又不进二进制。用户提出的 TODO（`module.prop` 里的
`version=0.1.0 todo 根据package脚本设置`）正是这个缺口。

同时暴露了一个更隐蔽的问题：**没有任何机制保证两者一致**。若构建时用了版本 A、
打包时写了版本 B，产物就是"包内版本与二进制自报版本不同"的 ZIP——而这种不一致
在设备上只能靠人肉发现（版本号写错不会让任何命令失败）。

约束：Android 目标的 ELF **不能在宿主机执行**，所以打包时无法"运行二进制问它版本"。

## Proposal

### 1. 版本号经环境变量注入，`module.prop` 只作缺省值

```rust
// crates/gadgetdisk-proto/src/lib.rs（cli 与 gdd 共用）
pub const VERSION: &str = match option_env!("GD_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
```

- `#[command(version = gadgetdisk_proto::VERSION)]` 让 `gadgetdisk --version` 与
  `gdd --version` 都输出注入值；
- `gadgetdisk_mkfsvfat::VERSION` 是**同一份表达式**（手写解析器新增 `-V`/`--version`）。
  不直接复用 proto 的常量，是为了不让 `serde`/`thiserror` 被拖进一个只做 FAT 格式化的
  工具；两者的一致性由验收步骤核对（同一次构建后三个二进制必须报同一版本）。
- **放在 `gadgetdisk-proto`**：两个 `main.rs` 都已经依赖它，比各自写一遍表达式更难写错。

`option_env!` 会被 cargo 记录为**环境依赖**：改 `GD_VERSION` 必然触发重编。
实测确认：默认 `0.1.0` → `GD_VERSION=2.3.4` 出 `2.3.4` → 不设环境变量又回 `0.1.0`。
这是选它而不是 `env!(concat!(env!(..)))` 之类技巧的关键理由——**不会留下
"版本号改了但二进制是旧的"的产物**。

`match option_env!(..) { Some(v) => v, None => env!(..) }` 的 const 求值在 1.85 及
以上均合法（已在 `1.85.1` 与 stable 上实测）。本仓库当前 MSRV 见根 `Cargo.toml`。

### 2. `gd-build` / `gd-package` 各自接受 `--version` / `--version-code`

两者都缺省读 `module_template/module.prop`（保持"缺省值单一来源"），传参则覆盖。
`versionCode` 仍强制纯数字（KernelSU 要求递增整数）。

### 3. `target/dist/build-info.json` 作为一致性守卫

`gd-build` 编译每个 ABI 时记录：

```json
{
  "version": "1.2.3",
  "version_code": "7",
  "targets": { "arm64-v8a": {"version": "1.2.3", "version_code": "7"}, ... }
}
```

`gd-package` 对**每个被打包的 ABI** 调 `check_built_version()`：清单缺失、该 ABI 无记录、
或版本/版本号任一不符 → **拒绝打包**，并给出"用同一版本重跑两条命令"的提示。

**fail closed**（缺失也报错）：宁可让人重跑一次构建，也不发布一个版本对不上的包。

打包时只替换 ZIP 内 `module.prop` 的 `version` / `versionCode` 两行（其余行含
`description` 逐字保留），并在 `verify_zip` 里重新读回确认替换生效。仓库里的模板文件
**保持缺省值不变**——它是缺省来源，不是产物。

`record_built_abi` 按 ABI **合并**写入：分多次 `--abi` 构建时后一次不会抹掉前一次的记录。

## Alternatives considered

**A. 不做守卫，只在文档里写"两处必须传同一版本"。**
成本最低，但传错只能靠人发现，且后果（版本号错）不会让任何测试变红。

**B. 打包时在 ELF 里搜版本字符串。**
不需要新文件，但依赖版本串恰好以原文出现在 `.rodata` 的某个位置；优化或内联
（版本串只被 clap 的静态表引用）一旦改变布局就会误判。相比"由构建方写下事实"，
这是脆弱的间接推断。

**C. 把版本号写进一个 Rust 源文件（生成代码）。**
`build.rs` 写 `OUT_DIR` 下的 `.rs` 是常见做法，但它需要为三个 crate 各配
`build.rs` 与重跑条件（`cargo:rerun-if-env-changed`），比一个常量表达式复杂，
且把"注入了吗"变成隐式行为。

**D. 让 `--version` 只影响 `module.prop`，二进制继续用 `CARGO_PKG_VERSION`。**
那样 `gadgetdisk --version` 永远报 `0.1.0`，用户排查时拿到的是错信息——
"版本号"这个概念会分裂成两个互不相干的值。

**E. 把 `<version>` 写进 `Cargo.toml`（`cargo set-version` 风格）。**
会改动受版本控制的 `Cargo.toml` 与 `Cargo.lock`，且"改版本"成为一次代码提交——
发布流程不该产生工作区变更。环境变量注入不落盘，更适合"同一次提交、多个版本"。

## Acceptance criteria

- `GD_VERSION=9.9.9 cargo run -p gadgetdisk-cli -- --version` → `gadgetdisk 9.9.9`；
  `-p gadgetdisk-gdd` → `gdd 9.9.9`；`-p gadgetdisk-mkfsvfat -- -V` → `mkfs.vfat 9.9.9`；
  不设环境变量时三者都回 `0.1.0`。
- `cargo nextest run -p gadgetdisk-mkfsvfat`：`version_is_reported_as_an_early_exit_with_the_injected_version`
  断言 `-V`/`--version` 都回显 `crate::VERSION`；
  `usage_errors_are_not_mistaken_for_early_exits` 断言"以 Usage 开头"这条文本前缀判据
  已被 `CliOutcome` 取代（帮助/版本/错误三态）。
- `uv run gd-build --version 1.2.3 --version-code 7`：`target/dist/build-info.json` 记录
  两个 ABI 均为 `1.2.3`/`7`，且 `grep -a 1.2.3` 在三个 Android 二进制里都能命中。
- `uv run gd-package --version 1.2.3 --version-code 7` 成功，ZIP 内 `module.prop` 的
  `version=1.2.3` / `versionCode=7`；改成 `uv run gd-package`（缺省 `0.1.0`）**必须失败**
  并提示重跑构建。
- 删除 `target/dist/build-info.json` 后 `gd-package` 必须失败（fail closed）。
- 真机：`bin/gadgetdisk --version`、`bin/gdd --version`、`bin/mkfs.vfat -V` 与模块详情页
  显示的版本号三者一致（[测试规范](../../../../docs/testing.md) 真机验收第 11 项）。

## Risks

- **清单只记录"构建时的意图"**。若有人手动替换 `target/dist/bin/<abi>/` 下的二进制，
  清单不会跟着变，守卫就失效了（它防的是"两处传了不同版本"，不防"人为换文件"）。
  真机验收第 11 项对此兜底。
- **`versionCode` 不编译进二进制**：目前没有运行期消费者，它只出现在 `module.prop` 与
  构建清单里。若将来有运行期需要（例如自升级判断），需要另加常量。
- **`mkfs.vfat -V` 是 dosfstools 没有的选项**。脚本若假设它与 `mkfs.fat` 完全兼容会意外；
  已记入 [磁盘镜像格式](../../../../docs/disk-image-format.md) 的"已知差异"表。
- **`scripts/deploy/cli.py` 的手工解包路径不执行 `customize.sh`**（它自行扁平化
  `bin/<abi>/`）。版本来自 ZIP 内 `module.prop`，与该路径不冲突；但该路径也不会
  校验二进制自报版本。属既有行为的边界，未扩大。
