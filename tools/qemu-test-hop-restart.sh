#!/bin/sh
# De kring van een herstart van Hop op QEMU: Hop wisselt onder zijn apps
# vandaan, en de apps merken niets.
#
# De kern boot met agentd-hopos in slot 1 (zoals tools/qemu-test-hop.sh),
# Hop plaatst welcome in slot 2 met poort 80 (zoals
# tools/qemu-test-welcome.sh). Dan drie keer POST /restart op de agent-API:
#
#   1. zonder body: hetzelfde image. Hop legt zijn staat bij de kern
#      (STATE_PUT), de kern zegt ja, belt de stop-bel van slot 1, plaatst
#      Hop opnieuw, en de nieuwe Hop leest de staat (STATE_GET) en adopteert
#      welcome;
#   2. met {"url","sha256"}: een nieuw image (de gestripte Hop van de
#      artifact-server), rauw gestroomd onder hopos.hop-image, getoetst en
#      overgenomen door de kern, dan dezelfde weg;
#   3. met een verkeerde som: de kern weigert (502), en Hop draait door.
#
# Groen alleen als:
#
#   de boot     zoals de welcome-kring: Hop op, welcome in slot 2 met zijn
#               pagina op 127.0.0.1:$WEBPORT;
#   elke wissel 202 op POST /restart, HOP_RESTART_ACCEPTED, de regel van de
#               kern (HOPOS_HOP_RESTART reason=request image=same n=1, dan
#               image=new n=2), Hop vertrekt zelf nadat zijn antwoord
#               buiten is (HOP_RESTART_LEAVING why=replied, HOP_STOPPING met
#               de laatste staat, HOPOS_HOP_STOPPED reason=request
#               left=self: geen stop-bel nodig), een niet-leeg
#               blob (HOPOS_HOP_STATE kept=N, N > 0), een nieuwe
#               HOPOS_HOP_START slot=1, HOP_ADOPTED n=1 en een nieuwe HOP_UP,
#               en de klok van de nieuwe Hop: een HOP_CLOCK_SYNCED na zijn
#               HOPOS_HOP_START (SNTP van de host, tools/lib.sh sntp, zoals
#               in elke ring) en nergens een
#               HOPOS_SYSTEM_REFUSED (de ORION, 24-10: de plaatsen van de oude
#               Hop telden nog tegen de nieuwe, en zijn kloktaak werd
#               geweigerd);
#   de apps     welcome blijft in slot 2: na elke wissel de pagina met
#               "slot 2", GET /tasks met welcome running, één HOP_JOB_PLACED
#               in de hele run, en geen stop van slot 2;
#   de weigering 502 op de verkeerde som, HOPOS_HOP_RESTART_REFUSED sha256
#               mismatch, en geen vierde HOPOS_HOP_START.
#
# Een HOPOS_PANIC, HOPOS_EXCEPTION, HOPOS_APP_PANIC, HOPOS_HOP_FAULT,
# HOPOS_HOP_EXIT, HOPOS_HOP_FAIL, HOP_STRAY_STOPPED,
# HOP_ADOPT_SKIPPED, HOP_KERNEL_STATE_NONE, HOP_KERNEL_STATE_FAIL,
# HOPOS_APP_STOP_FORCED (een Hop die de bel negeerde), HOPOS_PART_QUARANTINE,
# een vertrek dat de bel nodig had of niet op het antwoord wachtte
# (left=bell, left=fault, left=gone, HOP_RESTART_LEAVING why=grace) of een
# stop van slot 2 is meteen rood. Rood bewaart de console.
#
#   tools/qemu-test-hop-restart.sh            TIMEOUT=120 standaard, in s
#   KEEP_LOG=pad tools/qemu-test-hop-restart.sh   bewaart ook een groene console
#   HOP_REV=worktree tools/qemu-test-hop-restart.sh   Hop zoals de werkboom
#                                          van de hop-repo nu is
#                                          (tools/hop-build.sh)
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/WEBPORT   de host-poorten; standaard
#                                          vrije van het OS
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-120}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-hop-restart
PAGE="$ART/page.html"
ports SYS AGENT LEADER ART WEB

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), welcome, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
hop_elf
apps welcome
# Het nieuwe image van wissel 2: dezelfde Hop, gestript, met zijn som.
strip_elf "$HOP_ELF" "$ART/hop.elf"
HOP_SUM="$(shasum -a 256 "$ART/hop.elf" | cut -d' ' -f1)"
serve

echo "== booten op QEMU virt met Hop (tot ${TIMEOUT}s; agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT, web :$WEBPORT -> gast :80)"
hop_virt

BOOT_MARKS="HOPOS_BOOT|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_HOP_START slot=1 |slot 1: 2 port\\(s\\) published tcp\\+udp on the uplink: :8080 :9080 HOPOS_SLOT_PUBLISH|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 2: 1 port\\(s\\) published tcp\\+udp on the uplink: :80 HOPOS_SLOT_PUBLISH|slot 2: .*HOPOS_WELCOME_UP port=80"
SAME_MARKS="slot 1: .*HOP_RESTART_ACCEPTED|HOPOS_HOP_RESTART reason=request image=same n=1|slot 1: .*HOP_STOPPING|slot 1: .*HOP_RESTART_LEAVING why=replied|slot 1: Hop stopped for the restart HOPOS_HOP_STOPPED reason=request left=self|HOPOS_HOP_STATE kept=[1-9]|slot 1: .*HOP_ADOPTED n=1"
NEW_MARKS="HOPOS_HOP_RESTART reason=request image=new n=2"
REFUSE_MARKS="HOPOS_HOP_RESTART_REFUSED .*sha256 mismatch|slot 1: .*HOP_RESTART_FAIL"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_APP_PANIC|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOP_STRAY_STOPPED|HOP_ADOPT_SKIPPED|HOP_KERNEL_STATE_NONE|HOP_KERNEL_STATE_FAIL|HOPOS_APP_STOP_FORCED|left=bell|left=fault|left=gone|HOP_RESTART_LEAVING why=grace|slot 2: stopped"

JOB='{"name":"welcome","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}'

# page: de pagina van welcome uit slot 2 ($PAGE_OK wordt de uitkomst).
page() {
	code="$(curl -s -m 5 -o "$PAGE" -w '%{http_code}' "http://127.0.0.1:$WEBPORT/" 2>/dev/null || true)"
	if [ "$code" = 200 ] && grep -q -F '( -.-)' "$PAGE" && grep -q 'slot 2' "$PAGE"; then
		PAGE_OK="HTTP $code, $(wc -c <"$PAGE" | tr -d ' ') bytes, slot 2"
		return 0
	fi
	PAGE_OK=""
	return 1
}

# restart BODY: POST /restart op de agent-API; $RESTARTED wordt het
# antwoord met " HTTP nnn" erachter.
restart() {
	RESTARTED="$(curl -s -m 60 -w ' HTTP %{http_code}' -X POST \
		-H 'Content-Type: application/json' -d "$1" \
		"http://127.0.0.1:$AGENTPORT/restart" 2>&1 || true)"
}

# synced N: de N-de Hop (na de N-de HOPOS_HOP_START) zette zijn klok.
synced() {
	tr -d '\r' <"$LOG" | awk -v n="$1" '/HOPOS_HOP_START slot=1 / { s++ } s == n && /slot 1: .*HOP_CLOCK_SYNCED/ { f = 1 } END { exit !f }'
}

# swapped N: de N-de Hop is op, heeft zijn klok gezet en welcome
# overgenomen, en de pagina en GET /tasks zeggen dat welcome nog draait.
swapped() {
	[ "$(count 'HOPOS_HOP_START slot=1 ')" -ge "$1" ] &&
		[ "$(count 'slot 1: .*HOP_UP')" -ge "$1" ] &&
		synced "$1" &&
		[ "$(count 'slot 1: .*HOP_ADOPTED n=1')" -ge "$(($1 - 1))" ] &&
		page && running welcome
}

POSTED=""
PAGE_OK=""
SAME=""
SAME_PAGE=""
NEW=""
NEW_PAGE=""
REFUSED=""
T_SAME=""
T_NEW=""
started

# 1. Boot, de jobspec, de pagina.
while alive && [ -z "$POSTED" ]; do
	if all "$BOOT_MARKS"; then post_job "$JOB" || true; fi
	step
done
while alive && [ -z "$PAGE_OK" ]; do
	if all "$PLACE_MARKS"; then page || true; fi
	step
done

# 2. Hetzelfde image. De staat moet bij de kern liggen vóór de vraag; Hop
# schrijft hem zelf vlak ervoor nog een keer.
if [ -n "$PAGE_OK" ]; then
	t0=$(date +%s)
	restart ''
	SAME="$RESTARTED"
	while alive && ! { all "$SAME_MARKS" && swapped 2; }; do step; done
	if swapped 2; then
		SAME_PAGE="$PAGE_OK"
		T_SAME="$(($(date +%s) - t0)) s"
	fi
fi

# 3. Een nieuw image van de artifact-server.
if [ -n "$SAME_PAGE" ]; then
	t0=$(date +%s)
	restart '{"url":"http://10.0.2.2:'"$ARTPORT"'/hop.elf","sha256":"'"$HOP_SUM"'"}'
	NEW="$RESTARTED"
	while alive && ! { all "$NEW_MARKS" && swapped 3; }; do step; done
	if swapped 3; then
		NEW_PAGE="$PAGE_OK"
		T_NEW="$(($(date +%s) - t0)) s"
	fi
fi

# 4. Een verkeerde som: geweigerd, en deze Hop blijft.
if [ -n "$NEW_PAGE" ]; then
	BAD="$(printf '%064d' 0)"
	restart '{"url":"http://10.0.2.2:'"$ARTPORT"'/hop.elf","sha256":"'"$BAD"'"}'
	REFUSED="$RESTARTED"
	while alive && ! all "$REFUSE_MARKS"; do step; done
	# Even laten: een herstart die toch kwam, zou nu zijn regel zetten.
	sleep 2
fi

qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS" "$SAME_MARKS" "$NEW_MARKS" "$REFUSE_MARKS"
posted
for r in "same:$SAME" "new:$NEW"; do
	case "${r#*:}" in
	*"HTTP 202") echo "   ok  POST /restart (${r%%:*}): ${r#*:}" ;;
	*) echo "   ROOD POST /restart (${r%%:*}): ${r#*:}"; fail=1 ;;
	esac
done
case "$REFUSED" in
*"HTTP 502") echo "   ok  POST /restart (verkeerde som): $REFUSED" ;;
*) echo "   ROOD POST /restart (verkeerde som): ${REFUSED:-nooit gedaan}"; fail=1 ;;
esac
for r in "hetzelfde image:$SAME_PAGE:$T_SAME" "nieuw image:$NEW_PAGE:$T_NEW"; do
	what="${r%%:*}"
	rest="${r#*:}"
	if [ -n "${rest%%:*}" ]; then
		echo "   ok  na de wissel ($what, ${rest#*:}): welcome draait door, ${rest%%:*}"
	else
		echo "   ROOD na de wissel ($what): geen nieuwe Hop met welcome overgenomen, of de pagina weg"
		fail=1
	fi
done
n="$(count 'HOPOS_HOP_STOPPED reason=request left=self')"
if [ "$n" = 2 ]; then
	echo "   ok  twee keer left=self: Hop vertrok beide keren zelf, na zijn antwoord"
else
	echo "   ROOD $n keer HOPOS_HOP_STOPPED reason=request left=self, verwacht 2"
	fail=1
fi
n="$(count 'HOPOS_HOP_START slot=1 ')"
if [ "$n" = 3 ]; then
	echo "   ok  drie keer HOPOS_HOP_START slot=1: de boot en twee wissels, de weigering wisselde niet"
else
	echo "   ROOD $n keer HOPOS_HOP_START slot=1, verwacht 3"
	fail=1
fi
n="$(count 'HOPOS_SYSTEM_REFUSED')"
if [ "$n" = 0 ]; then
	echo "   ok  geen HOPOS_SYSTEM_REFUSED: elke nieuwe Hop kreeg al zijn system-verbindingen"
else
	echo "   ROOD $n keer HOPOS_SYSTEM_REFUSED, de eerste: $(first 'HOPOS_SYSTEM_REFUSED')"
	fail=1
fi
if synced 2 && synced 3; then
	echo "   ok  HOP_CLOCK_SYNCED van beide nieuwe Hops: hun kloktaak kwam door"
else
	echo "   ROOD een nieuwe Hop zette zijn klok niet (HOP_CLOCK_SYNCED na zijn HOPOS_HOP_START)"
	fail=1
fi
n="$(count 'HOP_JOB_PLACED')"
if [ "$n" = 1 ]; then
	echo "   ok  welcome één keer geplaatst: geen tweede plaatsing na een wissel"
else
	echo "   ROOD $n keer HOP_JOB_PLACED, verwacht 1"
	fail=1
fi
served welcome.elf
served hop.elf
reds
took
# De meetlat van een wissel: de regels van de kern, van de vraag tot op.
echo "   meting: $(tr -d '\r' <"$LOG" | grep -E 'HOPOS_HOP_RESTART |HOPOS_HOP_STOPPED|HOPOS_HOP_STATE kept|HOPOS_HOP_START slot=1' | sed -E 's/^.*(HOPOS_[A-Z_]+[^ ]*( [a-z]+=[^ ]+)*).*$/\1/' | tr '\n' ';')"
verdict qemu-hop-restart
echo "hop-herstartkring groen"
