#!/bin/sh
# De soak van HopOS v3: uren lang jobs plaatsen en stoppen op een node, met
# de kern-flip erin als optie, en aan het eind een rapport. De opvolger van
# tools/soak-cycle.sh (tag v2.2.8; de burn-jobs elk uur recyclen) en
# tools/soak-monitor.sh (elke minuut agent en leader pollen, ALARM bij
# een miss), in één lus.
#
# Eén ronde (elke $PERIOD seconden):
#
#   1. de vorige ronde weg: DELETE van elke soak-job;
#   2. $JOBS keer apps/bench met BURN=1 (het werk/rust-ritme toetst de
#      dvfs-terugklok, net als in Go) plus één bench als server met een
#      gepubliceerde poort ($WEBPORT op de node);
#   3. wachten tot /tasks ze running toont (tot $SETTLE s);
#   4. als netmeter er is: één korte meting tegen de server (rtt en
#      out), zodat een soak ook zegt of het pad langzamer werd;
#   5. met FLIP=1, om de $FLIP_EVERY rondes: POST /flip met de bundel
#      ($FLIP_URL en zijn sha256), dan wachten tot /tasks weer antwoordt en
#      de jobs nog running zijn;
#   6. tussendoor elke $POLL s agent en leader pollen; een miss is een
#      ALARM-regel (en de lus gaat door: één hikje is meetdata, de reeks is
#      het oordeel).
#
# Elke ronde is één regel `SOAK round=N ...`; het rapport aan het eind (of
# bij Ctrl-C) telt rondes, plaatsingen, stops, flips, alarmen en de
# spreiding van de metingen. De UART-capture ernaast is de zwarte doos:
# HOPOS_TICK (met hopos.tick=1 in hopos.cfg), BURN-regels en eventuele
# crashes staan daar.
#
#   ARTIFACT=http://LAPTOP:8000/bench.elf tools/soak.sh NODE [UREN]
#   FLIP=1 FLIP_URL=http://LAPTOP:8000/hopos-o6n.flip FLIP_SHA=... tools/soak.sh NODE 8
#   tools/soak.sh NODE 12 | tee soak-$(date +%Y%m%d).log
#
#   NODE           het IP van de node (agent :8080, leader :9080)
#   UREN           hoe lang (standaard 8); 0 is tot Ctrl-C
#   ARTIFACT       de URL van bench.elf, zoals de node hem ziet (verplicht)
#   PERIOD=600     seconden per ronde
#   JOBS=2         BURN-jobs per ronde (plus één server)
#   WEBPORT=80     de gepubliceerde poort van de server
#   METERPORT      waar netmeter hem bereikt (standaard $WEBPORT; op QEMU
#                  de hostfwd, bv. 8081 naar gast-80)
#   ROUNDS=0       na zoveel rondes stoppen (0: alleen de uren tellen)
#   AGENTPORT=8080, LEADERPORT=9080   de API van Hop (op QEMU de hostfwd)
#   MEM=67108864   memory_limit per job
#   POLL=60        seconden tussen twee health-polls
#   SETTLE=60      seconden om een ronde running te krijgen
#   FLIP=0|1, FLIP_EVERY=3, FLIP_URL, FLIP_SHA   de flip
#
# De node draait met hopos.insecure=1: de handtekening van de Hop-API
# (HMAC over methode, pad en body, hop/api) zit niet in dit script.
#   NETMETER       pad naar netmeter (standaard target/release/netmeter,
#                  gebouwd als hij ontbreekt); NETMETER= slaat meten over
set -u

DIR="$(cd "$(dirname "$0")/.." && pwd)"
NODE="${1:-}"
HOURS="${2:-8}"
[ -n "$NODE" ] || { echo "usage: ARTIFACT=url tools/soak.sh NODE [UREN]" >&2; exit 2; }
[ -n "${ARTIFACT:-}" ] || { echo "soak: ARTIFACT (de URL van bench.elf) ontbreekt" >&2; exit 2; }
PERIOD="${PERIOD:-600}"
JOBS="${JOBS:-2}"
WEBPORT="${WEBPORT:-80}"
METERPORT="${METERPORT:-$WEBPORT}"
ROUNDS_MAX="${ROUNDS:-0}"
MEM="${MEM:-67108864}"
POLL="${POLL:-60}"
SETTLE="${SETTLE:-60}"
FLIP="${FLIP:-0}"
FLIP_EVERY="${FLIP_EVERY:-3}"
AGENT="http://$NODE:${AGENTPORT:-8080}"
LEADER="http://$NODE:${LEADERPORT:-9080}"

if [ "$FLIP" = 1 ] && { [ -z "${FLIP_URL:-}" ] || [ -z "${FLIP_SHA:-}" ]; }; then
	echo "soak: FLIP=1 vraagt FLIP_URL en FLIP_SHA (image/flip-bundle.sh maakt beide)" >&2
	exit 2
fi
NETMETER="${NETMETER-$DIR/target/release/netmeter}"
if [ -n "$NETMETER" ] && [ ! -x "$NETMETER" ]; then
	(cd "$DIR" && cargo build --quiet --release -p netmeter) || NETMETER=""
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
say() { echo "$(ts) $*"; }
api() { # methode url [body]
	curl -s -m 20 -o /dev/null -w '%{http_code}' -X "$1" \
		-H 'Content-Type: application/json' ${3:+-d "$3"} "$2" 2>/dev/null
}
tasks() { curl -s -m 10 "$AGENT/tasks" 2>/dev/null; }
# Hoeveel soak-jobs /tasks als running toont.
running() {
	tasks | python3 -c '
import json, sys
try:
    t = json.load(sys.stdin)
except Exception:
    print(-1); sys.exit()
print(sum(1 for x in t if str(x.get("job_name","")).startswith("soak-") and x.get("state") == "running"))
' 2>/dev/null || echo -1
}

ROUNDS=0
PLACED=0
REFUSED=0
STOPPED=0
FLIPS=0
FLIPS_OK=0
ALARMS=0
POLLS=0
METER="$(mktemp -t hopos-soak.XXXXXX)"
T0=$(date +%s)

report() {
	echo
	echo "== soak-rapport $(ts), node $NODE, $((($(date +%s) - T0) / 60)) min"
	echo "   rondes          $ROUNDS (elke ${PERIOD}s, $JOBS burn + 1 server)"
	echo "   plaatsingen     $PLACED aangenomen, $REFUSED geweigerd"
	echo "   stops           $STOPPED"
	echo "   flips           $FLIPS gevraagd, $FLIPS_OK met alle jobs weer running"
	echo "   health-polls    $POLLS, alarmen $ALARMS"
	if [ -s "$METER" ]; then
		python3 - "$METER" <<'PY'
import sys
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
for i, name in ((0, "rtt p50 us"), (1, "rtt p99 us"), (2, "out MB/s")):
    v = [float(r[i]) for r in rows if len(r) > i and r[i] != "-"]
    if v:
        print(f"   {name:<15} min {min(v):.1f}  max {max(v):.1f}  over {len(v)} rondes")
PY
	fi
	if [ "$ALARMS" = 0 ] && [ "$REFUSED" = 0 ] && [ "$FLIPS" = "$FLIPS_OK" ]; then
		echo "SOAK_GREEN"
	else
		echo "SOAK_RED"
	fi
	rm -f "$METER"
}
trap 'report; exit 0' INT TERM

job() { # naam env-json extra
	api POST "$LEADER/v1/jobs" '{"name":"'"$1"'","driver":"hop","artifacts":[{"url":"'"$ARTIFACT"'"}],"memory_limit":'"$MEM"',"env":{'"$2"'}'"$3"'}'
}

# Pollt agent en leader tot `until` (epoch); één regel per miss.
watch_until() {
	while [ "$(date +%s)" -lt "$1" ]; do
		a="$(curl -s -m 5 -o /dev/null -w '%{http_code} %{time_total}' "$AGENT/health" 2>/dev/null)"
		l="$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$LEADER/health" 2>/dev/null)"
		POLLS=$((POLLS + 1))
		case "$a" in
		200*) [ "$l" = 200 ] || { ALARMS=$((ALARMS + 1)); say "ALARM leader=$l agent=ok(${a#200 }s)"; } ;;
		*) ALARMS=$((ALARMS + 1)); say "ALARM agent=${a:-none} leader=${l:-none}" ;;
		esac
		left=$(($1 - $(date +%s)))
		[ "$left" -gt 0 ] && sleep $((left < POLL ? left : POLL))
	done
}

say "SOAK start node=$NODE hours=$HOURS period=${PERIOD}s jobs=$JOBS flip=$FLIP artifact=$ARTIFACT"
END=$((T0 + HOURS * 3600))
while [ "$HOURS" = 0 ] || [ "$(date +%s)" -lt "$END" ]; do
	ROUNDS=$((ROUNDS + 1))
	# 1. De vorige ronde weg.
	n=1
	while [ "$n" -le "$JOBS" ]; do
		case "$(api DELETE "$LEADER/v1/jobs/soak-burn-$n")" in 2*) STOPPED=$((STOPPED + 1)) ;; esac
		n=$((n + 1))
	done
	case "$(api DELETE "$LEADER/v1/jobs/soak-serve")" in 2*) STOPPED=$((STOPPED + 1)) ;; esac
	sleep 10
	# 2. Plaatsen.
	n=1
	while [ "$n" -le "$JOBS" ]; do
		case "$(job "soak-burn-$n" '"BURN":"1"' '')" in
		2*) PLACED=$((PLACED + 1)) ;;
		*) REFUSED=$((REFUSED + 1)) ;;
		esac
		n=$((n + 1))
	done
	case "$(job soak-serve '' ',"ports":{"http":'"$WEBPORT"'}')" in
	2*) PLACED=$((PLACED + 1)) ;;
	*) REFUSED=$((REFUSED + 1)) ;;
	esac
	# 3. Running.
	want=$((JOBS + 1))
	i=0
	r="$(running)"
	while [ "$r" != "$want" ] && [ "$i" -lt "$SETTLE" ]; do
		sleep 1
		i=$((i + 1))
		r="$(running)"
	done
	[ "$r" = "$want" ] || { ALARMS=$((ALARMS + 1)); say "ALARM round=$ROUNDS running=$r of $want after ${SETTLE}s"; }
	# 4. Meten.
	m="-"
	if [ -n "$NETMETER" ]; then
		out="$("$NETMETER" "$NODE:$METERPORT" --phases rtt,out --rtt 100 --bytes 16777216 2>&1)"
		p50="$(printf '%s\n' "$out" | grep -o 'phase=rtt.* p50=[0-9]*' | grep -o 'p50=[0-9]*' | cut -d= -f2)"
		p99="$(printf '%s\n' "$out" | grep 'phase=rtt' | grep -o 'p99=[0-9]*' | cut -d= -f2)"
		mb="$(printf '%s\n' "$out" | grep 'phase=out' | grep -o 'MBps=[0-9.]*' | cut -d= -f2)"
		echo "${p50:--} ${p99:--} ${mb:--}" >>"$METER"
		m="rtt_p50=${p50:--}us rtt_p99=${p99:--}us out=${mb:--}MBps"
	fi
	# 5. De flip.
	f=""
	if [ "$FLIP" = 1 ] && [ $((ROUNDS % FLIP_EVERY)) = 0 ]; then
		FLIPS=$((FLIPS + 1))
		code="$(api POST "$AGENT/flip" '{"url":"'"$FLIP_URL"'","sha256":"'"$FLIP_SHA"'"}')"
		i=0
		r="$(running)"
		# Na de sprong: /tasks antwoordt weer en alle jobs zijn er nog.
		sleep 5
		while [ "$r" != "$want" ] && [ "$i" -lt 180 ]; do
			sleep 1
			i=$((i + 1))
			r="$(running)"
		done
		if [ "$r" = "$want" ]; then
			FLIPS_OK=$((FLIPS_OK + 1))
			f=" flip=ok(${code},$((i + 5))s)"
		else
			ALARMS=$((ALARMS + 1))
			f=" flip=ALARM(${code},running=$r)"
		fi
	fi
	say "SOAK round=$ROUNDS placed=$PLACED stopped=$STOPPED running=$r $m$f alarms=$ALARMS"
	# 6. Pollen tot de volgende ronde.
	[ "$ROUNDS_MAX" != 0 ] && [ "$ROUNDS" -ge "$ROUNDS_MAX" ] && break
	watch_until $(($(date +%s) + PERIOD))
done
report
