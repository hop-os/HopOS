#!/bin/sh
# Boot HopOS v3 op QEMU -M virt. Altijd virtualization=on: HopOS eist een
# EL2-boot (de stage-2-kooi is een invariant, geen optie); PSCI via SMC,
# GICv3, tot 12 cores: dezelfde bouwstenen als de O6N. De QEMU-regel is die
# van de Go-generatie (image/qemu-run.sh op tag v2.2.8), met een virtio-blk-schijf in
# plaats van de NVMe: hopfs mount hem bij boot, en Hop bewaart er zijn staat
# op (`/hop/`). Drie hostfwd's: de system-API (10.0.2.15:10100) op
# 127.0.0.1:$SYSPORT, en de API van Hop: de agent (:8080) op
# 127.0.0.1:$AGENTPORT en de leader (:9080) op 127.0.0.1:$LEADERPORT. De
# kern zet 8080 en 9080 van de uplink door naar het slot van Hop (DNAT).
#
#   image/qemu-run.sh                Hop in slot 1 (standaard)
#   APP=/pad/naar/elf image/qemu-run.sh   een kant-en-klare Hop
#   APP= image/qemu-run.sh           zonder image: de kern plaatst alleen
#                                    Hop (01-11, O7); een app plaatst Hop
#                                    (ARTIFACT hieronder)
#   image/qemu-run.sh -s -S          extra argumenten gaan naar QEMU (gdb)
#   SMP=2 image/qemu-run.sh          twee cores: de kern en Hop delen de
#                                    OS-core, de andere is een volle
#                                    app-core (PORT.md beslissing 2);
#                                    standaard 4
#   OSCORE=1 image/qemu-run.sh       de OS-core (bootparameter
#                                    hopos.oscore=<small|mid|big|N> in de
#                                    FDT-bootargs); de kern boot op core 0
#                                    en verhuist er vóór de eerste bewoner
#                                    heen. Standaard geen: de boot-core
#   GUI=display ARTIFACT=display image/qemu-run.sh
#                                    de gui-kern (`--features gui`) met
#                                    `-device ramfb` in een venster, en de
#                                    USB-invoer: `-device qemu-xhci` met
#                                    `usb-kbd` en `usb-mouse` erachter
#                                    (docs/gui.md). Klik in het venster en
#                                    typ: de kern leest de toetsen en de
#                                    muis en de display-app tekent ze (de
#                                    job van docs/gui.md naar Hop).
#                                    QDISPLAY=cocoa|gtk|sdl kiest de
#                                    weergave (standaard die van QEMU)
#   WIRE=1 image/qemu-run.sh         een trage draad met vertraging, zoals
#                                    een link van 1 Gbit met een paar ms RTT
#                                    (de ORION naar een Mac over wifi): de
#                                    virtio-net zendt hoogstens
#                                    WIRE_BURST frames (8) per WIRE_TIMER ns
#                                    (100000: ~80 kpps, ~120 MB/s), dus zijn
#                                    TX-ring loopt vol als de gast sneller
#                                    zendt; en een filter-buffer houdt elk
#                                    pakket tot WIRE_DELAY_US µs (3000) vast,
#                                    in beide richtingen. Standaard uit
#                                    (tools/qemu-test-bench.sh met WIRE=1)
#   DISK=pad image/qemu-run.sh       de schijf (raw); standaard
#                                    target/hopos-disk.img, 64 MiB, aangemaakt
#                                    (ijl) als hij ontbreekt. Een verse schijf
#                                    is een lege hopfs; een bestaande houdt de
#                                    volumes over een herstart (stateful).
#   DISKLAT=1500000000 image/qemu-run.sh
#                                    een trage schijf in plaats van DISK:
#                                    QEMU's null-co met zoveel nanoseconden
#                                    per verzoek (ook per FLUSH), leeg en
#                                    vluchtig (leest nullen, bewaart niets).
#                                    De proef van tools/qemu-test-slowdisk.sh:
#                                    op macOS is een FLUSH een F_FULLFSYNC
#                                    van 3 tot 786 ms (30-09)
#   BOOTARGS="hopos.s3.bucket=hop" image/qemu-run.sh
#                                    extra bootparameters, letterlijk achter
#                                    de rest in -append
#   CFG="a.cfg b.cfg" image/qemu-run.sh
#                                    hopos.cfg in lagen in het venster van de
#                                    kern (board/src/cfgwin.rs,
#                                    image/hopcfg.py), in een kopie van de
#                                    ELF: de build zelf blijft leeg. De
#                                    laatste waarde wint; het venster wint
#                                    van de bootargs, en die van QEMU_CFG
#                                    van de kern (voor Hop)
#   CONPORT=5555 image/qemu-run.sh   een hostfwd naar de console over TCP
#                                    (:5555, `nc 127.0.0.1 $CONPORT`)
#   WEBPORT=8081 image/qemu-run.sh   een vierde hostfwd: 127.0.0.1:$WEBPORT
#   DNSPORT=15353 image/qemu-run.sh  een UDP-hostfwd naar :5353 (hopdns)
#                                    naar poort 80 van de gast, de poort die
#                                    een jobspec met "ports":{"http":80}
#                                    publiceert (de kern zet hem door naar
#                                    het slot van de app)
#   ARTIFACT=welcome image/qemu-run.sh
#                                    een app-ELF voor Hop: een crate uit deze
#                                    werkruimte (of een pad naar een ELF),
#                                    zonder debug-info op een artifact-server
#                                    op 127.0.0.1:$ARTPORT (standaard 8000),
#                                    voor de gast
#                                    http://10.0.2.2:$ARTPORT/<naam>.elf. Het
#                                    script drukt de URL en het curl-commando
#                                    met de jobspec af; de server stopt met
#                                    QEMU.
#
# APP=hop bouwt `agentd-hopos` uit de hop-repo ($HOP_DIR, standaard
# ../hop/hop naast deze repo) met tools/hop-build.sh, tegen de applib van
# deze werkboom zoals de release en de toetsen, en neemt alleen het
# bestand: geen pad-dependency over de repo-grens (PORT.md beslissing 6).
#
# Het image gaat rauw in RAM op de staging van het board
# (board/qemuvirt/src/slots.rs: STAGE_PA), met zijn maat in het woord op
# STAGE_HDR_PA en zijn rol op STAGE_ROLE_PA (0 = app, 1 = Hop). De staging
# is 14 MiB; de debug-info gaat eraf (rust-objcopy --strip-debug), de
# symbolen blijven, want de plaatsing leest __hopos_* uit de symbooltabel.
#
# QEMU laadt de kern-ELF rechtstreeks op zijn fysieke adressen
# (hopos/link.ld); een raw image met objcopy is niet nodig.
set -e

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$DIR/tools/lib.sh"
SMP="${SMP:-4}"
SYSPORT="${SYSPORT:-10100}"
AGENTPORT="${AGENTPORT:-8080}"
LEADERPORT="${LEADERPORT:-9080}"
TARGET=aarch64-unknown-none-softfloat
DISK="${DISK:-$DIR/target/hopos-disk.img}"
DISK_MIB="${DISK_MIB:-64}"

cd "$DIR"
EXTRA_FEATURES="${FEATURES:-}"
# GUI=display: de gui-kern, ramfb in een venster, en de USB-invoer.
FEATURES=board-qemuvirt
SCREEN="-nographic"
QUSB=""
case "${GUI:-}" in
"") ;;
display)
	FEATURES=board-qemuvirt,gui
	SCREEN="-display ${QDISPLAY:-default} -device ramfb"
	QUSB="-device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-mouse,bus=xhci.0"
	;;
*)
	echo "qemu-run: GUI=$GUI: only GUI=display" >&2
	exit 2
	;;
esac
# WIRE=1: de trage draad (x-txburst per x-txtimer) en de vertraging
# (filter-buffer), via $QNETDEV en $QNETOBJ in tools/lib.sh qemu_virt.
QNETDEV=""
QNETOBJ=""
case "${WIRE:-}" in
"" | 0) ;;
1)
	QNETDEV="tx=timer,x-txburst=${WIRE_BURST:-8},x-txtimer=${WIRE_TIMER:-100000}"
	QNETOBJ="-object filter-buffer,id=wire0,netdev=n0,queue=all,interval=${WIRE_DELAY_US:-3000}"
	echo "qemu-run: wire ${WIRE_BURST:-8} frames per ${WIRE_TIMER:-100000} ns, ${WIRE_DELAY_US:-3000} us buffered each way" >&2
	;;
*)
	echo "qemu-run: WIRE=$WIRE: only WIRE=1" >&2
	exit 2
	;;
esac
# De watchdog is verplicht, ook op QEMU (O2 van 01-11): altijd de i6300esb,
# en een reset (geen exit, geen pauze) als hij afloopt. Zo reset een
# HOPOS_FATAL hier net als op ijzer. Er is geen knop om hem uit te zetten.
QWDT="-device i6300esb -action watchdog=reset"
# FEATURES=gui (of een andere lijst) komt achter de board-feature, zoals
# in uefi-run.sh.
FEATURES="$FEATURES${EXTRA_FEATURES:+,$EXTRA_FEATURES}"
cargo build --quiet --release --target "$TARGET" -p hopos --features "$FEATURES"
KERNEL="$DIR/target/$TARGET/release/hopos"
if [ -n "${CFG:-}" ]; then
	for f in $CFG; do
		[ -f "$f" ] || { echo "qemu-run: CFG: $f does not exist" >&2; exit 1; }
	done
	cp "$KERNEL" "$KERNEL.cfg"
	# shellcheck disable=SC2086 # CFG is een lijst bestanden
	python3 "$DIR/image/hopcfg.py" set "$KERNEL.cfg" $CFG
	KERNEL="$KERNEL.cfg"
fi

pick_app hop
if [ -n "$IMAGE" ]; then
	# Zonder debug-info: agentd-hopos is met debug-info 12 MB (gemeten
	# 29-09) tegen 594 KiB laadbaar. De rol in het woord op STAGE_ROLE_PA:
	# 1 = Hop, 0 = app.
	STAGED="$DIR/target/$TARGET/release/staged-$(basename "$IMAGE").elf"
	strip_elf "$IMAGE" "$STAGED"
	fits "$STAGED" "$IMAGE"
	case "$ROLE" in
	hop) ROLE=1 ;;
	app) ROLE=0 ;;
	esac
	set -- -device "loader,file=$STAGED,addr=0xb0200000,force-raw=on" \
		-device "loader,addr=0xb0100000,data=$SIZE,data-len=8" \
		-device "loader,addr=0xb0100008,data=$ROLE,data-len=8" "$@"
fi

# De bootparameters: QEMU legt -append in /chosen/bootargs van de DTB, en
# daar leest het board ze (board/qemuvirt `os_core`, hopos `bootparam`).
# Eén -append: een tweede zou de eerste vervangen.
ARGS=""
[ -n "${OSCORE:-}" ] && ARGS="hopos.oscore=$OSCORE"
# BOOTARGS: meer bootparameters, letterlijk (tools/qemu-test-store.sh geeft
# er `hopos.s3.*` mee voor Hop).
[ -n "${BOOTARGS:-}" ] && ARGS="${ARGS:+$ARGS }$BOOTARGS"
if [ -n "$ARGS" ]; then
	set -- -append "$ARGS" "$@"
fi

# De schijf: ijl aangemaakt als hij er niet is (dd met seek schrijft niets).
# Met DISKLAT een null-co met die latency per verzoek, zonder bestand.
if [ -n "${DISKLAT:-}" ]; then
	QDISK="-blockdev driver=null-co,node-name=disk0,size=$((DISK_MIB * 1048576)),latency-ns=$DISKLAT,read-zeroes=on"
else
	QDISK="-drive if=none,format=raw,file=$DISK,id=disk0"
fi
if [ -z "${DISKLAT:-}" ] && [ ! -e "$DISK" ]; then
	mkdir -p "$(dirname "$DISK")"
	dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek="$DISK_MIB" 2>/dev/null
	echo "qemu-run: new disk $DISK ($DISK_MIB MiB)" >&2
fi

# De artifact-server voor een app die Hop ophaalt (ARTIFACT). Zonder
# debug-info, zoals de staging hierboven; de symbolen blijven voor de
# plaatsing. QEMU draait dan als kind (geen exec), zodat de server met hem
# stopt.
ARTPID=""
if [ -n "${ARTIFACT:-}" ]; then
	ARTPORT="${ARTPORT:-8000}"
	case "$ARTIFACT" in
	*/*) ARTELF="$ARTIFACT" ;;
	*)
		cargo build --quiet --release --target "$TARGET" -p "$ARTIFACT"
		ARTELF="$DIR/target/$TARGET/release/$ARTIFACT"
		;;
	esac
	ARTDIR="$(mktemp -d -t hopos-artifact.XXXXXX)"
	ARTNAME="$(basename "$ARTELF" .elf).elf"
	strip_elf "$ARTELF" "$ARTDIR/$ARTNAME"
	(cd "$ARTDIR" && exec python3 -m http.server "$ARTPORT" --bind 127.0.0.1) >/dev/null 2>&1 &
	ARTPID=$!
	trap 'kill "$ARTPID" 2>/dev/null; rm -rf "$ARTDIR"' EXIT INT TERM
	{
		echo "qemu-run: artifact http://10.0.2.2:$ARTPORT/$ARTNAME (host 127.0.0.1:$ARTPORT)"
		echo "qemu-run: curl -X POST -d '{\"name\":\"$(basename "$ARTNAME" .elf)\",\"driver\":\"hop\",\"artifacts\":[{\"url\":\"http://10.0.2.2:$ARTPORT/$ARTNAME\"}],\"memory_limit\":33554432,\"ports\":{\"http\":80}}' http://127.0.0.1:$LEADERPORT/v1/jobs"
	} >&2
fi

# De QEMU-regel is tools/lib.sh qemu_virt; hier de hostfwd's.
FWD="hostfwd=tcp:127.0.0.1:${SYSPORT}-:10100"
FWD="$FWD,hostfwd=tcp:127.0.0.1:${AGENTPORT}-:8080,hostfwd=tcp:127.0.0.1:${LEADERPORT}-:9080"
if [ -n "${WEBPORT:-}" ]; then
	FWD="$FWD,hostfwd=tcp:127.0.0.1:${WEBPORT}-:80"
fi
# CONPORT=5555: een hostfwd naar de console over TCP (:5555).
if [ -n "${CONPORT:-}" ]; then
	FWD="$FWD,hostfwd=tcp:127.0.0.1:${CONPORT}-:5555"
fi
# DNSPORT=15353: een UDP-hostfwd naar :5353 van de gast, voor hopdns in een
# slot (de kern publiceert tcp én udp; slirp doet UDP alleen met deze knop).
if [ -n "${DNSPORT:-}" ]; then
	FWD="$FWD,hostfwd=udp:127.0.0.1:${DNSPORT}-:5353"
fi
QFWD="$FWD"
# shellcheck disable=SC2086
set -- $SCREEN -monitor none $QUSB $QWDT $QDISK -kernel "$KERNEL" "$@"
# Met een artifact-server draait QEMU als kind, zodat de server met hem
# stopt; anders vervangt hij dit script.
if [ -n "$ARTPID" ]; then
	(qemu_virt "$SMP" "$@")
else
	qemu_virt "$SMP" "$@"
fi
