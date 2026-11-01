#!/bin/sh
# De poort van HopOS v3 (handboek §9): host-tests, clippy met de harde set,
# rustfmt, de host-meetbank netmeter en de regelteller loc, de target-builds van de apps
# (de lijst APPS van tools/lib.sh) en van ELK board. Rood is rood.
#
# De boards: virt, rpi4, rpi5, rk3566 en de twee riscv64-boards (QEMU virt
# in machine mode en de LicheeRV, docs/boards-riscv.md) als debug-build in de gedeelde
# target-map (compileert en linkt tegen hun linkscript), en de drie
# UEFI-boards (QEMU/EDK2, O6N, Altra) via image/uefi-run.sh: een release-build
# als PIE in target/uefi plus de relocatie-toets en de PE-verpakking van dat
# script. Dat laatste is het deel dat op een stick stuk kan gaan zonder dat
# `cargo build` het ziet, dus de gate loopt het echte bouwpad.
#
# De drie builds (docs/sequence-hopos.md §14.3): headless (geen flag), `gui`
# en `media`, los van elkaar; beide trekken de bouwsteen `usb` mee. De toets
# dat headless geen regel scherm, media of USB meeneemt, staat hieronder
# (`cargo tree` per board).
#
# Het gui-vlak (docs/gui.md) bouwt voor elk board dat een framebuffer kan
# leveren ook met `--features gui`; de console op het glas bewijst
# `GUI=1 sh tools/qemu-test.sh` (ramfb en een screendump).
#
# Het media-vlak (docs/media.md): de codec-dienst van de kern, de client
# van applib en het aanzetten van de VPU bestaan alleen met hun feature
# `media`; hun tests en clippy krijgen een eigen ronde, hopos bouwt met
# media voor virt (de dienst zonder VPU, de optische drive over de USB,
# clippy erbij) en de O6N (de VPU, het echte bouwpad: `MEDIA=1 BOARD=o6n sh
# image/uefi-run.sh`, met clippy op dezelfde features), en apps/decode
# bouwt voor het target.
#
# De QEMU-ringen staan niet in de gate (die blijft onder tien minuten):
# tools/qemu-all.sh draait ze allemaal na elkaar.
#
# De duur staat onderaan, per stap: de grens is tien minuten.
set -e
cd "$(dirname "$0")/.."
DIR="$(pwd)"
. tools/lib.sh
T0=$(date +%s)
echo "== host: cargo test"
cargo test --quiet
echo "== host: cargo clippy"
cargo clippy --all-targets --quiet -- -D warnings
echo "== host: netmeter (test en clippy)"
# De meetbank op de host is std en dus geen default-member (de target-build
# van de bibliotheken slaat hem over); hier zijn eigen ronde.
cargo test --quiet -p netmeter
cargo clippy --all-targets --quiet -p netmeter -- -D warnings
echo "== host: loc (test en clippy)"
# De regelteller op de host, net zo'n std-crate buiten de default-members.
cargo test --quiet -p loc
cargo clippy --all-targets --quiet -p loc -- -D warnings
echo "== host: de gui-smaak en de bouwsteen usb van de boards (test en clippy)"
# De framebuffer-code van de boards (vcfb, gop, ramfb, de rk3566-keten)
# bestaat alleen met hun feature `gui`, de USB-bedrading per board
# (board/<x>/src/usb.rs: de VL805 van de Pi 4, de RP1 van de Pi 5, de
# DSDT-hosts van de O6N) alleen met `usb`; de werkruimte-ronde hierboven
# ziet ze dus niet (docs/gui.md). De bus zelf met zijn HID-kant
# (`usb/hid`) erbij.
GUI_BOARDS="-p board -p board-qemuvirt -p board-raspi -p board-uefi -p board-rk3566 -p board-rpi4 -p board-rpi5 -p board-o6n -p usb"
GUI_FEATS="board-qemuvirt/gui,board-raspi/gui,board-uefi/gui,board-rk3566/gui,board-rpi4/gui,board-rpi5/gui,board-o6n/gui"
USB_FEATS="board-qemuvirt/usb,board-raspi/usb,board-uefi/usb,board-rk3566/usb,board-rpi4/usb,board-rpi5/usb,board-o6n/usb,usb/hid"
cargo test --quiet $GUI_BOARDS --features "$GUI_FEATS,$USB_FEATS"
cargo clippy --all-targets --quiet $GUI_BOARDS --features "$GUI_FEATS,$USB_FEATS" -- -D warnings
echo "== host: de VHE-vorm van het UEFI-board (test en clippy)"
# De kern onder E2H = 1 (board/uefi/src/el2.rs) bestaat alleen met de
# feature `vhe` van board-uefi; de werkruimte-ronde ziet de nVHE-vorm. De
# target-build van de VHE-vorm is de O6N hieronder (board-o6n zet hem altijd
# aan); de proef op QEMU is `FEATURES=vhe CPU=neoverse-n1 sh
# tools/qemu-uefi-test.sh`.
cargo test --quiet -p board-uefi --features vhe
cargo clippy --all-targets --quiet -p board-uefi --features vhe -- -D warnings
echo "== host: het media-vlak (test en clippy)"
MEDIA_PKGS="-p abi -p kern -p applib -p board-o6n"
MEDIA_FEATS="abi/media,kern/media,applib/media,board-o6n/media"
cargo test --quiet $MEDIA_PKGS --features "$MEDIA_FEATS"
cargo clippy --all-targets --quiet $MEDIA_PKGS --features "$MEDIA_FEATS" -- -D warnings
echo "== host: mkcard (test en clippy)"
# De kaartbouwer op de host, net als netmeter std en geen default-member.
# Zijn toets legt drie kaarten byte voor byte naast de Go-mkcard, of naast
# de sha256 die Go op 02-10 gaf.
cargo test --quiet -p mkcard
cargo clippy --all-targets --quiet -p mkcard -- -D warnings
echo "== rustfmt"
cargo fmt --check
echo "== target: bibliotheken (aarch64)"
cargo build --quiet --target aarch64-unknown-none-softfloat
echo "== target: apps ($(echo $APPS | sed 's/ /, /g'))"
# shellcheck disable=SC2086
cargo build --quiet --target aarch64-unknown-none-softfloat $(printf ' -p %s' $APPS)
# De stage-1 van applib::mmu (de MMU-aan, de vectortabel, het glas)
# bestaat alleen op het target; de host-ronde ziet alleen de tabellen.
cargo clippy --quiet --target aarch64-unknown-none-softfloat -p display -- -D warnings
echo "== target: hopos (qemuvirt)"
cargo build --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-qemuvirt
echo "== target: hopos (rpi4, rpi5)"
cargo build --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-rpi4
cargo build --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-rpi5
echo "== target: hopos (rk3566)"
cargo build --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-rk3566
echo "== target: hopos (apple: de Mac mini M4, docs/boards-apple.md): clippy en het image"
# De Apple-lijm (cage.rs, watchdog.rs, telemetry.rs) bestaat alleen met
# board-apple; de werkruimte-clippy ziet hem niet. Het image loopt het echte
# bouwpad: de release-build, de stub-toets en het config-venster op 0xF000.
cargo clippy --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-apple -- -D warnings
sh image/apple-m4.sh
echo "== target: hopos (riscv64: qemuvirt-riscv, licheerv)"
cargo build --quiet --target riscv64gc-unknown-none-elf -p hopos --features board-qemuvirt-riscv
cargo build --quiet --target riscv64gc-unknown-none-elf -p hopos --features board-licheerv
echo "== target: appspike (riscv64)"
# De riscv-_start, de paniek en de timebase van applib bestaan alleen op dat
# doel; de host-ronde ziet ze niet. De keten zelf draait in
# tools/qemu-riscv-test.sh.
cargo build --quiet --target riscv64gc-unknown-none-elf -p appspike
cargo clippy --quiet --target riscv64gc-unknown-none-elf -p appspike -p applib -- -D warnings
echo "== headless: geen regel scherm, media of USB (§14.3)"
# Per board de crates van een kale build: geen van de vlakken mag erin.
for b in qemuvirt rpi4 rpi5 rk3566 uefi o6n altra apple; do
	if cargo tree --quiet --target aarch64-unknown-none-softfloat -p hopos --features "board-$b" \
		-e normal --prefix none | grep -E '^(usb|gui-[a-z]+|media-[a-z]+|driver-(fb|xhci|hid|dwc3|codec)) '; then
		echo "headless $b neemt een vlak mee" >&2
		exit 1
	fi
done
echo "== target: hopos met gui (qemuvirt, rpi4, rpi5, rk3566)"
for b in qemuvirt rpi4 rpi5 rk3566; do
	cargo build --quiet --target aarch64-unknown-none-softfloat -p hopos --features "board-$b,gui"
done
# Het glas (gui.rs), de USB-taak (usb.rs) en de input-listener
# (net/input.rs) bestaan alleen met gui; de werkruimte-clippy ziet de binary
# niet.
cargo clippy --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-qemuvirt,gui -- -D warnings
echo "== target: hopos met media (qemuvirt, o6n) en apps/decode"
# De bring-up, de firmware-lezing en het meetinstrument (hopos/src/codec.rs)
# en de optische drive (optical.rs, over usb.rs zonder gui) bestaan alleen
# met media, en op de O6N; de werkruimte-clippy ziet ze niet.
cargo clippy --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-qemuvirt,media -- -D warnings
RUSTFLAGS="-C relocation-model=pie" cargo clippy --quiet --target aarch64-unknown-none-softfloat \
	-p hopos --features board-o6n,media --target-dir target/uefi-media -- -D warnings
cargo build --quiet --target aarch64-unknown-none-softfloat -p hopos --features board-qemuvirt,media
MEDIA=1 BOARD=o6n sh image/uefi-run.sh
cargo build --quiet --release --target aarch64-unknown-none-softfloat -p decode
echo "== image: UEFI met gui (uefi, o6n, altra)"
GUI=1 BUILD_ONLY=1 BOARD=uefi sh image/uefi-run.sh
GUI=1 BOARD=o6n sh image/uefi-run.sh
GUI=1 BOARD=altra sh image/uefi-run.sh
echo "== image: UEFI (uefi, o6n, altra), kaal: de laatste ESP is de kale"
BUILD_ONLY=1 BOARD=uefi sh image/uefi-run.sh
BOARD=o6n sh image/uefi-run.sh
BOARD=altra sh image/uefi-run.sh
echo "poort groen in $(($(date +%s) - T0)) s"
