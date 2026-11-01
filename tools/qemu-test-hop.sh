#!/bin/sh
# De kring van HopOS v3 op QEMU: de kern start Hop, en Hop plaatst een app.
#
# De kern boot met agentd-hopos (de bewoner uit de hop-repo, gestaged door
# image/qemu-run.sh met de rol Hop) in slot 1, met het Privilege-token, de
# HOPOS_*-env, de wandklok en de poorten 8080/9080 van de uplink doorgezet
# naar het slot. Van buiten gaat er daarna één jobspec naar de leader van
# Hop; Hop haalt appspike van een artifact-server op de host (voor de gast
# 10.0.2.2), stroomt hem via de bevoegde system-API de kern in, en de kern
# plaatst hem in slot 2. Groen alleen als:
#
#   kern        HOPOS_BOOT, HOPOS_CLOCK_FIXED, HOPOS_PRIVILEGE, HOPOS_NET_UP,
#               HOPOS_SYSTEM_UP, HOPOS_DISK_UP en HOPOS_FS_UP fresh=1 (een
#               verse schijf per run), HOPOS_HOP_START slot=1, één
#               HOPOS_SLOT_PUBLISH van slot 1 (8080 en 9080), en de canary
#               van de watchdog: "self-dial 10.100.0.2:8080 connected ...
#               HOPOS_WD_CANARY_OK" (een nieuwe verbinding van de kern door
#               de switch naar de accept-laag van Hop);
#   Hop         via de servicer van slot 1: HOP_UP en HOP_LEADER, en
#               het zaad van de kern op zijn control-page: HOPOS_RNG_SLOTS
#               source=jitter (virt heeft geen TRNG) en "applib: rng seed
#               from the kernel ... HOPOS_APP_RNG source=jitter" (applib
#               van deze werkboom, dus ook met een Hop van vóór het zaad);
#   van buiten  POST http://127.0.0.1:$LEADERPORT/v1/jobs wordt aangenomen
#               (onbeveiligd: de kern geeft Hop HOPOS_INSECURE=1);
#   de plaatsing HOP_JOB_PLACED slot=2, HOPOS_SLOT_START slot=2 en
#               "slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0" (met de
#               FS-toets in de eigen root van slot 2);
#   Hop's staat geen agent-state.json op hopfs (Hop 3.0.2: alleen de
#               object-store of de init-jobs);
#   de herstart  een tweede boot op dezelfde schijf: HOPOS_FS_UP fresh=0
#               ("hopfs: tree restored"), en Hop begint schoon (HOP_UP, geen
#               HOP_ADOPTED, geen zwerver);
#   van buiten  GET http://127.0.0.1:$AGENTPORT/tasks toont de taak van
#               "spike" als running. Een exit bestaat daar niet als staat:
#               appspike stopt met code 0 (applib: shutdown code=0) en Hop
#               herstart een service, dus de toets pollt tot hij running ziet.
#
# De OS-core (PORT.md beslissing 2): Hop deelt de core van de kern
# (HOPOS_HOP_START slot=1 core=0, cpu = de OS-core), de overgang bewees
# zichzelf bij boot (HOPOS_OS_SELFTEST ok: terug op de timer, een yield en
# de kick-SGI), en appspike landt op de eerste app-core (HOPOS_SLOT_START
# slot=2 core=1). Met SMP=2 is dat de enige andere core; met OSCORE=1
# verhuist de kern eerst naar core 1 en wordt core 0 die app-core
# (HOPOS_OSCORE_PARKED, en cpu=0 voor slot 2).
#
# Een HOPOS_PANIC, HOPOS_EXCEPTION, HOPOS_HOP_FAULT, HOPOS_HOP_EXIT,
# HOP_STATE_SKIPPED, of een OS-core die niet deelt (HOPOS_OS_SELFTEST_FAIL,
# HOPOS_OS_CORE_FAIL, HOPOS_CAGE_FAIL; een OS-core die niet gehonoreerd
# wordt is HOPOS_FATAL) is meteen rood.
# Rood bewaart de console (en drukt hem af).
#
# Dezelfde kring op riscv64 is tools/qemu-riscv-test-hop.sh: dit script
# met HOP_ARCH=riscv64 (QEMU virt in machine mode, twee harts, Hop op hart
# 0, de app op hart 1; zonder het zaad en de canary, met HOPOS_OS_CORE_UP).
#
#   tools/qemu-test-hop.sh                 TIMEOUT=60 standaard (riscv64 90),
#                                          in seconden
#   SMP=2 tools/qemu-test-hop.sh           twee cores (standaard 4)
#   SMP=2 OSCORE=1 tools/qemu-test-hop.sh  de kern en Hop op core 1
#   KEEP_LOG=pad tools/qemu-test-hop.sh    bewaart ook een groene console
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT   de host-poorten; standaard vrije
#                                          van het OS (bezet = een vrije, luid)
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
#   HOP_PATCH=0                            Hop tegen de tag van de hop-repo in
#                                          plaats van de applib van deze
#                                          werkboom (tools/hop-build.sh)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
case "${HOP_ARCH:-arm64}" in
arm64)
	TIMEOUT="${TIMEOUT:-60}"
	TARGET=aarch64-unknown-none-softfloat
	BOARD=qemuvirt
	NAME=qemu-hop
	OSCPU="${OSCORE:-0}"
	APPCPU=1
	[ "$OSCPU" = 0 ] || APPCPU=0
	;;
riscv64)
	TIMEOUT="${TIMEOUT:-90}"
	TARGET=riscv64gc-unknown-none-elf
	BOARD=qemuvirt-riscv
	NAME=rv-hop
	OSCPU=0
	APPCPU=1
	POST_MAX=30
	;;
*)
	echo "HOP_ARCH=$HOP_ARCH: arm64 of riscv64" >&2
	exit 64
	;;
esac
scratch "$NAME"
ports SYS AGENT LEADER ART

cd "$DIR"
echo "== bouwen: hopos ($BOARD), appspike, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features "board-$BOARD"
KERNEL="$DIR/target/$TARGET/release/hopos"
# De artifact-server: appspike zonder debug-info (1,8 MB naar 226 KB,
# gemeten 29-09).
apps appspike
hop_elf
serve

# De boot: op arm64 image/qemu-run.sh (Hop gestaged met de rol Hop), op
# riscv64 Hop rauw op de staging met rol 1.
if [ "$TARGET" = riscv64gc-unknown-none-elf ]; then
	strip_elf "$HOP_ELF" "$ART/hop.elf"
	fits "$ART/hop.elf" agentd-hopos
	truncate -s 64m "$DISK"
	# Stateful, zoals de arm64-tak: de koude herstart moet de boom terugvinden.
	boot() {
		BOOTARGS=hopos.storage=stateful qemu_rv "$ART/hop.elf" 1 </dev/null >"$LOG" 2>&1 &
		QPID=$!
	}
	echo "== booten op QEMU virt riscv64 met Hop op hart 0 (tot ${TIMEOUT}s; system :$SYSPORT, agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT)"
else
	# Stateful: deze ring toetst dat de boom een koude herstart overleeft
	# (zonder venster is een node stateless, sinds 3.0.11).
	boot() { hop_virt BOOTARGS=hopos.storage=stateful; }
	echo "== booten op QEMU virt met Hop, ${SMP:-4} cores, OS-core $OSCPU (tot ${TIMEOUT}s; system :$SYSPORT, agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT)"
fi
boot

# De vaste markers (grep -E), in de volgorde waarin ze horen te komen.
BOOT_MARKS="HOPOS_BOOT|HOPOS_CLOCK_FIXED|HOPOS_PRIVILEGE|HOPOS_DISK_UP model=virtio-blk|HOPOS_FS_UP fresh=1|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_OS_SELFTEST ok"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_OS_SELFTEST_FAIL|HOPOS_OS_CORE_FAIL"
if [ "$TARGET" = riscv64gc-unknown-none-elf ]; then
	BOOT_MARKS="$BOOT_MARKS|HOPOS_OS_CORE_UP"
	RED="$RED|HOPOS_OS_CORE_NONE|HOPOS_CAGE_FAIL"
else
	RED="$RED|HOPOS_CAGE_FAIL"
fi
BOOT_MARKS="$BOOT_MARKS|HOPOS_HOP_START slot=1 core=0 cpu=$OSCPU |slot 1: 2 port\\(s\\) published tcp\\+udp on the uplink: :8080 :9080 HOPOS_SLOT_PUBLISH|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
if [ "$TARGET" != riscv64gc-unknown-none-elf ]; then
	BOOT_MARKS="$BOOT_MARKS|HOPOS_RNG_SLOTS source=jitter|slot 1: applib: rng seed from the kernel .*HOPOS_APP_RNG source=jitter|self-dial 10.100.0.2:8080 connected .*HOPOS_WD_CANARY_OK"
	[ "$OSCPU" = 0 ] || BOOT_MARKS="$BOOT_MARKS|HOPOS_OSCORE_PARKED"
fi
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|HOPOS_SLOT_START slot=2 core=1 cpu=$APPCPU |slot 2: HOPOS_APPSPIKE_FS ok|slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0"

# Na de POST: de plaatsing, dan de taak van buiten running (Hop herstart
# de service na zijn exit 0, dus even pollen).
TASKS=""
placed() { all "$PLACE_MARKS" && running spike; }
job_loop '{"name":"spike","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432}' placed

# De herstart hieronder leest terug wat er VASTGELEGD is: de FS-toets van
# appspike schreef in zijn root, en de committer (elke 10 s,
# kern::rpc::COMMIT_EVERY) legt de boom dan vast. Daarop wachten, anders is
# de schijf bij de tweede boot nog vers.
committed() { has 'HOPOS_FS_COMMIT($| )'; }
if [ -n "$TASKS" ] && [ "${TASKS#(nog niet running)}" = "$TASKS" ]; then
	i=0
	while ! committed && [ "$i" -lt 150 ] && ! has "$RED"; do
		sleep 0.1
		i=$((i + 1))
	done
fi
qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS"
posted
case "$TASKS" in
"(nog niet running)"* | "")
	echo "   ROOD GET /tasks: geen running spike: ${TASKS:-nooit gevraagd}"
	fail=1
	;;
*) echo "   ok  GET /tasks: $TASKS" ;;
esac
served appspike.elf
reds
if committed; then
	echo "   ok  vastgelegd: $(tr -d '\r' <"$LOG" | grep -E 'HOPOS_FS_COMMIT($| )' | tail -1)"
else
	echo "   ROOD geen HOPOS_FS_COMMIT na de FS-toets van appspike"
	fail=1
fi
# Hop houdt geen staat op hopfs (sinds Hop 3.0.2): een agent-state.json is rood.
if has "agent-state.json"; then
	echo "   ROOD Hop schreef een agent-state.json; die staat bestaat niet meer"
	fail=1
else
	echo "   ok  geen agent-staat op hopfs"
fi
took
# De meetlat: de rtt van appspike's NET-toets en de laatste tik met de
# overgangen van de OS-core (in/irq/ipi/timer/yield en de tijd van Hop).
echo "   meting: $(tr -d '\r' <"$LOG" | grep -o 'dial_us=[0-9]*' | tr '\n' ' ')"
echo "   meting: $(tr -d '\r' <"$LOG" | grep -o 'os(in=.*' | tail -1)"
verdict "$NAME"

# De herstart: dezelfde schijf, een nieuwe boot. hopfs vindt de boom terug
# (fresh=0); Hop begint schoon, want hij houdt geen staat op hopfs.
echo "== herstart op dezelfde schijf (tot ${TIMEOUT}s)"
LOG1="$LOG"
LOG="$(mktemp -t "hopos-$NAME-2.XXXXXX")"
boot
RESTART_MARKS="HOPOS_FS_UP fresh=0|hopfs: tree restored|HOPOS_HOP_START slot=1 core=0 cpu=$OSCPU |slot 1: .*HOP_UP"
started
while alive && ! all "$RESTART_MARKS"; do step; done
qemu_stop
marks "$RESTART_MARKS"
# Schoon begonnen: niets overgenomen uit een bestand (Hop 3.0.2: alleen de
# object-store of de init-jobs), geen bewoner uit de vorige boot.
if has "HOP_ADOPTED|HOP_STRAY_STOPPED"; then
	echo "   ROOD $(first "HOP_ADOPTED|HOP_STRAY_STOPPED")"
	fail=1
else
	echo "   ok  Hop begon schoon: niets overgenomen, geen zwerver"
fi
reds
took "de herstart"
if [ "$fail" != 0 ]; then
	KEEP="$(mktemp -t "hopos-$NAME-rood.XXXXXX")"
	tr -d '\r' <"$LOG" >"$KEEP"
	echo "== console van de herstart bewaard in $KEEP"
	cat "$KEEP"
	rm -f "$LOG"
	exit 1
fi
if [ -n "${KEEP_LOG:-}" ]; then tr -d '\r' <"$LOG" >"$KEEP_LOG.restart"; fi
rm -f "$LOG"
LOG="$LOG1"
if [ "$TARGET" = riscv64gc-unknown-none-elf ]; then
	echo "qemu-kring riscv64 groen"
else
	echo "qemu-kring groen"
fi
