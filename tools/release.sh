#!/bin/sh
# Bouwt een release van HopOS v3 op de host en zet hem klaar in
# target/release-<versie>/, met een SHA256SUMS. Publiceert niets: aan het eind
# staan de regels om te taggen en te publiceren, voor wie dat mag (xinix00).
#
#   sh tools/release.sh 3.0.0             bouwen, klaarzetten, de notes en de regels
#   sh tools/release.sh --dry-run 3.0.0   alleen bouwen en klaarzetten
#   JOBS=8 sh tools/release.sh ...        zoveel boards tegelijk (standaard 4)
#   HOP_DIR=pad sh tools/release.sh ...   de hop-repo (standaard ../hop/hop)
#   GO_APPS=0 sh tools/release.sh ...     de Go-apps overslaan
#
# Per board twee smaken, met de smaak in de naam:
#
#   headless   de kern zonder gui
#   headfull   de kern met gui (en media op de O6N)
#
# De config in het venster is per smaak één bestand uit image/cfg,
# headless.cfg of headfull.cfg (07-11); waar Hop woont is de standaard van
# het board (board::Board).
#
# Wat erin komt, per board de kaart of stick (gzip, dd-baar) en de
# flipbundel:
#
#   hopos-rpi4-<smaak>.img.gz      image/rpi4.sh, Hop in de initramfs
#   hopos-rpi5-<smaak>.img.gz      image/rpi5.sh
#   hopos-radxa-<smaak>.img.gz     image/radxa-zero3.sh
#   hopos-o6n-<smaak>.img.gz       image/uefi-run.sh plus tools/mkcard: de
#   hopos-altra-<smaak>.img.gz     ESP als stick (EFI/BOOT/BOOTAA64.EFI,
#                                  Hop als hopos-stage.elf)
#
# Op elk board staat de config in het venster van de kern
# (board/src/cfgwin.rs): `hop image` (de hop-repo) zet er een eigen config
# in, in het image, de flipbundel of op de kaart.
#   hopos-apple-headless.img.gz    geen bootmedium maar een FAT-stick (zoals
#                                  Go, tag v2.2.8): image/apple-m4.sh met de
#                                  config en Hop ingebakken als
#                                  hopos-apple.img, naast install.sh en de
#                                  README; installeren uit Recovery met
#                                  `sh /Volumes/HOPOS/install.sh go`
#   hopos-licheerv-headless.img.gz image/licheerv-agent.sh, met de config
#                                  in het venster en Hop (riscv64) in de
#                                  kern gebakken (STAGE=); en de
#   hopos-licheerv-fip.bin         losse fip.bin voor een kaart die er al een
#                                  heeft (alleen dat bestand vervangen)
#   hopos-<board>-<smaak>.flip     image/flip-bundle.sh met HOPOS_STAMP=<versie>
#                                  (niet voor de LicheeRV: riscv64)
#
# De Apple en de LicheeRV hebben geen gui-feature, dus alleen headless. En de
# apps van deze boom (APPS van tools/lib.sh plus decode), als
# <app>-<arch>.elf (jobs/README.md): arm64 allemaal, riscv64 appspike en
# welcome. Die gaan ook naar de rollende release `apps`, waar de gedeelde
# configs welcome vandaan halen.
#
# Optioneel de Go-apps (docs/go-apps.md): staat ~/tamago-go/bin/go er, dan
# bouwt go/apps-release.sh ze met PUBLISH=0, en gaan ze als
# <app>-<arch>-tamago.elf mee naar `apps`.
#
# Wat de gebruiker meegeeft: de versie, en HOP_DIR als de hop-repo niet naast
# deze staat. Een eigen config per machine hoort niet in een publieke
# release; die bouw je met CFG= en het image-script zelf.
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
TARGET=aarch64-unknown-none-softfloat
RV=riscv64gc-unknown-none-elf
DRY=0
if [ "${1:-}" = --dry-run ]; then
	DRY=1
	shift
fi
VERSION="${1:-}"
case "$VERSION" in
[0-9]*.[0-9]*.[0-9]*) ;;
*)
	echo "gebruik: $0 [--dry-run] <versie>   (bijvoorbeeld 3.0.0 of 3.0.0-beta.1)" >&2
	exit 64
	;;
esac
# De banner van de kern ("runtime ... (HopOS vX)") komt uit de workspace-versie;
# die moet dezelfde zijn als de release (02-10: 3.0.1 zei v3.0.0).
WS="$(sed -n 's/^version = "\(.*\)"/\1/p' "$DIR/Cargo.toml" | head -1)"
[ "$WS" = "$VERSION" ] || { echo "release $VERSION: Cargo.toml [workspace.package] version is $WS; zet die eerst gelijk" >&2; exit 65; }
OUT="$DIR/target/release-$VERSION"
NOTES="$DIR/target/release-$VERSION.notes.md"
export HOP_DIR="${HOP_DIR:-$DIR/../hop/hop}"
# Het stempel op de bootregel van elke kern (hopos/build.rs), ook van de
# kaarten: zo zegt een node welke release hij draait.
export HOPOS_STAMP="$VERSION"
LRV_DONOR_SHA=d85e68836f57a9fcb1bbfba3c1ccf93d1b062ac168305d7f0cc83a72a796c6b9

cd "$DIR"
rm -rf "$OUT" "$NOTES"
mkdir -p "$OUT"
T0=$(date +%s)
step() { echo "== release $VERSION: $*" >&2; }
skip() { echo "== release $VERSION: OVERGESLAGEN: $*" >&2; }
# Een kaart gaat gzip'd mee (vooral nullen: 64 MB wordt een paar MB);
# -n houdt de tijd en de naam eruit, zodat de som reproduceerbaar is.
card() { gzip -9 -n -c "$1" >"$OUT/$2.gz"; }
# flip <board> <smaak> <GUI> <FEATURES>
flip() {
	GUI=$3 FEATURES=$4 sh image/flip-bundle.sh "$1" >/dev/null
	cp "target/hopos-$1.flip" "$OUT/hopos-$1-$2.flip"
}

# Hop één keer, arm64 en riscv64 naast elkaar (tools/hop-build.sh heeft per
# target een eigen map): elk board krijgt hetzelfde image als bewoner.
step "Hop (tools/hop-build.sh, $HOP_DIR): arm64 en riscv64 naast elkaar"
sh tools/hop-build.sh "$TARGET" >"$OUT/.hop-arm64" &
sh tools/hop-build.sh "$RV" >"$OUT/.hop-riscv64" &
wait
HOP_ELF="$(cat "$OUT/.hop-arm64")"
HOP_RV="$(cat "$OUT/.hop-riscv64")"
rm -f "$OUT/.hop-arm64" "$OUT/.hop-riscv64"
[ -f "$HOP_ELF" ] && [ -f "$HOP_RV" ] || { echo "release $VERSION: Hop niet gebouwd (zie hierboven)" >&2; exit 1; }

# Elk board en elke smaak in een eigen kopie van de boom met een eigen
# target-map, JOBS tegelijk (standaard 4): de image-scripts schrijven op
# vaste paden in target/, en cargo zet één slot per target-map, dus in één
# boom gaat het één voor één (02-10: vijf minuten op één core in de
# link-fases). Een kopie bouwt zijn afhankelijkheden zelf opnieuw; dat is
# CPU-tijd, geen wachttijd.
JOBS="${JOBS:-4}"
RUNNING=0
# job <naam> <functie> [argumenten]: start de functie in een kopie.
job() {
	name=$1
	shift
	WORK="$DIR/target/rel-$name"
	rm -rf "$WORK"
	mkdir -p "$WORK"
	(cd "$DIR" && tar --exclude=./target --exclude=./.git -cf - .) | tar -x -C "$WORK"
	(
		cd "$WORK" || exit 1
		export CARGO_TARGET_DIR="$WORK/target"
		"$@" >"$WORK/build.log" 2>&1
		echo $? >"$WORK/status"
	) &
	RUNNING=$((RUNNING + 1))
	[ "$RUNNING" -lt "$JOBS" ] || batch
}
# batch: wacht op de lopende kopieën en stopt bij de eerste die faalde.
batch() {
	wait
	RUNNING=0
	for w in "$DIR"/target/rel-*; do
		[ -f "$w/status" ] || continue
		if [ "$(cat "$w/status")" != 0 ]; then
			echo "== release $VERSION: ${w##*/rel-} FAALDE; het einde van $w/build.log:" >&2
			tail -n 30 "$w/build.log" >&2
			exit 1
		fi
		rm -rf "$w"
	done
}
# De bouwstappen per board, in de kopie (cwd), met de uitkomst naar $OUT.
pi() {
	GUI=$2 CFG="$3" APP="$HOP_ELF" sh "image/$1.sh"
	card "target/hopos-$1.img" "hopos-$1-$4.img"
	flip "$1" "$4" "$2" ""
}
radxa() {
	GUI=$1 CFG="$2" APP="$HOP_ELF" sh image/radxa-zero3.sh
	card target/radxa-zero3/hopos-radxa-zero3.img "hopos-radxa-$3.img"
	flip radxa "$3" "$1" ""
}
uefi() {
	ESP="$PWD/target/release-esp"
	# De O6N heeft de VPU: headfull is daar de media-smaak (die zet gui
	# zelf aan, hopos/Cargo.toml).
	if [ "$1" = o6n ] && [ "$2" = 1 ]; then
		MEDIA=1 BOARD=$1 CFG="$3" APP="$HOP_ELF" ESP="$ESP" sh image/uefi-run.sh
		flip "$1" "$4" 0 media
	else
		GUI=$2 BOARD=$1 CFG="$3" APP="$HOP_ELF" ESP="$ESP" sh image/uefi-run.sh
		flip "$1" "$4" "$2" ""
	fi
	# De stick: dezelfde vorm als de Go-stick (tag v2.2.8, image/uefi-run.sh
	# stap 3b). UEFI leest FAT16 van removable media. De config staat in het
	# venster van BOOTAA64.EFI (image/uefi-run.sh), niet als hopos.cfg op de
	# stick: `hop image` zet er een andere in.
	cargo run -q -p mkcard -- -o "$ESP.img" -size 64 -start 8192 -label hopos -vollabel -verify \
		"$ESP/EFI/BOOT/BOOTAA64.EFI=EFI/BOOT/BOOTAA64.EFI" \
		"$ESP/hopos-stage.elf" >&2
	card "$ESP.img" "hopos-$1-$4.img"
}
apple() {
	CFG="$DIR/image/cfg/headless.cfg" EMBED="$HOP_ELF" sh image/apple-m4.sh
	# De stick: het bootobject, de installer en de uitleg op één FAT-partitie
	# (LBA 2048, label HOPOS), zodat er in Recovery niets te typen valt behalve
	# het pad naar install.sh; die zoekt het image naast zichzelf.
	cargo run -q -p mkcard -- -o target/apple-m4/hopos-apple-card.img -size 32 -start 2048 \
		-label HOPOS -vollabel -verify "target/apple-m4/hopos-apple.img=hopos-apple.img" \
		"image/apple/install.sh=install.sh" "image/apple/README-m4.txt=README.txt" >&2
	card target/apple-m4/hopos-apple-card.img hopos-apple-headless.img
	flip apple headless 0 ""
}
licheerv() {
	STAGE="$HOP_RV" CFG="$DIR/image/cfg/headless.cfg" LICHEERV_DONOR_SHA256=$LRV_DONOR_SHA sh image/licheerv-agent.sh
	card target/licheerv/hopos-licheerv.img hopos-licheerv-headless.img
	# Ook de losse fip.bin: een kaart die al een HopOS-kaart is, krijgt zo een
	# nieuwe versie door alleen dat bestand te vervangen (ook vanaf een
	# telefoon), zonder dd.
	cp target/licheerv/fip-licheerv.bin "$OUT/hopos-licheerv-fip.bin"
}

step "de boards, $JOBS tegelijk (JOBS=)"
for FLAVOR in headless headfull; do
	G=0
	if [ "$FLAVOR" = headfull ]; then
		G=1
	fi
	for b in rpi4 rpi5; do
		job "$b-$FLAVOR" pi "$b" "$G" "$DIR/image/cfg/$FLAVOR.cfg" "$FLAVOR"
	done
	job "radxa-$FLAVOR" radxa "$G" "$DIR/image/cfg/$FLAVOR.cfg" "$FLAVOR"
	for b in o6n altra; do
		job "$b-$FLAVOR" uefi "$b" "$G" "$DIR/image/cfg/$FLAVOR.cfg" "$FLAVOR"
	done
done
job apple-headless apple
job licheerv-headless licheerv
batch
skip "apple headfull: board-apple heeft geen gui-feature"
skip "licheerv headfull: board-licheerv heeft geen gui-feature; en geen flipbundel (riscv64)"

# De apps: zonder debug-info en zonder lokale symbolen, met de globale (de
# kern leest er RamStart en de stempel uit; een riscv64-ELF draagt 42 000
# lokale symbolen, 1 MB symtab, en die reserveerde de kern tot 03-10 op zijn
# heap: op de LicheeRV "out of memory (1012680 bytes)" bij elke welcome; nu
# zoekt hij in brokken, maar het image blijft 1,4 MB tegen 329 KB).
step "apps"
need_objcopy release
ARM_APPS="$APPS decode"
RV_APPS="$APPS_RISCV64"
cargo build --quiet --release --target "$TARGET" $(for a in $ARM_APPS; do printf ' -p %s' "$a"; done)
cargo build --quiet --release --target "$RV" $(for a in $RV_APPS; do printf ' -p %s' "$a"; done)
for a in $ARM_APPS; do
	"$OBJCOPY" --strip-debug --discard-all "target/$TARGET/release/$a" "$OUT/$a-arm64.elf"
done
for a in $RV_APPS; do
	"$OBJCOPY" --strip-debug --discard-all "target/$RV/release/$a" "$OUT/$a-riscv64.elf"
done

GO_ELFS=""
if [ "${GO_APPS:-1}" = 0 ]; then
	skip "Go-apps: GO_APPS=0"
elif [ ! -x "$HOME/tamago-go/bin/go" ]; then
	skip "Go-apps: ~/tamago-go/bin/go ontbreekt (docs/go-apps.md)"
else
	step "Go-apps (go/apps-release.sh, PUBLISH=0)"
	# Niet fataal: de Rust-release hangt niet aan de Go-apps.
	if PUBLISH=0 OUT="$OUT" sh go/apps-release.sh; then
		GO_ELFS=" $OUT/*-tamago.elf"
	else
		skip "Go-apps: go/apps-release.sh faalde (de reden staat erboven)"
	fi
fi

(cd "$OUT" && shasum -a 256 * >SHA256SUMS)
echo "" >&2
echo "release $VERSION klaar in $((($(date +%s) - T0) / 60)) min: $OUT" >&2
ls -l "$OUT" >&2
if [ "$DRY" = 1 ]; then
	echo "(--dry-run: geen notes, geen regels om te publiceren)" >&2
	exit 0
fi

# De notes: het sjabloon van elke release, met de som van elk bestand.
{
	cat <<EOF
HopOS v$VERSION, the Rust generation. Every board comes in two flavors:
**headless** (no gui) and **headfull** (gui, plus media on the O6N). The
config in each image is one file per flavor from \`image/cfg\`:
\`headless.cfg\` (open on your own LAN: no API key, console over TCP) or
\`headfull.cfg\` (the same, and **welcome** on port 80). Where Hop lives is
the board's default: the OS core on the Pis and the Radxa, a core of his
own, a small one where the board has them, on the O6N, the Altra, the M4
and the LicheeRV (\`hopos.hop.sharegroup\` and \`hopos.hop.core-class\`
override it). A node without \`hopos.node\` names itself after its board
and the tail of its MAC (\`rpi4-4c54\`).

| Board | Headless | Headfull |
| --- | --- | --- |
| Raspberry Pi 4 | \`hopos-rpi4-headless.img.gz\` | \`hopos-rpi4-headfull.img.gz\` |
| Raspberry Pi 5 | \`hopos-rpi5-headless.img.gz\` | \`hopos-rpi5-headfull.img.gz\` |
| Radxa Zero 3E | \`hopos-radxa-headless.img.gz\` | \`hopos-radxa-headfull.img.gz\` |
| Radxa Orion O6N (USB stick) | \`hopos-o6n-headless.img.gz\` | \`hopos-o6n-headfull.img.gz\` (media) |
| Ampere Altra (USB stick) | \`hopos-altra-headless.img.gz\` | \`hopos-altra-headfull.img.gz\` |
| Mac mini M4 | \`hopos-apple-headless.img.gz\` (USB stick: image, installer, README) | none |
| Sipeed LicheeRV Nano | \`hopos-licheerv-headless.img.gz\` | none |

Every image carries its node config (\`hopos.cfg\`) inside the kernel, in a
16 KiB config window, the same on every board and in every flip bundle. Put
your own config in (\`hopos.node\`, \`hopos.apikey\`, your jobs) and write the
card or stick in one go with the \`hop\` command from
[hop](https://github.com/xinix00/hop); your own lines go last, the last
value of a key wins:

\`\`\`
gunzip hopos-rpi4-headless.img.gz
cat headless.cfg my-node.cfg >node.cfg
hop image hopos-rpi4-headless.img --config node.cfg --write /dev/rdiskN
\`\`\`

\`hop image <file>\` shows the config in an image, a bundle or on a card;
\`--keep\` writes a new image but keeps the config already on the card.
Without \`hop\`: \`gunzip -c hopos-rpi4-headless.img.gz | sudo dd of=/dev/rdiskN bs=4m\`
gives a node with the default config of its flavor.

The M4 stick is not a boot medium: write it to a USB drive, boot the mini
into Recovery and run \`sh /Volumes/HOPOS/install.sh go\` (the README on
the stick has the steps); from then on the Mac powers on into HopOS.

Kernel flip bundles, \`hopos-<board>-<flavor>.flip\`, replace a running
kernel without a reboot (docs/flip.md): put one on a web server and
\`curl -X POST http://NODE:8080/flip -d '{"url":"...","sha256":"..."}'\`
with its sum from SHA256SUMS. A bundle's config window is empty, so the
node keeps its own config over the flip; \`hop image <bundle> --config
<cfg>\` gives it another one (and prints the new sha256).

Apps (rolling release [apps](https://github.com/xinix00/HopOS/releases/tag/apps),
named \`<app>-<arch>.elf\`): $(echo $ARM_APPS | sed 's/ /, /g') for arm64;
$(echo $RV_APPS | sed 's/ /, /g; s/, \([^,]*\)$/ and \1/') for riscv64. Jobspecs in \`jobs/\`.

SHA256SUMS:

\`\`\`
EOF
	cat "$OUT/SHA256SUMS"
	echo '```'
} >"$NOTES"

cat <<EOF

De notes: $NOTES
Taggen en publiceren (als xinix00, GH_TOKEN; dit script voert niets uit):

  git tag -a v$VERSION -m "HopOS v$VERSION"
  git push origin v$VERSION
  gh release create v$VERSION $OUT/* --repo xinix00/HopOS --title "HopOS v$VERSION" --notes-file $NOTES
  gh release upload apps $OUT/*-arm64.elf $OUT/*-riscv64.elf$GO_ELFS --repo xinix00/HopOS --clobber
EOF
