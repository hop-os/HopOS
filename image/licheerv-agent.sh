#!/bin/sh
# Bouw HopOS v3 voor de Sipeed LicheeRV Nano (Sophgo SG2002 / CV181x,
# XuanTie C906): de kern-binary als MONITOR-slot van fip.bin, zoals de
# Go-generatie (image/licheerv-agent.sh op tag v2.2.8). De vendor-FSBL doet klok- en
# DDR-init en springt ons in MACHINE MODE binnen op RUNADDR; OpenSBI, U-Boot
# en Linux komen er niet meer aan te pas. HopOS is zelf de monitor: de kooi is
# PMP plus Sv39, en PMP kan alleen machine mode (docs/boards-riscv.md).
#
#   image/licheerv-agent.sh                → target/licheerv/fip-licheerv.bin
#                                            en hopos-licheerv.img (de hele
#                                            kaart, dd-baar)
#   image/licheerv-agent.sh /dev/diskN     → idem, plus fip.bin op de
#                                            FAT-bootpartitie van een kaart
#                                            die er al een heeft (de snelle
#                                            iteratie)
#   CFG="a.cfg b.cfg" image/licheerv-agent.sh
#                                          → een andere hopos.cfg in het
#                                            venster van het image, de
#                                            bestanden aaneengeplakt, de
#                                            laatste waarde wint (standaard
#                                            image/cfg/headless.cfg; CFG=
#                                            zonder pad: een leeg venster)
#   STAGE=/pad/hop.elf image/licheerv-agent.sh
#                                          → met die ELF in de kern gebakken
#                                            als Hop: de eerste bewoner, in
#                                            slot 1 met de bevoegdheid, zoals
#                                            op elk board (de release)
#   FEATURES=gui image/licheerv-agent.sh
#                                          → met die features van hopos
#                                            achter board-licheerv (zoals
#                                            uefi-run.sh)
#
# hopos.cfg: de FSBL geeft geen DTB en geen bootargs, en de kern leest (nog)
# geen SD-kaart, dus de config gaat IN het image, in het venster van 16 KiB
# dat elke kern draagt (board/src/cfgwin.rs; Go deed hetzelfde met
# image/hopcfg). Het script patcht monitor.bin vóór genfip, dus de
# checksums van de FIP kloppen vanzelf; `hop image` op een kaart of op
# fip.bin rekent ze zelf na. Let op: wat erin staat (een hopos.apikey)
# staat dan ook in fip.bin op de kaart.
#
# Nodig: de donor-FIP en fiptool.py uit een Sipeed-release
# (LICHEERV_DONOR, LICHEERV_FIPTOOL; standaard image/firmware/licheerv/,
# herkomst in de LEESMIJ.txt daar) en rust-objcopy (rustup component add
# llvm-tools).
#
# GEHASHT: de donor is vendor-code die vóór ons draait (FSBL, DDR-training).
# Het script drukt de SHA-256 van de donor en van de nieuwe FIP af; met
# LICHEERV_DONOR_SHA256 gezet weigert het elke andere donor. Zo is "welke
# FSBL draaide er onder deze boot" een getal in het logboek, geen gok.
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
TARGET=riscv64gc-unknown-none-elf
OUT="$DIR/target/licheerv"
DONOR="${LICHEERV_DONOR:-$DIR/image/firmware/licheerv/donor-fip.bin}"
FIPTOOL="${LICHEERV_FIPTOOL:-$DIR/image/firmware/licheerv/fiptool.py}"

# RUNADDR is NIET DRAM-start (GEMETEN 30-07): de FSBL laadt na ons image ook
# LOADER_2ND (U-Boot) en decomprimeert dat naar 0x8020_0020 (~600 KB), en hij
# woont zelf in het lage DRAM terwijl hij dat doet. 0x8040_0000 is geprobeerd
# (19-08) en het board BOOT DAAR NIET. Dit getal staat ook in hopos/build.rs
# (KERN_BASE van board-licheerv) en board/licheerv/src/lib.rs (KERN_RAM).
RUNADDR=0x84000000

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }

[ -f "$DONOR" ] || { echo "donor-fip ontbreekt: $DONOR (zet LICHEERV_DONOR)" >&2; exit 1; }
[ -f "$FIPTOOL" ] || { echo "fiptool ontbreekt: $FIPTOOL (zet LICHEERV_FIPTOOL)" >&2; exit 1; }
need_objcopy licheerv-agent

DONOR_SHA="$(sha "$DONOR")"
if [ -n "${LICHEERV_DONOR_SHA256:-}" ] && [ "$DONOR_SHA" != "$LICHEERV_DONOR_SHA256" ]; then
	echo "WEIGER: donor $DONOR heeft sha256 $DONOR_SHA, verwacht $LICHEERV_DONOR_SHA256" >&2
	exit 1
fi
echo "donor: $DONOR sha256=$DONOR_SHA" >&2

mkdir -p "$OUT"
cd "$DIR"
# De eerste bewoner: er is geen QEMU die hem in het RAM legt, dus gaat hij
# in de kern (board/licheerv/build.rs, HOPOS_EMBED). Zonder debug-info en zonder lokale symbolen (zoals
# tools/release.sh), met de globale: de plaatsing leest RamStart en de rest
# uit de symbooltabel, en die ligt tijdens de plaatsing in de schrapruimte
# boven de segmenten (kern::system::place). Met de lokale labels van
# riscv64 is die tabel bij Hop 6,6 MB (image 8,3 MB) en bleef er in zijn
# 10 MiB 2 KB over; zonder is de staart 2 KB (image 1,7 MB, 03-10).
STAGE="${STAGE:-}"
if [ -n "$STAGE" ]; then
	[ -f "$STAGE" ] || { echo "STAGE=$STAGE bestaat niet" >&2; exit 1; }
	"$OBJCOPY" --strip-debug --discard-all "$STAGE" "$OUT/stage.elf"
	HOPOS_EMBED="$OUT/stage.elf"
	echo "stage: $STAGE als Hop, $(wc -c <"$HOPOS_EMBED" | tr -d ' ') bytes sha256=$(sha "$HOPOS_EMBED")" >&2
else
	HOPOS_EMBED=""
fi
export HOPOS_EMBED
echo "== kern bouwen (hopos --features board-licheerv${FEATURES:+,$FEATURES}, $TARGET) ==" >&2
cargo build --quiet --release --target "$TARGET" -p hopos --features "board-licheerv${FEATURES:+,$FEATURES}"
ELF="$DIR/target/$TARGET/release/hopos"

# Het ELF begint op RUNADDR met _start (link-riscv.ld, KERN_BASE uit
# build.rs); de monitor-blob is het platte beeld vanaf daar. Geen trapstub
# ervoor zoals in Go: `_start` zet zelf mtvec op de trap van de kern.
# De ELF-entry (e_entry, offset 24, little-endian) moet RUNADDR zijn: een
# ander linkscript is een image dat de FSBL midden in iets laat springen.
ENTRY="$(python3 -c 'import struct,sys; print(hex(struct.unpack_from("<Q", open(sys.argv[1],"rb").read(32), 24)[0]))' "$ELF")"
[ "$ENTRY" = "$RUNADDR" ] || { echo "WEIGER: entry $ENTRY is niet RUNADDR $RUNADDR (linkscript?)" >&2; exit 1; }
"$OBJCOPY" -O binary "$ELF" "$OUT/monitor.bin"
CFG="${CFG-$DIR/image/cfg/headless.cfg $DIR/image/cfg/licheerv.cfg}"
if [ -n "$CFG" ]; then
	for f in $CFG; do
		[ -f "$f" ] || { echo "config ontbreekt: $f" >&2; exit 1; }
	done
	# Het venster van elke kern (board/src/cfgwin.rs): precies één, en de
	# tekst moet passen en UTF-8 zijn, anders weigert image/hopcfg.py.
	# shellcheck disable=SC2086 # CFG is een lijst bestanden
	python3 "$DIR/image/hopcfg.py" set "$OUT/monitor.bin" $CFG
else
	echo "cfg: none (CFG=pad): the node boots on its defaults, HOPOS_MAC_FIXED" >&2
fi
echo "monitor: $(wc -c <"$OUT/monitor.bin" | tr -d ' ') bytes sha256=$(sha "$OUT/monitor.bin")" >&2

echo "== fip-licheerv.bin ==" >&2
# De donor draagt een param2-blok (CVLD02) met een runaddr voor LOADER_2ND;
# die op nul zetten, zoals de Go-build deed, anders springt de FSBL na ons
# alsnog U-Boot in.
python3 - "$DONOR" "$OUT/donor-runaddr0.bin" <<'PYEOF'
import struct, sys
d = bytearray(open(sys.argv[1], "rb").read())
hits, i = [], 0
while True:
    i = d.find(b"CVLD02\n\x00", i)
    if i < 0:
        break
    ra = struct.unpack_from("<I", d, i + 60)[0]
    if 0x80000000 <= ra < 0x90000000:
        hits.append(i)
    i += 1
if len(hits) != 1:
    sys.exit(f"fip: {len(hits)} param2 blocks found, expected 1")
struct.pack_into("<I", d, hits[0] + 60, 0)
open(sys.argv[2], "wb").write(bytes(d))
PYEOF
python3 "$FIPTOOL" genfip "$OUT/fip-licheerv.bin" \
	--OLD_FIP "$OUT/donor-runaddr0.bin" \
	--MONITOR "$OUT/monitor.bin" \
	--MONITOR_RUNADDR "$RUNADDR" 2>/dev/null
echo "fip: $OUT/fip-licheerv.bin sha256=$(sha "$OUT/fip-licheerv.bin")" >&2

# De hele kaart (tools/mkcard): MBR plus FAT16 met alleen fip.bin, de
# geometrie van het donor-image op LBA 1, dd-baar. GEEN -vollabel: de
# BROM-parser is niet van ons, en dit is de vorm die Go bewees (tag v2.2.8).
cargo run -q -p mkcard -- -o "$OUT/hopos-licheerv.img" -size 64 -verify \
	"$OUT/fip-licheerv.bin=fip.bin" >&2
echo "card: $OUT/hopos-licheerv.img (dd: diskutil unmountDisk /dev/diskN && sudo dd if=$OUT/hopos-licheerv.img of=/dev/rdiskN bs=4m)" >&2

DISK="${1:-}"
[ -n "$DISK" ] || { echo "flash: image/licheerv-agent.sh /dev/diskN" >&2; exit 0; }
diskutil mount "${DISK}s1" >/dev/null
MNT=$(diskutil info "${DISK}s1" | sed -n 's/.*Mount Point: *//p')
[ -n "$MNT" ] && mount | grep -q " on $MNT " || {
	echo "WEIGER: ${DISK}s1 is niet echt gemount (mountpoint: '$MNT')" >&2
	exit 1
}
cp "$OUT/fip-licheerv.bin" "$MNT/fip.bin"
sync
[ "$(sha "$MNT/fip.bin")" = "$(sha "$OUT/fip-licheerv.bin")" ] || {
	echo "FOUT: teruglezen van fip.bin klopt niet" >&2
	exit 1
}
diskutil unmount "${DISK}s1" >/dev/null
echo "fip.bin op ${DISK}s1 (sha256 geverifieerd): de kaart kan in de LicheeRV." >&2
