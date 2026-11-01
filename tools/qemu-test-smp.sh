#!/bin/sh
# SMP-apps op QEMU: een jobspec met twee cores, via Hop, in één kooi.
#
# QEMU met vier cores: de kern en Hop op de OS-core (core 0), drie
# app-cores. Van buiten gaat één jobspec naar de leader van Hop met
# `"cpu_shares": 2048` (Hop maakt daar `cores: 2` van, runner/hopos.rs) en
# `ROLE=SMP` in de env. Hop haalt appspike van de artifact-server, stroomt
# hem de kern in, en de kern plaatst hem op een span van twee aaneengesloten
# app-cores. De runtime van de app vraagt zijn tweede core via de
# control-page (CTRL_SMP_REQ), de servicer ziet het, en de kern dispatcht
# hem in de kooi van de app (prepare_smp, de SMP-trampoline, PSCI de eerste
# keer). Groen alleen als:
#
#   kern        de boot-markers van tools/qemu-test-hop.sh (tot HOP_UP);
#   plaatsing   HOP_JOB_PLACED slot=2, HOPOS_SLOT_START slot=2 core=1, de
#               contexten geketend over twee cores (HOPOS_CAGE_SMP), en de
#               tweede core in de kooi (HOPOS_SMP_DISPATCH_OK core 2,
#               HOPOS_SMP_CORE);
#   de app      "applib: core 1 of 2 up" (HOPOS_APP_SMP_UP), de SMP-toets
#               (HOPOS_APPSPIKE_SMP ok cores=2: een taak op core 1 telde
#               terwijl core 0 zijn outbox schreef) en DONE pass=10 fail=0;
#   de stop     appspike stopt met code 0 en Hop herstart hem: de stop moet
#               BEIDE cores stil zien (E9) en de herstart haalt dezelfde
#               span weer op, dus een tweede HOPOS_APPSPIKE_SMP ok.
#
# Rood is meteen: een paniek, een fault, een quarantaine, een geweigerd of
# mislukt SMP-verzoek (HOPOS_SMP_REJECT, HOPOS_SMP_DISPATCH_FAIL,
# HOPOS_APP_SMP_FAIL). Rood bewaart de console (en drukt hem af).
#
#   tools/qemu-test-smp.sh                 TIMEOUT=90 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-smp.sh    bewaart ook een groene console
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT   de host-poorten; standaard vrije
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-90}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-smp
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
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|HOPOS_SLOT_START slot=2 core=1 |slot 2: 2 cores from core 1, contexts chained HOPOS_CAGE_SMP|slot 2: SMP core 2 dispatched HOPOS_SMP_DISPATCH_OK|HOPOS_SMP_CORE|slot 2: applib: core 1 of 2 up .*HOPOS_APP_SMP_UP|slot 2: HOPOS_APPSPIKE_SMP ok cores=2|slot 2: HOPOS_APPSPIKE_DONE pass=10 fail=0"
AGAIN="HOPOS_APPSPIKE_SMP ok cores=2"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_APP_PANIC|HOPOS_SMP_REJECT|HOPOS_SMP_DISPATCH_FAIL|HOPOS_APP_SMP_FAIL|HOPOS_APPSPIKE_[A-Z_]* FAIL|HOPOS_CAGE_FAIL"

again() { all "$PLACE_MARKS" && [ "$(count "$AGAIN")" -ge 2 ]; }
job_loop '{"name":"smp","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"cpu_shares":2048,"env":{"ROLE":"SMP"}}' again
qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS"
n="$(count "$AGAIN")"
if [ "$n" -ge 2 ]; then
	echo "   ok  de stop over beide cores en de herstart: $n keer $AGAIN"
else
	echo "   ROOD na de stop kwam de SMP-toets niet terug ($n keer $AGAIN)"
	fail=1
fi
posted
reds
took
echo "   marker: $(tr -d '\r' <"$LOG" | grep -m1 -o 'HOPOS_APPSPIKE_SMP ok.*')"
verdict qemu-smp 150
echo "qemu-smp groen"
