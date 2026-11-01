#!/bin/sh
# De stille OS-core van 30-09 als toets: een trage schijf mag de kern, Hop
# en de switch niet stilzetten.
#
# De soak (ruim 1100 runs) vond één stille run van hoplb: de periodieke
# hopfs-commit (FLUSH, de boom, FLUSH) wachtte synchroon op de schijf, op de
# executor van de OS-core. Op macOS is een FLUSH van QEMU een F_FULLFSYNC van
# 3 tot 786 ms; met 2,5 s per verzoek nagebootst was dat een stilte van 7 s
# (`late_ms=6611` op de tik), en zolang geen tik, geen verkeer. Sinds 30-09
# is schijf-I/O een submit plus een await, sinds 04-10 een ticket in de
# wachtrij met een await op de eigen completion (blkdev::Queue, de bel van
# de IRQ-lijn), en deze toets houdt dat zo.
#
# QEMU krijgt een null-co met $DISKLAT nanoseconden per verzoek (standaard
# 1,5 s; image/qemu-run.sh DISKLAT), de kern Hop (slot 1, op de OS-core),
# en van buiten gaat er één jobspec naar Hop: welcome met "ports":{"http":80}
# (zoals tools/qemu-test-welcome.sh). Zodra de pagina er is, vraagt de toets
# hem $WINDOW seconden lang elke 0,2 s op en meet hij elke curl. Groen
# alleen als:
#
#   de boot      HOPOS_BOOT, HOPOS_DISK_UP, HOPOS_FS_UP, HOPOS_NET_UP,
#                HOPOS_HOP_START slot=1 core=0, HOP_UP, en de plaatsing van
#                welcome (HOP_JOB_PLACED slot=2, HOPOS_WELCOME_UP port=80);
#   de commit    minstens één HOPOS_FS_COMMIT in het venster, en tijdens de
#                I/O van die commit (de tikken vóór zijn regel: vier
#                verzoeken van $DISKLAT) minstens drie curls;
#   de pagina    elke curl in het venster 200 met de bunny, binnen 100 ms;
#   de tik       elke HOPOS_TICK na de eerste met late_ms onder 100, en geen
#                tiknummer overgeslagen.
#
# Een HOPOS_PANIC, HOPOS_EXCEPTION, HOPOS_APP_PANIC, HOPOS_HOP_FAULT,
# HOPOS_HOP_EXIT, HOPOS_HOP_FAIL of HOPOS_FS_COMMIT_FAIL is meteen rood. Rood
# bewaart de console (en drukt hem af).
#
#   tools/qemu-test-slowdisk.sh                TIMEOUT=120 s tot de pagina
#   DISKLAT=2500000000 tools/qemu-test-slowdisk.sh   de trage schijf van de soak
#   WINDOW=30 tools/qemu-test-slowdisk.sh      het meetvenster in seconden
#   KEEP_LOG=pad tools/qemu-test-slowdisk.sh   bewaart ook een groene console
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/WEBPORT   de host-poorten; standaard
#                                              vrije van het OS
#   HOP_DIR=pad                                de hop-repo (standaard ../hop/hop)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TIMEOUT="${TIMEOUT:-120}"
WINDOW="${WINDOW:-30}"
DISKLAT="${DISKLAT:-1500000000}"
TARGET=aarch64-unknown-none-softfloat
scratch qemu-slowdisk
PAGE="$ART/page.html"
CURLS="$ART/curls.txt"
ports SYS AGENT LEADER ART WEB

cd "$DIR"
echo "== bouwen: hopos (qemuvirt), welcome, en agentd-hopos in $HOP_DIR"
cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
apps welcome
hop_elf
serve

echo "== booten op QEMU virt met Hop op een schijf van $DISKLAT ns per verzoek (tot ${TIMEOUT}s; web :$WEBPORT -> gast :80)"
hop_virt DISKLAT="$DISKLAT"

# Het laatste tiknummer op de console: de klok van de gast.
tick() { tr -d '\r' <"$LOG" | sed -n 's/^HOPOS_TICK \([0-9]*\) .*/\1/p' | tail -1; }

BOOT_MARKS="HOPOS_BOOT|HOPOS_DISK_UP model=virtio-blk|HOPOS_FS_UP|HOPOS_NET_UP|HOPOS_HOP_START slot=1 core=0 |slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
PLACE_MARKS="slot 1: .*HOP_JOB_PLACED slot=2|slot 2: .*HOPOS_WELCOME_UP port=80"
RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_APP_PANIC|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_FS_COMMIT_FAIL"

JOB='{"name":"welcome","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}'
POSTED=""
UP=""
started

# 1. Boot, dan de jobspec naar de leader.
while alive && [ -z "$POSTED" ]; do
	if all "$BOOT_MARKS"; then POST_MAX=30 post_job "$JOB" || true; fi
	step
done

# 2. De plaatsing, dan de eerste pagina.
while alive && [ -z "$UP" ]; do
	if all "$PLACE_MARKS"; then
		code="$(curl -s -m 5 -o "$PAGE" -w '%{http_code}' "http://127.0.0.1:$WEBPORT/" 2>/dev/null || true)"
		if [ "$code" = 200 ] && grep -q -F '( -.-)' "$PAGE"; then
			UP="$(tick)"
		fi
	fi
	step
done

# 3. Het venster: elke 0,2 s de pagina, met de tik van de gast erbij.
: >"$CURLS"
if [ -n "$UP" ]; then
	W0=$(date +%s)
	while ! has "$RED" && kill -0 "$QPID" 2>/dev/null && [ $(($(date +%s) - W0)) -lt "$WINDOW" ]; do
		t="$(tick)"
		r="$(curl -s -m 5 -o "$PAGE" -w '%{http_code} %{time_total}' "http://127.0.0.1:$WEBPORT/" 2>/dev/null || echo "000 5")"
		bunny=0
		grep -q -F '( -.-)' "$PAGE" 2>/dev/null && bunny=1
		echo "$t $r $bunny" >>"$CURLS"
		: >"$PAGE"
		sleep 0.2
	done
fi
END_TICK="$(tick)"
qemu_stop

fail=0
marks "$BOOT_MARKS" "$PLACE_MARKS"
posted
if [ -z "$UP" ]; then
	echo "   ROOD de pagina van welcome kwam nooit"
	fail=1
fi

# De meting: tikken, curls en de commits in het venster.
tr -d '\r' <"$LOG" >"$ART/console.txt"
if ! python3 - "$ART/console.txt" "$CURLS" "${UP:-0}" "${END_TICK:-0}" "$DISKLAT" <<'PY'; then
import re, sys
console, curls, up, end, lat = sys.argv[1], sys.argv[2], int(sys.argv[3] or 0), int(sys.argv[4] or 0), int(sys.argv[5])
lines = open(console, errors="replace").read().splitlines()
fail = False

# De tik: elke seconde één, geen gat, late_ms klein (de eerste tik telt niet:
# daar zit de boot nog in).
ticks, last, worst = [], 0, (0, 0)
commits = []
for l in lines:
    m = re.match(r"HOPOS_TICK (\d+) late_ms=(\d+)", l)
    if m:
        n, late = int(m.group(1)), int(m.group(2))
        if last and n != last + 1:
            print(f"   ROOD tik {last} gevolgd door {n}: een tik overgeslagen")
            fail = True
        last = n
        ticks.append((n, late))
        if n > 1 and late > worst[1]:
            worst = (n, late)
    elif "HOPOS_FS_COMMIT" in l and "COMMIT_FAIL" not in l:
        commits.append(last)
late = [(n, l) for n, l in ticks if n > 1 and l >= 100]
if late:
    print(f"   ROOD {len(late)} tik(ken) met late_ms >= 100, de eerste: tik {late[0][0]} late_ms={late[0][1]}")
    fail = True
else:
    print(f"   ok  {len(ticks)} tikken, elke seconde, de laatste van tik 2 af: tik {worst[0]} late_ms={worst[1]}")

# De curls in het venster.
rows = []
for l in open(curls).read().splitlines():
    t, code, secs, bunny = l.split()
    rows.append((int(t or 0), code, float(secs), bunny == "1"))
bad = [r for r in rows if r[1] != "200" or not r[3] or r[2] >= 0.1]
if not rows:
    print("   ROOD geen curls in het venster")
    fail = True
elif bad:
    t, code, secs, bunny = bad[0]
    print(f"   ROOD {len(bad)} van {len(rows)} curls niet 200 met de bunny binnen 100 ms; de eerste bij tik {t}: HTTP {code} in {secs * 1000:.0f} ms")
    fail = True
else:
    slow = max(r[2] for r in rows)
    print(f"   ok  {len(rows)} curls naar welcome in het venster (tik {up} tot {end}), alle 200 met de bunny, de traagste {slow * 1000:.0f} ms")

# De commit: minstens één in het venster, en curls terwijl zijn I/O liep (de
# vier verzoeken vóór zijn regel: FLUSH, de boom, de kop, FLUSH).
span = max(1, -(-4 * lat // 1_000_000_000))
during = []
for c in commits:
    if up <= c <= end:
        n = [r for r in rows if c - span <= r[0] <= c]
        during.append((c, n))
if not during:
    print(f"   ROOD geen HOPOS_FS_COMMIT in het venster (tik {up} tot {end}; commits bij tik {commits})")
    fail = True
else:
    best = max(during, key=lambda d: len(d[1]))
    c, n = best
    if len(n) < 3:
        print(f"   ROOD de commit bij tik {c} had maar {len(n)} curl(s) tijdens zijn I/O (tik {c - span} tot {c})")
        fail = True
    else:
        slow = max(r[2] for r in n)
        print(f"   ok  HOPOS_FS_COMMIT bij tik {c}: {len(n)} curls tijdens zijn I/O (tik {c - span} tot {c}), de traagste {slow * 1000:.0f} ms")
sys.exit(1 if fail else 0)
PY
	fail=1
fi
reds
took
verdict qemu-slowdisk
echo "slowdisk groen"
