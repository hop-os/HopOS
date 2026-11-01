#!/bin/sh
# De vectortabel van EL1 op QEMU (applib::mmu, 30-09): appspike met
# `FAULT=1` in zijn env leest één woord net onder zijn RAM-declaratie,
# buiten zijn stage-1. Dat is een vertaalfout op EL1 (geen stage-2-fault:
# het IPA ligt buiten de map van de app zelf), en die moet de vectortabel
# van applib vangen en melden, niet een sprong naar een lege VBAR_EL1 (de
# eerste Pi 5-boot: `esr=0x82000005 far=0x200`, de tweede fault in plaats
# van de eerste). Hop plaatst appspike (een job met `"env":{"FAULT":"1"}`,
# sinds 01-11 de echte weg, docs/sequence-hopos.md §14.1 O7) in slot 2,
# en de kern drukt de fault bij de stop die Hop vraagt. Groen als de kern
# de échte fault drukt:
#
#   HOPOS_APP_MMU          de stage-1 staat aan (anders is er geen
#                          vertaalfout op EL1 te maken);
#   HOPOS_SLOT_FAULT       `fault at EL1 vec=4 esr=0x96000006 (data abort,
#                          translation fault) elr=... far=<het woord>`:
#                          vector 4 (huidige EL, SPx, synchroon), EC 0x25,
#                          en FAR het adres dat appspike las.
#
# Wat dit NIET bewijst: de alignment-val zelf. QEMU-TCG toetst alignment op
# Device niet zodra stage 2 aan staat (`aprofile_require_alignment` in
# target/arm/tcg/hflags.c geeft `false` bij HCR_EL2.VM), en de enige knop,
# SCTLR.A, toetst élke toegang, ook op Normal-geheugen: strenger dan het
# ijzer, dus geen toets van wat de Cortex-A76 doet. Dat bewijst de Pi 5.
#
#   tools/qemu-test-fault.sh           TIMEOUT=60 standaard, in seconden
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-60}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-fault
ports SYS AGENT LEADER ART

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), appspike en agentd-hopos"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
apps appspike
hop_elf
serve
echo "== booten op QEMU virt met Hop; Hop plaatst appspike FAULT=1 (tot ${TIMEOUT}s)"
hop_virt

BOOT_MARKS="HOPOS_SYSTEM_UP|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
JOB='{"name":"fault","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"env":{"FAULT":"1"}}'
MARKS="slot 2: applib: stage-1 on.*HOPOS_APP_MMU|slot 2: HOPOS_APPSPIKE_FAULT reading 0x4ffff000|slot 2: fault at EL1 vec=4 esr=0x96[0-9a-f]{6} \(data abort, translation fault\) elr=0x5[0-9a-f]+ far=0x4ffff000 HOPOS_SLOT_FAULT"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_APPSPIKE_FAULT FAIL|HOPOS_APP_NO_MMU"
POSTED=""
t=0
while [ "$t" -lt "$TIMEOUT" ]; do
	if has "$RED"; then
		break
	fi
	if [ -z "$POSTED" ] && all "$BOOT_MARKS"; then
		post_job "$JOB" || break
	fi
	ok=1
	IFS='|'
	for m in $MARKS; do
		has "$m" || ok=0
	done
	unset IFS
	[ "$ok" = 1 ] && break
	sleep 1
	t=$((t + 1))
done
unset IFS

fail=0
IFS='|'
for m in $MARKS; do
	if has "$m"; then
		echo "   ok  $(tr -d '\r' <"$LOG" | grep -m1 -E "$m")"
	else
		echo "   MIST  $m"
		fail=1
	fi
done
unset IFS
if has "$RED"; then
	echo "   ROOD  $(tr -d '\r' <"$LOG" | grep -m1 -E "$RED")"
	fail=1
fi
if [ "$fail" = 1 ]; then
	echo "== console"
	tr -d '\r' <"$LOG"
	echo "qemu-fault rood"
	exit 1
fi
echo "   tijd: $t s"
echo "qemu-fault groen"
