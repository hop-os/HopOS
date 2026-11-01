#!/bin/sh
# De UEFI-poort van HopOS v3: dezelfde toets als tools/qemu-test.sh, maar
# over de weg van de O6N en de Altra. De kern gaat als BOOTAA64.EFI op een
# ESP (image/uefi-run.sh), QEMU -M virt boot hem met echte EDK2-firmware
# uit $QEMU_SHARE, en alles wat de kern vindt, vindt hij zoals op ijzer:
# de cores, de GIC en de console uit ACPI (MADT, SPCR), de PCIe uit de
# MCFG, en de NIC en de schijf als virtio-net-pci en virtio-blk-pci, allebei
# op MSI-X via de ITS (01-11, O3). Hop staat als hopos-stage.elf op de ESP,
# met `hopos.stage=hop` en `hopos.insecure=1` in hopos.cfg, en plaatst
# appspike na een jobspec naar zijn leader (de echte weg, O7).
#
# De markers zijn die van tools/qemu-test.sh (HOPOS_BOOT, HOPOS_TICK 3,
# HOPOS_NIC_UP, HOPOS_NET_UP, HOPOS_SYSTEM_UP, HOPOS_SYSTEM_REFUSED, Hop in
# slot 1 en de appspike-keten in slot 2, twee keer: Hop herstart de
# service), plus de UEFI-eigen:
#
#   HOPOS_UEFI_STUB  de PE-stub draait onder de firmware (ConOut);
#   HOPOS_CFG        hopos.cfg kwam van de ESP.
#
#   tools/qemu-uefi-test.sh          TIMEOUT=150 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-uefi-test.sh   bewaart ook een groene console
#   SYSPORT=poort                    de host-kant van de hostfwd; standaard
#                                    een vrije van het OS
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-150}"
TARGET=aarch64-unknown-none-softfloat
scratch uefi
ESP="$ART/esp"
# De system-API, de agent en de leader van Hop, en de artifact-server.
ports SYS AGENT LEADER ART
# Een verse, ijle schijf van 64 MiB: hopfs begint leeg.
dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek=64 2>/dev/null

cd "$DIR"
echo "== bouwen: BOOTAA64.EFI (board-uefi) met Hop op de ESP, en appspike"
apps appspike
hop_elf
serve
BUILD_ONLY=1 APP="$HOP_ELF" ROLE=hop ESP="$ESP" sh image/uefi-run.sh 2>&1 | sed 's/^/   /'
[ -e "$ESP/EFI/BOOT/BOOTAA64.EFI" ] || {
	echo "ROOD: geen BOOTAA64.EFI"
	exit 1
}
# Alleen virt zet voor Hop zelf QEMU_CFG achter de config (kern::nodecfg):
# hier de insecure-regel, anders weigert Hop zijn API.
printf 'hopos.insecure=1\n' >>"$ESP/hopos.cfg"
# De markers van het ABI-bewijs (grep -E): het aantal toetsen groeit met
# appspike, dus "alles groen" is fail=0.
NEED="HOPOS_UEFI_STUB|HOPOS_CFG|HOPOS_BOOT|HOPOS_TICK 3|HOPOS_NIC_UP|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_SYSTEM_REFUSED"
BOOT_MARKS="HOPOS_SYSTEM_UP|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
NEED="$NEED|$BOOT_MARKS"
SLOT_MARKS="HOPOS_DISK_UP model=virtio-blk blocks=131072|HOPOS_FS_UP fresh=1|disk: virtio-blk-pci at .*MSI-X via the ITS"
SLOT_MARKS="$SLOT_MARKS|HOPOS_SLOT_START slot=2|slot 2: HOPOS_APPSPIKE_NETLOG|slot 2: HOPOS_APPSPIKE_FS ok|slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0|slot 2: stopped.*HOPOS_SLOT_STOPPED"
# De momenten van de toets van buiten (grep -E), in volgorde.
PROBE_AT="HOPOS_SYSTEM_UP|slot 2: .*HOPOS_APPNET_UP|slot 2: stopped.*HOPOS_SLOT_STOPPED"
JOB='{"name":"spike","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432}'
POSTED=""

echo "== booten op QEMU virt met EDK2 (tot ${TIMEOUT}s)"
# De QEMU-regel van image/uefi-run.sh, met de hostfwd's van Hop erbij.
QGUI=""
[ "${GUI:-0}" = 1 ] && QGUI="-device ramfb"
dd if=/dev/zero of="$ART/vars.fd" bs=1048576 count=64 2>/dev/null
# shellcheck disable=SC2086
qemu_edk2 "${CPU:-cortex-a57}" "$ART/vars.fd" "$ESP" $QGUI \
	-device virtio-net-pci,netdev=n0,romfile= \
	-netdev "user,id=n0,$QFWD" \
	-drive "if=none,format=raw,file=$DISK,id=disk0" \
	-device virtio-blk-pci,drive=disk0 \
	</dev/null >"$LOG" 2>&1 &
QPID=$!

# Wachten tot alle markers er zijn en alle toetsen van buiten gedaan,
# iets roods verschijnt, QEMU stopt, of de tijd op is.
elapsed=0
while :; do
	# De job naar de leader zodra Hop staat.
	if [ -z "$POSTED" ] && all "$BOOT_MARKS"; then
		post_job "$JOB" || break
	fi
	# De toets van buiten, op zijn moment; de keten loopt intussen door.
	probe_due && continue
	! probes_left && all "$NEED|$SLOT_MARKS" && [ "$(count 'slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0')" -ge 2 ] && break
	has "HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION" && break
	kill -0 "$QPID" 2>/dev/null || break
	[ "$elapsed" -ge "$((TIMEOUT * 10))" ] && break
	sleep 0.1
	elapsed=$((elapsed + 1))
done
qemu_stop

fail=0
marks "$NEED" "$SLOT_MARKS"
if [ "$(count 'slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0')" -ge 2 ]; then
	echo "   ok  de tweede ronde (de herstart door Hop)"
else
	echo "   ROOD de tweede ronde ontbreekt (Hop herstart appspike niet)"
	fail=1
fi
case "$POSTED" in
*" HTTP 2"*) echo "   ok  POST /v1/jobs:$POSTED" ;;
*)
	echo "   ROOD POST /v1/jobs: ${POSTED:-nooit gedaan}"
	fail=1
	;;
esac
probes_report
if has "HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION"; then
	echo "   ROOD panic of exception"
	fail=1
fi
verdict uefi
echo "uefi-poort groen"
