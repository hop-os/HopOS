#!/bin/sh
# Het NVMe-pad op QEMU: het Altra-image (de NVMe-kern van de O6N en de
# Altra, driver/nvme achter de wachtrij van blkdev) op QEMU virt met EDK2,
# de data-schijf als `-device nvme`, de NIC als igb, en appspike gestaged
# in slot 1 en daarna slot 2. De FS-toets van appspike schrijft, leest
# terug, leest een bundel (OP_READ_MANY: drie lezingen in één call, de
# laatste voorbij het einde) en ruimt op, allemaal door de NVMe-kern.
#
# Groen alleen als:
#
#   de kern   HOPOS_NVME_UP, "HOPOS_DISK_UP model=QEMU NVMe Ctrl",
#             HOPOS_FS_UP fresh=1 (een verse schijf) en HOPOS_NVME_IRQ op
#             MSI-X via de ITS (de wachtrij slaapt op de lijn; de zelftest
#             kwam aan);
#   per slot  HOPOS_APPSPIKE_FS ok en HOPOS_APPSPIKE_DONE ... fail=0.
#
# Rood is ook: HOPOS_PANIC, HOPOS_FATAL (ook een blokfout of een mislukte
# mount), HOPOS_EXCEPTION en HOPOS_NVME_FAIL. Rood bewaart de console (en drukt hem af).
#
#   tools/qemu-test-nvme.sh                TIMEOUT=120 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-nvme.sh   bewaart ook een groene console
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-120}"
scratch nvme
VARS="$ART/vars.fd"
ESP="$ART/esp"
CFG="$ART/hopos.cfg"
# Een verse, ijle schijf van 64 MiB: hopfs begint leeg. Een lege config:
# geen Hop, alleen de rol van het gestagede image (uefi-run.sh zet hem).
dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek=64 2>/dev/null
dd if=/dev/zero of="$VARS" bs=1048576 count=64 2>/dev/null
printf '# qemu-test-nvme: geen sleutels\n' >"$CFG"

cd "$DIR"
echo "== bouwen: BOOTAA64.EFI (board-altra) en appspike op de ESP"
BUILD_ONLY=1 BOARD=altra APP=appspike ESP="$ESP" CFG="$CFG" sh image/uefi-run.sh 2>&1 | sed 's/^/   /'
[ -e "$ESP/EFI/BOOT/BOOTAA64.EFI" ] || {
	echo "ROOD: geen BOOTAA64.EFI"
	exit 1
}

MARKS="HOPOS_NVME_UP|HOPOS_DISK_UP model=QEMU NVMe Ctrl blocks=131072|HOPOS_FS_UP fresh=1"
MARKS="$MARKS|MSI-X via the ITS, LPI [0-9]+.*HOPOS_NVME_IRQ"
MARKS="$MARKS|slot 1: HOPOS_APPSPIKE_FS ok|slot 1: HOPOS_APPSPIKE_DONE pass=[0-9]+ fail=0"
MARKS="$MARKS|slot 2: HOPOS_APPSPIKE_FS ok|slot 2: HOPOS_APPSPIKE_DONE pass=[0-9]+ fail=0"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_NVME_FAIL|HOPOS_APPSPIKE_FS FAIL"

echo "== booten op QEMU virt met EDK2 en een NVMe (tot ${TIMEOUT}s)"
qemu_edk2 neoverse-n1 "$VARS" "$ESP" \
	-device igb,netdev=n0,romfile= -netdev user,id=n0 \
	-drive "if=none,format=raw,file=$DISK,id=nv0" -device nvme,drive=nv0,serial=hopnvme \
	</dev/null >"$LOG" 2>&1 &
QPID=$!

elapsed=0
while ! all "$MARKS" && ! has "$RED" && kill -0 "$QPID" 2>/dev/null && [ "$elapsed" -lt "$((TIMEOUT * 5))" ]; do
	sleep 0.2
	elapsed=$((elapsed + 1))
done
qemu_stop

fail=0
marks "$MARKS"
if has "$RED"; then
	echo "   ROOD: $(first "$RED")"
	fail=1
fi
[ -n "${KEEP_LOG:-}" ] && cp "$LOG" "$KEEP_LOG"
if [ "$fail" = 0 ]; then
	echo "GROEN: het NVMe-pad op QEMU, met een bundel"
	exit 0
fi
echo "ROOD; de console:"
tr -d '\r' <"$LOG" | tail -120
exit 1
