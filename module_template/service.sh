#!/system/bin/sh

MODDIR=${0%/*}
DATA=/data/adb/gadget-disk

BIN="$MODDIR/bin/gadgetdisk"

if [ ! -x "$BIN" ]; then
  echo "gadgetdisk service: executable not found: $BIN (MODDIR=$MODDIR)" > /dev/kmsg 2>/dev/null
  exit 1
fi

mkdir -p "$DATA/images" "$DATA/run" "$DATA/config" "$DATA/logs" "$DATA/mnt" "$DATA/tmp"
chmod 0700 "$DATA" "$DATA/run" "$DATA/logs" 2>/dev/null

resetprop -w sys.boot_completed 2>/dev/null

"$BIN" boot --data-dir "$DATA" 2>> "$DATA/logs/service.log" > /dev/null < /dev/null

exit 0
