#!/bin/sh
# Bouw HopOS v3 voor de Mac mini M4 (Apple t8132, board/apple): één raw
# bestand dat tegelijk de payload voor m1n1's proxy is én het bootobject voor
# `kmutil configure-boot --raw --entry-point 2048`.
#
#   image/apple-m4.sh                       → target/apple-m4/hopos-apple.img
#   CFG="a.cfg b.cfg" image/apple-m4.sh     een andere hopos.cfg in lagen in
#                                           het venster van het image, de
#                                           laatste waarde wint (standaard
#                                           image/cfg/default.cfg, apple.cfg
#                                           en headless.cfg; CFG= zonder pad:
#                                           een leeg venster)
#   APP=hop image/apple-m4.sh               + Hop (../hop/hop, rol hop)
#   image/apple/boot-cycle.sh target/apple-m4/hopos-apple.img [s]
#                                           → via m1n1's proxy laden en starten
#
# Het image is gelinkt op RAM_BASE = DRAM + 4 GB = 0x101_0000_0000
# (hopos/link-apple.ld) en begint met de bootstub (board/apple/src/head.rs):
# offset 0 waar een core uit reset landt (RVBAR), 0x800 waar de firmware de
# boot-core aflevert. Anders dan in Go (mkkernel -apple) vult de linker het
# parameterblok; dit script doet alleen objcopy en de toets die op een stick
# stuk kan gaan zonder dat cargo het ziet: staan de stub, de parameters en
# de kern waar de firmware ze zoekt?
#
# CFG=<pad>: de platform-config MEE IN HET IMAGE, in het venster van elke
# kern (board/src/cfgwin.rs, image/hopcfg.py): de kopregel "#HOPCFG1
# window=16384 len=...", de config, en '#'-padding (het formaat van Go's
# image/hopcfg, dus `hop image` kan hem bewerken). Nodig zodra wij het
# bootobject zijn: dan is er geen loader meer die hem in het geheugen legt.
# Tot 04-10 lag het venster (4 KB) op 0xF000; dat is nu weer alleen de plek
# van de loader (load.py), en de kern leest hem als het venster leeg is.
# Les van Go (25-09): een agent-image zonder config draaide als
# hopos-<random> met een open API; APP=hop zonder CFG is daarom luid.
#
# De loader (image/apple/load.py) komt van de Go-meetbank en praat m1n1's
# proxy: M1N1 (de m1n1-clone, standaard ~/Git/m1n1) en een venv met
# pyserial en construct (PYTHON=~/Git/m1n1/venv/bin/python3). Het draaiboek
# en de checklist: docs/boards-apple.md.
set -e

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
TARGET=aarch64-unknown-none-softfloat
OUT="$DIR/target/apple-m4"
RAM_BASE=0x10100000000

cd "$DIR"
mkdir -p "$OUT"
need_objcopy apple-m4
# EMBED=<ELF>: een gestripte app-ELF (Hop) in het kernimage zelf, voor een
# boot zonder loader (kmutil): board/apple/build.rs bakt hem in, de kern
# plaatst hem als Hop in slot 1 (Go: cmd/hopos-embed). Zonder EMBED= niets.
if [ -n "${EMBED-}" ]; then
	[ -f "$EMBED" ] || { echo "apple-m4: EMBED=$EMBED does not exist" >&2; exit 1; }
	"$OBJCOPY" --strip-debug "$EMBED" "$OUT/embed.elf"
	export HOPOS_EMBED="$OUT/embed.elf"
else
	export HOPOS_EMBED=""
fi
cargo build --quiet --release --target "$TARGET" -p hopos --features board-apple
ELF="$DIR/target/$TARGET/release/hopos"
IMG="$OUT/hopos-apple.img"
"$OBJCOPY" -O binary "$ELF" "$IMG"
# kmutil neemt een bootobject van hele 16 KiB-pagina's (de installer op de
# stick weigert anders: "not a whole number of 16K pages", 30-09): nullen
# achteraan tot de volgende grens; de stub, de parameters en de kern staan
# vooraan en de nullen doen niets.
SIZE=$(wc -c <"$IMG" | tr -d ' ')
PAD=$(((16384 - SIZE % 16384) % 16384))
if [ "$PAD" -gt 0 ]; then
	dd if=/dev/zero bs=1 count="$PAD" >>"$IMG" 2>/dev/null
fi

# De toets: het parameterblok op 0x100 ("HOPASTUB", doel, grootte, entry)
# moet kloppen met het ELF, de stub-ingang op 0x800 moet code zijn, en
# 0xF000 (de plek van de loader) leeg: de linker legt er niets, head.rs.
CFG="${CFG-$DIR/image/cfg/default.cfg $DIR/image/cfg/apple.cfg $DIR/image/cfg/headless.cfg}"
for f in $CFG; do
	[ -f "$f" ] || {
		echo "apple-m4: CFG: $f does not exist" >&2
		exit 1
	}
done
if [ "${APP-}" = hop ] && [ -z "$CFG" ]; then
	echo "apple-m4: WARNING: APP=hop without CFG= bakes no hopos.cfg into the image (Go 25-09: a node without config runs with a random name and an open API)" >&2
fi
python3 - "$IMG" "$RAM_BASE" <<'PY'
import struct, sys
path = sys.argv[1]
img = bytearray(open(path, "rb").read())
base = int(sys.argv[2], 0)
CFG_OFF, CFG_SIZE = 0xF000, 0x1000
magic, dst, size, entry = struct.unpack_from("<4Q", img, 0x100)
ok = True
def need(cond, what):
    global ok
    if not cond:
        print("apple-m4: " + what, file=sys.stderr)
        ok = False
need(magic == 0x4255545341504f48, "no HOPASTUB magic at 0x100")
need(dst == base, "stub target %#x is not RAM_BASE %#x" % (dst, base))
need(size <= len(img) < size + 16384 and len(img) % 16384 == 0,
     "stub size %#x does not fit the file size %#x (whole 16K pages, the stub first)" % (size, len(img)))
need(not any(img[size:]), "the padding after the stub is not zero")
need(size % 64 == 0, "stub size %#x is not a multiple of 64" % size)
need(entry == base + 0x10000, "entry %#x is not RAM_BASE + 0x10000" % entry)
need(img[0x800:0x804] != b"\0\0\0\0", "no code at the stub entry 0x800")
need(img[0:4] != b"\0\0\0\0", "no code at the reset stub 0x0")
need(len(img) >= CFG_OFF + CFG_SIZE, "the image ends before the loader's config place at 0xF000")
need(img[CFG_OFF:CFG_OFF + CFG_SIZE] == bytes(CFG_SIZE), "the loader's config place at 0xF000 is not empty")
if not ok:
    sys.exit(1)
print("apple-m4: stub ok: %d bytes, target %#x, entry %#x" % (size, dst, entry), file=sys.stderr)
PY
if [ -n "$CFG" ]; then
	# shellcheck disable=SC2086 # CFG is een lijst bestanden
	python3 "$DIR/image/hopcfg.py" set "$IMG" $CFG
fi

# Het image voor de staging (optioneel): de loader legt het in de
# loader-regio (board_apple::slots::STAGE_PA) met maat en magic.
pick_app ""
if [ -n "$IMAGE" ]; then
	strip_elf "$IMAGE" "$OUT/stage.elf"
else
	rm -f "$OUT/stage.elf"
fi

echo "apple-m4: $IMG ($(wc -c <"$IMG" | tr -d ' ') bytes)${CFG:+, config $CFG}${APP:+, stage.elf}" >&2
echo "apple-m4: load with  STAGE=\${STAGE:-} image/apple/boot-cycle.sh $IMG 90" >&2
