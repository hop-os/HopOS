#!/bin/sh
# De UEFI-poort van HopOS v3: dezelfde toets als tools/qemu-test.sh, maar
# over de weg van de O6N en de Altra. De kern gaat als BOOTAA64.EFI op een
# ESP (image/uefi-run.sh), QEMU -M virt boot hem met echte EDK2-firmware
# uit $QEMU_SHARE, en alles wat de kern vindt, vindt hij zoals op ijzer:
# de cores, de GIC en de console uit ACPI (MADT, SPCR), de PCIe uit de
# MCFG, en de NIC en de schijf als virtio-net-pci en virtio-blk-pci. Het
# app-image (appspike) staat als hopos-stage.elf op de ESP, met
# `hopos.stage=app` in hopos.cfg.
#
# De markers zijn die van tools/qemu-test.sh (HOPOS_BOOT, HOPOS_TICK 3,
# HOPOS_NIC_UP, HOPOS_NET_UP, HOPOS_SYSTEM_UP, HOPOS_SYSTEM_REFUSED en de
# hele appspike-keten in slot 1 en 2), plus de UEFI-eigen:
#
#   HOPOS_UEFI_STUB  de PE-stub draait onder de firmware (ConOut);
#   HOPOS_CFG        hopos.cfg kwam van de ESP.
#
#   tools/qemu-uefi-test.sh          TIMEOUT=60 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-uefi-test.sh   bewaart ook een groene console
#   SYSPORT=poort                    de host-kant van de hostfwd; standaard
#                                    een vrije van het OS
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-60}"
TARGET=aarch64-unknown-none-softfloat
scratch uefi
ESP="$ART/esp"
ports SYS # de host-kant van de hostfwd naar de system-API
# Een verse, ijle schijf van 64 MiB: hopfs begint leeg.
dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek=64 2>/dev/null

cd "$DIR"
echo "== bouwen: BOOTAA64.EFI (board-uefi) en appspike op de ESP"
BUILD_ONLY=1 APP=appspike ESP="$ESP" sh image/uefi-run.sh 2>&1 | sed 's/^/   /'
[ -e "$ESP/EFI/BOOT/BOOTAA64.EFI" ] || {
	echo "ROOD: geen BOOTAA64.EFI"
	exit 1
}
# De markers van het ABI-bewijs (grep -E): het aantal toetsen groeit met
# appspike, dus "alles groen" is fail=0.
NEED="HOPOS_UEFI_STUB|HOPOS_CFG|HOPOS_BOOT|HOPOS_TICK 3|HOPOS_NIC_UP|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_SYSTEM_REFUSED"
SLOT_MARKS="HOPOS_DISK_UP model=virtio-blk blocks=131072|HOPOS_FS_UP fresh=1"
SLOT_MARKS="$SLOT_MARKS|HOPOS_SLOT_START slot=1|slot 1: HOPOS_APPSPIKE_NETLOG|slot 1: HOPOS_APPSPIKE_FS ok|slot 1: HOPOS_APPSPIKE_DONE pass=9 fail=0|HOPOS_SLOT_DONE slot=1 exit=0|slot 1: stopped.*HOPOS_SLOT_STOPPED"
SLOT_MARKS="$SLOT_MARKS|HOPOS_SLOT_START slot=2|slot 2: HOPOS_APPSPIKE_NETLOG|slot 2: HOPOS_APPSPIKE_FS ok|slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0|HOPOS_SLOT_DONE slot=2 exit=0|slot 2: stopped.*HOPOS_SLOT_STOPPED"
# De momenten van de toets van buiten (grep -E), in volgorde.
PROBE_AT="HOPOS_SYSTEM_UP|slot 1: .*HOPOS_APPNET_UP|slot 2: stopped.*HOPOS_SLOT_STOPPED"

echo "== booten op QEMU virt met EDK2 (tot ${TIMEOUT}s)"
APP=appspike ESP="$ESP" DISK="$DISK" VARS="$ART/vars.fd" SYSPORT="$SYSPORT" \
	sh image/uefi-run.sh </dev/null >"$LOG" 2>&1 &
QPID=$!

# Wachten tot alle markers er zijn en alle toetsen van buiten gedaan,
# iets roods verschijnt, QEMU stopt, of de tijd op is.
elapsed=0
while :; do
	# De toets van buiten, op zijn moment; de keten loopt intussen door.
	probe_due && continue
	! probes_left && all "$NEED|$SLOT_MARKS" && break
	has "HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION" && break
	kill -0 "$QPID" 2>/dev/null || break
	[ "$elapsed" -ge "$((TIMEOUT * 10))" ] && break
	sleep 0.1
	elapsed=$((elapsed + 1))
done
qemu_stop

fail=0
marks "$NEED" "$SLOT_MARKS"
probes_report
if has "HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION"; then
	echo "   ROOD panic of exception"
	fail=1
fi
verdict uefi
echo "uefi-poort groen"
