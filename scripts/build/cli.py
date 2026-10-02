#!/usr/bin/env python3
"""交叉编译各 ABI 的 `gadgetdisk`、`gdd` 与 `mkfs.vfat` 二进制。

构建规格参考 docs/build-and-release.md：

- NDK 路径按公认环境变量探测（`ANDROID_NDK_HOME` / `ANDROID_HOME`，见 `scripts/lib/common.py`）；
- `RUSTFLAGS="-C target-feature=+crt-static"` 静态链接；
- `x86_64-linux-android` 额外需要 `libclang_rt.builtins-x86_64-android.a`。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from scripts.lib.common import (
    ABI_TARGETS,
    BIN_NAMES,
    BIN_RENAMES,
    OUT_DIR,
    REPO_ROOT,
    VERSION_ENV,
    BuildError,
    Ndk,
    Version,
    cargo_env,
    find_ndk,
    record_built_abi,
    run,
)


def rustflags_for(target: str, ndk: Ndk) -> str:
    """构造 `RUSTFLAGS`。

    静态链接避免依赖设备上的 libc 版本；x86_64 需要补 builtins archive。
    """
    flags = ["-C", "target-feature=+crt-static"]

    if target == "x86_64-linux-android":
        archive = ndk.builtins_archive(target)
        if archive is None:
            raise BuildError(
                "libclang_rt.builtins-x86_64-android.a not found.\n"
                "This archive is required for x86_64 static linking; without it you get\n"
                "  undefined symbol: __cpu_model\n"
                "Check that the NDK is complete (expected under "
                "<ndk>/toolchains/llvm/prebuilt/<host>/lib/clang/<ver>/lib/linux/)."
            )
        flags += ["-C", f"link-arg={archive}"]

    return " ".join(flags)


def build_target(target: str, abi: str, ndk: Ndk, *, release: bool, version: Version) -> list[Path]:
    """编译单个 ABI，返回该 ABI 的所有构建产物二进制路径（gadgetdisk, gdd, mkfsvfat）。"""
    linker = ndk.clang(target)
    if not linker.is_file():
        raise BuildError(
            f"the NDK has no clang wrapper for {target}: {linker}\n"
            "Check that the NDK ships the toolchain for that ABI."
        )

    env = cargo_env()
    # 环境变量名中的 `-` 需按下划线形式书写。
    env[f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER"] = str(linker)
    # 同时导出 CC_<target>，便于依赖的构建脚本（如 cc crate）使用同一编译器。
    env[f"CC_{target.replace('-', '_')}"] = str(linker)
    if target.endswith("android"):
        env[f"AR_{target.replace('-', '_')}"] = str(ndk.bin_dir / "llvm-ar")

    env["RUSTFLAGS"] = rustflags_for(target, ndk)

    # **版本号注入**：三个二进制都读 `option_env!("GD_VERSION")`
    # （`gadgetdisk-proto::VERSION` / `gadgetdisk_mkfsvfat::VERSION`）。
    #
    # `option_env!` 会被 cargo 记录为**环境依赖**，因此改值必然重编，不会留下
    # 旧版本号的产物（实测：默认 0.1.0 → GD_VERSION=2.3.4 出 2.3.4 → 不设又回 0.1.0）。
    env[VERSION_ENV] = version.version

    # 三个 crate 一次构建：`gadgetdisk-cli` → `gadgetdisk`，
    # `gadgetdisk-gdd` → `gdd`，`gadgetdisk-mkfsvfat` → `mkfsvfat`
    # （打包时重命名为 `mkfs.vfat`）。
    command = [
        "cargo",
        "build",
        "--target",
        target,
        "-p",
        "gadgetdisk-cli",
        "-p",
        "gadgetdisk-gdd",
        "-p",
        "gadgetdisk-mkfsvfat",
    ]
    if release:
        command.append("--release")

    print(f"\n=== {abi} ({target}) ===")
    print(f"    version  : {version.version} (versionCode {version.version_code})")
    print(f"    linker   : {linker}")
    builtins = ndk.builtins_archive(target)
    if builtins is not None:
        print(f"    builtins : {builtins}")
    print(f"    RUSTFLAGS: {env['RUSTFLAGS']}")

    run(command, env=env)

    profile = "release" if release else "debug"
    binaries: list[Path] = []
    for name in BIN_NAMES:
        binary = REPO_ROOT / "target" / target / profile / name
        if not binary.is_file():
            raise BuildError(f"build succeeded but the artifact was not found: {binary}")
        binaries.append(binary)
    return binaries


def stage_binary(binary: Path, abi: str) -> Path:
    """将二进制复制到 `target/dist/bin/<abi>/` 并赋予 0755 权限。

    复制时根据 [`BIN_RENAMES`] 映射为发布文件名——因 Cargo 禁止产物名包含 `.`，
    构建产物 `mkfsvfat` 在暂存阶段被重命名为标准命名 `mkfs.vfat`。
    """
    packaged_name = BIN_RENAMES.get(binary.name, binary.name)
    destination = OUT_DIR / "bin" / abi / packaged_name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(binary.read_bytes())
    destination.chmod(0o755)
    print(f"    -> {destination.relative_to(REPO_ROOT)} ({destination.stat().st_size} bytes)")
    return destination


def main(argv: list[str] | None = None) -> int:
    """命令行入口。"""
    parser = argparse.ArgumentParser(
        description="Cross-compile GadgetDisk binaries (gadgetdisk, gdd, mkfs.vfat) for Android"
    )
    parser.add_argument(
        "--abi",
        action="append",
        choices=sorted(ABI_TARGETS),
        help="Build only the specified ABI (repeatable; defaults to all supported ABIs)",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Build debug profile instead of release",
    )
    parser.add_argument(
        "--no-stage",
        action="store_true",
        help="Do not copy artifacts to target/dist/bin/ (compile-check only)",
    )
    parser.add_argument(
        "--version",
        help="Version string baked into the binaries (defaults to module.prop)",
    )
    parser.add_argument(
        "--version-code",
        help="Integer version code recorded alongside the build (defaults to module.prop)",
    )
    args = parser.parse_args(argv)

    abis = args.abi or sorted(ABI_TARGETS)

    try:
        version = Version.resolve(args.version, args.version_code)
        ndk = find_ndk()
        print(f"NDK root    : {ndk.root}")
        print(f"NDK prebuilt: {ndk.prebuilt}")
        print(f"Version     : {version.version} (versionCode {version.version_code})")

        results: list[Path] = []
        for abi in abis:
            target = ABI_TARGETS[abi]
            built = build_target(target, abi, ndk, release=not args.debug, version=version)
            for binary in built:
                results.append(binary if args.no_stage else stage_binary(binary, abi))
            # 记录「这个 ABI 是用哪一版构建的」：打包时会核对，防止包内版本
            # 与二进制自报版本不一致（Android ELF 不能在宿主执行，只能靠记录）。
            record_built_abi(abi, version)

    except BuildError as exc:
        print(f"\nBuild failed:\n{exc}", file=sys.stderr)
        return 1

    print(f"\nDone: {len(results)} binaries built ({'debug' if args.debug else 'release'})")
    print(f"  version: {version.version} (versionCode {version.version_code})")
    for path in results:
        try:
            print(f"  {path.relative_to(REPO_ROOT)}")
        except ValueError:
            print(f"  {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
