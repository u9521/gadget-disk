#!/system/bin/sh

SKIPUNZIP=0

# ---------------------------------------------------------------- 目标架构
#
# 直接使用安装器**已经算好**的 `$ARCH` / `$IS64BIT`，不再自己 getprop/uname：
# 安装脚本在执行本文件之前先运行架构探测，且是 `source`（同一 shell），因此这两个
# 变量在这里一定可见。自己再探测一遍等于把同一件事算两次，而两份结果可能不一致
# （探测源不同），那时以哪一份为准就成了隐式约定。
#
# 取值与安装器一致（KernelSU `installer.sh` 的 `api_level_arch_detect`、
# Magisk `util_functions.sh` 的 `api_level_arch_detect`）：
# `arm64` / `x64` / `arm` / `x86` / `riscv64`。
case "$ARCH" in
  arm64) abi="arm64-v8a" ;;
  x64)   abi="x86_64" ;;
  *)     abi="" ;;
esac

if [ -z "$abi" ]; then
  ui_print "! Unsupported device architecture: ARCH='${ARCH:-unset}' IS64BIT='${IS64BIT:-unset}'"
  ui_print "! GadgetDisk currently only supports arm64-v8a and x86_64 architectures."
  abort "! Installation aborted."
fi

ui_print "- Device architecture: $abi"

# ---------------------------------------------------------------- 扁平化二进制
#
# 安装阶段按设备架构将对应二进制扁平化至 bin/，避免运行期动态探测 ABI。
#
# `mkfs.vfat` 是本项目自带的 FAT 格式化工具：Android 不含 dosfstools，
# toybox 也没有 mkfs，因此镜像创建需要它（见 docs/disk-image-format.md）。

for bin in gadgetdisk gdd mkfs.vfat; do
  if [ ! -f "$MODPATH/bin/$abi/$bin" ]; then
    abort "! The module package is missing required binary: bin/$abi/$bin"
  fi
done

# 将目标 ABI 的二进制移动至扁平的 bin/，并清理其余 ABI 目录以节省空间。
for bin in gadgetdisk gdd mkfs.vfat; do
  mv "$MODPATH/bin/$abi/$bin" "$MODPATH/bin/$bin"
done

for dir in "$MODPATH/bin"/*; do
  [ -d "$dir" ] || continue
  ui_print "- Cleaning up unused ABI files: ${dir##*/}"
  rm -rf "$dir"
done

# 二进制必须可执行。ZIP 不保留 Unix 权限位，因此这一步不能省。
set_perm "$MODPATH/bin/gadgetdisk" 0 0 0755
set_perm "$MODPATH/bin/gdd" 0 0 0755
set_perm "$MODPATH/bin/mkfs.vfat" 0 0 0755

ui_print "- GadgetDisk installed successfully ($abi)"
