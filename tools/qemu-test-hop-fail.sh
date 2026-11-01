#!/bin/sh
# De kring van een Hop die faalt: een falende Hop is een falende node
# (Derek, 23-10). De kern laat hem liggen, de watchdog reset de node, en de
# koude boot start de Hop van het bootmedium.
#
# De kern boot met agentd-hopos in slot 1 en de watchdog van virt (de
# i6300esb, die elke ring meekrijgt: `-device i6300esb -action
# watchdog=reset`, board/qemuvirt/src/wdt.rs); Hop plaatst welcome in slot 2. Dan één POST /restart met een
# "nieuw image" dat een geldige ELF is maar geen Hop: appspike. Hij plaatst
# (de som en de ELF-toets van de kern kloppen), draait zijn toetsen, en
# vertrekt dan uit zichzelf, ongevraagd: HOPOS_HOP_EXIT (of een fault). De
# kern herstart hem niet: HOPOS_HOP_DOWN en HOPOS_FATAL (docs/
# sequence-hopos.md §13.4), de kern parkeert, de pets stoppen, de i6300esb
# loopt af, QEMU reset, en de reset laadt de loader-blobs opnieuw: de
# staging draagt weer de Hop van de start. Groen alleen als:
#
#   de boot      zoals de welcome-kring, plus "watchdog: hardware reset
#                armed (i6300esb at ... on PCIe ...) HOPOS_WD_ARMED" en de
#                canary (HOPOS_CANARY_LIVE);
#   de vraag     202 op POST /restart, HOPOS_HOP_RESTART reason=request
#                image=new n=1, HOPOS_HOP_STOPPED reason=request left=self,
#                en een HOPOS_HOP_START slot=1 met de maat van het kapotte
#                image;
#   de val       HOPOS_HOP_EXIT of HOPOS_HOP_FAULT, dan HOPOS_HOP_DOWN en
#                HOPOS_FATAL, en geen tweede HOPOS_HOP_RESTART: de kern laat
#                hem liggen;
#   de watchdog  een tweede HOPOS_BOOT, met "the last reset came from this
#                watchdog" in zijn HOPOS_WD_ARMED;
#   de koude boot HOPOS_HOP_START slot=1 met de maat van de gestagede Hop
#                (dezelfde als bij de eerste boot), HOP_UP, en schoon
#                begonnen: geen HOP_ADOPTED (het blob stierf met de boot).
#
# De meting: de tijd van de val (HOPOS_HOP_EXIT/FAULT) tot de HOP_UP na de
# reset, in seconden van de host.
#
# Een HOPOS_PANIC, HOPOS_EXCEPTION, HOPOS_PART_QUARANTINE, een vertrek dat
# de bel nodig had (left=bell, left=gone), een tweede HOPOS_HOP_RESTART of
# HOPOS_WD_NONE is meteen rood. Rood bewaart de console.
#
#   tools/qemu-test-hop-fail.sh            TIMEOUT=180 standaard, in s
#   KEEP_LOG=pad tools/qemu-test-hop-fail.sh   bewaart ook een groene console
#   HOP_REV=worktree tools/qemu-test-hop-fail.sh   Hop zoals de werkboom van
#                                          de hop-repo nu is
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/WEBPORT   de host-poorten; standaard
#                                          vrije van het OS
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-180}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-hop-fail
ports SYS AGENT LEADER ART WEB

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), welcome, appspike, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
hop_elf
apps welcome appspike
# Het kapotte "nieuwe image": appspike, een geldige ELF die geen Hop is.
BAD_SUM="$(shasum -a 256 "$ART/appspike.elf" | cut -d' ' -f1)"
BAD_SIZE="$(wc -c <"$ART/appspike.elf" | tr -d ' ')"
serve

echo "== booten op QEMU virt met Hop en de i6300esb (tot ${TIMEOUT}s; agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT, web :$WEBPORT -> gast :80)"
hop_virt

BOOT_MARKS="HOPOS_BOOT|HOPOS_NET_UP|HOPOS_SYSTEM_UP|watchdog: hardware reset armed \\(i6300esb at 0x[0-9a-f]+ on PCIe.*HOPOS_WD_ARMED|HOPOS_HOP_START slot=1 |slot 1: .*HOP_UP|HOPOS_CANARY_LIVE"
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 2: .*HOPOS_WELCOME_UP port=80"
ASK_MARKS="HOPOS_HOP_RESTART reason=request image=new n=1|slot 1: Hop stopped for the restart HOPOS_HOP_STOPPED reason=request left=self|HOPOS_HOP_START slot=1 .* image=$BAD_SIZE "
FALL_MARKS="HOPOS_HOP_EXIT|HOPOS_HOP_FAULT"
DOWN_MARKS="HOPOS_HOP_DOWN|HOPOS_FATAL slot 1: Hop fell"
RED="HOPOS_PANIC|HOPOS_EXCEPTION|HOPOS_PART_QUARANTINE|left=bell|left=gone|HOPOS_WD_NONE|HOPOS_HOP_RESTART reason=crash"

JOB='{"name":"welcome","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}'

# after N: de console vanaf de N-de HOPOS_BOOT (1 = alles).
BOOTED='^boot: .*HOPOS_BOOT gen='
after() {
	tr -d '\r' <"$LOG" | awk -v n="$1" '/^boot: .*HOPOS_BOOT gen=/ { b++ } b >= n && !/^  \| /'
}
# again PATROON: staat het in de console van de tweede boot?
again() { after 2 | grep -q -E "$1"; }

POSTED=""
ASKED=""
T_FALL=""
T_UP=""
started

# 1. Boot, de jobspec, welcome op.
while alive && [ -z "$POSTED" ]; do
	if all "$BOOT_MARKS"; then post_job "$JOB" || true; fi
	step
done
while alive && ! all "$PLACE_MARKS"; do step; done

# De maat van de gestagede Hop, uit de eerste HOPOS_HOP_START.
STAGED_SIZE="$(tr -d '\r' <"$LOG" | grep -m1 -o 'HOPOS_HOP_START slot=1 .* image=[0-9]*' | sed 's/.*image=//')"

# 2. Het kapotte image.
if all "$PLACE_MARKS"; then
	ASKED="$(curl -s -m 60 -w ' HTTP %{http_code}' -X POST \
		-H 'Content-Type: application/json' \
		-d '{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf","sha256":"'"$BAD_SUM"'"}' \
		"http://127.0.0.1:$AGENTPORT/restart" 2>&1 || true)"
	while alive && ! { all "$ASK_MARKS" && has "$FALL_MARKS"; }; do step; done
	has "$FALL_MARKS" && T_FALL=$(date +%s)
fi

# 3. De watchdog, de reset en de koude boot.
UP_MARKS="HOPOS_HOP_START slot=1 .* image=$STAGED_SIZE |slot 1: .*HOP_UP"
if [ -n "$T_FALL" ]; then
	while alive && ! { [ "$(count "$BOOTED")" -ge 2 ] && again "slot 1: .*HOP_UP" && again "HOPOS_HOP_START slot=1 .* image=$STAGED_SIZE "; }; do
		step
	done
	again "slot 1: .*HOP_UP" && T_UP=$(date +%s)
	# Even laten: een overname zou nu zijn regel zetten.
	[ -z "$T_UP" ] || sleep 3
fi

qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS" "$ASK_MARKS" "$DOWN_MARKS"
posted
case "$ASKED" in
*"HTTP 202") echo "   ok  POST /restart (kapot image, $BAD_SIZE bytes): $ASKED" ;;
*) echo "   ROOD POST /restart (kapot image): ${ASKED:-nooit gedaan}"; fail=1 ;;
esac
if has "$FALL_MARKS"; then
	echo "   ok  de val: $(first "$FALL_MARKS")"
else
	echo "   ROOD het kapotte image viel niet (geen HOPOS_HOP_EXIT of HOPOS_HOP_FAULT)"
	fail=1
fi
n="$(count 'HOPOS_HOP_RESTART ')"
if [ "$n" = 1 ]; then
	echo "   ok  één HOPOS_HOP_RESTART: de kern herstartte het kapotte image niet"
else
	echo "   ROOD $n keer HOPOS_HOP_RESTART, verwacht 1"
	fail=1
fi
if [ "$(count "$BOOTED")" -ge 2 ]; then
	echo "   ok  de reset van de watchdog: een tweede HOPOS_BOOT"
else
	echo "   ROOD geen tweede HOPOS_BOOT: de watchdog resette niet"
	fail=1
fi
# De watchdog wapent als eerste in setup, dus vóór de bootregel: alleen de
# tweede boot kan dit zeggen.
if has "the last reset came from this watchdog.*HOPOS_WD_ARMED"; then
	echo "   ok  de tweede boot weet waarom: $(first 'the last reset came from this watchdog')"
else
	echo "   ROOD de tweede boot zegt niet dat de watchdog hem resette"
	fail=1
fi
if again "HOPOS_HOP_START slot=1 .* image=$STAGED_SIZE " && again "slot 1: .*HOP_UP"; then
	echo "   ok  de koude boot startte de gestagede Hop ($STAGED_SIZE bytes) en hij kwam op"
else
	echo "   ROOD na de reset geen HOPOS_HOP_START met de gestagede Hop ($STAGED_SIZE bytes) en HOP_UP"
	fail=1
fi
if again "HOP_ADOPTED"; then
	echo "   ROOD na de reset $(after 2 | grep -m1 'HOP_ADOPTED')"
	fail=1
else
	echo "   ok  schoon begonnen: geen HOP_ADOPTED na de reset"
fi
served welcome.elf
served appspike.elf
reds
took
if [ -n "$T_FALL" ] && [ -n "$T_UP" ]; then
	echo "   meting: van de val tot HOP_UP na de reset $((T_UP - T_FALL)) s"
fi
echo "   meting: $(tr -d '\r' <"$LOG" | grep -v '^  | ' | grep -E 'HOPOS_HOP_(RESTART|STOPPED|START|EXIT|FAULT|DOWN) |HOPOS_HOP_(EXIT|FAULT|DOWN)$|HOPOS_FATAL|HOPOS_BOOT gen' | sed -E 's/^.*(HOPOS_[A-Z_]+).*$/\1/' | tr '\n' ';')"
verdict qemu-hop-fail
echo "hop-faalkring groen"
