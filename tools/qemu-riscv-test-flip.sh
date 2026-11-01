#!/bin/sh
# De koude kern-flip op riscv64 (QEMU virt, machine mode, twee harts): de
# riscv-vorm van `COLD=1 tools/qemu-test-flip.sh`, met de hop-CLI als
# trigger (`hop flip <url> <sha256> --cold`, POST /flip naar de agent).
#
# Kern A (stempel A) boot met Hop op hart 0, zoals
# tools/qemu-riscv-test-hop.sh, en Hop plaatst een appspike die blijft
# (HOLD=1) op hart 1: een init-job (`hopos.init[]` in de bootargs,
# HOPOS_INIT_JOBS), want zo komen de apps na een koude flip terug (Hop
# bewaart zijn eigen jobs niet; de leader of de init-jobs dragen ze). Kern B is dezelfde bron met stempel B, als bundel van
# image/flip-bundle.sh virt-riscv. Groen alleen als:
#
#   warm        `hop flip` zonder --cold faalt: riscv64 flipt alleen koud
#               ("HOPOS_FLIP_REFUSED warm flip not on riscv64", HOP_FLIP_FAIL),
#               en kern A draait door;
#   koud        `hop flip --cold` slaagt (202): HOPOS_FLIP_COLD_ASKED,
#               HOPOS_FLIP_STAGED, HOP_FLIP_COLD_STOP (Hop stopt de appspike),
#               HOPOS_FS_FROZEN, "HOPOS_FLIP_COLD stopped=N cores_off=1" (hart
#               1 verliet het image: de uit-stub van de switcher) en
#               HOPOS_FLIP_JUMP gen=2;
#   kern B      HOPOS_BOOT gen=2 stamp=B, HOPOS_FLIP_BOOT gen=2
#               HOPOS_FLIP_COLD_BOOT, en ná de sprong: de kooi-zelftest op
#               hart 1 (HOPOS_RV_CAGE_UP: het hart kwam uit de stub naar de
#               nieuwe `_start` en de parkeerlus), Hop koud uit de staging
#               (HOPOS_HOP_START, HOP_UP), dezelfde hopfs-generatie (fresh=0),
#               HOPOS_FLIP_SETTLED, en de appspike terug: Hop plaatst de
#               init-job opnieuw (HOP_JOB_PLACED, HOPOS_SLOT_START core=1,
#               HOPOS_APPSPIKE_DONE pass=9);
#   Hop         twee HOP_BOOT's en twee HOPOS_HOP_START's in de console (koud
#               herstart), en GET /tasks antwoordt na de flip.
#
# Rood is ook: HOPOS_PANIC, HOPOS_EXCEPTION, een fout van Hop, en elke
# andere flip-weigering of -fout. Rood bewaart de console en drukt hem af.
#
#   tools/qemu-riscv-test-flip.sh           TIMEOUT=150 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-riscv-test-flip.sh   bewaart ook een groene console
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT    de host-poorten
#   HOP_DIR=pad                             de hop-repo (standaard ../hop/hop)
#   HOP_CLI=pad                             een hop-CLI; anders gebouwd uit de
#                                           kopie van tools/hop-build.sh
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-150}"
TARGET=riscv64gc-unknown-none-elf
scratch rv-flip
ports SYS AGENT LEADER ART

cd "$DIR"
echo "== bouwen: kern A (stempel A), bundel B (stempel B), appspike, agentd-hopos en de hop-CLI"
HOPOS_STAMP=A cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt-riscv
KERNEL="$DIR/target/$TARGET/release/hopos"
apps appspike
hop_elf
if [ -z "${HOP_CLI:-}" ]; then
	# De CLI uit dezelfde kopie van de hop-repo als Hop zelf, voor de host.
	(cd "$DIR/target/hop-patched-$TARGET/src" &&
		cargo build --quiet --release -p cli --target-dir "$DIR/target/hop-cli") >&2
	HOP_CLI="$DIR/target/hop-cli/release/hop"
fi
HOPOS_STAMP=B sh "$DIR/image/flip-bundle.sh" virt-riscv 2>&1 | grep '^flip-bundle:' | sed 's/^/   /'
BUNDLE=hopos-virt-riscv.flip
cp "$DIR/target/$BUNDLE" "$ART/$BUNDLE"
SHA="$(cat "$DIR/target/$BUNDLE.sha256")"

strip_elf "$HOP_ELF" "$ART/hop.elf"
serve
truncate -s 64m "$DISK"
HOLD_JOB='{"name":"holder","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"env":{"HOLD":"1"}}'

# QEMU met Hop op de staging, zoals tools/qemu-riscv-test-hop.sh. De kern
# van de flip komt niet van QEMU: kern A legt hem achter Hop in de staging.
qemu_rv "$ART/hop.elf" 1 -append "hopos.init[]=$HOLD_JOB" </dev/null >"$LOG" 2>&1 &
QPID=$!
echo "== booten op QEMU virt riscv64, kern A (tot ${TIMEOUT}s; agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT)"

# Alleen wat ná de sprong op de console kwam: kern B. Niet pas vanaf
# HOPOS_FLIP_BOOT, want de kooi-zelftest op hart 1 draait in de discover
# van het board, vóór de landing.
after() { tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_JUMP/ { f = 1 } f' | grep -q -E "$1"; }
all_after() {
	(
		IFS='|'
		for m in $1; do after "$m" || exit 1; done
	)
}

A_MARKS="HOPOS_BOOT gen=1 stamp=A|HOPOS_HOP_START slot=1 core=0 |slot 1: 2 port\\(s\\) published tcp\\+udp on the uplink: :8080 :9080 HOPOS_SLOT_PUBLISH|slot 1: .*HOP_UP|slot 1: .*job holder .*HOP_JOB_PLACED|HOPOS_SLOT_START slot=[2-9] core=1 "
FLIP_MARKS="HOPOS_FLIP_REFUSED warm flip not on riscv64|slot 1: .*HOP_FLIP_FAIL|HOPOS_FLIP_COLD_ASKED|HOPOS_FLIP_STAGED|slot 1: .*HOP_FLIP_COLD_STOP|HOPOS_FS_FROZEN generation=|HOPOS_FLIP_COLD stopped=|HOPOS_FLIP_JUMP gen=2|HOPOS_BOOT gen=2 stamp=B|HOPOS_FLIP_BOOT gen=2|HOPOS_FLIP_COLD_BOOT|HOPOS_FLIP_SETTLED"
AFTER_MARKS="HOPOS_RV_CAGE_UP|HOPOS_HOP_START slot=1 core=0 |slot 1: .*HOP_UP|HOPOS_FS_UP fresh=0|slot 1: .*job holder .*HOP_JOB_PLACED|HOPOS_SLOT_START slot=[2-9] core=1 |slot [2-9]: HOPOS_APPSPIKE_DONE pass=9 fail=0"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_OS_SELFTEST_FAIL|HOPOS_CAGE_FAIL|HOPOS_RV_CAGE_FAIL|HOPOS_FLIP_BLOB_BAD|HOPOS_FLIP_GUARD|HOPOS_FS_FREEZE_FAIL|HOPOS_FLIP_FAIL|HOPOS_FLIP_ADOPT|HOPOS_HOP_RESUMED|HOPOS_FLIP_COLD_CORE|HOPOS_FLIP_COLD_STOP_FAIL|HOPOS_FLIP_CORE_BACK|HOPOS_FLIP_REFUSED (sha256|bundle|flip ABI|switch|image|same|firmware|cold)"

URL="http://10.0.2.2:$ARTPORT/$BUNDLE"
hop() { "$HOP_CLI" --agent "127.0.0.1:$AGENTPORT" "$@" 2>&1; }
WARM=""
COLD=""
TASKS=""
started
while alive; do
	if [ -z "$COLD" ]; then
		# Kern A met Hop, en de appspike die blijft draait op hart 1.
		{ all "$A_MARKS" && has "slot [2-9]: HOPOS_APPSPIKE_DONE pass=9 fail=0"; } || {
			step
			continue
		}
		if [ -z "$WARM" ]; then
			# Eerst warm: riscv64 weigert dat vóór de sprong.
			if out="$(hop flip "$URL" "$SHA")"; then WARM="ROOD geslaagd: $out"; else WARM="geweigerd: $out"; fi
			step
			continue
		fi
		if out="$(hop flip "$URL" "$SHA" --cold)"; then COLD="$out"; else
			COLD="ROOD: $out"
			break
		fi
		step
		continue
	fi
	if all "$FLIP_MARKS" && all_after "$AFTER_MARKS"; then
		if t="$(curl -s -m 5 -w ' HTTP %{http_code}' "http://127.0.0.1:$AGENTPORT/tasks" 2>&1)"; then
			case "$t" in *"HTTP 200"*)
				TASKS="$t"
				break
				;;
			esac
		fi
	fi
	step
done
qemu_stop

fail=0
marks "$A_MARKS" "$FLIP_MARKS"
IFS_WAS="$IFS"
IFS='|'
for m in $AFTER_MARKS; do
	if after "$m"; then
		echo "   ok  na de flip: $(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_JUMP/ { f = 1 } f' | grep -m1 -E "$m")"
	else
		echo "   ROOD $m ontbreekt na de flip"
		fail=1
	fi
done
IFS="$IFS_WAS"
case "$WARM" in
geweigerd:*) echo "   ok  hop flip (warm) $WARM" ;;
*) echo "   ROOD hop flip (warm) had geweigerd moeten worden: ${WARM:-nooit gedaan}"; fail=1 ;;
esac
case "$COLD" in
"" | ROOD*) echo "   ROOD hop flip --cold: ${COLD:-nooit gedaan}"; fail=1 ;;
*) echo "   ok  hop flip --cold: $COLD" ;;
esac
case "$TASKS" in
*"HTTP 200"*) echo "   ok  GET /tasks na de flip: $TASKS" ;;
*) echo "   ROOD GET /tasks na de flip: ${TASKS:-nooit beantwoord}"; fail=1 ;;
esac
FROZEN="$(tr -d '\r' <"$LOG" | sed -n 's/.*HOPOS_FS_FROZEN generation=\([0-9]*\).*/\1/p' | head -1)"
MOUNTED="$(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | sed -n 's/.*HOPOS_FS_UP fresh=0 generation=\([0-9]*\).*/\1/p' | head -1)"
if [ -n "$FROZEN" ] && [ "$FROZEN" = "$MOUNTED" ]; then
	echo "   ok  hopfs: generatie $FROZEN vastgelegd door kern A, gemount door kern B (fresh=0)"
else
	echo "   ROOD hopfs: kern A bevroor generatie '${FROZEN:-?}', kern B mountte '${MOUNTED:-geen fresh=0}'"
	fail=1
fi
OFF="$(tr -d '\r' <"$LOG" | sed -n 's/.*HOPOS_FLIP_COLD stopped=\([0-9]*\) cores_off=\([0-9]*\).*/\1 \2/p' | head -1)"
set -- ${OFF:-x 0}
refused="$(count 'HOPOS_FLIP_REFUSED')"
if [ "${2:-0}" = 1 ] && [ "$refused" = 1 ]; then
	echo "   ok  koud: $1 bewoner(s) door de kern gestopt, hart 1 uit het image, één warme weigering ervoor"
else
	echo "   ROOD koud: gestopt/uit '${OFF:-?}', weigeringen $refused"
	fail=1
fi
boots="$(count 'slot 1: .*HOP_BOOT')"
starts="$(count 'HOPOS_HOP_START slot=1')"
if [ "$boots" = 2 ] && [ "$starts" = 2 ]; then
	echo "   ok  Hop koud herstart: 2x HOP_BOOT en 2x HOPOS_HOP_START over twee kernen"
else
	echo "   ROOD Hop: HOP_BOOT ${boots}x, HOPOS_HOP_START ${starts}x (verwacht 2x)"
	fail=1
fi
if grep -q "GET /$BUNDLE" "$ART/http.log" 2>/dev/null; then
	echo "   ok  artifact-server: de bundel is opgehaald"
else
	echo "   ROOD artifact-server: de bundel is nooit gevraagd"
	fail=1
fi
reds
took
verdict rv-flip
echo "qemu-flip groen (riscv64, cold)"
