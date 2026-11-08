#!/bin/sh
# De kring van een devicedag op QEMU: de kern start Hop, Hop plaatst
# welcome met een gepubliceerde poort, en van buiten staat er een pagina.
#
# De kern boot met agentd-hopos in slot 1 (zoals tools/qemu-test-hop.sh).
# Van buiten gaat er één jobspec naar de leader van Hop: welcome van een
# artifact-server op de host (voor de gast 10.0.2.2), met "ports":{"http":80}.
# Hop zet ER_PORT_HTTP=80 in de env van de app en geeft de poort mee in
# START_SLOT; de kern zet uplink-poort 80 door naar het slot (DNAT, tcp en
# udp) vóór het startschot, en QEMU's hostfwd brengt 127.0.0.1:$WEBPORT naar
# poort 80 van de gast. Groen alleen als:
#
#   kern        HOPOS_BOOT, HOPOS_NET_UP, HOPOS_SYSTEM_UP, HOPOS_HOP_START
#               slot=1 en Hop's twee poorten (HOPOS_SLOT_PUBLISH van slot 1);
#   Hop         HOP_UP en HOP_LEADER via de servicer van slot 1;
#   van buiten  POST http://127.0.0.1:$LEADERPORT/v1/jobs wordt aangenomen;
#   de plaatsing HOP_JOB_PLACED slot=2, "slot 2: 1 port(s) published tcp+udp
#               on the uplink: :80 HOPOS_SLOT_PUBLISH" en
#               "slot 2: ... HOPOS_WELCOME_UP port=80";
#   van buiten  GET http://127.0.0.1:$WEBPORT/ geeft 200 met de bunny
#               ("( -.-)") en "slot 2", en GET /health geeft 200 "ok";
#   10.100.0.1  een job "gw" met als artifact http://10.100.0.1:80/health:
#               Hop haalt hem door de hairpin bij welcome (de publicatie op
#               het gateway-adres) en weigert hem pas bij de plaatsing (geen
#               ELF). De app laat de checksum naar 10.100.0.1 leeg
#               (link_trusted): welcome toetst hem wel (08-11, M4);
#   de stop     DELETE /v1/jobs/welcome: de kern trekt de poort in
#               ("slot 2: ports withdrawn from the uplink
#               HOPOS_SLOT_UNPUBLISH"), de app hoort de stopbel ("slot 2:
#               applib: stop requested ... HOPOS_APP_STOP grace_ms=2500"),
#               keert zelf terug uit main en doet het net-afscheid ("slot 2:
#               applib: shutdown code=0 ... HOPOS_APP_SHUTDOWN"), en de kern
#               ziet hem stil zonder intrekking ("slot 2: stopped, partition
#               and core released HOPOS_SLOT_STOPPED"); daarna antwoordt
#               127.0.0.1:$WEBPORT niet meer met de pagina. De tijd van het
#               verzoek tot stil staat in de meting (een Go-welcome kent de
#               bel niet: dan alleen de intrekking van de poort en
#               HOPOS_SLOT_STOPPED).
#
# Een HOPOS_PANIC, HOPOS_EXCEPTION, HOPOS_APP_PANIC, HOPOS_HOP_FAULT,
# HOPOS_HOP_EXIT, HOPOS_HOP_FAIL, HOPOS_SLOT_PUBLISH_FAIL, en bij de stop
# een HOPOS_APP_STOP_FORCED (de app negeerde de bel) of een
# HOPOS_PART_QUARANTINE (de kern moest intrekken) is meteen rood.
# Rood bewaart de console (en drukt hem af).
#
#   tools/qemu-test-welcome.sh             TIMEOUT=60 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-welcome.sh   bewaart ook een groene console
#   KEEP_PAGE=pad tools/qemu-test-welcome.sh  bewaart de pagina zoals curl hem gaf
#   GO_ELF=pad tools/qemu-test-welcome.sh   een Go-welcome (tamago)
#                                          als artifact (docs/go-apps.md): de
#                                          Go-app zegt "serving http://" en
#                                          kent /healthz in plaats van /health
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/WEBPORT   de host-poorten; standaard
#                                          vrije van het OS
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-60}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-welcome
PAGE="$ART/page.html"
ports SYS AGENT LEADER ART WEB

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), welcome, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
hop_elf

# De artifact-server: welcome zonder debug-info, de symbolen blijven voor
# de plaatsing. Met GO_ELF het tamago-image, dat zijn eigen regel en pad heeft.
UP_MARK="HOPOS_WELCOME_UP port=80"
HEALTH_PATH=/health
if [ -n "${GO_ELF:-}" ]; then
	cp "$GO_ELF" "$ART/welcome.elf"
	UP_MARK="serving http://"
	HEALTH_PATH=/healthz
else
	apps welcome
fi
serve

echo "== booten op QEMU virt met Hop (tot ${TIMEOUT}s; leader :$LEADERPORT, artifacts :$ARTPORT, web :$WEBPORT -> gast :80)"
hop_virt

BOOT_MARKS="HOPOS_BOOT|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_HOP_START slot=1 |slot 1: 2 port\\(s\\) published tcp\\+udp on the uplink: :8080 :9080 HOPOS_SLOT_PUBLISH|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 2: 1 port\\(s\\) published tcp\\+udp on the uplink: :80 HOPOS_SLOT_PUBLISH|slot 2: .*$UP_MARK"
STOP_MARKS="slot 2: ports withdrawn from the uplink HOPOS_SLOT_UNPUBLISH|slot 2: stopped, partition and core released HOPOS_SLOT_STOPPED"
if [ -z "${GO_ELF:-}" ]; then
	STOP_MARKS="$STOP_MARKS|slot 2: applib: stop requested by the kernel, 2500 ms to finish HOPOS_APP_STOP grace_ms=2500|slot 2: applib: shutdown code=0 .*HOPOS_APP_SHUTDOWN"
fi
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_APP_PANIC|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_SLOT_PUBLISH_FAIL|HOPOS_APP_STOP_FORCED"

JOB='{"name":"welcome","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}'
POSTED=""
PAGE_OK=""
HEALTH=""
DELETED=""
GONE=""
started

# 1. Boot, dan de jobspec naar de leader.
while alive && [ -z "$POSTED" ]; do
	if all "$BOOT_MARKS"; then post_job "$JOB" || true; fi
	step
done

# 2. De plaatsing, dan de pagina en /health van buiten.
while alive && [ -z "$PAGE_OK" ]; do
	if all "$PLACE_MARKS"; then
		code="$(curl -s -m 5 -o "$PAGE" -w '%{http_code}' "http://127.0.0.1:$WEBPORT/" 2>/dev/null || true)"
		if [ "$code" = 200 ] && grep -q -F '( -.-)' "$PAGE" && grep -q 'slot 2' "$PAGE"; then
			PAGE_OK="HTTP $code, $(wc -c <"$PAGE" | tr -d ' ') bytes"
			HEALTH="$(curl -s -m 5 -w ' HTTP %{http_code}' "http://127.0.0.1:$WEBPORT$HEALTH_PATH" 2>&1 || true)"
			break
		fi
	fi
	step
done
[ -n "${KEEP_PAGE:-}" ] && [ -s "$PAGE" ] && cp "$PAGE" "$KEEP_PAGE"

# 2b. Een gepubliceerde poort op 10.100.0.1: Hop haalt een "artifact" bij
# welcome via het gateway-adres.
GW_MARK="slot 1: .*job gw .*image ended before its program headers"
if [ -n "$PAGE_OK" ]; then
	P0="$POSTED"
	post_job '{"name":"gw","driver":"hop","artifacts":[{"url":"http://10.100.0.1:80/health"}],"memory_limit":16777216,"max_restarts":0}' || true
	POSTED="$P0"
	while alive && ! grep -a -q -E "slot 1: .*job gw .*HOP_JOB_FAILED" "$LOG"; do step; done
fi

# 3. De stop: de job weg, de poort dicht.
if [ -n "$PAGE_OK" ]; then
	DELETED="$(curl -s -m 20 -w ' HTTP %{http_code}' -X DELETE \
		"http://127.0.0.1:$LEADERPORT/v1/jobs/welcome" 2>&1 || true)"
	while alive && ! all "$STOP_MARKS"; do step; done
	if all "$STOP_MARKS"; then
		# Na de intrekking bereikt 127.0.0.1:$WEBPORT het slot niet meer:
		# geen pagina (een lege reply, een reset of een timeout).
		sleep 0.5
		code="$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$WEBPORT/" 2>/dev/null || true)"
		case "$code" in
		200) GONE="" ;;
		*) GONE="no page any more (curl code ${code:-none})" ;;
		esac
	fi
fi

qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS" "$GW_MARK" "$STOP_MARKS"
posted
if [ -n "$PAGE_OK" ]; then
	echo "   ok  GET http://127.0.0.1:$WEBPORT/: $PAGE_OK, met de bunny"
else
	echo "   ROOD GET http://127.0.0.1:$WEBPORT/: geen pagina met de bunny"
	fail=1
fi
case "$HEALTH" in
"ok"*"HTTP 200") echo "   ok  GET $HEALTH_PATH: $(printf '%s' "$HEALTH" | tr '\n' ' ')" ;;
*) echo "   ROOD GET $HEALTH_PATH: ${HEALTH:-nooit gevraagd}"; fail=1 ;;
esac
case "$DELETED" in
*"HTTP 2"*) echo "   ok  DELETE /v1/jobs/welcome: $DELETED" ;;
*) echo "   ROOD DELETE /v1/jobs/welcome: ${DELETED:-nooit gedaan}"; fail=1 ;;
esac
if [ -n "$GONE" ]; then
	echo "   ok  na de stop: $GONE"
else
	echo "   ROOD na de stop: de poort gaf nog de pagina, of de stop kwam nooit"
	fail=1
fi
served welcome.elf
reds
took
# De meetlat van de stop: van het verzoek (de poort weg) tot stil.
echo "   meting: $(tr -d '\r' <"$LOG" | grep -E 'HOPOS_SLOT_UNPUBLISH|HOPOS_APP_STOP|HOPOS_APP_SHUTDOWN|HOPOS_SLOT_STOPPED' | sed -E 's/^.*(slot 2:.*)$/\1/' | tr '\n' ';')"
verdict qemu-welcome
echo "welcome-kring groen"
