#!/bin/sh
# De meetketen van een devicedag op QEMU: de kern start Hop, Hop plaatst
# apps/bench met een gepubliceerde poort, en tools/netmeter meet van de
# host door de hostfwd. QEMU-getallen zijn geen ijzer-getallen (TCG, slirp
# termineert TCP op de host); wat deze toets bewijst is de KETEN, zodat
# morgen op elk board hetzelfde commando dezelfde tabel geeft.
#
# Twee boots:
#
#   1. Hop + bench, met hopos.idlestat=1 in de bootargs.
#      kern        HOPOS_BOOT, HOPOS_IDLESTAT_ON, HOPOS_NET_UP,
#                  HOPOS_HOP_START slot=1, Hop's poorten, HOP_UP en HOP_LEADER;
#      bench       POST /v1/jobs met "ports":{"http":80}: HOP_JOB_PLACED,
#                  "slot 2: 1 port(s) published ... :80 HOPOS_SLOT_PUBLISH"
#                  en "slot 2: ... HOPOS_BENCH_UP role=serve port=80";
#      netmeter    van de host naar 127.0.0.1:$WEBPORT (gast :80): rtt, storm,
#                  in en out, alle vier zonder fout, out zonder foute byte
#                  (--json, de rij voor docs/measurements.md);
#      in de node  twee jobs tegen de bench in slot 2, door de switch:
#                  BENCH=pull (HOPOS_BENCH_PULL, bad=0) en BENCH=ping
#                  (HOPOS_BENCH_RTT en HOPOS_BENCH_COLD); daarna weg;
#      last        BURN=1 met een ritme van 5 s werk / 5 s rust:
#                  HOPOS_BENCH_BURN, met de idlestat-regels ernaast;
#      multicast   de bench van poort 80 maakt plaats, MCAST=listen joint
#                  224.0.0.251 (HOPOS_BENCH_UP role=mcast-listen), MCAST=send
#                  zendt, de switch floodt: HOPOS_BENCH_MCAST recv=3;
#      de meetlat  minstens drie HOPOS_IDLESTAT-regels.
#   2. Zonder app, met hopos.nvmebench=1 op een verse schijf: de Go-tabel
#      per commandomaat, sequentieel, willekeurig en hopfs
#      (HOPOS_NVMEBENCH_SEQ, _RAND, "hopfs bench:" en HOPOS_NVMEBENCH_DONE),
#      en daarna mount de opslag dezelfde schijf (HOPOS_FS_UP): de bench
#      leent hem, `probe_disk` gebeurt één keer. HOPOS_FATAL is rood.
#
# UDP meet netmeter wel, maar niet hier: de hostfwd van image/qemu-run.sh
# is alleen TCP. Op ijzer: `netmeter NODE:PORT --phases udp`.
#
# Een HOPOS_PANIC, HOPOS_EXCEPTION, HOPOS_APP_PANIC, HOPOS_HOP_FAULT,
# HOPOS_HOP_EXIT, HOPOS_HOP_FAIL, HOPOS_SLOT_PUBLISH_FAIL of
# HOPOS_BENCH_FAIL is meteen rood. Rood bewaart de console (en drukt hem af).
#
#   tools/qemu-test-bench.sh               TIMEOUT=90 standaard, in seconden
#   BYTES=16777216 tools/qemu-test-bench.sh   bytes per doorvoerfase
#   NVME=0 tools/qemu-test-bench.sh        zonder de tweede boot
#   WIRE=1 NVME=0 TIMEOUT=180 CFG=c tools/qemu-test-bench.sh
#                                          onder een trage draad met een paar
#                                          ms RTT (image/qemu-run.sh WIRE=1,
#                                          WIRE_BURST/WIRE_TIMER/WIRE_DELAY_US);
#                                          met `hopos.tick=1` en
#                                          `hopos.virtio.txq=16` in c de
#                                          zendring die volloopt. Lees de
#                                          netmeter out en de laatste
#                                          HOPOS_TICK: sw(txdrop xoff) en
#                                          nic(full held err). De slirp van
#                                          QEMU houdt het venster op ~50 KB,
#                                          dus een ring van 64 loopt onder
#                                          één stroom nooit vol (24-10)
#   KEEP_LOG=pad / KEEP_JSON=pad           bewaart console en netmeter-json
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/WEBPORT   de host-poorten; standaard
#                                          vrije van het OS
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-90}"
BYTES="${BYTES:-16777216}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-bench
JSON="$ART/netmeter.json"
ports SYS AGENT LEADER ART WEB

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), bench, netmeter (host) en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
# De artifact-server: bench zonder debug-info, de symbolen blijven voor de
# plaatsing.
apps bench
cargo build --quiet --release -p netmeter
NETMETER="$DIR/target/release/netmeter"
hop_elf
serve

# De bootargs: idlestat, en de OS-core als die gevraagd is (qemu-run.sh zet
# hopos.oscore ervoor, in één -append).
ARGS="hopos.idlestat=1"
[ -n "${OSCORE:-}" ] && ARGS="hopos.oscore=$OSCORE $ARGS"

echo "== boot 1: Hop + bench (tot ${TIMEOUT}s; leader :$LEADERPORT, artifacts :$ARTPORT, web :$WEBPORT -> gast :80; $ARGS)"
hop_virt BOOTARGS=hopos.idlestat=1

BOOT_MARKS="HOPOS_BOOT|HOPOS_IDLESTAT_ON|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_HOP_START slot=1 |slot 1: 2 port\\(s\\) published tcp\\+udp on the uplink: :8080 :9080 HOPOS_SLOT_PUBLISH|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 2: 1 port\\(s\\) published tcp\\+udp on the uplink: :80 HOPOS_SLOT_PUBLISH|slot 2: .*HOPOS_BENCH_UP role=serve port=80"
NODE_MARKS="HOPOS_BENCH_PULL|HOPOS_BENCH_RTT|HOPOS_BENCH_COLD"
BURN_MARKS="HOPOS_BENCH_UP role=burn|HOPOS_BENCH_BURN_WORK|HOPOS_BENCH_BURN$"
MCAST_MARKS="HOPOS_BENCH_UP role=mcast-listen|HOPOS_BENCH_MCAST recv=3"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_APP_PANIC|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_SLOT_PUBLISH_FAIL|HOPOS_BENCH_FAIL"

started
job() { # naam env-json
	curl -s -m 20 -w ' HTTP %{http_code}' -X POST -H 'Content-Type: application/json' \
		-d '{"name":"'"$1"'","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/bench.elf"}],"memory_limit":67108864'"$2"'}' \
		"http://127.0.0.1:$LEADERPORT/v1/jobs" 2>&1 || true
}
stop() { curl -s -m 20 -o /dev/null -w '%{http_code}' -X DELETE "http://127.0.0.1:$LEADERPORT/v1/jobs/$1" 2>&1 || true; }

POSTED=""
METER=""
METER_RC=""
NODE_OK=""
BURN_OK=""

# 1. Boot, dan de bench als job met poort 80.
while alive && [ -z "$POSTED" ]; do
	all "$BOOT_MARKS" && POSTED="$(job bench ',"ports":{"http":80}')"
	step
done

# 2. De plaatsing, dan netmeter van buiten.
while alive && [ -z "$METER_RC" ]; do
	if all "$PLACE_MARKS"; then
		set +e
		"$NETMETER" "127.0.0.1:$WEBPORT" --phases rtt,storm,in,out --rtt 200 --storm 100 \
			--bytes "$BYTES" --label "qemu virt TCG, hostfwd" --json >"$JSON" 2>"$ART/netmeter.txt"
		METER_RC=$?
		set -e
		break
	fi
	step
done

# 3. In de node: pull en ping tegen de bench in slot 2, door de switch.
PEER="$(tr -d '\r' <"$LOG" | grep -m1 'HOPOS_BENCH_UP role=serve' | grep -o 'tcp [0-9][0-9.]*:80' | cut -d' ' -f2 || true)"
# Eén client-job tegelijk: Hop plant tegen de app-cores die hij kent, en
# de bench in slot 2 heeft er al één. Een client blijft na zijn meting
# staan (HOPOS_BENCH_HOLD); de toets stopt hem.
client() { # naam env-json markers
	job "$1" ',"env":{'"$2"'}' >/dev/null
	while alive && ! all "$3"; do step; done
	stop "$1" >/dev/null
	all "$3"
}
if [ "$METER_RC" = 0 ] && [ -n "$PEER" ]; then
	client pull '"BENCH":"pull","BENCH_PEER":"'"$PEER"'","BENCH_BYTES":"'"$BYTES"'"' "HOPOS_BENCH_PULL" &&
		client ping '"BENCH":"ping","BENCH_PEER":"'"$PEER"'"' "HOPOS_BENCH_RTT|HOPOS_BENCH_COLD" &&
		NODE_OK=1
fi

# 4. Last: BURN met een kort ritme, en de idlestat-regels ernaast.
if [ -n "$NODE_OK" ]; then
	client burn '"BURN":"1","BURN_WORK":"5","BURN_REST":"5"' "$BURN_MARKS" && BURN_OK=1
fi

# 5. Multicast: twee jobs tegelijk, dus de bench van poort 80 gaat eerst weg.
if [ -n "$BURN_OK" ]; then
	stop bench >/dev/null
	job mlisten ',"env":{"MCAST":"listen"}' >/dev/null
	while alive && ! has 'role=mcast-listen'; do step; done
	client msend '"MCAST":"send"' "$MCAST_MARKS" || true
	stop mlisten >/dev/null
fi
sleep 1
qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS" "$NODE_MARKS" "$BURN_MARKS" "$MCAST_MARKS"
case "$POSTED" in
*"HTTP 2"*) echo "   ok  POST /v1/jobs bench: $POSTED" ;;
*) echo "   ROOD POST /v1/jobs bench: ${POSTED:-nooit gedaan}"; fail=1 ;;
esac
echo "== netmeter (exit ${METER_RC:-nooit gedraaid}):"
[ -s "$ART/netmeter.txt" ] && sed 's/^/   /' "$ART/netmeter.txt"
if [ "$METER_RC" != 0 ]; then
	echo "   ROOD netmeter"
	fail=1
elif ! grep -q 'NETMETER phase=out .* lost=0' "$ART/netmeter.txt"; then
	echo "   ROOD netmeter: out had foute of ontbrekende bytes"
	fail=1
fi
IDLE_N="$(count 'HOPOS_IDLESTAT$')"
if [ "$IDLE_N" -ge 3 ]; then
	echo "   ok  $IDLE_N idlestat-regels"
else
	echo "   ROOD idlestat: $IDLE_N regels"
	fail=1
fi
reds

# De tabel: de rij van deze run, in de kolommen van docs/measurements.md.
if [ -s "$JSON" ]; then
	echo "== tabel (QEMU virt, TCG: de keten, geen ijzer-getallen)"
	python3 - "$JSON" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
r = {x["phase"]: x for x in d["results"]}
def lat(p):
    u = r.get(p, {}).get("us")
    return f'{u["p50"]} / {u["p99"]} us' if u else "-"
print(f'   | {"meting":<34} | {"QEMU virt":<24} |')
print(f'   | {"-"*34} | {"-"*24} |')
print(f'   | {"in de node (host -> slot), MB/s":<34} | {r.get("in",{}).get("MBps",0):<24.2f} |')
print(f'   | {"uit de node (slot -> host), MB/s":<34} | {r.get("out",{}).get("MBps",0):<24.2f} |')
print(f'   | {"rtt p50 / p99":<34} | {lat("rtt"):<24} |')
s = r.get("storm", {})
print(f'   | {"storm conn/s, cyclus p50 / p99":<34} | {str(round(s.get("conn_per_s",0)))+" conn/s, "+lat("storm"):<24} |')
PY
	echo "   | in de node, app -> app (pull)      | $(tr -d '\r' <"$LOG" | grep -m1 -o 'MBps=[0-9.]* bad=[0-9]* HOPOS_BENCH_PULL' | cut -d' ' -f1) |"
	echo "   | in de node, app -> app (rtt)       | $(tr -d '\r' <"$LOG" | grep -m1 -o 'p50=[0-9]*us p90=[0-9]*us p99=[0-9]*us' ) |"
	echo "   | koud, na 1 s stilte                | $(tr -d '\r' <"$LOG" | grep -m1 -o 'BENCH_COLD.*HOPOS' | sed 's/ HOPOS//') |"
fi
echo "== idlestat (de laatste drie):"
tr -d '\r' <"$LOG" | grep 'HOPOS_IDLESTAT$' | tail -3 | sed 's/^/   /'
took
[ -n "${KEEP_JSON:-}" ] && [ -s "$JSON" ] && cp "$JSON" "$KEEP_JSON"
verdict qemu-bench

# Boot 2: de schijf-bench, zonder app, op een verse schijf.
if [ "${NVME:-1}" != 0 ]; then
	echo "== boot 2: hopos.nvmebench=1 op een verse schijf (tot ${TIMEOUT}s)"
	rm -f "$DISK"
	LOG2="$(mktemp -t hopos-qemu-nvme.XXXXXX)"
	LOG1="$LOG"
	LOG="$LOG2"
	hop_virt APP= OSCORE= BOOTARGS=hopos.nvmebench=1
	NVME_MARKS="HOPOS_NVMEBENCH_START|HOPOS_NVMEBENCH_SEQ|HOPOS_NVMEBENCH_RAND|hopfs bench: .*HOPOS_NVMEBENCH|HOPOS_NVMEBENCH_DONE|HOPOS_DISK_UP|hopfs: mounted .*HOPOS_FS_UP"
	started
	while alive && ! all "$NVME_MARKS"; do step; done
	qemu_stop
	tr -d '\r' <"$LOG" | grep -E 'nvme bench:|hopfs bench:|HOPOS_DISK_UP|HOPOS_FS_UP' | sed 's/^/   /'
	IFS='|'
	for m in $NVME_MARKS; do
		has "$m" || {
			echo "   ROOD $m ontbreekt"
			fail=1
		}
	done
	IFS="$IFS_WAS"
	if has "HOPOS_NVMEBENCH_FAIL|HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION"; then
		echo "   ROOD $(first 'HOPOS_NVMEBENCH_FAIL|HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION')"
		fail=1
	fi
	if [ "$fail" != 0 ]; then
		KEEP="$(mktemp -t hopos-qemu-nvme-rood.XXXXXX)"
		tr -d '\r' <"$LOG" >"$KEEP"
		echo "== console bewaard in $KEEP"
		cat "$KEEP"
		rm -f "$LOG2"
		exit 1
	fi
	rm -f "$LOG2"
	LOG="$LOG1"
fi
echo "bench-keten groen"
