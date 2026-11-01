#!/bin/sh
# De QEMU-poort van HopOS v3 op riscv64 (docs/boards-riscv.md): de kern in
# machine mode op -M virt met -bios none (HopOS is zelf de monitor, zoals
# op de LicheeRV), twee harts, 1 GB, een virtio-net met user-net en een
# verse virtio-blk-schijf. Slaagt alleen als de markers er staan:
#
#   HOPOS_BOOT      de kern haalde kmain, de console en de executor;
#   HOPOS_TICK 3    drie seconden executor, de CLINT-slaap en de IRQ-deur;
#   HOPOS_NIC_UP    de virtio-net over mmio;
#   HOPOS_DISK_UP   de virtio-blk over mmio;
#   HOPOS_FS_UP     hopfs erop, vers;
#   HOPOS_NET_UP    pomp, switch, poort 0 en een DHCP-lease van user-net;
#   HOPOS_RV_CAGE_UP  de zelftest van de kooi op hart 1
#                   (board/qemuvirt-riscv/src/cage.rs): de M-mode-switcher
#                   parkeert op zijn CLINT, de kick (msip) wekt hem, een
#                   bewoner boot koud in S-mode achter PMP (TOR) plus Sv39
#                   (LINK_BASE naar de pool), yieldt, wordt hervat op mepc+4
#                   en exit; een tweede die naar de kern schrijft faalt op de
#                   whitelist (mcause 7); een derde die nooit yieldt, velt de
#                   kill-tick na zijn intrekking; een vierde die slaapt tot
#                   nooit, gaat dood bij de ronde na zijn intrekking;
#   HOPOS_RV_HART_UP  de kooi-lijm (hopos/src/cage_riscv.rs) zette hart 1
#                   klaar: sched-blok, wekker, bel, slaap en kill-tick;
#   HOPOS_SYSTEM_UP de system-listener op poort 10100;
#   extern          een TCP-verbinding van de host via hostfwd naar
#                   10.0.2.15:10100 die de kern accepteert en weigert (geen
#                   slot achter 10.0.2.2): de host ziet EOF en de kern meldt
#                   een NIEUWE regel HOPOS_SYSTEM_REFUSED. Drie keer, op de
#                   momenten van tools/qemu-test.sh: meteen na
#                   HOPOS_SYSTEM_UP, zodra de app van slot 1 zijn netwerk
#                   heeft, en na de stop van slot 2;
#   appspike        het ABI-bewijs op riscv64 (standaard aan; APP= slaat het
#                   over): appspike, door QEMU gestaged, in slot 1 op hart 1
#                   door de lifecycle van de kern (PMP plus Sv39, de
#                   M-mode-switcher), al zijn toetsen groen via de servicer op
#                   de console (HOPOS_APPSPIKE_DONE pass=9 fail=0: control-page,
#                   env, log, frame, klok, heartbeat, net met de system-API,
#                   hopfs en heap), exit 0 (HOPOS_SLOT_DONE) en de bevestigde
#                   stop (HOPOS_SLOT_STOPPED); daarna hetzelfde in slot 2 op
#                   hetzelfde hart, nu warm (de switcher draait al).
#
# Rood is rood: een ontbrekende marker, een HOPOS_PANIC of een
# HOPOS_EXCEPTION drukt de hele console af en faalt.
#
#   tools/qemu-riscv-test.sh            TIMEOUT=40 standaard, in seconden
#   APP= tools/qemu-riscv-test.sh       alleen de boot-poort (geen slots)
#   KEEP_LOG=pad tools/qemu-riscv-test.sh   bewaart ook een groene console
#   SYSPORT=poort                       de host-kant van de hostfwd;
#                                       standaard een vrije van het OS
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-40}"
TARGET=riscv64gc-unknown-none-elf
APP="${APP-appspike}"
scratch rv
ports SYS

cd "$DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt-riscv
KERNEL="$DIR/target/$TARGET/release/hopos"
truncate -s 64m "$DISK"

# De kern gebruikt geen f-registers: daarop rust de overgang naar een
# bewoner op de OS-core (cpu/src/riscv/oscore.rs), die de f-registers van de
# kern niet bewaart. De uitzonderingen zijn de switcher van de app-harts
# (__hopos_parkenter tot __hopos_mmode_end, cpu/src/riscv/switch.rs) en die
# overgang zelf (__hopos_os_enter tot __hopos_os_end): die bewaren juist de
# f-registers van hun bewoners, met de hand geschreven, en tellen hier niet
# mee. Met een riscv-objdump in het PATH wordt dat hier getoetst.
if command -v riscv64-elf-objdump >/dev/null 2>&1; then
	SW_LO=$(riscv64-elf-nm "$KERNEL" | awk '$3 == "__hopos_parkenter" { print $1 }')
	SW_HI=$(riscv64-elf-nm "$KERNEL" | awk '$3 == "__hopos_mmode_end" { print $1 }')
	OS_LO=$(riscv64-elf-nm "$KERNEL" | awk '$3 == "__hopos_os_enter" { print $1 }')
	OS_HI=$(riscv64-elf-nm "$KERNEL" | awk '$3 == "__hopos_os_end" { print $1 }')
	# Veld 3 van objdump is het mnemonic (adres, hex, mnemonic, operanden):
	# zo telt een hexwoord als `fadde7e3` niet mee. Alles min de switcher en
	# de overgang.
	fp_count() {
		riscv64-elf-objdump -d "$@" "$KERNEL" | awk -F'\t' '$3 ~ /^(fl[dw]|fs[dw]|fmv\.|fcvt|fadd|fsub|fmul|fdiv|fsqrt|fsgnj|fmin|fmax|fmadd|fmsub|fnm|feq|flt|fle|fclass|fr?csr|fs?csr|frflags|fsflags)/ { n++ } END { print n + 0 }'
	}
	FP_ALL=$(fp_count)
	FP_SW=$(fp_count --start-address="0x${SW_LO:-0}" --stop-address="0x${SW_HI:-0}")
	FP_OS=$(fp_count --start-address="0x${OS_LO:-0}" --stop-address="0x${OS_HI:-0}")
	FP=$((FP_ALL - FP_SW - FP_OS))
	if [ "$FP" != 0 ]; then
		echo "FAIL: the kern image carries $FP FP instructions outside the switcher and the OS-core switch, which do not save the kern's own f0..f31 (cpu/src/riscv/oscore.rs)"
		exit 1
	fi
fi

# De staging: het image zonder debug-info, rol 0 (app).
STAGED=""
if [ -n "$APP" ]; then
	cargo build --quiet --release --target "$TARGET" -p "$APP"
	STAGED="$ART/stage.elf"
	strip_elf "$DIR/target/$TARGET/release/$APP" "$STAGED"
	fits "$STAGED" "$APP"
fi
qemu_rv "$STAGED" 0 </dev/null >"$LOG" 2>&1 &
QPID=$!

# De vaste markers, en die van de appspike-keten (grep -E, per slot).
NEED="HOPOS_BOOT|HOPOS_TICK 3 |HOPOS_NIC_UP|HOPOS_DISK_UP|HOPOS_FS_UP fresh=1|HOPOS_NET_UP|HOPOS_RV_CAGE_UP|HOPOS_SYSTEM_UP"
PROBE_AT=""
if [ -n "$APP" ]; then
	NEED="$NEED|HOPOS_RV_HART_UP|HOPOS_SYSTEM_REFUSED"
	for i in 1 2; do
		NEED="$NEED|HOPOS_SLOT_START slot=$i|slot $i: HOPOS_APPSPIKE_NETLOG|slot $i: HOPOS_APPSPIKE_FS ok|slot $i: HOPOS_APPSPIKE_DONE pass=9 fail=0|HOPOS_SLOT_DONE slot=$i exit=0|slot $i: stopped.*HOPOS_SLOT_STOPPED"
	done
	PROBE_AT="HOPOS_SYSTEM_UP|slot 1: .*HOPOS_APPNET_UP|slot 2: stopped.*HOPOS_SLOT_STOPPED"
fi

# De toetsen van buiten op hun momenten (tools/lib.sh probe_at).
elapsed=0
while :; do
	probe_due && continue
	! probes_left && all "$NEED" && break
	has "HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION" && break
	kill -0 "$QPID" 2>/dev/null || break
	[ "$elapsed" -ge "$((TIMEOUT * 10))" ] && break
	sleep 0.1
	elapsed=$((elapsed + 1))
done
qemu_stop

fail=0
marks "$NEED"
probes_report
if has "HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION"; then
	echo "   ROOD panic of exception"
	fail=1
fi
if [ "$fail" != 0 ]; then
	KEEP="${LOG}.rood"
	cp "$LOG" "$KEEP"
	echo "== console bewaard in $KEEP"
	tr -d '\r' <"$LOG"
	echo
	echo "FAIL: qemu riscv64"
	exit 1
fi
if [ -n "${KEEP_LOG:-}" ]; then tr -d '\r' <"$LOG" >"$KEEP_LOG"; fi
echo "qemu riscv64 groen in $((elapsed / 10)) s"
