#!/bin/sh
# Sharegroups op een app-core: twee kooien op één core, in coöperatieve
# rotatie, via Hop.
#
# QEMU met vier cores: de kern en Hop op de OS-core, drie app-cores. Van
# buiten gaan twee jobspecs naar de leader van Hop, allebei met
# `"tags":{"sharegroup":"demo"}` en `ROLE=SHARE` in de env (Hop maakt er
# `share: "demo"`, `pool_cores: 1` van, runner/hopos.rs). De kern reserveert
# voor de groep één app-core; het eerste lid krijgt hem met een gewoon
# startschot, het tweede komt erbij in de rotatie van die draaiende core
# (boot-pending, `el2::join`), met een eigen kooi en partitie. Beide leden
# yielden in hun idle, en de switcher geeft de core om de beurt. Groen alleen
# als:
#
#   kern        de boot-markers van tools/qemu-test-hop.sh (tot HOP_UP);
#   plaatsing   HOP_JOB_PLACED slot=2 en slot=3, beide HOPOS_SLOT_START op
#               dezelfde app-core (core=1), en het tweede lid in de rotatie
#               naast het eerste (HOPOS_SHARE_JOIN);
#   de apps     elk lid HOPOS_APPSPIKE_SHARE ok (CTRL_SHARED gezien, en zijn
#               beurten geteld: yields=...) en DONE pass=10 fail=0;
#   de stop     share-a stopt na zijn toetsen (exit 0; Hop herstart hem),
#               share-b blijft leven (HOLD=1): de stop van het ene lid laat
#               het andere en de groepsreservering staan, en share-a komt
#               terug in de rotatie naast share-b (een tweede
#               HOPOS_SHARE_JOIN, een derde SHARE ok, share-b één start).
#
# Rood is meteen: een paniek, een fault, een quarantaine, een lid dat niet
# opgepikt werd (HOPOS_SHARE_PENDING), of een toets die faalt.
#
#   tools/qemu-test-share.sh               TIMEOUT=120 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-share.sh  bewaart ook een groene console
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT   de host-poorten; standaard vrije
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-120}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-share
ports SYS AGENT LEADER ART

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), appspike, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
apps appspike
hop_elf
serve

echo "== booten op QEMU virt met Hop, 4 cores (tot ${TIMEOUT}s; leader :$LEADERPORT, artifacts :$ARTPORT)"
hop_virt SMP=4

BOOT_MARKS="HOPOS_BOOT|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_HOP_START slot=1 core=0|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 1: .*HOP_JOB_PLACED slot=3|HOPOS_SLOT_START slot=2 core=1 |HOPOS_SLOT_START slot=3 core=1 |joined shared core 1 next to 1 resident.*HOPOS_SHARE_JOIN|slot 2: HOPOS_APPSPIKE_SHARE ok .*shared=1|slot 3: HOPOS_APPSPIKE_SHARE ok .*shared=1|slot 2: HOPOS_APPSPIKE_DONE pass=10 fail=0|slot 3: HOPOS_APPSPIKE_DONE pass=10 fail=0"
AGAIN="HOPOS_APPSPIKE_SHARE ok"
JOIN="joined shared core 1 next to 1 resident.*HOPOS_SHARE_JOIN"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_APP_PANIC|HOPOS_SHARE_PENDING|HOPOS_CAGE_ROSTER|HOPOS_APPSPIKE_[A-Z_]* FAIL|HOPOS_CAGE_FAIL"

# share-b blijft na zijn toetsen leven (HOLD=1): de stop van share-a (exit 0,
# Hop herstart hem) gebeurt dan naast een levende buur.
job() {
	hold=""
	[ "$1" = share-b ] && hold=',"HOLD":"1"'
	printf '{"name":"%s","driver":"hop","artifacts":[{"url":"http://10.0.2.2:%s/appspike.elf"}],"memory_limit":33554432,"cpu_shares":1024,"tags":{"sharegroup":"demo"},"env":{"ROLE":"SHARE"%s}}' "$1" "$ARTPORT" "$hold"
}
POSTED=""
started
while alive; do
	if [ -z "$POSTED" ]; then
		if all "$BOOT_MARKS"; then
			posts=""
			for name in share-a share-b; do
				if post_job "$(job "$name")"; then
					posts="$posts $name: $POSTED"
				else
					posts="ROOD curl $name: ${POSTED#ROOD curl: }"
					break
				fi
			done
			POSTED="$posts"
			case "$POSTED" in ROOD*) break ;; esac
		fi
		step
		continue
	fi
	if all "$PLACE_MARKS" && [ "$(count "$AGAIN")" -ge 3 ] && [ "$(count "$JOIN")" -ge 2 ]; then
		break
	fi
	step
done
qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS"
n="$(count "$AGAIN")"
j="$(count "$JOIN")"
b="$(count "HOPOS_SLOT_START slot=3 ")"
if [ "$n" -ge 3 ] && [ "$j" -ge 2 ] && [ "$b" = 1 ]; then
	echo "   ok  share-a stopte en kwam terug naast de levende share-b: $n keer SHARE ok, $j keer HOPOS_SHARE_JOIN, share-b $b start"
else
	echo "   ROOD de stop van één lid: $n keer SHARE ok (>= 3), $j keer HOPOS_SHARE_JOIN (>= 2), share-b $b start(s) (moet 1)"
	fail=1
fi
case "$POSTED" in
*"share-a: "*"HTTP 2"*"share-b: "*"HTTP 2"*) echo "   ok  POST /v1/jobs:$POSTED" ;;
"") echo "   ROOD POST /v1/jobs nooit gedaan (Hop niet op tijd op)"; fail=1 ;;
*) echo "   ROOD POST /v1/jobs: $POSTED"; fail=1 ;;
esac
reds
took
echo "   marker: $(first 'slot 2: HOPOS_APPSPIKE_SHARE ok')"
echo "   marker: $(first 'slot 3: HOPOS_APPSPIKE_SHARE ok')"
echo "   kern:   $(first 'HOPOS_SHARE_JOIN')"
verdict qemu-share 150
echo "qemu-share groen"
