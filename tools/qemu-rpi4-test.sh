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
#   stage:          een -initrd in het laadvenster (appspike), gelezen;
#   HOPOS_RNG_INSECURE
#                   de RNG200 wordt NIET aangeraakt: QEMU haalt zijn node uit
#                   de DTB en een lees op 0xFE10_4000 is een external abort
#                   (board_raspi::rng); de DRBG zaait op jitter, luid;
#   de reset        dan wapent de kern de PM-watchdog, als eerste stap na
#                   `discover` (docs/sequence-hopos.md §13.2, O2: de watchdog
#                   is verplicht, er is geen knop om hem uit te zetten). QEMU
#                   modelleert het PM-blok alleen als reset-knop: de schrijf
#                   van FULL_RESET reset de machine METEEN
#                   (hw/misc/bcm2835_powermgt.c), en op ijzer telt de teller
#                   pas ná FULL_RESET (Pi 5, 30-09), dus er is geen proef
#                   zonder wapenen en geen weg eromheen zonder testpad in de
#                   kern. Met -no-reboot is die reset het einde van QEMU: de
#                   ring eist dat QEMU vanzelf stopt, met precies één P2 en
#                   geen HOPOS_BOOT. Alles na het wapenen (de GIC-400, de
#                   kooi, de zelftest van de OS-core, de tikken) bewijst
#                   QEMU raspi4b dus niet meer; dat is ijzerwerk
#                   (docs/boards-pi.md).
#
# Wat QEMU raspi4b verder NIET kan: de GENET (QEMU haalt de node uit de
# DTB) en een app-core (de andere cores staan in een spin-table zonder
# PSCI). GUI=1 bouwt de gui-smaak en toetst dezelfde boot tot de reset.
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

MARKERS="P2|HopOS"
set -- -kernel "$TMP/kernel8.img" -append "hopos.stage=none" -no-reboot
if [ -f "$DTB" ]; then
	set -- "$@" -dtb "$DTB" -initrd "$TMP/app.elf"
	MARKERS="$MARKERS|fdt: |mem: |vcmail: |KB at 0x8000000, role unknown|no such node in the DTB (QEMU models none), not touched|HOPOS_RNG_INSECURE"
else
	echo "qemu-rpi4-test: no DTB at $DTB, boot markers only" >&2
fi

qemu-system-aarch64 -M raspi4b -display none -monitor none \
	-serial "file:$TMP/console.log" "$@" 2>"$TMP/qemu.err" &
QPID=$!
# De reset van het wapenen stopt QEMU (-no-reboot); wie na SECS nog
# draait, wapende niet.
t=0
while kill -0 "$QPID" 2>/dev/null && [ "$t" -lt "$((SECS * 10))" ]; do
	sleep 0.1
	t=$((t + 1))
done
reset=0
if kill -0 "$QPID" 2>/dev/null; then
	kill "$QPID" 2>/dev/null || true
else
	reset=1
fi
wait "$QPID" 2>/dev/null || true

fail=0
if [ "$reset" != 1 ] || [ "$(grep -c '^P2' "$TMP/console.log")" != 1 ] || grep -q "HOPOS_BOOT" "$TMP/console.log"; then
	echo "qemu-rpi4-test: expected the reset of the PM watchdog right after discover (QEMU stops, one P2, no HOPOS_BOOT)" >&2
	fail=1
fi
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
head -40 "$TMP/console.log"
echo "qemu-rpi4-test: groen (tot de reset van de PM-watchdog)"
