#!/usr/bin/env python3
"""构建、打包、测试、部署脚本的共用工具。

## 本文件不做任何本机假设

只使用**公认的环境变量与标准 SDK 布局**探测工具链：

| 变量 | 用途 |
|---|---|
| `ANDROID_NDK_HOME` | NDK 根目录（最高优先级） |
| `ANDROID_HOME` | Android SDK 根目录（据此推导 NDK 与 platform-tools） |
| `ADB` / `ANDROID_ADB` | adb 可执行文件 |
| `ANDROID_SERIAL` | adb 多设备时选择目标（由 adb 自身识别） |

探测**只认上述环境变量**，不读取 `local.properties`，也不提供任何链接器覆盖机制。

NDK 必须与构建宿主**同平台**：Linux 宿主使用 Linux 版 NDK，其 `prebuilt/<host>/bin/clang`
是原生可执行文件，能直接解析 rustc 传入的 Linux 路径。跨平台混用（如在 Linux 宿主上指向
Windows 版 NDK，其 clang 是调用 `clang.exe` 的包装脚本）会让链接阶段报
`no such file or directory`，本仓库不做路径形式转换来兼容这种用法。
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# ---------------------------------------------------------------- 仓库布局

#: 仓库根目录。
REPO_ROOT = Path(__file__).resolve().parents[2]

#: 模块模板目录（随版本控制；非设备端安装目录）。
MODULE_TEMPLATE_DIR = REPO_ROOT / "module_template"

#: 模板里的 `module.prop`：**版本号的缺省来源**。
#:
#: 构建（`gd-build`）与打包（`gd-package`）都可用 `--version` / `--version-code`
#: 覆盖它；两者必须给同一个值，由 `build-info.json` 守住（见下）。
MODULE_PROP_FILE = MODULE_TEMPLATE_DIR / "module.prop"

#: WebUI 源码目录（打包时复制到模块 webroot/）。
WEBUI_DIR = REPO_ROOT / "webui"

#: WebUI 的测试目录（**不**进模块包）。
WEBUI_TESTS_DIR = WEBUI_DIR / "tests"

#: 打包产物目录。
#:
#: 落在 `target/` 下而非仓库根：`target/` 本就是构建产物根且整体被忽略，
#: 产物与其来源生命周期一致，`cargo clean` 会一并清除。
OUT_DIR = REPO_ROOT / "target" / "dist"

#: 支持的目标 ABI → Rust target。
ABI_TARGETS: dict[str, str] = {
    "arm64-v8a": "aarch64-linux-android",
    "x86_64": "x86_64-linux-android",
}

#: Android API level（与 NDK clang wrapper 的 `-androidNN` 后缀一致）。
ANDROID_API = 30

#: 需要交叉编译的 Cargo 产物名 → 包内文件名。
#:
#: - `gdd` 是独立的 mass_storage 执行进程（按需启动，无状态）。它与 `gadgetdisk`
#:   必须一起发布：`gadgetdisk` 在需要导出时通过**同目录**找到 `gdd` 拉起它。
#: - `mkfs.vfat` 是本项目自带的 FAT 格式化工具（设备上通常不存在 dosfstools）。
#:   Cargo **不允许二进制名含 `.`**，故 crate 里的产物名是 `mkfsvfat`，
#:   打包时重命名为 dosfstools 惯用的 `mkfs.vfat`。
BIN_RENAMES = {
    "gadgetdisk": "gadgetdisk",
    "gdd": "gdd",
    "mkfsvfat": "mkfs.vfat",
}

#: Cargo 产物名（用于在 `target/<triple>/<profile>/` 下定位文件）。
BIN_NAMES = tuple(BIN_RENAMES)

#: 包内文件名（模块 `bin/` 下的实际名字）。
PACKAGED_BIN_NAMES = tuple(BIN_RENAMES.values())

#: 主二进制名。
BIN_NAME = "gadgetdisk"


class BuildError(RuntimeError):
    """构建与部署脚本的预期退出异常。"""


# ---------------------------------------------------------------- 版本号

#: 注入 Rust 编译的版本号环境变量（`option_env!("GD_VERSION")`）。
VERSION_ENV = "GD_VERSION"

#: 版本号（`versionCode`）环境变量。
#:
#: **不编译进二进制**：目前没有任何运行期消费者，它只出现在 `module.prop` 与
#: 构建清单里。保留该常量是为了让"版本"与"版本号"始终成对传递、一起校验。
VERSION_CODE_ENV = "GD_VERSION_CODE"

#: 构建清单文件名（落在 `OUT_DIR` 下）。
BUILD_INFO_NAME = "build-info.json"


def read_module_prop() -> dict[str, str]:
    """解析 `module_template/module.prop`，返回键值对。

    只支持 `key=value` 单行格式（KernelSU 的约定），不处理转义或多行值。
    """
    if not MODULE_PROP_FILE.is_file():
        raise BuildError(f"missing {MODULE_PROP_FILE.relative_to(REPO_ROOT)}")

    values: dict[str, str] = {}
    for raw in MODULE_PROP_FILE.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        values[key.strip()] = value.strip()

    for required in ("id", "name", "version", "versionCode"):
        if not values.get(required):
            raise BuildError(f"module.prop is missing required fields: {required}")
    return values


def validate_version_code(value: str) -> str:
    """校验 `versionCode` 是纯数字（KernelSU 要求递增整数）。"""
    if not re.fullmatch(r"\d+", value):
        raise BuildError(f"versionCode must be an integer: {value!r}")
    return value


@dataclass(frozen=True)
class Version:
    """一次构建/打包使用的版本信息。"""

    version: str
    version_code: str

    @classmethod
    def resolve(cls, version: str | None, version_code: str | None) -> Version:
        """用参数覆盖 `module.prop` 里的缺省值。

        `module.prop` 只作**缺省值**：不传参时行为与历史一致，传参时以参数为准。
        """
        prop = read_module_prop()
        resolved_version = (version or prop["version"]).strip()
        resolved_code = validate_version_code((version_code or prop["versionCode"]).strip())
        if not resolved_version:
            raise BuildError("version must not be empty")
        return cls(version=resolved_version, version_code=resolved_code)

    def rebuild_hint(self) -> str:
        """版本不一致时的可操作提示。"""
        return (
            f"Rebuild with the same version, e.g.\n"
            f"  uv run gd-build   --version {self.version} --version-code {self.version_code}\n"
            f"  uv run gd-package --version {self.version} --version-code {self.version_code}"
        )


def build_info_path() -> Path:
    """构建清单的路径。"""
    return OUT_DIR / BUILD_INFO_NAME


def record_built_abi(abi: str, version: Version) -> None:
    """在构建清单里登记「某个 ABI 是用哪一版构建的」。

    ## 为什么需要它

    `gd-build` 把版本注入二进制的编译环境，`gd-package` 把版本写进包内
    `module.prop`。两者若各传各的，就会产出**包内版本与二进制自报版本不一致**的
    ZIP——而这种不一致在设备上只能靠人肉发现（版本号写错不会让任何命令失败）。

    Android 的 ELF 无法在宿主执行，所以不能在打包时"运行二进制问它版本"；
    用清单文件记录下来是唯一可靠且可被测试覆盖的办法。

    按 ABI **合并**写入：`gd-build --abi X` 分多次调用时，后一次不会抹掉前一次的记录。
    """
    path = build_info_path()
    info = read_build_info()
    info["version"] = version.version
    info["version_code"] = version.version_code

    targets = info.get("targets")
    if not isinstance(targets, dict):
        targets = {}
    targets[abi] = {"version": version.version, "version_code": version.version_code}
    info["targets"] = targets

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(info, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def read_build_info() -> dict[str, object]:
    """读取构建清单；不存在或损坏时返回空字典（由调用方决定是否报错）。"""
    path = build_info_path()
    if not path.is_file():
        return {}
    try:
        loaded = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}
    return loaded if isinstance(loaded, dict) else {}


def check_built_version(abi: str, version: Version) -> None:
    """断言某个 ABI 的二进制确实是按 `version` 构建的。

    **fail closed**：清单缺失或该 ABI 没有记录时同样报错。宁可让人重跑一次
    `gd-build`，也不要发布一个"版本号对不上二进制"的包。
    """
    info = read_build_info()
    targets = info.get("targets")
    if not isinstance(targets, dict) or abi not in targets:
        raise BuildError(
            f"no build record for {abi} in {build_info_path().relative_to(REPO_ROOT)}.\n"
            f"The staged binaries may predate the version-injection change "
            f"(or were built by an older gd-build).\n{version.rebuild_hint()}"
        )

    recorded = targets[abi]
    if not isinstance(recorded, dict):
        raise BuildError(f"malformed build record for {abi} in build-info.json")
    same_version = recorded.get("version") == version.version
    same_code = recorded.get("version_code") == version.version_code
    if not (same_version and same_code):
        raise BuildError(
            f"{abi} was built as version {recorded.get('version')} "
            f"(versionCode {recorded.get('version_code')}), but this package is "
            f"{version.version} (versionCode {version.version_code}).\n"
            f"{version.rebuild_hint()}"
        )


def render_module_prop(version: Version) -> str:
    """把 `module.prop` 的内容渲染为发布版本。

    只替换 `version` / `versionCode` 两行，其余行（含注释与 `description`）**逐字**
    保留：`module.prop` 仍是内容的唯一来源，版本只是被参数覆盖的两个字段。
    """
    lines: list[str] = []
    for raw in MODULE_PROP_FILE.read_text(encoding="utf-8").splitlines():
        key = raw.partition("=")[0].strip() if "=" in raw else ""
        if key == "version":
            lines.append(f"version={version.version}")
        elif key == "versionCode":
            lines.append(f"versionCode={version.version_code}")
        else:
            lines.append(raw)
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------- NDK 探测


@dataclass(frozen=True)
class Ndk:
    """已探测到的 NDK 布局。"""

    root: Path
    prebuilt: Path

    @property
    def bin_dir(self) -> Path:
        return self.prebuilt / "bin"

    def clang(self, target: str) -> Path:
        """返回 `<target><API>-clang` 编译器包装脚本的路径。"""
        arch = target.replace("-linux-android", "")
        return self.bin_dir / f"{arch}-linux-android{ANDROID_API}-clang"

    def builtins_archive(self, target: str) -> Path | None:
        """返回静态链接所需的 `libclang_rt.builtins-*.a` 归档文件路径。

        x86_64 静态链接需此静态库归档以解决 `__cpu_model` 符号缺失问题；arm64 则不需要。
        """
        arch = target.replace("-linux-android", "")
        pattern = f"libclang_rt.builtins-{arch}-android.a"
        for candidate in sorted((self.prebuilt / "lib" / "clang").glob(f"*/lib/linux/{pattern}")):
            if candidate.is_file():
                return candidate
        return None


def _first_dir(*values: str | None) -> Path | None:
    """返回第一个存在且是目录的路径。"""
    for value in values:
        if value:
            candidate = Path(value).expanduser()
            if candidate.is_dir():
                return candidate
    return None


def _ndk_from_env() -> Path | None:
    return _first_dir(os.environ.get("ANDROID_NDK_HOME"))


def _sdk_from_env() -> Path | None:
    return _first_dir(os.environ.get("ANDROID_HOME"))


def _newest_dir(parent: Path) -> Path | None:
    """返回指定目录下按字典序排序最新（版本最高）的子目录。"""
    if not parent.is_dir():
        return None
    versions = sorted((p for p in parent.iterdir() if p.is_dir()), key=lambda p: p.name)
    return versions[-1] if versions else None


def _ndk_candidate_roots() -> list[Path]:
    """按标准布局推导 NDK 候选位置。

    **不含任何本机路径**：候选来源只有 `ANDROID_HOME` 这一个公认入口。
    """
    candidates: list[Path] = []
    sdk = _sdk_from_env()
    if sdk is not None:
        # SDK 根目录本身可能就是 NDK 根（少数打包方式如此）。
        candidates.append(sdk)
        candidates.append(sdk / "ndk")
    return candidates


def find_ndk() -> Ndk:
    """按顺序探测 NDK，失败时列出已尝试的路径。

    探测顺序：

    1. `ANDROID_NDK_HOME`；
    2. `ANDROID_HOME` 下的 `ndk/`（或 `ANDROID_HOME` 本身即 NDK 根）。

    绝不静默使用猜测值：全部失败时列出已尝试路径并报错。
    """
    attempted: list[str] = []

    direct = _ndk_from_env()
    if direct is not None:
        return _finish_ndk(direct, attempted)
    attempted.append("env var ANDROID_NDK_HOME (unset or not a directory)")

    for base in _ndk_candidate_roots():
        attempted.append(str(base))
        if not base.is_dir():
            continue
        # base 本身可能就是某个具体版本目录。
        if (base / "toolchains").is_dir():
            return _finish_ndk(base, attempted)
        version = _newest_dir(base)
        if version is not None and (version / "toolchains").is_dir():
            return _finish_ndk(version, attempted)

    raise BuildError(
        "No usable Android NDK found. Tried the following locations:\n"
        + "\n".join(f"  - {item}" for item in attempted)
        + "\n\nSet ANDROID_NDK_HOME, or set ANDROID_HOME and install ndk/ under it.\n"
        "Note: The NDK must match the build host platform (e.g., use a Linux NDK on a Linux host)."
    )


def _finish_ndk(root: Path, attempted: list[str]) -> Ndk:
    """校验并补全 NDK 的 prebuilt host 目录。

    动态扫描包含 clang wrapper 的 prebuilt 目录，兼容不同宿主平台命名。
    """
    prebuilt_root = root / "toolchains" / "llvm" / "prebuilt"
    if not prebuilt_root.is_dir():
        raise BuildError(f"NDK is missing the toolchains/llvm/prebuilt directory: {root}")

    available = sorted(p for p in prebuilt_root.iterdir() if p.is_dir())
    for prebuilt in available:
        # clang wrapper 必须存在，且必须覆盖我们要构建的目标之一。
        if any(prebuilt.glob("bin/*-linux-android*-clang")):
            return Ndk(root=root, prebuilt=prebuilt)
        attempted.append(f"{prebuilt} (directory exists but has no clang wrapper)")

    raise BuildError(
        f"No prebuilt directory containing a clang wrapper found under {prebuilt_root}.\n"
        f"Available directories: {[p.name for p in available]}\n"
        f"Expected to find bin/<arch>-linux-android{ANDROID_API}-clang in one of them."
    )


# ---------------------------------------------------------------- adb 探测


def _adb_from_env() -> str | None:
    for key in ("ADB", "ANDROID_ADB"):
        value = os.environ.get(key)
        if value and Path(value).is_file():
            return value
    return None


def find_adb() -> str:
    """按顺序探测 adb：环境变量 → PATH → `$ANDROID_HOME/platform-tools`。

    **不含任何本机路径**。多设备时由 adb 自身的 `ANDROID_SERIAL` 机制选择。
    """
    direct = _adb_from_env()
    if direct:
        return direct

    found = shutil.which("adb")
    if found:
        return found

    for sdk in (_sdk_from_env(),):
        if sdk is None:
            continue
        candidate = sdk / "platform-tools" / "adb"
        if candidate.is_file():
            return str(candidate)

    raise BuildError(
        "The `adb` executable was not found. Checked: environment variables ADB/ANDROID_ADB, "
        "system PATH, and $ANDROID_HOME/platform-tools/adb.\n"
        "Please install platform-tools or set ANDROID_HOME."
    )


# ---------------------------------------------------------------- 命令执行


def cargo_env() -> dict[str, str]:
    """返回 cargo 命令使用的环境变量副本。

    使用用户默认的 `CARGO_HOME`（`~/.cargo`）及其既有缓存，**不做**任何仓库内改写：
    依赖缓存与已安装的 cargo 子命令（如 cargo-nextest）在多个检出之间共享。
    """
    env = os.environ.copy()
    env.setdefault("CARGO_TERM_COLOR", "always")
    return env


def run(
    command: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: Path | None = None,
    check: bool = True,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    """执行外部命令，非 0 退出时抛出 BuildError。"""
    printable = " ".join(command)
    print(f"+ {printable}", flush=True)

    completed = subprocess.run(
        command,
        cwd=str(cwd or REPO_ROOT),
        env=env,
        text=True,
        capture_output=capture,
        check=False,
    )
    if check and completed.returncode != 0:
        detail = ""
        if capture and completed.stderr:
            detail = f"\n--- stderr ---\n{completed.stderr.strip()}"
        raise BuildError(f"Command failed (exit code {completed.returncode}): {printable}{detail}")
    return completed


def quit_if_missing(tool: str, hint: str) -> None:
    """工具缺失时给出明确指引后退出。"""
    if shutil.which(tool) is None:
        print(f"error: `{tool}` not found.\n{hint}", file=sys.stderr)
        raise SystemExit(1)
