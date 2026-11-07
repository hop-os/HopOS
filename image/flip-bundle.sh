#!/bin/sh
# Bouwt de KERN-FLIP-BUNDEL van één board: het artifact waarmee een
# draaiende HopOS zijn kern vervangt zonder herstart, terwijl de apps (en
# Hop) doordraaien (image/flip-bundle.sh op tag v2.2.8, docs/flip.md).
#
#   image/flip-bundle.sh <board>    -> target/hopos-<board>.flip (+ .sha256)
#       board: virt, uefi, o6n, altra, rpi4, rpi5, radxa, apple
#              (riscv64 flipt niet: hopos/src/flip.rs `FLIPS`)
#   HOPOS_STAMP=B image/flip-bundle.sh virt   een ander versie-stempel op
#                                   de boot-regel (tools/qemu-test-flip.sh)
#   GUI=1 image/flip-bundle.sh rpi5 de gui-smaak (de korte vorm van
#                                   FEATURES=gui, zoals image/uefi-run.sh)
#   CFG="a.cfg b.cfg" image/flip-bundle.sh <board>  hopos.cfg in het
#                                   venster van de bundel
#                                   (board/src/cfgwin.rs), op elk board, de
#                                   bestanden aaneengeplakt, de laatste
#                                   waarde wint; zonder CFG neemt de flip
#                                   de config van de draaiende kern mee
#   CFG=headless|headfull image/flip-bundle.sh <board>  de config van die
#                                   smaak uit image/cfg
#
# Een bundel is de kern-ELF (zonder debug-info, mét symbolen) plus een
# HOPRELO1-staart (versie 2, kern::kernflip::Bundle) met de relocatietabel
# en de som van de EL2-switch-code van deze kern.
#
# De relocatietabel, per soort board:
#
# - Een vast linkadres (virt, de Pi's, de Radxa): de tabel komt uit een
#   DIFF. Dezelfde build op twee linkadressen, waarbij elk verschillend
#   8-byte-woord exact de basis-delta moet dragen; elk ander verschil laat
#   de bouw hard falen. Daarom linkt dit script de kern twee keer
#   (HOPOS_LINK_SHIFT, hopos/build.rs). De ELF in de bundel is die op het
#   SCHADUWadres: de draaiende kern legt hem plat neer en relokeert hem naar
#   zijn eigen koude adres. Dat maakt de relocatie op elke flip echt werk
#   (geen delta nul die een kapotte tabel verbergt), en dit script bewijst
#   vooraf dat het resultaat byte voor byte de koude build is.
# - UEFI (uefi, o6n, altra): de kern is een PIE op basis 0 die zichzelf
#   reloceert (`_start_efi`, board/uefi/src/entry.rs: elke
#   R_AARCH64_RELATIVE wordt basis + addend). De tabel is dan leeg: de
#   stub van de NIEUWE kern doet het werk op de basis die de firmware ooit
#   koos. Dit script toetst dat elke relocatie RELATIVE is en in de
#   RW-sectie valt, zoals image/uefi-run.sh.
#
# De switch-code-som: de FNV-1a-64 over de drie nVHE-blobs
# (cpu::el2::image_hash, `hopos_el2_nvhe_{entry,tramp,smp}`), gelezen uit
# de symbolen van de KOUDE link. De draaiende kern weigert de bundel vóór
# de sprong als die som niet die van de geïnstalleerde kopie is
# (`HOPOS_FLIP_REFUSED switch code mismatch`): de bewoners draaien in die
# kopie, en de nieuwe kern mag hem alleen adopteren bij een gelijke som.
#
# De node krijgt de bundel op aanvraag, via de agent-API van Hop, achter
# dezelfde HMAC als een jobspec: POST /flip {"url","sha256"} met de som die
# dit script print. Die som is het vertrouwensanker.
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
BOARD="${1:-}"
TARGET=aarch64-unknown-none-softfloat

# Per board: de feature, het koude linkadres (de laagste PT_LOAD van de
# koude link; op UEFI 0) en of het een PIE is. De schaduwlink ligt SHIFT
# hoger: dat doet niets in het eindproduct, het is diff-bewijs, maar moet
# dezelfde uitlijning houden (een veelvoud van 2 MB: de ADRP-paginaoffsets
# en de 2 MB-blokken blijven gelijk) en ver genoeg weg liggen voor een echte
# delta.
SHIFT=0x20000000
# FLAVOR is de EL2-smaak van de switcher van dat board (hopos/src/cage.rs
# `FLAVOR`): de som in de bundel moet over de blobs van die smaak gaan, en
# de O6N draait VHE.
case "$BOARD" in
virt) FEATURE=board-qemuvirt COLD=0x40200000 PIE=0 FLAVOR=nvhe ;;
rpi4) FEATURE=board-rpi4 COLD=0x80000 PIE=0 FLAVOR=nvhe ;;
rpi5) FEATURE=board-rpi5 COLD=0x80000 PIE=0 FLAVOR=nvhe ;;
radxa) FEATURE=board-rk3566 COLD=0x2210000 PIE=0 FLAVOR=nvhe ;;
uefi) FEATURE=board-uefi COLD=0 PIE=1 FLAVOR=nvhe ;;
o6n) FEATURE=board-o6n COLD=0 PIE=1 FLAVOR=vhe ;;
altra) FEATURE=board-altra COLD=0 PIE=1 FLAVOR=nvhe ;;
# De M4: vast linkadres (hopos/link-apple.ld KERN_BASE), de apple-switcher;
# board/apple/build.rs wil HOPOS_EMBED (de Hop-ELF, of leeg).
apple) FEATURE=board-apple COLD=0x10100000000 PIE=0 FLAVOR=apple ;;
*)
	echo "gebruik: $0 virt|uefi|o6n|altra|rpi4|rpi5|radxa|apple" >&2
	exit 64
	;;
esac
# FEATURES (zoals bij image/uefi-run.sh) komt achter de board-feature. Met
# `vhe` erin draait de kern onder E2H = 1 met de VHE-switcher
# (hopos/src/cage.rs `FLAVOR`), en gaat de som over de vhe-blobs: de proef
# `FEATURES=vhe CPU=neoverse-n1 BOARD=uefi sh tools/qemu-test-flip.sh`.
if [ -n "${FEATURES:-}" ]; then
	FEATURE="$FEATURE,$FEATURES"
	case ",$FEATURES," in
	*,vhe,*) FLAVOR=vhe ;;
	esac
fi
# De gui-smaak heeft dezelfde switch-code als kaal (de EL2-blobs kennen
# geen feature), dus een kale kern neemt een gui-bundel warm aan: zo kreeg
# de Pi 5 op 30-09 zijn console op het glas, van generatie 3 (kaal) naar 4.
if [ "${GUI:-0}" = 1 ]; then
	FEATURE="$FEATURE,gui"
fi
need_objcopy flip-bundle
OUT="$DIR/target/hopos-$BOARD.flip"
# Een eigen target-map: de gewone build (image/qemu-run.sh, uefi-run.sh)
# blijft staan, en de PIE-build heeft andere RUSTFLAGS.
TD="$DIR/target/flip-$BOARD"
STAMP="${HOPOS_STAMP:-dev}"
build() {
	if [ "$PIE" = 1 ]; then
		(cd "$DIR" && CARGO_TARGET_DIR="$TD" RUSTFLAGS="-C relocation-model=pie" \
			HOPOS_LINK_SHIFT="$1" HOPOS_STAMP="$STAMP" \
			cargo build --quiet --release --target "$TARGET" -p hopos --features "$FEATURE")
	else
		(cd "$DIR" && CARGO_TARGET_DIR="$TD" HOPOS_LINK_SHIFT="$1" HOPOS_STAMP="$STAMP" \
			cargo build --quiet --release --target "$TARGET" -p hopos --features "$FEATURE")
	fi
	cp "$TD/$TARGET/release/hopos" "$2"
}
if [ "$PIE" = 1 ]; then
	build "" "$TD/flip-cold.elf"
	SHADOW_ELF="$TD/flip-cold.elf"
else
	build "$SHIFT" "$TD/flip-shadow.elf"
	build "" "$TD/flip-cold.elf"
	SHADOW_ELF="$TD/flip-shadow.elf"
fi

if [ -n "$OBJCOPY" ]; then
	"$OBJCOPY" --strip-debug "$SHADOW_ELF" "$TD/flip-bundle.stripped"
else
	cp "$SHADOW_ELF" "$TD/flip-bundle.stripped"
fi

# CFG=<pad>: hopos.cfg in het venster van de bundel (board/src/cfgwin.rs,
# image/hopcfg.py), op elk board, zoals de image-scripts het in het image
# zetten. Zonder CFG blijft het venster leeg en geeft de draaiende kern het
# zijne mee (hopos/src/flip.rs, HOPOS_FLIP_CFG); een bundel met een gevuld
# venster houdt het zijne (HOPOS_FLIP_CFG_OWN): zo brengt een flip bewust
# een andere config. Het venster valt buiten elke relocatie: in beide links
# stonden dezelfde bytes.
CFG="${CFG-}"
case "$CFG" in
headless | headfull) CFG="$DIR/image/cfg/$CFG.cfg" ;;
esac
if [ -n "$CFG" ]; then
	for f in $CFG; do
		[ -f "$f" ] || {
			echo "flip-bundle: CFG: $f does not exist" >&2
			exit 64
		}
	done
	# shellcheck disable=SC2086 # CFG is een lijst bestanden
	python3 "$DIR/image/hopcfg.py" set "$TD/flip-bundle.stripped" $CFG
else
	# Precies één leeg venster: de plek waar de flip de config neerlegt.
	WIN="$(python3 "$DIR/image/hopcfg.py" show "$TD/flip-bundle.stripped" 2>/dev/null)"
	[ -z "$WIN" ] || {
		echo "flip-bundle: the config window of the kernel is not empty" >&2
		exit 1
	}
fi
PYTHONPATH="$DIR/image" python3 - "$SHADOW_ELF" "$TD/flip-cold.elf" "$TD/flip-bundle.stripped" "$OUT" "$SHIFT" "$COLD" "$PIE" "$FLAVOR" <<'PY'
import hashlib, struct, sys
import elf as elfs  # image/elf.py

shadow_path, cold_path, stripped_path, out_path = sys.argv[1:5]
shift, cold_base, pie = int(sys.argv[5], 16), int(sys.argv[6], 16), sys.argv[7] == "1"
flavor = sys.argv[8]  # nvhe | vhe | apple: de symboolnaam van de blobs
MAGIC = 0x314F4C4552504F48  # "HOPRELO1"
VERSION = 2                 # kern::kernflip::BUNDLE_VERSION
FLIP_ABI = 5                # kern::kernflip::FLIP_ABI
M64 = (1 << 64) - 1

def die(msg):
    sys.exit("flip-bundle: " + msg)

def loads(elf):
    try:
        return elfs.loads(elf)
    except ValueError as e:
        die(str(e))

def flat(elf):
    entry, segs = loads(elf)
    base = min(s[0] for s in segs)
    end = max(s[0] + s[3] for s in segs)
    size = (end - base + 7) & ~7
    img = bytearray(size)
    for paddr, off, filesz, _memsz in segs:
        img[paddr - base:paddr - base + filesz] = elf[off:off + filesz]
    return base, entry, img

def switch_sum(elf):
    # cpu::el2::dispatch::place: FNV-1a-64 over entry, tramp en smp, in die
    # volgorde, van de smaak van dit board (hopos/src/cage.rs `FLAVOR`; de
    # symbolen hopos_el2_{nvhe,vhe,apple}_* uit cpu/src/el2/switch.rs).
    syms = elfs.symbols(elf)
    base, _entry, img = flat(elf)
    h = 0xcbf29ce484222325
    for blob in ("entry", "tramp", "smp"):
        a = syms.get(f"hopos_el2_{flavor}_{blob}")
        b = syms.get(f"hopos_el2_{flavor}_{blob}_end")
        if a is None or b is None or b <= a:
            die(f"no switch code symbol hopos_el2_{flavor}_{blob} in the kernel")
        for x in img[a - base:b - base]:
            h = ((h ^ x) * 0x100000001b3) & M64
    return h

def pie_check(elf):
    # De toets van image/elf.py, ook die van image/uefi-run.sh.
    n, _data_start, bad = elfs.relative_only(elf)
    if bad:
        k, _sec, typ, off = bad[0]
        die(f"relocation {k} is type {typ} at {off:#x}: not RELATIVE in the RW section")
    return n

s_elf = open(shadow_path, "rb").read()
c_elf = open(cold_path, "rb").read()
s_base, s_entry, s_img = flat(s_elf)
c_base, c_entry, c_img = flat(c_elf)
if c_base != cold_base:
    die(f"cold link base {c_base:#x}, expected {cold_base:#x} (the board's FLIP_LINK_BASE)")
relocs = []
if pie:
    rel = pie_check(c_elf)
    proof = f"PIE with {rel} RELATIVE relocation(s) the new kernel's stub applies itself"
else:
    if len(s_img) != len(c_img):
        die(f"the two links differ in size ({len(s_img)} vs {len(c_img)})")
    delta = (c_base - s_base) & M64
    if (s_base - c_base) & M64 != shift:
        die(f"shadow link at {s_base:#x}, expected {c_base + shift:#x}")
    if (c_entry - s_entry) & M64 != delta:
        die("the entries do not differ by the base delta")
    for off in range(0, len(s_img), 8):
        a = struct.unpack_from("<Q", s_img, off)[0]
        b = struct.unpack_from("<Q", c_img, off)[0]
        if a == b:
            continue
        if (b - a) & M64 != delta:
            die(f"word at +{off:#x} differs by {(b - a) & M64:#x}, not by the base delta "
                f"{delta:#x}: an absolute address that is no 8-byte word (ADRP to a fixed symbol?)")
        relocs.append(off)
    # Het bewijs: het gerelokeerde schaduwbeeld IS de koude build.
    check = bytearray(s_img)
    for off in relocs:
        v = struct.unpack_from("<Q", check, off)[0]
        struct.pack_into("<Q", check, off, (v + delta) & M64)
    if check != c_img:
        die("relocated image differs from the cold build")
    proof = f"{len(relocs)} relocations to {c_base:#x}, proven equal to the cold build"
# Uit de KOUDE link: die landt. De blobs zijn positie-onafhankelijke
# tekst (geen relocatie erin), dus op UEFI geldt de som voor elke basis.
sw = switch_sum(c_elf)

elf = open(stripped_path, "rb").read()
b = bytearray(elf)
while len(b) % 8:
    b.append(0)
head = len(b)
b += struct.pack("<QIIQQQQQQ", MAGIC, VERSION, FLIP_ABI, len(elf), s_base, len(s_img), s_entry, len(relocs), sw)
for off in relocs:
    b += struct.pack("<I", off)
while len(b) % 8:
    b.append(0)
b += struct.pack("<QQ", head, MAGIC)
open(out_path, "wb").write(b)
sha = hashlib.sha256(b).hexdigest()
open(out_path + ".sha256", "w").write(sha + "\n")
open(out_path + ".switch", "w").write(f"{sw:#018x}\n")
print(f"flip-bundle: {len(s_img) // 1024} KiB image linked at {s_base:#x}, {proof}, "
      f"switch code {sw:#018x}, bundle {len(b)} bytes", file=sys.stderr)
PY

SHA="$(cat "$OUT.sha256")"
echo "" >&2
echo "$OUT gebouwd (board $BOARD, stempel $STAMP), sha256 $SHA" >&2
echo "Zet hem op een webserver en vraag de flip aan op de agent-API van Hop:" >&2
echo "  curl -X POST http://<node>:8080/flip -d '{\"url\":\"http://<ip>:<poort>/$(basename "$OUT")\",\"sha256\":\"$SHA\"}'" >&2
echo "Weigert de node hem (switch code mismatch), dan is de weg een herinstallatie van het bootmedium." >&2
