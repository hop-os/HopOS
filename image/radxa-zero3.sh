#!/bin/sh
# Bouw HopOS v3 voor de Radxa Zero 3E (Rockchip RK3566): de kern, het
# arm64-Image, en een compleet, dd-baar kaart-image. De vorm van
# image/radxa-zero3.sh op tag v2.2.8 (de Go-generatie), zonder Go: de
# ELF-naar-Image-stap (Go's mkkernel) staat hieronder als python3, de kaart
# bouwt tools/mkcard (de port van Go's mkcard).
#
#   image/radxa-zero3.sh                 de kaart met Hop als bewoner
#   APP=/pad/naar/elf image/...          een kant-en-klare Hop (de kern
#                                        plaatst alleen Hop, 01-11 O7)
#   APP= image/...                       zonder image (HOPOS_SLOT_NONE)
#   HOP_DIR=pad image/...                de hop-repo (standaard ../hop/hop)
#   CFG="a.cfg b.cfg" image/...          de config van de node, de bestanden
#                                        aaneengeplakt (standaard
#                                        image/cfg/headless.cfg, met GUI=1
#                                        headfull.cfg), in het venster van
#                                        hopos.img (board/src/cfgwin.rs);
#                                        de laatste waarde wint
#   NODE=radxa-2 image/...               hopos.node in de APPEND (ook de
#                                        bron van het MAC-adres); zonder
#                                        geen APPEND, en de node heet naar
#                                        zijn board en zijn MAC
#   RADXA_DONOR=pad image/...            een eigen donor-boot-blok (standaard
#                                        image/firmware/radxa/donor-boot.bin)
#   GUI=1 image/...                      de gui-smaak (`--features gui`,
#                                        docs/gui.md): de console op het glas
#                                        via VOP2 en HDMI, de DWC3's voor de
#                                        invoer, en de framebuffer-grant
#
# Uitvoer in target/radxa-zero3/: hopos.elf, hopos.img (het arm64-Image,
# met de config in zijn venster), hop.elf (het image van de bewoner,
# gestript), hopos.ird (de initrd: een lege hopos.cfg plus hop.elf),
# extlinux.conf en hopos-radxa-zero3.img (de kaart). Na de bouw leest het
# script hopos.img, extlinux.conf en hop.elf terug uit het kaart-image en
# vergelijkt hun sha256, en de config uit het enige venster op de kaart.
#
#   diskutil unmountDisk /dev/diskN
#   sudo dd if=target/radxa-zero3/hopos-radxa-zero3.img of=/dev/rdiskN bs=4m
#
# Boot-route: de BootROM leest de U-Boot-keten raw van vaste LBA's vóór de
# eerste partitie (idbloader met TPL/SPL op LBA 64, u-boot.itb met TF-A en
# U-Boot op LBA 16384). Die bytes zijn van Radxa: de donor, eenmalig uit hun
# officiële image gehaald en gehasht (image/firmware/radxa/LEESMIJ.txt). U-Boot's distro-boot vindt daarna
# /extlinux/extlinux.conf op onze FAT-partitie (type 0x0C: macOS mount hem
# na het flashen, dus een nieuwe kern is een cp en de APPEND-regel is
# bewerkbaar), laadt het Image en hopos.ird (als initrd) en `booti` springt
# op EL2 met x0 = DTB. Seriële console: USB-UART op de 40-pins header (pin
# 8 = TX, 10 = RX, 6 = GND), 1500000 8N1.
#
# Hop als bewoner (30-09): de kern heeft geen SD-driver en U-Boot 2023.10
# laadt per label precies één initrd, dus draagt die ene initrd hopos.cfg
# én hop.elf (de container van image/radxa-initrd.py; de kern splitst hem in
# board/rk3566/src/initrd.rs, waar ook staat waarom niet de komma-lijst of
# een ingebakken Hop).
set -e
DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
OUT="$DIR/target/radxa-zero3"
TARGET=aarch64-unknown-none-softfloat
# De grootste initrd die de kern naar de heap haalt (board_rk3566::INITRD_MAX).
INITRD_MAX=16777216
mkdir -p "$OUT"

# 1. De kern.
echo "== cargo build (board-rk3566)" >&2
# Een eigen target-map: een build voor een ander board in de gedeelde map
# zou anders het ELF onder deze stap vervangen (dezelfde binary-naam).
FEATURE=board-rk3566
if [ "${GUI:-0}" = 1 ]; then
	FEATURE="$FEATURE,gui"
fi
(cd "$DIR" && cargo build --release --target "$TARGET" \
	--target-dir "$OUT/cargo" -p hopos --features "$FEATURE")
ELF="$OUT/cargo/$TARGET/release/hopos"
cp "$ELF" "$OUT/hopos.elf"

# 2. ELF naar arm64-Image (Linux Documentation/arch/arm64/booting.rst): de
#    PT_LOAD-segmenten plat vanaf IMAGE_BASE, met de 64-byte header vooraan.
#    text_offset = IMAGE_BASE - DRAM_BASE: U-Boot VERPLAATST een Image met
#    relocatable = 0 naar bi_dram[0].start + text_offset, ongeacht waar het
#    bestand geladen werd. GEMETEN 05-08: DRAM-start is 0x20_0000; met
#    text_offset = load landde de kern 2 MB te hoog en zweeg hij na
#    "Starting kernel". Moet gelijk zijn aan hopos/link-rk3566.ld.
python3 - "$OUT/hopos.elf" "$OUT/hopos.img" <<'PYEOF'
import struct, sys
IMAGE_BASE, DRAM_BASE = 0x02200000, 0x00200000
elf = open(sys.argv[1], "rb").read()
if elf[:4] != b"\x7fELF" or elf[4] != 2 or struct.unpack_from("<H", elf, 18)[0] != 183:
    sys.exit("hopos.elf is geen aarch64-ELF64")
entry, phoff = struct.unpack_from("<QQ", elf, 24)
phentsize, phnum = struct.unpack_from("<HH", elf, 54)
file_end, mem_end, segs = IMAGE_BASE, IMAGE_BASE, []
for i in range(phnum):
    p_type, _, off, vaddr, paddr, filesz, memsz, _ = struct.unpack_from("<IIQQQQQQ", elf, phoff + i * phentsize)
    if p_type != 1 or memsz == 0:
        continue
    if paddr < IMAGE_BASE + 64:
        sys.exit(f"segment op {paddr:#x} ligt onder IMAGE_BASE+64: geen plaats voor de header")
    segs.append((paddr, elf[off:off + filesz]))
    file_end = max(file_end, paddr + filesz)
    mem_end = max(mem_end, paddr + memsz)
img = bytearray(file_end - IMAGE_BASE)
for paddr, data in segs:
    img[paddr - IMAGE_BASE:paddr - IMAGE_BASE + len(data)] = data
rel = entry - IMAGE_BASE
if rel <= 0 or rel % 4 or rel >= 1 << 27:
    sys.exit(f"entry {entry:#x} is niet te bereiken met een branch vanaf {IMAGE_BASE:#x}")
struct.pack_into("<I", img, 0, 0x14000000 | (rel // 4))  # code0: b _start
struct.pack_into("<I", img, 4, 0)                          # code1
struct.pack_into("<Q", img, 8, IMAGE_BASE - DRAM_BASE)     # text_offset
struct.pack_into("<Q", img, 16, mem_end - IMAGE_BASE)      # image_size (incl. BSS en stack)
struct.pack_into("<Q", img, 24, 0b010)                     # flags: LE, 4K, relocatable = 0
struct.pack_into("<I", img, 56, 0x644D5241)                # magic "ARM\x64"
open(sys.argv[2], "wb").write(img)
print(f"hopos.img: {len(img)} bytes, image_size {mem_end - IMAGE_BASE:#x}, entry {entry:#x} at {IMAGE_BASE:#x}", file=sys.stderr)
PYEOF
# De config in het venster van de kern (board/src/cfgwin.rs), zoals op elk
# board: hij wint van hopos.cfg in de initrd en van de APPEND.
CFG="${CFG:-$DIR/image/cfg/$([ "${GUI:-0}" = 1 ] && echo headfull || echo headless).cfg}"
for f in $CFG; do
	[ -f "$f" ] || { echo "FOUT: CFG: $f bestaat niet" >&2; exit 1; }
done
# shellcheck disable=SC2086 # CFG is een lijst bestanden
python3 "$DIR/image/hopcfg.py" set "$OUT/hopos.img" $CFG

# 3. Het image van de bewoner: dezelfde keuzes als image/rpi4.sh. Gestript
#    (Hop 19 MB naar 1,5 MB, 30-09): de kern haalt de hele initrd naar zijn
#    heap, en de symbolen staan in de build van de hop-repo.
need_objcopy radxa-zero3
pick_app hop
rm -f "$OUT/hop.elf"
if [ -n "$IMAGE" ]; then
	"$OBJCOPY" --strip-debug "$IMAGE" "$OUT/hop.elf"
fi

# 4. De initrd en de extlinux-regel. De APPEND-regel is het bootargs-kanaal,
#    INITRD het bestand-kanaal; beide GEMETEN werkend op 05-08 (de kern
#    leest ze in Board::boot_param). De config staat in het venster
#    van hopos.img (stap 2), dus hopos.cfg in de initrd blijft leeg: die is
#    de terugval van een kern met een leeg venster. Een sleutel in het
#    venster wint van dezelfde sleutel in de APPEND. De APPEND draagt
#    alleen NODE: de kaart uit de release heeft geen, want een vaste naam
#    gaf elke Radxa dezelfde (07-11). Zonder hopos.node heet de node naar
#    zijn board en zijn MAC, en die MAC is dan de vaste van net::nodemac
#    (HOPOS_MAC_FIXED): het board heeft er geen eigen.
: >"$OUT/hopos.cfg"
python3 "$DIR/image/radxa-initrd.py" pack "$OUT/hopos.ird" "$OUT/hopos.cfg" \
	${IMAGE:+"$OUT/hop.elf"}
IRD_SIZE=$(wc -c <"$OUT/hopos.ird" | tr -d ' ')
if [ "$IRD_SIZE" -gt "$INITRD_MAX" ]; then
	echo "FOUT: hopos.ird is $IRD_SIZE bytes, de kern haalt hoogstens $INITRD_MAX naar zijn heap" >&2
	exit 1
fi
cat > "$OUT/extlinux.conf" <<EOF
# HopOS v3, Radxa Zero 3E. U-Boot's distro-boot pakt de eerste entry.
# hopos.ird is het image van de bewoner (image/radxa-initrd.py); de config
# staat in het venster van hopos.img (board/src/cfgwin.rs).
timeout 1
default hopos

label hopos
    kernel /hopos.img
    initrd /hopos.ird
${NODE:+    append hopos.node=$NODE}
EOF

# 5. De donor: de bytes LBA 64..32767 van het officiële Radxa Zero 3-image,
#    release b6, gepind met hash (image/firmware/radxa/LEESMIJ.txt: de
#    herkomst en hoe hij opnieuw te maken is). Dit zijn exact de bytes
#    waarmee alle metingen sinds 05-08 gedaan zijn.
DONOR="${RADXA_DONOR:-$DIR/image/firmware/radxa/donor-boot.bin}"
DONOR_SHA="9a582a0c6fcd8b41d5627284aa9b831a46a08e7606c6fc31901f48725354a7b9"
if [ ! -f "$DONOR" ]; then
	echo "FOUT: de donor ontbreekt: $DONOR (zie image/firmware/radxa/LEESMIJ.txt)" >&2
	exit 1
fi
GOT=$(shasum -a 256 "$DONOR" | cut -d' ' -f1)
if [ "$GOT" != "$DONOR_SHA" ]; then
	echo "FOUT: $DONOR heeft hash $GOT, verwacht $DONOR_SHA" >&2
	echo "  (zie image/firmware/radxa/LEESMIJ.txt, of zet RADXA_DONOR)" >&2
	exit 1
fi

# 6. De kaart (tools/mkcard): onze MBR (één FAT16-partitie, type 0x0C,
#    actief, vanaf LBA 32768 = 16 MiB), de donor raw op LBA 64 (byte 32768),
#    en de drie bestanden in de FAT. De vorm van de Go-kaart (tag v2.2.8);
#    het venster van Go's -cfgwindow zit nu in de kern (hopos.img).
CARD="$OUT/hopos-radxa-zero3.img"
(cd "$DIR" && cargo run -q -p mkcard -- -o "$CARD" -size 64 -start 32768 \
	-label hopos -vollabel -verify -raw "$DONOR@32768" \
	"$OUT/hopos.img" "$OUT/hopos.ird" "$OUT/extlinux.conf=extlinux/extlinux.conf") >&2

# 7. De proef op de kaart: mkcard -verify las hierboven de FAT terug zoals
#    U-Boot hem leest (MBR, BPB, root, de clusterketen, LFN) en vond elk
#    bestand en de donor zoals ze erin gingen. Hier: hopos.ird gesplitst
#    zoals de kern hem splitst, de APPEND, en precies één venster
#    op de hele kaart. Een fout in de container is hier rood, niet pas op de
#    seriële console.
RB="$OUT/readback"
rm -rf "$RB"
mkdir -p "$RB"
python3 "$DIR/image/radxa-initrd.py" split "$OUT/hopos.ird" "$RB/hopos.cfg" "$RB/hop.elf"
same() {
	a=$(shasum -a 256 "$1" | cut -d' ' -f1)
	b=$(shasum -a 256 "$2" | cut -d' ' -f1)
	if [ "$a" != "$b" ]; then
		echo "FOUT: $(basename "$2") uit de kaart heeft sha256 $b, erin ging $a" >&2
		exit 1
	fi
	echo "   ok  $(basename "$2") terug uit de kaart, sha256 $a" >&2
}
echo "== proef: de initrd gesplitst, de APPEND en het venster" >&2
same "$OUT/hopos.cfg" "$RB/hopos.cfg"
if [ -n "$IMAGE" ]; then
	same "$OUT/hop.elf" "$RB/hop.elf"
elif [ -s "$RB/hop.elf" ]; then
	echo "FOUT: er zit een image in hopos.ird terwijl APP leeg was" >&2
	exit 1
fi
# De APPEND: hopos.node=$NODE, en zonder NODE geen.
APPEND="$(grep "^ *append" "$OUT/extlinux.conf" || true)"
if [ "$APPEND" != "${NODE:+    append hopos.node=$NODE}" ]; then
	echo "FOUT: de APPEND van de kaart is '$APPEND', met NODE='${NODE:-}'" >&2
	exit 1
fi
# De config: precies één venster op de hele kaart, met wat erin ging.
python3 "$DIR/image/hopcfg.py" show "$OUT/hopos.img" >"$RB/want.cfg" 2>/dev/null
python3 "$DIR/image/hopcfg.py" show "$CARD" >"$RB/window.cfg"
same "$RB/want.cfg" "$RB/window.cfg"

echo "" >&2
echo "$CARD klaar (dd-baar): hopos.img (config $CFG in het venster) + hopos.ird${IMAGE:+ (hop.elf)} + extlinux/extlinux.conf" >&2
echo "flash:   diskutil unmountDisk /dev/diskN && sudo dd if=$CARD of=/dev/rdiskN bs=4m" >&2
echo "console: 1500000 8N1 op de 40-pins header (pin 8 TX, 10 RX, 6 GND)" >&2
