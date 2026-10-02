#!/system/bin/sh

MODDIR=${0%/*}
DATA=/data/adb/gadget-disk

log() { echo "gadgetdisk uninstall: $1" > /dev/kmsg 2>/dev/null; }

# ---------------------------------------------------------------- 1. api.json
#
# **本脚本不终止任何进程**：卸载脚本在开机早期执行（KernelSU 的
# `prune_modules`、Magisk daemon 的 `remove_modules`），都**早于** `service.sh`，
# 那时 `gadgetdisk`/`gdd` 不可能在跑（`gdd` 只在有镜像导出期间存在，导出由
# `service.sh` → `gadgetdisk boot` 恢复）。因此扫 `/proc` 杀进程既无对象，又会
# 带来误伤他人进程的风险。
#
# 但凭据文件仍要兜底清理：若用户在开机前手动拉起过 `serve` 并留下 `api.json`，
# 模块目录删除后它不该继续存在。
rm -f "$MODDIR/webroot/api.json" 2>/dev/null

# ---------------------------------------------------------------- 2. 收尾
#
# 交给 `gadgetdisk uninstall`：拆除导出（经 gdd）、还原 Android 身份、
# 清理 run/ 下的状态文件。
BIN="$MODDIR/bin/gadgetdisk"

if [ -x "$BIN" ]; then
  "$BIN" uninstall --data-dir "$DATA" >> "$DATA/logs/service.log" 2>&1 < /dev/null
  log "executed gadgetdisk uninstall cleanup (exit code: $?)"
else
  log "gadgetdisk not found; skipping cleanup (only fallback cleanup will run)"
fi

# ---------------------------------------------------------------- 3. 兜底清理
#
# 刷新脏页缓存并卸载本模块各挂载点。
sync
for m in "$DATA/mnt"/*; do
  [ -d "$m" ] || continue
  umount "$m" 2>/dev/null
done

# 只分离后备文件位于本模块 images/ 下的 loop 设备，绝不触碰他人的。
if [ -d /sys/block ]; then
  for dev in /sys/block/loop*; do
    [ -e "$dev/loop/backing_file" ] || continue
    backing=$(cat "$dev/loop/backing_file" 2>/dev/null)
    case "$backing" in
      "$DATA"/images/*)
        name="${dev##*/}"
        losetup -d "/dev/block/$name" 2>/dev/null
        ;;
    esac
  done
fi

# ---------------------------------------------------------------- 4. 清理数据
# 镜像文件也一并删除——用户卸载模块时不应把几十 GiB 的镜像永久留在 /data 上。
rm -rf "$DATA" 2>/dev/null

log "uninstall cleanup finished"
