#!/bin/sh
# Bouw de SD-kaart van HopOS v3 voor een Raspberry Pi (board/rpi4 of
# board/rpi5, samen board/raspi): de kern, config.txt, cmdline.txt en het
# image van Hop, en als de firmware er ligt het complete, dd-bare
# kaart-image. image/rpi4.sh en image/rpi5.sh roepen dit aan.
#
#   image/rpi4.sh                    Hop als bewoner (standaard); zo ook rpi5
#   APP=/pad/naar/elf image/rpi4.sh  een kant-en-klare Hop (de kern plaatst
#                                    alleen Hop, 01-11 O7)
#   APP= image/rpi4.sh               zonder image
#   GUI=1 image/rpi4.sh              de gui-smaak (`--features gui`, docs/gui.md):
#                                    de console op het glas via de VideoCore-
#                                    mailbox, de invoer (de VL805 over PCIe op
#                                    de Pi 4, de RP1-xHCI's op de Pi 5), en de
#                                    framebuffer-grant aan een display-app
#   FW=pad image/rpi4.sh             waar de firmware ligt (standaard
#                                    image/firmware/rpi4 of rpi5; herkomst,
#                                    en op de Pi 4 het bl31-bouwrecept, in de
#                                    LEESMIJ.txt daar): Pi 4 start4.elf,
#                                    fixup4.dat, bcm2711-rpi-4-b.dtb en
#                                    bl31.bin, Pi 5 bcm2712-rpi-5-b.dtb en
#                                    overlays/bcm2712d0.dtbo
#   CFG="a.cfg b.cfg" image/rpi4.sh  de config van de node, de bestanden
#                                    aaneengeplakt in die volgorde
#                                    (standaard image/cfg/headless.cfg, met
#                                    GUI=1 headfull.cfg), in het venster van
#                                    de kern (board/src/cfgwin.rs,
#                                    image/hopcfg.py); de laatste waarde wint
#   EXTRA="hopos.node=..." image/... meer tokens in cmdline.txt; een
#                                    sleutel die ook in het venster staat,
#                                    neemt het venster
#
# Het boot-recept is dat van de Go-generatie (image/rpi4-agent.sh en
# image/rpi5-agent.sh op tag v2.2.8). De Pi 4: de firmware laadt
# kernel8.img RAUW op 0x80000 (geen arm64-Image-header, het bewezen pad),
# de DTB op 0x0f000000, en TF-A bl31.bin als armstub: de stock armstub8
# heeft GEEN PSCI, en de eerste CPU_ON zou in een lege EL3-vector hangen.
# De Pi 5 heeft geen start*.elf (de firmware zit in de EEPROM), laadt
# hop-agent5.img RAUW op 0x80000 (hij negeert kernel_address, gemeten
# 09-07) en eist `os_check=0`; zonder passende DTB weigert hij te booten.
# De armstub van de EEPROM (TF-A BL31) levert PSCI. Nieuw in v3: het image
# van Hop gaat als `initramfs` op 0x0f200000 de kaart op
# (board/raspi/src/map.rs: het laadvenster).
#
# Uitvoer: target/sd-<pi>/ (de losse bestanden, voor een bestaande
# FAT-kaart) en target/hopos-<pi>.img (MBR + FAT16, dd-baar).
set -e

B="$1"
DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
TARGET=aarch64-unknown-none-softfloat
FW="${FW:-$DIR/image/firmware/$B}"
OUT="$DIR/target/sd-$B"
CARD="$DIR/target/hopos-$B.img"
# Per Pi: de naam van de kern, de firmware op de kaart, en de regels van
# config.txt vóór en na uart_2ndstage.
case "$B" in
rpi4)
	KIMG=kernel8.img
	FILES="start4.elf fixup4.dat bcm2711-rpi-4-b.dtb bl31.bin"
	PRE="device_tree_address=0x0f000000
# TF-A BL31 als armstub: levert PSCI. De stock armstub8 heeft dat NIET.
armstub=bl31.bin
enable_uart=1"
	POST="# Houd de PL011 bij GPIO14/15 (anders claimt Bluetooth hem).
dtoverlay=disable-bt"
	;;
rpi5)
	KIMG=hop-agent5.img
	FILES="bcm2712-rpi-5-b.dtb overlays/bcm2712d0.dtbo"
	PRE="os_check=0
device_tree_address=0x0f000000"
	POST="# De stil-vloer van het klokbeleid (board/raspi/src/clock.rs vraagt het
# minimum op en volgt): zonder deze regel klemt de Pi 5-firmware de vloer op
# 1500 MHz (Go, gemeten 11-07), gelijk aan arm_freq hieronder, en dan is er
# niets te draaien (HOPOS_CLOCK_NONE, \"one ARM clock\").
arm_freq_min=800
# Thermische cap voor fanloos bedrijf (arm_freq is het max; arm_freq_max
# bestaat niet, gemeten 11-07). 2400 MHz liep zonder fan binnen minuten naar
# 84 C; het klokbeleid volgt dit maximum vanzelf.
arm_freq=1500"
	;;
*)
	echo "raspi: rpi4 of rpi5, niet '$B' (image/rpi4.sh, image/rpi5.sh)" >&2
	exit 64
	;;
esac

cd "$DIR"
FEATURE="board-$B"
if [ "${GUI:-0}" = 1 ]; then
	FEATURE="$FEATURE,gui"
fi
cargo build --quiet --release --target "$TARGET" -p hopos --features "$FEATURE"
need_objcopy "$B"
rm -rf "$OUT"
mkdir -p "$OUT"
"$OBJCOPY" -O binary "$DIR/target/$TARGET/release/hopos" "$OUT/$KIMG"

# Het image voor de staging (tools/lib.sh pick_app, zoals elk image-script):
# in het laadvenster (board_raspi::map::STAGE_MAX).
pick_app hop
INITRAMFS=""
if [ -n "$IMAGE" ]; then
	strip_elf "$IMAGE" "$OUT/hop.elf"
	fits "$OUT/hop.elf" "$IMAGE"
	INITRAMFS="initramfs hop.elf 0x0f200000"
fi

cat >"$OUT/config.txt" <<EOF
# HopOS v3 - Raspberry Pi ${B#rpi} (image/$B.sh, docs/boards-pi.md)
arm_64bit=1
kernel=$KIMG
$PRE
uart_2ndstage=1
$POST
# Het image van Hop in het laadvenster (board/raspi/src/map.rs).
$INITRAMFS
EOF
# De config: in het venster van de kern (board/src/cfgwin.rs), zoals op
# elk board; de kern leest eerst het venster en dan /chosen/bootargs
# (cmdline.txt: alleen EXTRA).
CFG="${CFG:-$DIR/image/cfg/$([ "${GUI:-0}" = 1 ] && echo headfull || echo headless).cfg}"
for f in $CFG; do
	[ -f "$f" ] || {
		echo "$B: CFG: $f does not exist" >&2
		exit 1
	}
done
# shellcheck disable=SC2086 # CFG is een lijst bestanden
python3 "$DIR/image/hopcfg.py" set "$OUT/$KIMG" $CFG
echo "${EXTRA:-}" >"$OUT/cmdline.txt"

echo "$B: $OUT/$KIMG ($(wc -c <"$OUT/$KIMG" | tr -d ' ') bytes), config.txt, cmdline.txt${IMAGE:+, hop.elf ($SIZE bytes)}" >&2

# Het kaart-image: de firmware erbij, en een FAT die de Pi-firmware leest.
# tools/mkcard is de port van het bewezen gereedschap van de Go-generatie
# (MBR + FAT16 met LFN, reproduceerbaar, geen mount, en -verify leest hem
# terug); ontbreekt de firmware, dan LUID overslaan en is target/sd-<pi>/
# plus de firmware op een bestaande kaart de weg.
rm -f "$CARD"
MISSING=""
set -- "$OUT/$KIMG" "$OUT/config.txt" "$OUT/cmdline.txt"
for f in $FILES; do
	mkdir -p "$(dirname "$OUT/$f")"
	if [ -f "$FW/$f" ]; then cp "$FW/$f" "$OUT/$f"; else MISSING="$MISSING $f"; fi
	set -- "$@" "$OUT/$f=$f"
done
if [ -n "$MISSING" ]; then
	echo "$B: NO card image, missing in $FW:$MISSING (see image/firmware/$B/LEESMIJ.txt)" >&2
else
	[ -f "$OUT/hop.elf" ] && set -- "$@" "$OUT/hop.elf"
	cargo run -q -p mkcard -- -o "$CARD" -size 64 -start 8192 -label bootfs -vollabel -verify "$@" >&2
	echo "$B: $CARD (dd: diskutil unmountDisk /dev/diskN && sudo dd if=$CARD of=/dev/rdiskN bs=4m)" >&2
fi
