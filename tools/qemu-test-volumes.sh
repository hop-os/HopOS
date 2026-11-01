#!/bin/sh
# De volumes van een jobspec op QEMU: van Hop's START_SLOT tot in hopfs, en
# over een herstart van de taak heen.
#
# De kern boot met Hop in slot 1 (zoals tools/qemu-test-hop.sh). Van buiten
# gaat er één jobspec naar de leader met een volume en de VOLUME-toets van
# appspike:
#
#   "volumes": {"/volumes/demo": "/data"}      gedeeld pad naar taakpad
#   "env":     {"VOLUME": "/data/spike.txt"}
#
# Groen alleen als:
#
#   de start     de kern zet het volume in de tabel van de hopfs-actor:
#                "slot 2: 1 volume(s) mounted: /data -> /volumes/demo
#                HOPOS_SLOT_MOUNTS";
#   leven 1      appspike vindt niets, schrijft (HOPOS_APPSPIKE_VOLUME ok
#                wrote), de kern ziet de schrijf in het volume landen
#                ("hopfs: slot 2 saved /data/spike.txt as
#                /volumes/demo/spike.txt" HOPOS_FS_SAVED), en de hele doorloop
#                is groen (HOPOS_APPSPIKE_DONE pass=10 fail=0);
#   leven 2      appspike stopt met code 0, Hop plaatst de service opnieuw
#                (een nieuwe levensduur, een verse lege root), en die vindt
#                het bestand terug met dezelfde bytes (HOPOS_APPSPIKE_VOLUME
#                ok found ... root=fresh): het volume overleefde, de root niet.
#
# Een HOPOS_APPSPIKE_VOLUME FAIL, HOPOS_FS_MOUNTS (volumes geweigerd bij de
# registratie), "persistent volumes require" (een Hop op de ABI van
# alpha.10), en de rode markers van qemu-test-hop.sh zijn meteen rood. Rood
# bewaart de console (en drukt hem af).
#
# Hop stuurt de volumes mee sinds hop op de ABI van HopOS alpha.11 staat
# (hopos-runner, start_mounts.rs).
#
#   tools/qemu-test-volumes.sh             TIMEOUT=90 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test-volumes.sh
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT   de host-poorten (standaard vrije)
#   HOP_DIR=pad                            de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-90}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-vol
ports SYS AGENT LEADER ART

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), appspike, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
apps appspike
hop_elf
serve

echo "== booten op QEMU virt met Hop (tot ${TIMEOUT}s; system :$SYSPORT, agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT)"
hop_virt

BOOT_MARKS="HOPOS_BOOT|HOPOS_FS_UP fresh=1|HOPOS_SYSTEM_UP|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
LIFE1_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 2: 1 volume\\(s\\) mounted: /data -> /volumes/demo HOPOS_SLOT_MOUNTS|slot 2: HOPOS_APPSPIKE_VOLUME ok wrote path=/data/spike.txt|hopfs: slot 2 saved /data/spike.txt as /volumes/demo/spike.txt .*HOPOS_FS_SAVED|slot 2: HOPOS_APPSPIKE_DONE pass=10 fail=0"
LIFE2_MARKS="slot [0-9]+: HOPOS_APPSPIKE_VOLUME ok found path=/data/spike.txt bytes=[0-9]+ root=fresh"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOP_STATE_SKIPPED|HOPOS_CAGE_FAIL|HOPOS_APPSPIKE_VOLUME FAIL|HOPOS_FS_MOUNTS|persistent volumes require"

lives() { all "$LIFE1_MARKS" && all "$LIFE2_MARKS"; }
job_loop '{"name":"spike","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"env":{"VOLUME":"/data/spike.txt"},"volumes":{"/volumes/demo":"/data"}}' lives
qemu_stop

fail=0
marks "$BOOT_MARKS" "$LIFE1_MARKS" "$LIFE2_MARKS"
posted
reds
took
verdict qemu-vol
echo "qemu-volumes groen"
