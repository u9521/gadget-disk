#!/usr/bin/env python3
"""组装 KernelSU/APatch 模块 ZIP。

契约见 docs/build-and-release.md「模块包结构」与「module.prop 字段」。

构建规范参考 docs/build-and-release.md：
- 版本号以 `module_template/module.prop` 为**缺省值**，可用 `--version` /
  `--version-code` 覆盖；
- 覆盖时二进制必须是用同一版本构建的——由 `target/dist/build-info.json` 校验；
- 二进制产物统一赋予 0755 权限，固定 ZIP 文件修改时间以实现二进制可复现构建；
- 排除未完成的 ABI、测试文件与运行期临时状态。
"""

from __future__ import annotations

import argparse
import sys
import zipfile
from pathlib import Path

from scripts.lib.common import (
    ABI_TARGETS,
    MODULE_TEMPLATE_DIR,
    OUT_DIR,
    PACKAGED_BIN_NAMES,
    REPO_ROOT,
    WEBUI_DIR,
    WEBUI_TESTS_DIR,
    BuildError,
    Version,
    check_built_version,
    read_module_prop,
    render_module_prop,
)

#: 交叉编译产物落点。
BIN_DIR = OUT_DIR / "bin"

#: 必须存在的模块文件（在模板目录下）。缺任何一个都说明仓库或构建不完整。
REQUIRED_FILES = (
    "module.prop",
    "customize.sh",
    "service.sh",
    "uninstall.sh",
)

#: 必须存在的 WebUI 文件（零构建：只有静态资源，无打包器产物）。
#:
#: 前端已按职责拆分为多个原生 ES 模块（无打包器依赖）。此处显式约束
#: **核心入口与基础支撑模块**——避免因核心文件缺失导致 WebUI 运行时加载失败。
REQUIRED_WEBUI = (
    "index.html",
    "style.css",
    "ksu.js",
    "main.js",
    "backend.js",
    "dom.js",
    "task.js",
)

#: WebUI 的**子目录**（相对 `webui/`），其内容平铺复制进 `webroot/<子目录>/`。
#:
#: 前端划分为「纯函数层」与「视图层」。视图模块位于根目录（由 `webui_sources`
#: 遍历直接获取），纯函数层位于 `pure/` 目录下。因顶层扫描非递归，此处须**显式登记**，
#: 否则该子目录不会被打包，导致设备端加载时发生模块缺失错误
#: （开发测试环境直接引用源码目录，无法察觉该问题）。
WEBUI_SUBDIRS = ("pure",)

#: `serve` 在运行期写入 `webroot/` 的文件名（端口 + token）。**不得进包**。
API_JSON_NAME = "api.json"

#: 安装脚本需要可执行位的文件。
EXECUTABLE_SCRIPTS = (
    "customize.sh",
    "service.sh",
    "uninstall.sh",
)


def collect_abis() -> list[str]:
    """列出 `target/dist/bin/` 下已构建且所需二进制（gadgetdisk, gdd, mkfs.vfat）均齐备的 ABI。"""
    return [
        abi
        for abi in sorted(ABI_TARGETS)
        if all((BIN_DIR / abi / name).is_file() for name in PACKAGED_BIN_NAMES)
    ]


def webui_sources() -> list[tuple[Path, str]]:
    """返回要打进包里的 WebUI 文件，以及各自的**包内相对路径**。

    返回 `(源文件, 包内相对路径)`：顶层文件保持原名，子目录文件带上前缀
    （如 `pure/bytes.js`）。必须保留相对路径而非仅文件名，以确保 `pure/` 下的文件
    正确输出至 `webroot/pure/` 目录；若扁平化放置于 `webroot/` 根目录，会导致前端的
    相对导入路径解析失败。
    """
    missing = [name for name in REQUIRED_WEBUI if not (WEBUI_DIR / name).is_file()]
    if missing:
        raise BuildError(f"{WEBUI_DIR.name}/ is missing required files: " + ", ".join(missing))

    sources: list[tuple[Path, str]] = [
        (path, path.name)
        for path in sorted(WEBUI_DIR.iterdir())
        if path.is_file()
        and path != WEBUI_DIR / API_JSON_NAME
        # `tests/` 是 WebUI 自己的测试，不是模块内容。
        and WEBUI_TESTS_DIR not in path.parents
    ]

    # 子目录（纯函数层）显式登记：顶层扫描不递归，漏登记会让模块在设备上缺失。
    for name in WEBUI_SUBDIRS:
        subdir = WEBUI_DIR / name
        if not subdir.is_dir():
            raise BuildError(f"{WEBUI_DIR.name}/ is missing the subdirectory: {name}/")
        files = sorted(p for p in subdir.iterdir() if p.is_file())
        if not files:
            raise BuildError(f"{WEBUI_DIR.name}/{name}/ is empty")
        sources.extend((path, f"{name}/{path.name}") for path in files)

    # 排除运行期生成的 api.json，避免打入失效端口与凭据。
    stale = WEBUI_DIR / API_JSON_NAME
    if stale.exists():
        stale.unlink()
        print(f"Removed runtime artifact: {WEBUI_DIR.name}/{API_JSON_NAME}")

    return sources


def build_zip(abis: list[str], output: Path, version: Version) -> None:
    """写出模块 ZIP。"""
    output.parent.mkdir(parents=True, exist_ok=True)

    # 固定时间戳：让相同输入的 ZIP 逐字节可复现（便于校验产物来源）。
    fixed_time = (1980, 1, 1, 0, 0, 0)

    def add_file(zf: zipfile.ZipFile, source: Path, arcname: str, mode: int) -> None:
        info = zipfile.ZipInfo(arcname, date_time=fixed_time)
        # ZIP 的外部属性高 16 位是 Unix 权限位，低 16 位保留。
        info.external_attr = (mode & 0xFFFF) << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        zf.writestr(info, source.read_bytes())

    # `module.prop` 的版本行按本次解析结果**在内存中**替换：模板文件本身保持
    # 缺省值不动（它是缺省来源，不是产物）。
    module_prop_bytes = render_module_prop(version).encode("utf-8")

    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as zf:
        # 模块根文件。
        for name in REQUIRED_FILES:
            source = MODULE_TEMPLATE_DIR / name
            if not source.is_file():
                raise BuildError(f"missing module file: {source.relative_to(REPO_ROOT)}")
            mode = 0o755 if name in EXECUTABLE_SCRIPTS else 0o644
            if name == "module.prop":
                info = zipfile.ZipInfo(name, date_time=fixed_time)
                info.external_attr = (mode & 0xFFFF) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                zf.writestr(info, module_prop_bytes)
                continue
            add_file(zf, source, name, mode)

        # WebUI：**不**改权限（安装器负责上下文与权限）。
        # 用打包脚本给出的包内相对路径，保留 `pure/` 这类子目录结构——
        # 拍平会让前端的相对 import 在设备上解析失败。
        for source, relative in webui_sources():
            add_file(zf, source, f"webroot/{relative}", 0o644)

        # 各 ABI 的二进制。
        for abi in abis:
            for name in PACKAGED_BIN_NAMES:
                source = BIN_DIR / abi / name
                if not source.is_file():
                    raise BuildError(
                        f"missing binary: {source.relative_to(REPO_ROOT)}"
                        f"(run `uv run gd-build --abi {abi}` first)"
                    )
                add_file(zf, source, f"bin/{abi}/{name}", 0o755)


def verify_zip(output: Path, abis: list[str], version: Version) -> None:
    """重新打开 ZIP 校验结构，避免只「写成功」就当交付合格。"""
    with zipfile.ZipFile(output) as zf:
        names = set(zf.namelist())

        expected = set(REQUIRED_FILES)
        expected |= {f"webroot/{name}" for name in REQUIRED_WEBUI}
        expected |= {f"bin/{abi}/{name}" for abi in abis for name in PACKAGED_BIN_NAMES}

        # 子目录（纯函数层）里的每个文件都必须在包里。这类文件**不在顶层扫描
        # 范围内**，漏掉的症状只在设备上出现（模块解析失败），因此显式校验。
        for name in WEBUI_SUBDIRS:
            for path in sorted((WEBUI_DIR / name).iterdir()):
                if path.is_file():
                    expected.add(f"webroot/{name}/{path.name}")

        missing = expected - names
        if missing:
            raise BuildError("ZIP is missing entries: " + ", ".join(sorted(missing)))

        for name in names:
            # 项目前提是纯 WebUI，无配套 APK。
            if name.endswith(".apk"):
                raise BuildError(f"the module package must not contain an APK: {name}")
            # 运行期产物，内含短时效的 token 与端口。
            if name == f"webroot/{API_JSON_NAME}":
                raise BuildError(
                    "the module package must not contain the runtime artifact: "
                    f"webroot/{API_JSON_NAME}"
                )
            # WebUI 的测试属于仓库，不属于模块。
            if name.startswith("webroot/tests/"):
                raise BuildError(f"the module package must not contain WebUI tests: {name}")

        # 权限位必须写对：二进制与脚本可执行，其余不可执行。
        for abi in abis:
            for name in PACKAGED_BIN_NAMES:
                info = zf.getinfo(f"bin/{abi}/{name}")
                if (info.external_attr >> 16) & 0o777 != 0o755:
                    raise BuildError(f"bin/{abi}/{name} does not have mode 0755")
        for name in EXECUTABLE_SCRIPTS:
            info = zf.getinfo(name)
            if (info.external_attr >> 16) & 0o777 != 0o755:
                raise BuildError(f"{name} does not have mode 0755")

        # 包内 `module.prop` 的版本必须**就是**本次发布的版本，而不是模板里的
        # 缺省值——打包脚本替换了这两行，这里确认替换真的生效。
        prop_text = zf.read("module.prop").decode("utf-8")
        fields: dict[str, str] = {}
        for raw in prop_text.splitlines():
            key, sep, value = raw.partition("=")
            if sep:
                fields[key.strip()] = value.strip()
        if fields.get("version") != version.version:
            raise BuildError(
                f"module.prop in the ZIP reports version {fields.get('version')!r}, "
                f"expected {version.version!r}"
            )
        if fields.get("versionCode") != version.version_code:
            raise BuildError(
                f"module.prop in the ZIP reports versionCode {fields.get('versionCode')!r}, "
                f"expected {version.version_code!r}"
            )


def main(argv: list[str] | None = None) -> int:
    """命令行入口。"""
    parser = argparse.ArgumentParser(
        description="Assemble the KernelSU/APatch/Magisk module ZIP package for GadgetDisk"
    )
    parser.add_argument(
        "--abi",
        action="append",
        choices=sorted(ABI_TARGETS),
        help="Package only the specified ABI (repeatable; "
        "defaults to all ABIs present in target/dist/bin/)",
    )
    parser.add_argument(
        "--out",
        type=Path,
        help="Custom output ZIP path (defaults to target/dist/<name>-<version>.zip)",
    )
    parser.add_argument(
        "--version",
        help="Version string written into module.prop (defaults to module.prop)",
    )
    parser.add_argument(
        "--version-code",
        help="Integer versionCode written into module.prop (defaults to module.prop)",
    )
    args = parser.parse_args(argv)

    try:
        prop = read_module_prop()
        version = Version.resolve(args.version, args.version_code)

        abis = args.abi or collect_abis()
        if not abis:
            raise BuildError("no built binaries under target/dist/bin. Run `uv run gd-build` first")

        # 显式指定的 ABI 必须已完成构建（所有必需二进制均齐备），否则会导致打包缺件。
        for abi in abis:
            for name in PACKAGED_BIN_NAMES:
                binary = BIN_DIR / abi / name
                if not binary.is_file():
                    raise BuildError(
                        f"{binary.relative_to(REPO_ROOT)} not found; "
                        f"run `uv run gd-build --abi {abi}` first"
                    )
            # **版本一致性守卫**：二进制是"用哪个版本构建的"无法从 ELF 读出
            # （Android 目标不能在宿主执行），只能核对构建时写下的清单。
            # 不一致就拒绝打包，而不是发布一个自报版本与包内版本不同的模块。
            check_built_version(abi, version)

        output = args.out or OUT_DIR / f"{prop['name']}-{version.version}.zip"
        build_zip(abis, output, version)
        verify_zip(output, abis, version)

    except BuildError as exc:
        print(f"\nPackaging failed:\n{exc}", file=sys.stderr)
        return 1

    size = output.stat().st_size
    print(f"\nDone: {output.relative_to(REPO_ROOT)} ({size:,} bytes)")
    print(f"  version: {version.version} (versionCode {version.version_code})")
    print(f"  ABI    : {', '.join(abis)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
