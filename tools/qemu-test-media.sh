#!/bin/sh
# De media-kern op QEMU (docs/sequence-hopos.md §14.3): `--features media`
# zonder gui, met een kale qemu-xhci en zonder Hop. Groen alleen als:
#
#   kern    HOPOS_BOOT, HOPOS_NET_UP en HOPOS_SYSTEM_UP;
#   usb     de bouwsteen usb neemt de xHCI over en kijkt alleen naar in- en
#           uitpluggen: zonder HID geen ronde van 4 ms, dus HOPOS_USB_UP met
#           "polling every 500 ms";
#   codec   de codec-dienst zonder VPU: HOPOS_CODEC_NONE;
#   glas    geen regel van het scherm: media trekt gui niet mee (geen
#           `fb:`, geen HOPOS_FB_*).
#
# Rood is meteen: HOPOS_PANIC, HOPOS_FATAL, HOPOS_EXCEPTION. Rood bewaart de
# console (en drukt hem af).
#
#   tools/qemu-test-media.sh               TIMEOUT=60 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-media.sh  bewaart ook een groene console
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-60}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-media
ports SYS AGENT LEADER

cd "$DIR"
echo "== bouwen: hopos (qemuvirt, media)"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt,media
echo "== booten op QEMU virt met een qemu-xhci, zonder Hop (tot ${TIMEOUT}s)"
hop_virt MEDIA=1

MARKS="HOPOS_BOOT|HOPOS_NET_UP|HOPOS_SYSTEM_UP|usb: [1-9][0-9]* controller\\(s\\) up, polling every 500 ms HOPOS_USB_UP|HOPOS_CODEC_NONE"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION"
started
while alive && ! all "$MARKS"; do step; done
qemu_stop

fail=0
marks "$MARKS"
if has "^fb:|HOPOS_FB_"; then
	echo "   ROOD een regel van het scherm in de media-kern: $(tr -d '\r' <"$LOG" | grep -m1 -E '^fb:|HOPOS_FB_')"
	fail=1
else
	echo "   ok  geen regel van het scherm"
fi
reds
took
verdict qemu-media
echo "qemu-media groen"
