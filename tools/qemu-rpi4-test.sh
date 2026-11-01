#!/bin/sh
# De QEMU-rook van de Pi 4 (board/rpi4, handboek §9): bouwt kernel8.img
# zoals image/rpi4.sh, boot hem op QEMU `-M raspi4b` met de echte Pi 4-DTB,
# en slaagt alleen als de markers er staan:
#
#   P2              de Pi-ingang (board_raspi::pi_entry!) draait, op EL2;
#   HopOS           de bunny: cpu::boot, de MMU met de vaste tabellen, kmain;
#   fdt:            de DTB uit x0 (QEMU legt hem achter het -initrd-venster);
#   mem:            de kaart uit de DTB: RAM erbij gemapt, een pool;
#   vcmail:         de VideoCore-mailbox (QEMU emuleert de property-tags);
#   HOPOS_BOOT      de executor, met het board;
#   irq: GIC-400    de GIC-400 als cpu::irq::Controller;
#   HOPOS_CAGE_UP   de kooi-regio in het Device-venster van de Pi-kaart;
#   stage:          een -initrd in het laadvenster (appspike), gelezen maar
#                   met `hopos.stage=none` niet geplaatst (zie hieronder);
#   HOPOS_TICK 3    drie seconden executor en slaap (WFE + event-stream);
#   HOPOS_OS_SELFTEST ok
#                   de overgang van de OS-core: de CNTHP (PPI 26) haalt een
#                   spinnende bewoner terug, een HVC #1 komt terug als yield,
#                   en de kick-SGI via GICD_SGIR naar de core zelf komt terug
#                   als IPI (board_raspi::KICK_SGI; geen tweede core nodig);
#   kicks=1         de dispatch claimde die kick als bekende lijn en telde
#                   hem, in plaats van hem als onbekend uit te zetten;
#   HOPOS_RNG_INSECURE
#                   de RNG200 wordt NIET aangeraakt: QEMU haalt zijn node uit
#                   de DTB en een lees op 0xFE10_4000 is een external abort
#                   (board_raspi::rng); de DRBG zaait op jitter, luid;
#   HOPOS_WD_OFF    de PM-watchdog staat uit (hopos.wd=off in de append):
#                   QEMU modelleert het PM-blok alleen als reset-knop, een
#                   FULL_RESET reset de machine meteen, en op ijzer telt de
#                   teller pas ná FULL_RESET (Pi 5, 30-09), dus er is geen
#                   proef zonder wapenen (board_raspi::watchdog).
#
# Wat QEMU raspi4b NIET kan en dit script dus niet bewijst: de GENET (QEMU
# haalt de node uit de DTB; de kern ziet dat en zegt HOPOS_NIC_NONE), en een
# app-core: QEMU zet de andere cores in een spin-table zonder PSCI, dus de
# eerste CPU_ON van een plaatsing hangt: een SMC zonder EL3-firmware
# (gemeten 29-09: na "cage: slot 1 built" niets meer). Daarom leest de kern
# de staging wel maar plaatst hij niets (HOPOS_SLOT_NONE). App-cores zijn
# ijzerwerk: docs/boards-pi.md.
#
# GUI=1 bouwt de gui-smaak (docs/gui.md) en toetst daarbovenop dat de
# VL805-keten (board/rpi4/src/usb.rs) de PCIe-RC niet aanraakt als de DTB
# hem niet aanzet: QEMU raspi4b modelleert geen RC en haalt de node weg, en
# de eerste schrijf op 0xFD50_9210 was daar een synchrone abort (30-09).
# Markers: "no enabled brcm,bcm2711-pcie in the DTB, PCIe not touched" en
# HOPOS_USB_NONE, zonder HOPOS_EXCEPTION.
#
# De DTB: DTB=pad, standaard image/firmware/rpi4/bcm2711-rpi-4-b.dtb
# (herkomst in de LEESMIJ.txt daar). Zonder DTB boot de kern ook, maar
# zonder kaart, pool en staging; dan toetst het script alleen de boot.
set -e

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
TARGET=aarch64-unknown-none-softfloat
DTB="${DTB:-$DIR/image/firmware/rpi4/bcm2711-rpi-4-b.dtb}"
SECS="${SECS:-8}"
GUI="${GUI:-0}"
case "$GUI" in
0) FEATS=board-rpi4 ;;
1) FEATS=board-rpi4,gui ;;
*)
	echo "GUI=$GUI: kies 0 of 1" >&2
	exit 2
	;;
esac
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

cd "$DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features "$FEATS"
cargo build --quiet --release --target "$TARGET" -p appspike
need_objcopy qemu-rpi4-test
"$OBJCOPY" -O binary "$DIR/target/$TARGET/release/hopos" "$TMP/kernel8.img"
strip_elf "$DIR/target/$TARGET/release/appspike" "$TMP/app.elf"

MARKERS="P2|HopOS|HOPOS_BOOT|irq: GIC-400|HOPOS_TICK 3|HOPOS_OS_SELFTEST ok|kicks=1)"
set -- -kernel "$TMP/kernel8.img" -append "hopos.stage=none hopos.wd=off"
if [ -f "$DTB" ]; then
	set -- "$@" -dtb "$DTB" -initrd "$TMP/app.elf"
	MARKERS="$MARKERS|fdt: |mem: |vcmail: |HOPOS_CAGE_UP|KB at 0x8000000, role unknown|HOPOS_SLOT_NONE|no such node in the DTB (QEMU models none), not touched|HOPOS_RNG_INSECURE|HOPOS_WD_OFF"
	if [ "$GUI" = 1 ]; then
		MARKERS="$MARKERS|no enabled brcm,bcm2711-pcie in the DTB, PCIe not touched|HOPOS_USB_NONE"
	fi
else
	echo "qemu-rpi4-test: no DTB at $DTB, boot markers only" >&2
fi

qemu-system-aarch64 -M raspi4b -display none -monitor none \
	-serial "file:$TMP/console.log" "$@" 2>"$TMP/qemu.err" &
QPID=$!
sleep "$SECS"
kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true

fail=0
if grep -q "HOPOS_PANIC\|HOPOS_FATAL\|HOPOS_EXCEPTION" "$TMP/console.log"; then
	echo "qemu-rpi4-test: panic or exception" >&2
	fail=1
fi
OLDIFS="$IFS"
IFS='|'
for m in $MARKERS; do
	if ! grep -q "$m" "$TMP/console.log"; then
		echo "qemu-rpi4-test: missing marker '$m'" >&2
		fail=1
	fi
done
IFS="$OLDIFS"
if [ "$fail" -ne 0 ]; then
	cat "$TMP/console.log" >&2
	exit 1
fi
grep -v "HOPOS_TICK" "$TMP/console.log" | head -40
grep "HOPOS_TICK 3 " "$TMP/console.log" | head -1
echo "qemu-rpi4-test: groen"
