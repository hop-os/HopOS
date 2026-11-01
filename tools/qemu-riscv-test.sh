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
#                   een NIEUWE regel HOPOS_SYSTEM_REFUSED, meteen na
#                   HOPOS_SYSTEM_UP.
#
# Het ABI-bewijs met appspike loopt de echte weg: Hop plaatst hem
# (tools/qemu-riscv-test-hop.sh). De kern plaatst sinds 01-11 alleen Hop
# (docs/sequence-hopos.md §14.1 O7).
#
# Rood is rood: een ontbrekende marker, een HOPOS_PANIC of een
# HOPOS_EXCEPTION drukt de hele console af en faalt.
#
#   tools/qemu-riscv-test.sh            TIMEOUT=40 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-riscv-test.sh   bewaart ook een groene console
#   SYSPORT=poort                       de host-kant van de hostfwd;
#                                       standaard een vrije van het OS
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-40}"
TARGET=riscv64gc-unknown-none-elf
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

# Geen staging: de boot-poort zonder bewoner.
qemu_rv "" </dev/null >"$LOG" 2>&1 &
QPID=$!

# De vaste markers (grep -E), en de schijf op zijn PLIC-bron (01-11, O3).
NEED="HOPOS_BOOT|HOPOS_TICK 3 |HOPOS_NIC_UP|disk: virtio-blk at 0x[0-9a-f]+, plic source [0-9]+|HOPOS_DISK_UP|HOPOS_FS_UP fresh=1|HOPOS_NET_UP|HOPOS_RV_CAGE_UP|HOPOS_RV_HART_UP|HOPOS_SYSTEM_UP|HOPOS_SYSTEM_REFUSED"
PROBE_AT="HOPOS_SYSTEM_UP"

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
