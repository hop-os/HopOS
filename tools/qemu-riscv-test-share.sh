#!/bin/sh
# Twee bewoners op het ene app-hart van QEMU virt riscv64, en één van de twee
# yieldt nooit: de tijdschijf van de switcher (cpu/src/riscv/switch.rs, de
# kill-tick als einde van de beurt) moet de ander zijn beurten blijven geven.
# De vorm van de LicheeRV sinds 3.0.3 (kooien tellen niet als cores): daar
# deelden vier apps één hart, en een CASE of TLS-handshake van seconden
# hield de rest zonder beurt (02-10, stulp onbereikbaar op :80).
#
#   kern        de boot-markers van tools/qemu-riscv-test-hop.sh (tot HOP_UP);
#   plaatsing   twee jobs via de leader: web (welcome op :80) en burn (bench
#               met BURN=1, werk zonder rust), in één sharegroup (Hop telt
#               een lid dat een lopende groep joint niet als core; zo delen
#               de jobs op de LicheeRV het ene app-hart), allebei op core=1, en
#               HOPOS_BENCH_BURN van slot 3: de hog rekent;
#   de meting   twintig GETs naar welcome terwijl burn rekent: allemaal een
#               antwoord, en geen enkele langzamer dan MAX_MS (standaard
#               1000 ms; op TCG is de tick van 10 ms zelf al grof). Zonder
#               tijdschijf antwoordt welcome nooit meer zodra burn begint.
#
# Rood is rood: een paniek, een fault, of een GET zonder antwoord.
#
#   tools/qemu-riscv-test-share.sh         TIMEOUT=120 standaard, in seconden
#   MAX_MS=500 ...                          de lat voor de traagste GET
#   KEEP_LOG=pad ...                        bewaart ook een groene console
#   HOP_DIR=pad                             de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-120}"
MAX_MS="${MAX_MS:-1000}"
TARGET=riscv64gc-unknown-none-elf
scratch rv-share
ports SYS AGENT LEADER ART WEB

cd "$DIR"
echo "== bouwen: hopos (qemuvirt-riscv), welcome, bench, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt-riscv
KERNEL="$DIR/target/$TARGET/release/hopos"
apps welcome bench
hop_elf
strip_elf "$HOP_ELF" "$ART/hop.elf"
serve
truncate -s 64m "$DISK"

echo "== booten op QEMU virt riscv64 met Hop op hart 0 (tot ${TIMEOUT}s; leader :$LEADERPORT, welcome :$WEBPORT)"
qemu_rv "$ART/hop.elf" 1 </dev/null >"$LOG" 2>&1 &
QPID=$!

BOOT_MARKS="HOPOS_BOOT|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_OS_CORE_UP|HOPOS_HOP_START slot=1 core=0 cpu=0 |slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
PLACE_MARKS="HOPOS_SLOT_START slot=2 core=1 cpu=1 |HOPOS_SLOT_START slot=3 core=1 cpu=1 |slot 3: BURN: .*HOPOS_BENCH_BURN"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_OS_SELFTEST_FAIL|HOPOS_OS_CORE_FAIL|HOPOS_OS_CORE_NONE|HOPOS_CAGE_FAIL|HOPOS_APP_PANIC"
WEB='{"name":"web","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"tags":{"sharegroup":"demo"},"ports":{"http":80}}'
BURN='{"name":"burn","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/bench.elf"}],"memory_limit":33554432,"tags":{"sharegroup":"demo"},"env":{"BURN":"1","BURN_WORK":"600","BURN_REST":"0"}}'
POSTED=""
started
while alive; do
	if [ -z "$POSTED" ] && all "$BOOT_MARKS"; then
		for j in "$WEB" "$BURN"; do
			POST_MAX=30 post_job "$j" || true
			echo "   POST: ${POSTED#ROOD curl: }" >>"$ART/posts"
			case "$POSTED" in *"HTTP 2"*) ;; *) POSTED="ROOD ${POSTED#ROOD curl: }"; break 2 ;; esac
		done
		POSTED="ok"
	fi
	[ "$POSTED" = ok ] && all "$PLACE_MARKS" && break
	step
done

# De meting: twintig GETs naar welcome terwijl burn rekent (welcome kreeg
# eerst even de tijd om zijn stack op te zetten).
MEASURED=""
if [ "$POSTED" = ok ] && all "$PLACE_MARKS" && ! has "$RED"; then
	sleep 2
	MEASURED="$(for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
		curl -s -m 5 -o /dev/null -w "%{http_code} %{time_total}\n" "http://127.0.0.1:$WEBPORT/" || echo "000 5"
	done)"
fi

# De boekhouding van Hop, voor als het rood is.
TASKS="$(curl -s -m 5 "http://127.0.0.1:$AGENTPORT/v1/tasks" 2>&1 || true)"
JOBS="$(curl -s -m 5 "http://127.0.0.1:$LEADERPORT/v1/jobs" 2>&1 || true)"
qemu_stop
fail=0
IFS_WAS="$IFS"
IFS='|'
for m in $BOOT_MARKS $PLACE_MARKS; do
	if has "$m"; then
		echo "   ok  $m"
	else
		echo "   ROOD $m ontbreekt"
		fail=1
	fi
done
IFS="$IFS_WAS"
case "$POSTED" in
ok) echo "   ok  POST /v1/jobs: web en burn" ;;
"") echo "   ROOD POST /v1/jobs nooit gedaan (Hop niet op tijd op)"; fail=1 ;;
*) echo "   $POSTED"; fail=1 ;;
esac
if [ -n "$MEASURED" ]; then
	echo "$MEASURED" | awk -v max="$MAX_MS" '
		$1 == "200" { n++; ms = $2 * 1000; if (ms > worst) worst = ms; sum += ms; next }
		{ bad++ }
		END {
			printf "   %s welcome naast burn: %d van 20 antwoorden, traagste %.0f ms, gemiddeld %.0f ms (lat %s ms)\n",
				(bad == 0 && worst <= max) ? "ok " : "ROOD", n, worst, (n ? sum / n : 0), max
			exit !(bad == 0 && worst <= max)
		}' || fail=1
else
	echo "   ROOD geen meting: burn of welcome kwam niet op"
	fail=1
fi
reds
took
if [ "$fail" != 0 ]; then
	KEEP="$(mktemp -t hopos-rv-share-rood.XXXXXX)"
	tr -d '\r' <"$LOG" >"$KEEP"
	cat "$ART/posts" 2>/dev/null
	echo "   leader /v1/jobs: $(printf '%s' "$JOBS" | cut -c1-400)"
	echo "   agent /v1/tasks: $(printf '%s' "$TASKS" | cut -c1-600)"
	echo "== console bewaard in $KEEP"
	echo "ROOD"
	exit 1
fi
if [ -n "${KEEP_LOG:-}" ]; then tr -d '\r' <"$LOG" >"$KEEP_LOG"; fi
echo "GROEN"
