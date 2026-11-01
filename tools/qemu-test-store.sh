#!/bin/sh
# De store-ops op QEMU: een app kopieert tussen zijn eigen map in een
# object-store en zijn hopfs-zicht, via de kern en Hop.
#
# Op de host draait een kleine S3-nep (tools/fakes3.py: één bucket,
# path-style, PUT/GET/DELETE/LIST, de Authorization-kop geëist, SigV4 niet
# nagerekend, wel de payload-hash), voor de gast op 10.0.2.2 (slirp), over
# http: de nep heeft geen certificaat, en Hop zegt dat luid
# (HOP_STORE_PLAIN_HTTP). De kern boot met Hop in slot 1 en hopos.s3.* in de
# bootargs (image/qemu-run.sh BOOTARGS, kern/src/nodecfg.rs geeft ze als
# HOPOS_S3_* aan Hop). Met een bucket is dat ook de lock van Hop's cluster,
# en een lease vraagt een gezette klok: die van de host, zoals in elke ring
# (tools/lib.sh sntp, HOP_CLOCK_SYNCED). Van buiten gaat er
# één jobspec naar de leader:
# appspike met ROLE=STORE (appspike/src/main.rs, Go's store_demo.go):
#
#   pull van iets dat er nooit was   een nette fout (NotFound)
#   push state.json                  de nep ziet PUT apps/hopos/spike/state.json
#   list ""                          precies [state.json], relatief aan de map
#   pull state.json -> copy.json     een vuil, langer bestand wordt vervangen
#   drop state.json, list ""         leeg
#
# Groen alleen als:
#
#   Hop       "HOP_STORE_UP" (de bucket onder apps/hopos/) en
#             HOP_STORE_PLAIN_HTTP;
#   de app    "slot 2: HOPOS_APPSPIKE_STORE ok" en de hele doorloop groen
#             (HOPOS_APPSPIKE_DONE pass=10 fail=0);
#   de kern   "store: slot 2 push done (size 30) HOPOS_STORE_DONE" en de
#             pull en drop idem, en de miss (HOPOS_STORE_MISS);
#   de nep    PUT, GET en DELETE van apps/hopos/spike/state.json, de 404 van
#             apps/hopos/spike/never-pushed.json, en geen 403 (elke call
#             getekend) en geen 400 (elke hash klopte).
#
# Rood is ook: HOPOS_APPSPIKE_STORE FAIL, HOP_STORE_FAIL, HOP_STORE_KERNEL,
# HOPOS_STORE_NO_SERVICE, HOPOS_STORE_FULL, en de rode markers van
# qemu-test-hop.sh. Rood bewaart de console (en drukt hem af).
#
#   tools/qemu-test-store.sh                TIMEOUT=90 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-store.sh
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/S3PORT/NTPPORT  de host-poorten
#                                           (standaard vrije van het OS)
#   HOP_DIR=pad                             de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-90}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-store
S3LOG="$ART/s3.log"
ports SYS AGENT LEADER ART S3

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), appspike, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
apps appspike
hop_elf
serve
python3 "$DIR/tools/fakes3.py" "$S3PORT" hop >"$S3LOG" 2>&1 &
PIDS="$PIDS $!"

S3ARGS="hopos.s3.endpoint=http://10.0.2.2:$S3PORT hopos.s3.bucket=hop hopos.s3.region=us-east-1 hopos.s3.key=hopkey hopos.s3.secret=hopsecret hopos.s3.pathstyle=1"
echo "== booten op QEMU virt met Hop en een S3-nep (tot ${TIMEOUT}s; agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT, s3 :$S3PORT)"
hop_virt BOOTARGS="$S3ARGS"

s3has() { grep -q -E "$1" "$S3LOG" 2>/dev/null; }
s3all() {
	(
		IFS='|'
		for m in $1; do s3has "$m" || exit 1; done
	)
}

BOOT_MARKS="HOPOS_BOOT|HOPOS_FS_UP fresh=1|HOPOS_SYSTEM_UP|slot 1: .*HOP_CLOCK_SYNCED|slot 1: .*HOP_STORE_PLAIN_HTTP|slot 1: .*HOP_STORE_UP|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
RUN_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|store: slot 2 pull: no such object HOPOS_STORE_MISS|store: slot 2 push done \\(size 30\\) HOPOS_STORE_DONE|store: slot 2 list done \\(size 1\\) HOPOS_STORE_DONE|store: slot 2 pull done \\(size 30\\) HOPOS_STORE_DONE|store: slot 2 drop done|store: slot 2 list done \\(size 0\\) HOPOS_STORE_DONE|slot 2: HOPOS_APPSPIKE_STORE ok|slot 2: HOPOS_APPSPIKE_DONE pass=10 fail=0"
S3_MARKS="S3 GET apps/hopos/spike/never-pushed.json 404|S3 PUT apps/hopos/spike/state.json 200|S3 GET \\?list prefix=apps/hopos/spike/ 200|S3 GET apps/hopos/spike/state.json 200|S3 DELETE apps/hopos/spike/state.json 204"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_CAGE_FAIL|HOPOS_APPSPIKE_STORE FAIL|HOP_STORE_FAIL|HOP_STORE_KERNEL|HOPOS_STORE_NO_SERVICE|HOPOS_STORE_FULL"
S3RED="S3 [A-Z]+ .* (403|400)$"

JOB='{"name":"spike","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"env":{"ROLE":"STORE"}}'
POSTED=""
started
while alive && ! s3has "$S3RED"; do
	if [ -z "$POSTED" ]; then
		if all "$BOOT_MARKS"; then post_job "$JOB" || break; fi
	elif all "$RUN_MARKS" && s3all "$S3_MARKS"; then
		break
	fi
	step
done
qemu_stop

fail=0
marks "$BOOT_MARKS" "$RUN_MARKS"
IFS_WAS="$IFS"
IFS='|'
for m in $S3_MARKS; do
	if s3has "$m"; then
		echo "   ok  nep-S3: $(grep -m1 -E "$m" "$S3LOG")"
	else
		echo "   ROOD nep-S3: $m ontbreekt"
		fail=1
	fi
done
IFS="$IFS_WAS"
posted
reds
if s3has "$S3RED"; then
	echo "   ROOD nep-S3: $(grep -m1 -E "$S3RED" "$S3LOG")"
	fail=1
fi
echo "   nep-S3: $(grep -c '^S3 ' "$S3LOG") regels; alle keys: $(grep -o 'apps/[^ ]*' "$S3LOG" | sort -u | tr '\n' ' ')"
took
if [ "$fail" != 0 ]; then
	echo "== nep-S3:"
	cat "$S3LOG"
fi
verdict qemu-store
echo "qemu-store groen"
