#!/bin/sh
# De soak van de hoplb-kring op QEMU: N keer achter elkaar Hop op de
# OS-core, welcome in een slot, hoplb in een tweede slot, en verkeer van de
# host door de DNAT naar hoplb en van hoplb door de switch (hairpin op het
# node-IP) naar welcome. De vorm van hop/hoplb/tools/qemu-test.sh, maar met
# een waakhond op de hartslag van de kern in plaats van één toets: de kring
# viel op 30-09 één run van de negentien stil (de console stopte na
# HOPOS_TICK 10, `kicks` liep met ~420/s op, de eerste curl door hoplb kreeg
# geen antwoord).
#
# Per run:
#
#   1. een verse schijf, vrije host-poorten (van het OS, zodat een tweede
#      soak of een andere QEMU ernaast kan draaien), QEMU met een eigen
#      monitor-socket;
#   2. Hop boot, dan welcome ("ports":{"http":8081}, tag
#      hoplb-urlprefix=welcome.local), dan hoplb ("ports":{"http":80});
#   3. zodra hoplb zijn route heeft: $LOAD seconden curl -H 'Host:
#      welcome.local' naar hoplb, telkens één verzoek, dat hoplb over de
#      switch naar welcome brengt;
#   4. de waakhond leest intussen elke HOPOS_TICK-regel. Blijft de tik meer
#      dan $STALL seconden uit, of stijgt `kicks` $KICKRUN tikken achter
#      elkaar meer dan $KICKMAX per seconde, dan dumpt hij via de monitor
#      `info registers -a` (alle cores: PC, EL, PSTATE) en de meetlat van de
#      OS-core uit het geheugen (`xp` op cpu::el2::oscore::STATS, via de
#      symbolen van de kern-ELF), twee keer met een seconde ertussen, zodat
#      te zien is wat nog beweegt. De PC's staan erbij als symbool.
#
# Groen is: elke tik op tijd, geen kick-storm, en elke curl met de bunny
# (de eerste curl zonder antwoord geeft ook een dump). De soak stopt bij de
# eerste rode run en bewaart die run (console, tikken, dumps, de kern-ELF)
# in $OUT/run-N/. Van een groene run blijven alleen de tik-regels. De
# SOAK-regel per run draagt maxgap: de langste afstand tussen twee tikken.
#
# Wat de stilte van 30-09 was (de uitkomst van deze soak, ~1100 runs): de
# console stopte precies waar elke groene run "hopfs: tree committed as
# generation 1 (every 10 s)" drukt. Die commit wacht synchroon op twee
# FLUSHes van de schijf (op macOS een F_FULLFSYNC van het image), en de hele
# OS-core met Hop staat zolang stil; met 2,5 s per verzoek (null-co met
# latency-ns) is dat 7,6 s stilte en daarna een salvo tikken met late_ms. De
# tik-regel draagt daarom late_ms.
#
#   tools/qemu-soak-hop.sh [N]            N runs (standaard 30)
#   SMP=8 tools/qemu-soak-hop.sh 30       acht cores (met SMP=3 plaatst Hop
#                                         maar één app: geen plaats voor hoplb)
#   SMP=2 tools/qemu-soak-hop.sh 30       twee cores, dus twee slots: Hop en
#                                         welcome op poort 80, verkeer zonder
#                                         hoplb (geen hairpin)
#   OSCORE=1 tools/qemu-soak-hop.sh 30    de kern en Hop op core 1
#   OUT=pad                               waar de runs landen (standaard een
#                                         mktemp-map, afgedrukt)
#   STALL=3 KICKMAX=5000 KICKRUN=2        de grenzen van de waakhond (onder
#                                         vol verkeer ~2500 kicks/s, 30-09)
#   LOAD=20                               seconden verkeer per run
#   PAR=16                                per ronde ook een salvo van 16
#                                         verzoeken tegelijk door hoplb
#   TIMEOUT=150                           seconden per run tot hoplb staat
#   HOP_DIR=pad                           de hop-repo (standaard ../hop/hop)
#   HOPLB_DIR=pad                         de hoplb-repo (standaard ../hop/hoplb)
#   KEEP_GOING=1                          niet stoppen bij rood, alleen tellen
#   DISKLOAD=1                            een schrijver op de host (1 GB per
#                                         ronde, F_FULLFSYNC per 64 MB) naast
#                                         de schijf van de gast
#   HOSTLB=pad/naar/hoplb                 een hoplb-daemon op de host tegen de
#                                         agent van Hop in de gast (SSE open)
#   NOISE=0                               zonder de poller (standaard aan:
#                                         /tasks en /v1/jobs op Hop, door de
#                                         DNAT naar de OS-core, plus een
#                                         verzoek door hoplb dat na 30 ms
#                                         afbreekt; ~10 per seconde. De nacht
#                                         van 30-09 had andere poortgebruikers
#                                         op de host)
#
# Twee soaks naast elkaar (de last van de nacht van 30-09) kan gewoon: elke
# soak kiest zijn eigen poorten en schijven. Wel met dezelfde HOP_DIR:
# image/qemu-run.sh zet het Hop-image op één vaste plek in target/, en twee
# verschillende hop-bomen overschrijven elkaar daar midden in een boot
# (HOPOS_HOP_FAIL "section headers ... past the end"). Een draaiende soak
# leest zijn script regel voor regel: niet bewerken terwijl hij loopt.
set -u

DIR="$(cd "$(dirname "$0")/.." && pwd)"
N="${1:-30}"
STALL="${STALL:-3}"
KICKMAX="${KICKMAX:-5000}"
KICKRUN="${KICKRUN:-2}"
LOAD="${LOAD:-20}"
TIMEOUT="${TIMEOUT:-150}"
HOP_DIR="$(cd "${HOP_DIR:-$DIR/../hop/hop}" && pwd)"
HOPLB_DIR="$(cd "${HOPLB_DIR:-$DIR/../hop/hoplb}" && pwd)"
TARGET=aarch64-unknown-none-softfloat
OUT="${OUT:-$(mktemp -d -t hopos-soak.XXXXXX)}"
mkdir -p "$OUT"
ART="$(mktemp -d -t hopos-soak-art.XXXXXX)"
QPID=""
HPID=""
WPID=""
NPID=""
DPID=""
cleanup() {
	[ -n "$QPID" ] && kill "$QPID" 2>/dev/null
	[ -n "$NPID" ] && kill "$NPID" 2>/dev/null
	[ -n "$WPID" ] && kill "$WPID" 2>/dev/null
	[ -n "$DPID" ] && kill "$DPID" 2>/dev/null
	[ -n "$HPID" ] && kill "$HPID" 2>/dev/null
	rm -rf "$ART"
	true
}
trap cleanup EXIT INT TERM

# Poorten altijd van het OS (`port 0`), nooit de standaardpoort: twee soaks
# naast elkaar zouden allebei 8080 vrij zien en dan botsen.
. "$(dirname "$0")/lib.sh"

echo "== bouwen: hopos (qemuvirt) en welcome hier, hoplb-hopos in $HOPLB_DIR, agentd-hopos in $HOP_DIR"
(cd "$DIR" && cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt) || exit 1
(cd "$DIR" && cargo build --quiet --release --target "$TARGET" -p welcome) || exit 1
(cd "$HOPLB_DIR" && cargo build --quiet --release --target "$TARGET" --no-default-features --features hopos --bin hoplb-hopos) || exit 1
HOP_ELF="$(HOP_DIR="$HOP_DIR" sh "$DIR/tools/hop-build.sh" "$TARGET")" || exit 1
KERN="$DIR/target/$TARGET/release/hopos"
strip_elf "$DIR/target/$TARGET/release/welcome" "$ART/welcome.elf"
strip_elf "$HOPLB_DIR/target/$TARGET/release/hoplb-hopos" "$ART/hoplb.elf"
ARTPORT="$(port 0)"
(cd "$ART" && exec python3 -m http.server "$ARTPORT" --bind 127.0.0.1) >"$ART/http.log" 2>&1 &
HPID=$!

WELCOME='{"name":"welcome","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"ports":{"http":8081},"tags":{"hoplb-urlprefix":"welcome.local"}}'
HOPLB='{"name":"hoplb","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/hoplb.elf"}],"memory_limit":67108864,"ports":{"http":80,"admin":9091}}'
# Met twee cores heeft de kern twee slots (Hop en één app): dan geen hoplb,
# en welcome zelf op poort 80, zodat het verkeer van de host door de DNAT
# naar welcome gaat en de OS-core-rotatie (Hop naast de kern) dezelfde last
# ziet. Zonder hairpin: de volle kring vraagt minstens drie cores (SMP=3).
DIRECT=""
if [ "${SMP:-4}" = 2 ]; then
	DIRECT=1
	WELCOME='{"name":"welcome","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}'
fi
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_APP_PANIC|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_SLOT_PUBLISH_FAIL|HOPOS_OS_SELFTEST_FAIL|HOPOS_OSCORE_FALLBACK|HOPOS_CAGE_FAIL"
BOOT_MARKS="HOPOS_NET_UP|HOPOS_SYSTEM_UP|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
WELCOME_MARKS="slot 2: .*HOPOS_WELCOME_UP port=80"
HOPLB_MARKS="HOPOS_HOPLB_UP port=80|HOPOS_HOPLB_ROUTES n=1 backends=1"

# De waakhond: leest de console, beoordeelt de tik, en dumpt via de monitor.
# Schrijft zijn oordeel (GREEN, STALL, KICKS) in $1/verdict zodra hij het
# weet; stopt als $1/stop verschijnt of QEMU weg is.
watch() {
	python3 - "$1" "$2" "$3" "$KERN" "$STALL" "$KICKMAX" "$KICKRUN" <<'PY'
import os, re, socket, subprocess, sys, time, bisect
run, log, mon, kern = sys.argv[1:5]
stall, kickmax, kickrun = float(sys.argv[5]), int(sys.argv[6]), int(sys.argv[7])
tick_re = re.compile(r"HOPOS_TICK (\d+) .*kicks=(\d+)\)")

def symbols():
    out = subprocess.run(["nm", "-n", kern], capture_output=True, text=True).stdout
    syms = []
    for line in out.splitlines():
        p = line.split()
        if len(p) == 3:
            try:
                syms.append((int(p[0], 16), p[2]))
            except ValueError:
                pass
    return syms

SYMS = symbols()
ADDRS = [a for a, _ in SYMS]
def sym(pc):
    i = bisect.bisect_right(ADDRS, pc) - 1
    if i < 0 or pc - ADDRS[i] > 0x100000:
        return "?"
    return f"{SYMS[i][1]}+{pc - ADDRS[i]:#x}"
def addr_of(part):
    for a, n in SYMS:
        if part in n:
            return a
    return None

def hmp(cmds):
    s = socket.socket(socket.AF_UNIX)
    s.connect(mon)
    s.settimeout(1.0)
    def drain():
        out = b""
        try:
            while True:
                b = s.recv(65536)
                if not b:
                    break
                out += b
        except OSError:
            pass
        return out.decode(errors="replace")
    drain()
    res = []
    for c in cmds:
        s.sendall((c + "\n").encode())
        time.sleep(0.8)
        # De echo van de HMP-regeleditor (ANSI) en de FP-registers eruit.
        out = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", drain())
        out = "\n".join(l for l in out.splitlines()
                        if not l.startswith("Q") and not l.startswith("(qemu)") and c not in l)
        res.append(f"(qemu) {c}\n{out}")
    s.close()
    return "\n".join(res)

def dump(tag):
    stats = addr_of("oscore5STATS")
    netst = addr_of("hopos3net5STATS")
    cmds = ["info cpus", "info registers -a"]
    if stats:
        cmds.append(f"xp /16gx {stats:#x}")
    if netst:
        cmds.append(f"xp /24gx {netst:#x}")
    # De dood-vlag van de PL011 (board_qemuvirt::UART, na LTO een los
    # symbool): staat hij, dan is de console stil maar de kern niet.
    uart = addr_of("4UART")
    if uart:
        cmds.append(f"xp /2wx {uart:#x}")
    with open(os.path.join(run, f"dump-{tag}.txt"), "w") as f:
        for k in range(2):
            try:
                txt = hmp(cmds)
            except OSError as e:
                txt = f"monitor: {e}"
            f.write(f"=== dump {k} at {time.time():.1f}\n{txt}\n")
            for m in re.finditer(r"PC=([0-9a-f]{16})", txt):
                pc = int(m.group(1), 16)
                f.write(f"   PC {pc:#x} = {sym(pc)}\n")
            f.flush()
            time.sleep(1.0)

def verdict(v):
    with open(os.path.join(run, "verdict"), "w") as f:
        f.write(v + "\n")

pos = 0
buf = ""
maxgap = 0.0
last_t = None
last_kicks = None
hot = 0
kicked = False
while True:
    if os.path.exists(os.path.join(run, "stop")):
        verdict("KICKS" if kicked else "GREEN")
        break
    try:
        with open(log, "rb") as f:
            f.seek(pos)
            chunk = f.read()
            pos += len(chunk)
    except OSError:
        chunk = b""
    buf += chunk.decode(errors="replace")
    *lines, buf = buf.split("\n")
    now = time.time()
    for line in lines:
        m = tick_re.search(line)
        if not m:
            continue
        k = int(m.group(2))
        if last_kicks is not None and last_t is not None:
            rate = (k - last_kicks) / max(now - last_t, 0.5)
            hot = hot + 1 if rate > kickmax else 0
            if hot >= kickrun and not kicked:
                kicked = True
                with open(os.path.join(run, "watch.txt"), "a") as w:
                    w.write(f"KICKS tick {m.group(1)}: {rate:.0f}/s for {hot} ticks\n")
                dump("kicks")
        if last_t is not None:
            maxgap = max(maxgap, now - last_t)
            with open(os.path.join(run, "gap"), "w") as g:
                g.write(f"{maxgap:.1f}\n")
        last_t, last_kicks = now, k
    req = os.path.join(run, "dumpreq")
    if os.path.exists(req):
        tag = open(req).read().strip() or "req"
        os.remove(req)
        dump(tag)
    if last_t is not None and now - last_t > stall:
        with open(os.path.join(run, "watch.txt"), "a") as w:
            w.write(f"STALL: no tick for {now - last_t:.1f} s after kicks={last_kicks}\n")
        dump("stall")
        verdict("STALL")
        break
    time.sleep(0.2)
PY
}

post() {
	curl -s -m 20 -w ' HTTP %{http_code}' -X POST -H 'Content-Type: application/json' \
		-d "$2" "http://127.0.0.1:$1/v1/jobs" 2>&1 || echo "curl failed"
}

# De poller op Hop (NOISE): de API van Hop in slot 1 op de OS-core, door
# de DNAT, zolang de run duurt. Telt zijn mislukkingen in $1/noise.txt.
noise() {
	[ "${NOISE:-1}" = 0 ] && return 0
	n=0
	bad=0
	# HOSTLB=pad: ook een hoplb-daemon op de host met -agent naar Hop in de
	# gast (zijn standaard is http://127.0.0.1:8080, en zo'n daemon uit een
	# andere toets praat dan met de Hop van wie 8080 net heeft): hij houdt
	# de SSE-stroom /v1/events open en pollt de agent.
	if [ -n "${HOSTLB:-}" ]; then
		"$HOSTLB" -agent "http://127.0.0.1:$AGENTPORT" -listen "127.0.0.1:$(port 0)" \
			-admin-listen "127.0.0.1:$(port 0)" >"$1/hostlb.log" 2>&1 &
		LBPID=$!
		trap 'kill "$LBPID" 2>/dev/null; exit 0' TERM
	fi
	while :; do
		curl -s -m 5 -o /dev/null "http://127.0.0.1:$AGENTPORT/tasks" || bad=$((bad + 1))
		curl -s -m 5 -o /dev/null "http://127.0.0.1:$LEADERPORT/v1/jobs" || bad=$((bad + 1))
		# En een verzoek door hoplb dat de host na 30 ms afbreekt: een
		# verbinding die midden in de proxy wegvalt (de curl -m 5 van 30-09
		# die zijn verbinding opgaf).
		curl -s -m 0.03 -o /dev/null -H 'Host: welcome.local' "http://127.0.0.1:$WEBPORT/" || true
		n=$((n + 2))
		echo "noise requests=$n failed=$bad" >"$1/noise.txt"
		sleep 0.2
	done
}

# De schijflast (DISKLOAD=1): de host-schijf onder de schijf van de gast
# vol schrijven met F_FULLFSYNC ertussen, zoals een nacht met builds en
# andere QEMU's. Elke flush van de gast wordt op macOS een F_FULLFSYNC op
# het image (QEMU file-posix), en die wacht dan op de rest.
diskload() {
	[ -n "${DISKLOAD:-}" ] || return 0
	python3 - "$1" <<'PY' &
import os, fcntl, sys, time, signal
d = sys.argv[1]
signal.signal(signal.SIGTERM, lambda *a: sys.exit(0))
buf = b"\xa5" * (1 << 20)
try:
    while True:
        p = os.path.join(d, "diskload.bin")
        fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        for i in range(1024):
            os.write(fd, buf)
            if i % 64 == 63:
                fcntl.fcntl(fd, getattr(fcntl, "F_FULLFSYNC", 51))
        os.close(fd)
        os.remove(p)
finally:
    try:
        os.remove(os.path.join(d, "diskload.bin"))
    except OSError:
        pass
PY
	DPID=$!
}

# Eén run in $OUT/run-$1. Geeft 0 als hij groen is.
run_one() {
	R="$OUT/run-$1"
	mkdir -p "$R"
	LOG="$R/console.log"
	# De socket kort houden: een unix-pad mag hooguit 104 bytes zijn.
	MON="$ART/m$1.sock"
	SYSPORT="$(port 0)"
	AGENTPORT="$(port 0)"
	LEADERPORT="$(port 0)"
	WEBPORT="$(port 0)"
	SYSPORT="$SYSPORT" AGENTPORT="$AGENTPORT" LEADERPORT="$LEADERPORT" WEBPORT="$WEBPORT" \
		HOP_DIR="$HOP_DIR" APP="$HOP_ELF" ROLE=1 DISK="$R/disk.img" \
		sh "$DIR/image/qemu-run.sh" -monitor "unix:$MON,server,nowait" </dev/null >"$LOG" 2>&1 &
	QPID=$!
	watch "$R" "$LOG" "$MON" &
	WPID=$!
	diskload "$R"
	start=$(date +%s)
	phase=boot
	ok=0
	fail=0
	why=""
	while :; do
		el=$(($(date +%s) - start))
		[ -e "$R/verdict" ] && break
		if has "$RED"; then why="$(tr -d '\r' <"$LOG" | grep -m1 -E "$RED")"; break; fi
		kill -0 "$QPID" 2>/dev/null || { why="QEMU exited"; break; }
		case "$phase" in
		boot)
			if all "$BOOT_MARKS"; then
				echo "   welcome: $(post "$LEADERPORT" "$WELCOME")" >>"$R/steps.txt"
				phase=welcome
			fi
			;;
		welcome)
			if all "$WELCOME_MARKS" && [ -n "$DIRECT" ]; then
				phase=load
				load_end=$(($(date +%s) + LOAD))
				noise "$R" &
				NPID=$!
			elif all "$WELCOME_MARKS"; then
				echo "   hoplb: $(post "$LEADERPORT" "$HOPLB")" >>"$R/steps.txt"
				phase=hoplb
			fi
			;;
		hoplb)
			if all "$HOPLB_MARKS"; then
				phase=load
				load_end=$(($(date +%s) + LOAD))
				noise "$R" &
				NPID=$!
			fi
			;;
		load)
			# PAR>1: eerst een salvo van PAR verzoeken tegelijk (meer dan de
			# acht werkers van hoplb en de vier van welcome), dan het ene
			# verzoek met de bunny-toets.
			if [ "${PAR:-1}" -gt 1 ]; then
				seq "$PAR" | xargs -P "$PAR" -I{} curl -s -m 5 -o /dev/null -w '%{http_code} %{time_total} %{errormsg}\n' -H 'Host: welcome.local' "http://127.0.0.1:$WEBPORT/" >"$R/par.txt" 2>/dev/null || true
				grep -v '^200 ' "$R/par.txt" | sed "s/^/   at ${el}s: /" >>"$R/steps.txt"
				pok=$(grep -c '^200 ' "$R/par.txt" || true)
				ok=$((ok + pok))
				fail=$((fail + PAR - pok))
				[ "$pok" = "$PAR" ] || { echo "   salvo at ${el}s: $pok of $PAR" >>"$R/steps.txt"; [ "$fail" -gt 0 ] && [ ! -e "$R/dumped" ] && touch "$R/dumped" && echo curl >"$R/dumpreq"; }
			fi
			code="$(curl -s -m 5 -o "$R/page.html" -w '%{http_code}' -H 'Host: welcome.local' "http://127.0.0.1:$WEBPORT/" 2>/dev/null || true)"
			if [ "$code" = 200 ] && grep -q -F '( -.-)' "$R/page.html"; then
				ok=$((ok + 1))
			else
				fail=$((fail + 1))
				echo "   curl $((ok + fail)) at ${el}s: HTTP ${code:-none}" >>"$R/steps.txt"
				# De eerste stille curl: meteen een dump (de stilte van 30-09
				# kwam een paar seconden ná de eerste curl zonder antwoord).
				[ "$fail" = 1 ] && echo curl >"$R/dumpreq"
			fi
			[ "$(date +%s)" -ge "$load_end" ] && break
			continue
			;;
		esac
		if [ "$phase" != load ] && [ "$el" -ge "$TIMEOUT" ]; then
			why="timeout in phase $phase"
			break
		fi
		sleep 0.2
	done
	[ -n "$NPID" ] && kill "$NPID" 2>/dev/null
	NPID=""
	[ -n "$DPID" ] && kill "$DPID" 2>/dev/null
	DPID=""
	# Een rood oordeel van de waakhond: hij dumpt nog; wachten tot hij klaar
	# is, dan pas QEMU weg.
	[ -e "$R/verdict" ] || touch "$R/stop"
	wait "$WPID" 2>/dev/null
	WPID=""
	kill "$QPID" 2>/dev/null
	wait "$QPID" 2>/dev/null
	QPID=""
	v="$(cat "$R/verdict" 2>/dev/null || echo NONE)"
	tr -d '\r' <"$LOG" | grep HOPOS_TICK >"$R/ticks.txt"
	ticks=$(wc -l <"$R/ticks.txt" | tr -d ' ')
	last="$(tail -1 "$R/ticks.txt" | sed -E 's/.*os\(//; s/\).*//')"
	[ -n "$why" ] || { [ "$ok" -gt 0 ] && [ "$fail" = 0 ] || why="$fail of $((ok + fail)) curls through hoplb unanswered"; }
	[ "$v" = GREEN ] || why="${why:+$why; }watchdog $v: $(cat "$R/watch.txt" 2>/dev/null | tr '\n' ' ')"
	echo "SOAK run=$1 verdict=$v curl_ok=$ok curl_fail=$fail $(cat "$R/noise.txt" 2>/dev/null) maxgap=$(cat "$R/gap" 2>/dev/null || echo -)s ticks=$ticks last_os=($last)${why:+ RED: $why}"
	if [ -z "$why" ]; then
		rm -f "$LOG" "$R/disk.img" "$R/page.html"
		return 0
	fi
	cp "$KERN" "$R/hopos.elf"
	rm -f "$R/disk.img"
	return 1
}

echo "== $N runs, SMP=${SMP:-4}${OSCORE:+ OSCORE=$OSCORE}, waakhond: tik > ${STALL}s of kicks > ${KICKMAX}/s ${KICKRUN}x; runs in $OUT"
green=0
red=0
i=1
while [ "$i" -le "$N" ]; do
	if run_one "$i"; then
		green=$((green + 1))
	else
		red=$((red + 1))
		echo "== rood: run $i bewaard in $OUT/run-$i"
		ls "$OUT/run-$i/" | sed 's/^/      /'
		[ -n "${KEEP_GOING:-}" ] || break
	fi
	i=$((i + 1))
done
echo "== soak: $green groen, $red rood van $((green + red)) runs (SMP=${SMP:-4}${OSCORE:+ OSCORE=$OSCORE}); runs in $OUT"
[ "$red" = 0 ]
