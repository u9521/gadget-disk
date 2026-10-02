#!/usr/bin/env python3
"""把模块包推送到设备并安装。

契约见 docs/build-and-release.md「发布流程」与 docs/testing.md 的真机验收清单。

部署规范参考 docs/build-and-release.md「发布流程」：
- 优先尝试模块管理器（ksud / magisk）安装，回退至手工解包；
- 安装成功后如实报告**是否走了暂存路径**（暂存版本要到下次开机才生效）；
- 宿主环境保持中立，不绑定固定机器路径。

**本脚本不执行安装后功能验收**：设备端功能验证须在系统重启后于真实使用路径中
进行（通过 WebUI 完成端到端操作）。先前部署脚本中针对暂存目录的冒烟测试无法
反映 `ksud` 的实际生效状态（暂存更新在重启前未被加载），避免给出虚假的通过反馈。
"""

from __future__ import annotations

import argparse
import os
import sys
import time
from pathlib import Path

from scripts.lib.common import (
    MODULE_TEMPLATE_DIR,
    OUT_DIR,
    REPO_ROOT,
    BuildError,
    find_adb,
    run,
)

#: 设备上**已生效**的模块目录。
#:
#: 模块 id 从 `module_template/module.prop` 读，脚本不重复维护（版本号同理）。
DEVICE_MODULES_ROOT = "/data/adb/modules"

# KernelSU 的暂存路径：`ksud module install <zip>` 并不直接改写 `modules/`，
# 而是解到 `modules_update/<id>/` 并写 `modules/<id>/update` 标记，等**下次开机**
# 才提升为生效版本。因此 `install()` 的第二个返回值必须如实报告「是否暂存」，
# 否则用户不会知道要重启才能生效。同理，**在设备上验证安装结果必须重启后进行** ——
# 本脚本不再执行脱离真实环境的提前检查，设备端的功能验收须在系统重启后开展。
DEVICE_MODULES_UPDATE_ROOT = "/data/adb/modules_update"

#: 数据目录（与 Rust 侧 `DEFAULT_DATA_ROOT` 一致）。
DEVICE_DATA_DIR = "/data/adb/gadget-disk"


def read_module_id() -> str:
    """从 `module_template/module.prop` 读取模块 id。"""
    prop = MODULE_TEMPLATE_DIR / "module.prop"
    if not prop.is_file():
        raise BuildError(f"missing {prop.relative_to(REPO_ROOT)}")

    for raw in prop.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if line.startswith("id="):
            value = line.partition("=")[2].strip()
            if value:
                return value
    raise BuildError("module.prop is missing the id field")


def selected_serial() -> str:
    """当前选择的目标设备（`ANDROID_SERIAL`，空串表示未指定）。"""
    return os.environ.get("ANDROID_SERIAL", "").strip()


def adb(adb_path: str, *args: str, check: bool = True) -> str:
    """执行一条 adb 命令并返回 stdout。

    **显式传 `-s <serial>`**，而不是只依赖 `ANDROID_SERIAL` 环境变量：
    环境变量要由 `adb` 进程自己读取，而某些宿主上的 `adb` 只是一个**包装脚本**
    （例如 WSL 里转发给 Windows 的 `adb.exe`，经 PowerShell 启动），环境变量
    根本传不过去——实测症状是 `failed to get feature set: more than one
    device/emulator`，而 `-s` 参数能正常透传。命令行参数是唯一在所有宿主上
    都可靠的通道。

    `ANDROID_SERIAL` 仍是**选择目标的方式**（`scripts/lib/common.py` 的契约），
    只是由本函数把它翻译成 `-s`。
    """
    serial = selected_serial()
    if serial:
        return run([adb_path, "-s", serial, *args], check=check, capture=True).stdout or ""
    return run([adb_path, *args], check=check, capture=True).stdout or ""


def adb_su(adb_path: str, script: str) -> str:
    """以 root 身份在设备上执行一段 shell。

    通过 `su 0 sh <脚本>` 调用。注意：adb shell 会二次解析引号，
    因此复杂脚本先写入设备上的临时文件再执行（这是实测的必要做法）。
    """
    remote = f"/data/local/tmp/gadgetdisk-deploy-{int(time.time() * 1000)}.sh"
    local = Path("/tmp") / Path(remote).name
    local.write_text(script, encoding="utf-8")
    adb(adb_path, "push", str(local), remote)
    try:
        return adb(adb_path, "shell", f"su 0 sh {remote}")
    finally:
        local.unlink(missing_ok=True)
        adb(adb_path, "shell", f"rm -f {remote}", check=False)


def ensure_device(adb_path: str) -> None:
    """确认有一台可用的目标设备。

    **多设备时由 `ANDROID_SERIAL` 选择**（`scripts/lib/common.py` 的既定契约：
    「多设备时选择目标（由 adb 自身识别）」）。选择结果由 [`adb`] 翻译成 `-s`
    参数下发——不依赖 adb 自己读环境变量，因为包装脚本形态的 adb 读不到它。

    只有既有多台、又没指定 `ANDROID_SERIAL` 时才报错：那种情况下 adb 自己也会
    拒绝执行（`more than one device/emulator`），提前报错只是为了给出更清楚的话。
    """
    output = adb(adb_path, "devices")
    devices = [
        line.split()[0]
        for line in output.splitlines()[1:]
        if line.strip() and len(line.split()) > 1 and line.split()[1] == "device"
    ]
    if not devices:
        raise BuildError(
            "no adb device in the 'device' state. Connect a device or start an emulator first."
        )

    selected = selected_serial()
    if len(devices) > 1 and not selected:
        raise BuildError(
            f"multiple devices detected ({', '.join(devices)}); select one with ANDROID_SERIAL."
        )
    if selected and selected not in devices:
        raise BuildError(
            f"ANDROID_SERIAL='{selected}' is not among the connected devices "
            f"({', '.join(devices)})."
        )


def push_zip(adb_path: str, zip_path: Path) -> str:
    """把模块包推到设备，返回设备上的路径。"""
    remote = f"/data/local/tmp/{zip_path.name}"
    adb(adb_path, "push", str(zip_path), remote)
    return remote


def install(adb_path: str, remote_zip: str, module_id: str) -> tuple[str, bool]:
    """安装模块，返回 `(方式名, 是否走暂存路径)`。

    优先级：`ksud module install` → `magisk --install-module` → 手工解包。
    前者会执行 `customize.sh`（架构裁剪、权限设置），因此优先；
    手工解包时必须在部署脚本里补做同一件事，否则二进制没有可执行位。

    「是否暂存」作为**独立的布尔返回值**而不是从方式名里搜关键字：那种写法把
    逻辑耦合在一条面向用户的文案上，文案一改（例如翻成英文）判定就静默失效，
    而失效的表现是「验证错了目录却仍然全绿」。
    """
    ksud = adb_su(
        adb_path,
        f"if command -v ksud >/dev/null 2>&1; then ksud module install {remote_zip}; "
        f"echo INSTALLED_BY=ksud; else echo NO_KSUD; fi",
    )
    if "INSTALLED_BY=ksud" in ksud:
        # ksud 走暂存路径：新版本要到下次开机才生效。
        return "ksud (staged, takes effect after reboot)", True

    magisk = adb_su(
        adb_path,
        f"if command -v magisk >/dev/null 2>&1; then magisk --install-module {remote_zip}; "
        f"echo INSTALLED_BY=magisk; else echo NO_MAGISK; fi",
    )
    if "INSTALLED_BY=magisk" in magisk:
        return "magisk", False

    # 手工解包：模块目录是 KernelSU 与 Magisk 的事实标准。
    abi = adb_su(adb_path, "getprop ro.product.cpu.abi").strip()
    target = f"{DEVICE_MODULES_ROOT}/{module_id}"
    script = f"""
set -e
rm -rf {target}
mkdir -p {target}
cd {target}
unzip -o {remote_zip} >/dev/null
# customize.sh 的等价动作：将目标 ABI 的所有二进制（gadgetdisk, gdd,
# mkfs.vfat）移至扁平 bin/ 并清理多余 ABI 目录。
for b in gadgetdisk gdd mkfs.vfat; do
  mv "bin/{abi}/$b" "bin/$b"
done
for d in bin/*; do
  [ -d "$d" ] && rm -rf "$d"
done
chmod 0755 bin/gadgetdisk bin/gdd bin/mkfs.vfat
for s in customize.sh service.sh uninstall.sh; do
  [ -f "$s" ] && chmod 0755 "$s"
done
echo MANUAL_OK
"""
    result = adb_su(adb_path, script)
    if "MANUAL_OK" not in result:
        raise BuildError("manual unpack install failed.")
    return "manual", False


def main(argv: list[str] | None = None) -> int:
    """命令行入口。"""
    parser = argparse.ArgumentParser(
        description="Push and install the GadgetDisk module package onto a connected Android device"
    )
    parser.add_argument(
        "--zip",
        type=Path,
        help="Path to module ZIP file (defaults to the latest GadgetDisk-*.zip in target/dist/)",
    )
    parser.add_argument(
        "--reboot",
        action="store_true",
        help="Reboot the device after installation (disabled by default)",
    )
    parser.add_argument(
        "--no-install",
        action="store_true",
        help="Push the module ZIP to device without triggering installation (for debugging)",
    )
    args = parser.parse_args(argv)

    try:
        module_id = read_module_id()
        adb_path = find_adb()
        ensure_device(adb_path)

        zip_path = args.zip
        if zip_path is None:
            candidates = sorted(
                OUT_DIR.glob("GadgetDisk-*.zip"),
                key=lambda p: p.stat().st_mtime,
            )
            if not candidates:
                raise BuildError("no module ZIP under target/dist/. Run `uv run gd-package` first")
            zip_path = candidates[-1]
        if not zip_path.is_file():
            raise BuildError(f"module ZIP does not exist: {zip_path}")

        print(f"adb           : {adb_path}")
        print(f"module id     : {module_id}")
        display = (
            zip_path.relative_to(REPO_ROOT) if zip_path.is_relative_to(REPO_ROOT) else zip_path
        )
        print(f"module package: {display}")

        remote = push_zip(adb_path, zip_path)
        print(f"pushed to     : {remote}")

        if args.no_install:
            print("\nSkipped installation (--no-install).")
            return 0

        method, staged = install(adb_path, remote, module_id)
        print(f"install method: {method}")

        if staged:
            print(
                "\nNote: ksud used the staged update path; the new version will only become "
                "active after a device reboot."
            )

        if args.reboot:
            print("\nRebooting the device...")
            adb(adb_path, "reboot")
            print("Reboot requested; service.sh will start the module after boot.")
        elif staged:
            print("\nNext: uv run gd-deploy --reboot (to make the staged version active)")
        else:
            print(
                "\nModule installed successfully. Services start on demand "
                "(no resident background processes when no images are exported)."
            )

    except BuildError as exc:
        print(f"\nDeployment failed:\n{exc}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
