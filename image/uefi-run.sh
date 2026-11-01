#!/bin/sh
# Bouw HopOS als UEFI-app (BOOTAA64.EFI) en boot hem op QEMU -M virt met
# echte EDK2-firmware: de proeftuin voor de Orion O6N en de Ampere Altra
# (zelfde firmware-familie, zelfde weg: FAT-medium, EFI/BOOT/BOOTAA64.EFI,
# PE-stub, ExitBootServices, kmain). Wat hier boot, hoort van een stick te
# booten. De Go-voorganger: image/uefi-run.sh op tag v2.2.8.
#
#   image/uefi-run.sh                 bouwen en booten op QEMU/EDK2
#   BUILD_ONLY=1 image/uefi-run.sh    alleen de ESP-map bouwen
#   BOARD=o6n|altra image/uefi-run.sh de fysieke borden (feature board-o6n of
#                                     board-altra van hopos): alleen bouwen,
#                                     want QEMU heeft er geen model van
#   APP=/pad/elf image/uefi-run.sh    een kant-en-klare Hop als
#                                     hopos-stage.elf op de ESP (de kern
#                                     plaatst alleen Hop, 01-11 O7)
#   CFG="a.cfg b.cfg" image/uefi-run.sh
#                                     hopos.cfg in lagen in het venster van
#                                     BOOTAA64.EFI, de laatste waarde wint
#                                     (standaard op o6n en altra
#                                     image/cfg/default.cfg, <bord>.cfg en
#                                     headless.cfg, met GUI=1 of MEDIA=1
#                                     headfull.cfg; op QEMU een leeg venster
#                                     en een minimale hopos.cfg op de ESP
#                                     zonder sleutels)
#   GUI=1 image/uefi-run.sh           de gui-smaak (`--features gui`, docs/gui.md):
#                                     de console op de GOP; QEMU krijgt
#                                     `-device ramfb` (EDK2 maakt er een GOP van)
#   MEDIA=1 BOARD=o6n image/uefi-run.sh
#                                     de media-smaak (`--features media`,
#                                     docs/media.md): de codec-dienst, op de
#                                     O6N met de VPU. Eigen target-map
#                                     (target/uefi-media) en eigen ESP
#                                     (…-media), zodat de kale er niet door
#                                     verdwijnt; CFG=jobs/hopos-media-o6n.cfg
#                                     is de config van de mediatest
#   image/uefi-run.sh -s -S           de rest gaat naar QEMU (gdb)
#
# Het resultaat is een ESP-map, target/uefi-esp[-$BOARD]/: EFI/BOOT/
# BOOTAA64.EFI (met de config in zijn venster), eventueel hopos-stage.elf,
# en zonder CFG een hopos.cfg. Op een stick: een
# FAT32-partitie (GPT of MBR) en die boom erop; Secure Boot uit (een
# ongesigneerde BOOTAA64 weigert DxeImageVerificationLib anders, Go 13-07).
#
# Het image: de kern als PIE op basis 0 (hopos/efi.ld, RUSTFLAGS
# relocation-model=pie), met de PE-header in de eerste pagina
# (board/uefi/src/entry.rs), plat gemaakt met rust-objcopy. Vóór het
# verpakken toetst dit script dat elke relocatie een R_AARCH64_RELATIVE is
# en in de RW-sectie valt: EDK2 mapt de code read-only, en een relocatie
# daar zou de stub bij zijn eerste store laten vallen (Go-les 13-07). Faalt
# die toets, dan is een toolchain-aanname gebroken: luid, nooit een stille
# kapotte stick.
#
# QEMU: pflash 0 = edk2-aarch64-code.fd uit $QEMU_SHARE (readonly), pflash 1
# = een verse varstore van 64 MB nullen (een oude verwijst naar een andere
# topologie en EDK2 valt dan in de Shell, Go 13-07). De ESP hangt als
# usb-storage achter qemu-xhci (de semantiek van de stick; en zo is de
# enige virtio-blk-pci de data-schijf van hopfs), met bootindex=0 zodat de
# BDS niet eerst PXE probeert op de virtio-net.
set -e

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
TARGET=aarch64-unknown-none-softfloat
QEMU_SHARE="${QEMU_SHARE:-/opt/homebrew/share/qemu}"
BOARD="${BOARD:-uefi}"
SMP="${SMP:-4}"
MEM="${MEM:-3G}"
CPU="${CPU:-cortex-a57}"
SYSPORT="${SYSPORT:-10100}"
DISK="${DISK:-$DIR/target/hopos-uefi-disk.img}"
DISK_MIB="${DISK_MIB:-64}"

case "$BOARD" in
uefi) FEATURE=board-uefi ;;
o6n) FEATURE=board-o6n BUILD_ONLY=1 ;;
altra) FEATURE=board-altra BUILD_ONLY=1 ;;
*)
	echo "uefi-run: BOARD=$BOARD onbekend (uefi|o6n|altra)" >&2
	exit 64
	;;
esac
if ! grep -q "^$FEATURE *=" "$DIR/hopos/Cargo.toml"; then
	echo "uefi-run: hopos kent de feature $FEATURE nog niet (het board-crate ontbreekt)" >&2
	exit 1
fi

# FEATURES=vhe (of een andere lijst) komt achter de board-feature: de proef
# van de VHE-switcher op QEMU met CPU=neoverse-n1 (hopos/src/cage.rs).
if [ -n "${FEATURES:-}" ]; then
	FEATURE="$FEATURE,$FEATURES"
fi

QGUI=""
if [ "${GUI:-0}" = 1 ]; then
	FEATURE="$FEATURE,gui"
	QGUI="-device ramfb"
fi
# De media-smaak: een eigen target-map (dezelfde als die van de gate, dus
# één cache) en een eigen ESP, zodat een kale build ernaast blijft staan.
FLAVOR=""
if [ "${MEDIA:-0}" = 1 ]; then
	FEATURE="$FEATURE,media"
	FLAVOR="-media"
fi
ESP="${ESP:-$DIR/target/uefi-esp$([ "$BOARD" = uefi ] || echo "-$BOARD")$FLAVOR}"
TDIR="$DIR/target/uefi$FLAVOR"
need_objcopy uefi-run

cd "$DIR"
# Een eigen target-map: andere RUSTFLAGS dan de virt-build, en zo gooien de
# twee elkaars cache niet weg.
RUSTFLAGS="-C relocation-model=pie" cargo build --quiet --release --target "$TARGET" \
	-p hopos --features "$FEATURE" --target-dir "$TDIR"
ELF="$TDIR/$TARGET/release/hopos"

mkdir -p "$ESP/EFI/BOOT"
EFI="$ESP/EFI/BOOT/BOOTAA64.EFI"
"$OBJCOPY" -O binary "$ELF" "$EFI.raw"
# De PIE-toets (alleen RELATIVE, alleen in RW: de stub past ze zelf toe)
# en de PE-verpakking.
python3 "$DIR/image/elf.py" pe "$ELF" "$EFI.raw" "$EFI"
rm -f "$EFI.raw"

# Het gestagede image (de UEFI-tegenhanger van QEMU's -device loader).
pick_app ""
rm -f "$ESP/hopos-stage.elf"
if [ -n "$IMAGE" ]; then
	strip_elf "$IMAGE" "$ESP/hopos-stage.elf"
fi

# De config: CFG, op de fysieke borden standaard de gedeelde (image/cfg),
# in het venster van BOOTAA64.EFI (board/src/cfgwin.rs, image/hopcfg.py).
# Het venster wint van een hopos.cfg op de ESP, dus die gaat weg. Zonder CFG
# (QEMU, BOARD=uefi) blijft het venster leeg en staat op de ESP een
# hopos.cfg zonder sleutels: de terugval van de stub (de QEMU-toetsen rekenen
# op een dichte API).
#
# Op QEMU staat er de SNTP-server van de host vóór (tools/lib.sh sntp,
# hopos.ntp=10.0.2.2:$NTPPORT): die van de ring als hij $NTPPORT meegeeft
# (ook bij BUILD_ONLY), anders zonder BUILD_ONLY een eigen die met QEMU
# stopt. Een eigen hopos.ntp in CFG wint (de laatste waarde); een build
# voor ijzer, zonder ring, krijgt geen.
[ -n "${BUILD_ONLY:-}" ] || [ -n "${NTPPORT:-}" ] || sntp
NTPCFG=""
[ -z "${NTPPORT:-}" ] || NTPCFG="hopos.ntp=10.0.2.2:$NTPPORT"
if [ -z "${CFG:-}" ] && [ "$BOARD" != uefi ]; then
	FLAVOR=headless
	if [ "${GUI:-0}" = 1 ] || [ "${MEDIA:-0}" = 1 ]; then FLAVOR=headfull; fi
	CFG="$DIR/image/cfg/default.cfg $DIR/image/cfg/$BOARD.cfg $DIR/image/cfg/$FLAVOR.cfg"
fi
if [ -n "${CFG:-}" ]; then
	for f in $CFG; do
		[ -f "$f" ] || { echo "uefi-run: CFG: $f does not exist" >&2; exit 1; }
	done
	{
		[ -z "$NTPCFG" ] || echo "$NTPCFG"
		# shellcheck disable=SC2086 # CFG is een lijst bestanden
		cat $CFG
	} >"$TDIR/hopos.cfg"
	python3 "$DIR/image/hopcfg.py" set "$EFI" "$TDIR/hopos.cfg"
	rm -f "$ESP/hopos.cfg"
else
	printf '# HopOS node config (hopos.cfg op de ESP-root)\n' >"$ESP/hopos.cfg"
	[ -z "$NTPCFG" ] || echo "$NTPCFG" >>"$ESP/hopos.cfg"
fi
echo "uefi-run: ESP in $ESP" >&2
[ -n "${BUILD_ONLY:-}" ] && exit 0

for f in "$QEMU_SHARE/edk2-aarch64-code.fd"; do
	[ -e "$f" ] || {
		echo "uefi-run: $f ontbreekt (QEMU_SHARE)" >&2
		exit 1
	}
done
VARS="${VARS:-$DIR/target/uefi-vars.fd}"
dd if=/dev/zero of="$VARS" bs=1048576 count=64 2>/dev/null
if [ ! -e "$DISK" ]; then
	dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek="$DISK_MIB" 2>/dev/null
fi

# shellcheck disable=SC2086
qemu_edk2 "$CPU" "$VARS" "$ESP" $QGUI \
	-device virtio-net-pci,netdev=n0,romfile= \
	-netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${SYSPORT}-:10100" \
	-drive "if=none,format=raw,file=$DISK,id=disk0" \
	-device virtio-blk-pci,drive=disk0 \
	"$@"
