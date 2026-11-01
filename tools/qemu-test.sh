#!/bin/sh
# De QEMU-poort van HopOS v3 (handboek §9: een emulator-boot met dezelfde
# markers als het ijzer). Bouwt de kern, boot hem op -M virt met dezelfde
# regel als image/qemu-run.sh, vangt de console en slaagt alleen als de
# markers er staan:
#
#   HOPOS_BOOT      de kern haalde kmain, de console en de executor;
#   HOPOS_TICK 3    drie seconden executor, timer en IRQ-deur;
#   HOPOS_NIC_UP    de virtio-net (QEMU heeft er altijd een in deze regel).
#   HOPOS_DISK_UP   de virtio-blk op een verse schijf van 64 MiB (per run
#                   een eigen tijdelijk bestand, dus altijd leeg);
#   HOPOS_FS_UP     hopfs erop gemount, vers (fresh=1);
#   HOPOS_CFG_WINDOW de config in het venster van het kern-image
#                   (board/src/cfgwin.rs), in twee lagen erin gezet door
#                   image/hopcfg.py zoals elk image-script het doet: de kern
#                   leest hem (de bootregel) en gebruikt hem, en de laatste
#                   waarde wint (de eerste laag zegt system, de tweede hop,
#                   en de bootregel moet "Hop in the sharegroup hop
#                   (hopos.hop.sharegroup)" zeggen; zonder config staat er
#                   "(default)", HOPOS_HOP_GROUP);
#   HOPOS_NET_UP    pomp, switch, poort 0 en een DHCP-lease van user-net;
#   HOPOS_SYSTEM_UP de system-listener op poort 10100;
#   HOPOS_WD_CANARY_OK de canary van de watchdog: een nieuwe verbinding van
#                   de node-stack naar zijn eigen system-poort slaagde (virt
#                   heeft geen watchdog-blok, de taak draait toch), en
#                   zonder weigerregel: die drie blijven van de toets
#                   van buiten;
#   extern          een TCP-verbinding van de host via hostfwd naar
#                   10.0.2.15:10100 die de kern accepteert en weigert (geen
#                   slot achter 10.0.2.2): de host ziet EOF en de kern meldt
#                   een NIEUWE regel HOPOS_SYSTEM_REFUSED. EOF alleen bewijst
#                   niets (slirp sluit ook als de gast niet luistert), de
#                   marker wel. Drie keer, op willekeurige momenten van de
#                   keten: meteen na HOPOS_SYSTEM_UP, zodra de app van slot 2
#                   zijn netwerk heeft (vlak voor zijn NET-toets, die dan met
#                   de toets van buiten samenvalt), en na de stop van slot 2
#                   (bewijst dat de listener na de keten niet hangt).
#   het zaad        HOPOS_RNG_SLOTS source=jitter (de kern zaait zijn DRBG
#                   uit jitter, virt heeft geen TRNG) en in slot 2
#                   "applib: rng seed from the kernel (gen N) is jitter ...
#                   HOPOS_APP_RNG source=jitter": de app kreeg zaad op zijn
#                   control-page (CTRL_RNG_SEED), niet "geen zaad";
#   Hop en appspike het ABI-bewijs langs de echte weg (sinds 01-11,
#                   docs/sequence-hopos.md §14.1 O7: de kern plaatst alleen
#                   Hop): Hop staat gestaged in slot 1 (HOP_UP, HOP_LEADER),
#                   en van buiten gaat er één jobspec naar zijn leader. Hop
#                   haalt appspike van een artifact-server op de host en
#                   plaatst hem in slot 2 op een app-core in zijn
#                   stage-2-kooi, al zijn toetsen groen via de servicer op de
#                   console (HOPOS_APPSPIKE_DONE pass=9 fail=0, met
#                   HOPOS_APPSPIKE_FS: schrijven, stat, lezen, lijst en weg in
#                   de eigen root via de system-API naar hopfs), en de
#                   bevestigde stop (HOPOS_SLOT_STOPPED). appspike stopt met
#                   code 0 en Hop herstart een service: de tweede ronde loopt
#                   op de warm geparkeerde core (twee keer
#                   HOPOS_APPSPIKE_DONE).
#
# Faalt er een, dan drukt het script de hele console af en faalt: rood is
# rood. Een HOPOS_PANIC of HOPOS_EXCEPTION is meteen rood.
#
#   GUI=1 tools/qemu-test.sh    de gui-smaak (docs/gui.md): bouwt met
#                               `--features gui`, geeft QEMU `-device ramfb`
#                               en eist ook HOPOS_FB_UP en HOPOS_FB_CONSOLE,
#                               plus een screendump via de monitor waarop de
#                               console staat (voorgrond- en
#                               achtergrondpixels van driver-fb, niet leeg)
#   GUI=display tools/qemu-test.sh   de gui-smaak met de display-app en de
#                               USB-invoer (docs/gui.md): QEMU krijgt ook
#                               `-device qemu-xhci` met `usb-kbd` en
#                               `usb-mouse`, en in plaats van appspike
#                               plaatst Hop apps/display, met
#                               `GUI=display` en `DISPLAY_QUIT=27` in de
#                               env van de job. Per ronde
#                               eist het script HOPOS_FB_GRANT (de console
#                               gaat van het glas), HOPOS_FB_ARM (het
#                               venster in de kooi), HOPOS_DISPLAY_UP en
#                               HOPOS_DISPLAY_CONN (de app belt de
#                               input-listener van de kern); dan een toets
#                               en een muisbeweging via de monitor
#                               (`sendkey`, `mouse_move`) en
#                               HOPOS_DISPLAY_INPUT keys>=1 moves>=1. Na de
#                               eerste ronde volgt een screendump waarop de
#                               app tekende (zijn achtergrond, tekst en
#                               cursor; niet de console), dan Escape: de app
#                               stopt (HOPOS_DISPLAY_QUIT, exit 0), de grant
#                               gaat terug (HOPOS_FB_RELEASE), en Hop
#                               herstart hem, die het glas en de invoer
#                               overneemt (een tweede verbinding van
#                               10.100.0.3). De slotscreendump toont weer
#                               de app.
#   SHOT_KEEP=pad.ppm           bewaart die screendump (met GUI=1 of display)
#   tools/qemu-test.sh          TIMEOUT=30 standaard, in seconden
#   KEEP_LOG=pad tools/qemu-test.sh   bewaart ook een groene console
#   SYSPORT=poort               de host-kant van de hostfwd; standaard een
#                               vrije van het OS (bezet = een vrije, luid)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
TARGET=aarch64-unknown-none-softfloat
GUI="${GUI:-0}"
FEATURES=board-qemuvirt
QGUI="-monitor none"
QUSB=""
# De app van de job: appspike (het ABI-bewijs), of met GUI=display de
# display-app.
APP_PKG=appspike
APP_ENV=""
case "$GUI" in
0 | 1) ;;
display)
	APP_PKG=display
	APP_ENV=',"env":{"GUI":"display","DISPLAY_QUIT":"27"}'
	QUSB="-device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-mouse,bus=xhci.0"
	;;
*)
	echo "GUI=$GUI: kies 0, 1 of display" >&2
	exit 2
	;;
esac
# De display-keten heeft twee plaatsingen met invoer ertussen: iets meer tijd.
if [ "$GUI" = display ]; then TIMEOUT="${TIMEOUT:-75}"; else TIMEOUT="${TIMEOUT:-60}"; fi
scratch qemu
KCFG="$ART/kern"
CFGWIN="$ART/cfg"
CFGBORD="$ART/cfg-bord"
if [ "$GUI" != 0 ]; then
	FEATURES=board-qemuvirt,gui
	# Kort pad: een unix-socket mag niet langer dan ~104 tekens zijn.
	MON="$(mktemp -u /tmp/hopos-mon.XXXXXX)"
	SHOT="$ART/shot"
	QGUI="-device ramfb -monitor unix:$MON,server,nowait"
	trap 'cleanup; rm -f "$MON"' EXIT INT TERM
fi
# De system-API, de agent en de leader van Hop, en de artifact-server.
ports SYS AGENT LEADER ART
# Een verse, ijle schijf van 64 MiB: hopfs begint leeg.
dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek=64 2>/dev/null

cd "$DIR"
echo "== bouwen: hopos ($FEATURES)"
cargo build --quiet --release --target "$TARGET" -p hopos --features "$FEATURES" 2>/dev/null ||
	cargo build --release --target "$TARGET" -p hopos --features "$FEATURES"
# De kern met een config in zijn venster: een kopie, zodat de build zelf
# leeg blijft (de andere toetsen rekenen op hun eigen config).
printf '# tools/qemu-test.sh: de config in het venster van de kern\nhopos.hop.sharegroup=system\n' >"$CFGWIN"
printf '# de bordlaag erachter: de laatste waarde wint\nhopos.hop.sharegroup=hop\n' >"$CFGBORD"
cp "$DIR/target/$TARGET/release/hopos" "$KCFG"
python3 "$DIR/image/hopcfg.py" set "$KCFG" "$CFGWIN" "$CFGBORD"
KERNEL="$KCFG"
echo "== bouwen: $APP_PKG en agentd-hopos"
apps "$APP_PKG"
hop_elf
# Hop op de staging, zonder debug-info, met de rol Hop (1).
strip_elf "$HOP_ELF" "$ART/hop.elf"
fits "$ART/hop.elf" agentd-hopos
HOP_SIZE=$(wc -c <"$ART/hop.elf" | tr -d ' ')
serve
JOB='{"name":"'"$APP_PKG"'","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/'"$APP_PKG"'.elf"}],"memory_limit":33554432'"$APP_ENV"'}'
SLOT_MARKS="HOPOS_DISK_UP model=virtio-blk blocks=131072|HOPOS_FS_UP fresh=1"
SLOT_MARKS="$SLOT_MARKS|cfg: hopos.cfg from the window in the kernel image, [0-9]+ bytes HOPOS_CFG_WINDOW|slots: Hop in the sharegroup hop \(hopos.hop.sharegroup\) HOPOS_HOP_GROUP"
# De acties op de monitor (GUI=display), elk op zijn moment (grep -E), in
# volgorde: "moment;commando;commando...". Een commando `shot` is de
# screendump waarop de app moet staan.
ACT_AT=""
if [ "$GUI" = display ]; then
	# De USB-keten van de kern: de qemu-xhci op PCIe, beide apparaten, de
	# listener op 10.100.0.1:7879.
	SLOT_MARKS="$SLOT_MARKS|usb: xHCI 1b36:000d at .* on PCIe|HOPOS_USB_UP|usb: qemu-xhci port [0-9]+: keyboard|usb: qemu-xhci port [0-9]+: mouse|HOPOS_INPUT_UP"
	# Per ronde (de eerste en de herstart door Hop, allebei in slot 2):
	# het glas, de app erop, zijn verbinding met de invoer, en een toets en
	# een beweging die hem bereiken.
	SLOT_MARKS="$SLOT_MARKS|HOPOS_SLOT_START slot=2|slot 2: fb granted .*HOPOS_FB_GRANT|slot 2: fb window mapped at ipa .*HOPOS_FB_ARM|slot 2: display: window 0x20000000.* mapped Normal-NC|slot 2: display: glass 1280x800 .*HOPOS_DISPLAY_UP w=1280 h=800|input: 10.100.0.3 connected .*HOPOS_INPUT_CONN|slot 2: display: input stream .*HOPOS_DISPLAY_CONN|slot 2: display: .*HOPOS_DISPLAY_INPUT keys=[1-9][0-9]* moves=[1-9]"
	# De eerste ronde stopt op Escape: exit 0, en het glas terug aan de
	# console; Hop herstart de service.
	SLOT_MARKS="$SLOT_MARKS|slot 2: display: quit key 27.*HOPOS_DISPLAY_QUIT|slot 2: fb grant released, console back on glass HOPOS_FB_RELEASE|slot 2: stopped.*HOPOS_SLOT_STOPPED"
	# Een moment "2*patroon" is de tweede keer dat het patroon er staat.
	ACT_AT="slot 2: .*HOPOS_DISPLAY_CONN;sendkey a;mouse_move 40 30"
	ACT_AT="$ACT_AT|slot 2: .*HOPOS_DISPLAY_INPUT keys=[1-9][0-9]* moves=[1-9];shot;sendkey esc"
	ACT_AT="$ACT_AT|2*slot 2: .*HOPOS_DISPLAY_CONN;sendkey b;mouse_move -20 10"
	# De tweede ronde: weer de invoer, nu op de herstarte app.
	TWICE="slot 2: .*HOPOS_DISPLAY_CONN|slot 2: display: .*HOPOS_DISPLAY_INPUT keys=[1-9][0-9]* moves=[1-9]"
	# De momenten van de toets van buiten (grep -E), in volgorde.
	PROBE_AT="HOPOS_SYSTEM_UP|slot 2: .*HOPOS_DISPLAY_CONN"
else
	# De markers van het ABI-bewijs (grep -E): het aantal toetsen groeit met
	# appspike, dus "alles groen" is fail=0.
	# Het zaad (hopos/src/seed.rs, applib::rand): virt heeft geen TRNG
	# (cortex-a53, geen EL3), dus de kern zaait uit jitter en zegt dat, en
	# de app ziet zaad uit jitter, niet "geen zaad".
	SLOT_MARKS="$SLOT_MARKS|trng: WARNING the kernel DRBG is seeded from timer jitter.*HOPOS_RNG_INSECURE|HOPOS_RNG_SLOTS source=jitter|slot 2: applib: rng seed from the kernel \(gen [0-9]+\) is jitter.*HOPOS_APP_RNG source=jitter"
	SLOT_MARKS="$SLOT_MARKS|HOPOS_SLOT_START slot=2|slot 2: HOPOS_APPSPIKE_NETLOG|slot 2: HOPOS_APPSPIKE_FS ok|slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0|slot 2: stopped.*HOPOS_SLOT_STOPPED"
	# De tweede ronde (Hop herstart de service) op de warm geparkeerde core.
	TWICE="slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0"
	# De momenten van de toets van buiten (grep -E), in volgorde.
	PROBE_AT="HOPOS_SYSTEM_UP|slot 2: .*HOPOS_APPNET_UP|slot 2: stopped.*HOPOS_SLOT_STOPPED"
fi

echo "== booten op QEMU virt (tot ${TIMEOUT}s)"
# shellcheck disable=SC2086
qemu_virt 4 -nographic $QGUI $QUSB \
	-drive "if=none,format=raw,file=$DISK,id=disk0" \
	-device "loader,file=$ART/hop.elf,addr=0xb0200000,force-raw=on" \
	-device "loader,addr=0xb0100000,data=$HOP_SIZE,data-len=8" \
	-device "loader,addr=0xb0100008,data=1,data-len=8" \
	-kernel "$KERNEL" </dev/null >"$LOG" 2>&1 &
QPID=$!

# De monitor: elk argument is één commando. `shot` is een screendump naar
# $SHOT; de rest gaat zoals hij staat naar QEMU.
monitor() {
	python3 - "$MON" "$SHOT" "$@" <<'PY'
import socket, sys, time
mon, shot, cmds = sys.argv[1], sys.argv[2], sys.argv[3:]
s = socket.socket(socket.AF_UNIX)
s.connect(mon)
s.settimeout(2)
try:
    s.recv(4096)  # de banner
except OSError:
    pass
for c in cmds:
    if c == "shot":
        c = f"screendump {shot}"
    s.sendall((c + "\n").encode())
    time.sleep(1 if c.startswith("screendump") else 0.3)
PY
}

# Telt de pixels van de screendump in $SHOT: `console` (de console van de
# kern: wit op 0x101828) of `app` (de display-app: 0x1e3a2f met tekst in
# 0xf0e68c en de cursor in 0xff5030, en geen console-achtergrond).
shot_check() {
	python3 - "$SHOT" "$1" <<'PY'
import sys
from collections import Counter
data = open(sys.argv[1], "rb").read()
mode = sys.argv[2]
# P6: magic, breedte hoogte, maxval, dan RGB.
parts = data.split(maxsplit=4)
w, h = int(parts[1]), int(parts[2])
px = parts[4]
c = Counter(px[i:i + 3] for i in range(0, min(len(px), w * h * 3), 3))
if mode == "console":
    fg, bg = c[b"\xff\xff\xff"], c[b"\x10\x18\x28"]
    ok = fg > 500 and bg > w * h // 2
    print(f"{'ok' if ok else 'ROOD'} console {w}x{h}, {fg} text pixels, {bg} background pixels")
else:
    bg, fg, cur, con = c[b"\x1e\x3a\x2f"], c[b"\xf0\xe6\x8c"], c[b"\xff\x50\x30"], c[b"\x10\x18\x28"]
    ok = bg > w * h // 2 and fg > 500 and cur > 10 and con == 0
    print(f"{'ok' if ok else 'ROOD'} app {w}x{h}, {bg} background, {fg} text, {cur} cursor, {con} console pixels")
PY
}

# Wachten tot alle markers er zijn en alle toetsen van buiten gedaan,
# iets roods verschijnt, QEMU stopt, of de tijd op is.
NEED="HOPOS_BOOT|HOPOS_TICK 3|HOPOS_NIC_UP|HOPOS_NET_UP|HOPOS_SYSTEM_UP|HOPOS_SYSTEM_REFUSED|HOPOS_WD_CANARY_OK"
# Hop staat, dan de job naar zijn leader.
BOOT_MARKS="HOPOS_SYSTEM_UP|slot 1: .*HOP_LEADER|slot 1: .*HOP_UP"
NEED="$NEED|$BOOT_MARKS"
POSTED=""
# Elk patroon van $TWICE staat er twee keer (de herstart door Hop)?
twice() {
	IFS_WAS="$IFS"
	IFS='|'
	for m in $TWICE; do
		[ "$(count "$m")" -ge 2 ] || {
			IFS="$IFS_WAS"
			return 1
		}
	done
	IFS="$IFS_WAS"
}
ACTS=""
next_act=1
nacts=0
[ -n "$ACT_AT" ] && nacts=$(echo "$ACT_AT" | awk -F'|' '{print NF}')
elapsed=0
while :; do
	# De job naar de leader zodra Hop staat.
	if [ -z "$POSTED" ] && all "$BOOT_MARKS"; then
		post_job "$JOB" || break
	fi
	# De acties op de monitor, elk op zijn moment.
	if [ "$next_act" -le "$nacts" ]; then
		act=$(echo "$ACT_AT" | cut -d'|' -f"$next_act")
		at="${act%%;*}"
		seen=0
		case "$at" in
		2\**) [ "$(count "${at#2?}")" -ge 2 ] && seen=1 ;;
		*) has "$at" && seen=1 ;;
		esac
		if [ "$seen" = 1 ]; then
			cmds="${act#*;}"
			IFS_WAS="$IFS"
			IFS=';'
			# shellcheck disable=SC2086
			set -- $cmds
			IFS="$IFS_WAS"
			if monitor "$@"; then
				ACTS="$ACTS
   ok  monitor na '$at': $cmds"
			else
				ACTS="$ACTS
   ROOD monitor na '$at': $cmds"
			fi
			case ";$cmds;" in
			*";shot;"*)
				ACTS="$ACTS
   screendump na '$at': $(shot_check app || echo ROOD unreadable)"
				if [ -n "${SHOT_KEEP:-}" ]; then cp "$SHOT" "$SHOT_KEEP.$next_act.ppm"; fi
				;;
			esac
			next_act=$((next_act + 1))
			continue
		fi
	fi
	# De toets van buiten, op zijn moment; de keten loopt intussen door.
	probe_due && continue
	ok=1
	all "$NEED|$SLOT_MARKS" || ok=0
	twice || ok=0
	probes_left && ok=0
	[ "$next_act" -le "$nacts" ] && ok=0
	[ "$ok" = 1 ] && break
	has "HOPOS_PANIC|HOPOS_EXCEPTION" && break
	kill -0 "$QPID" 2>/dev/null || break
	[ "$elapsed" -ge "$((TIMEOUT * 10))" ] && break
	sleep 0.1
	elapsed=$((elapsed + 1))
done
# De gui-smaak: wachten op de console op het glas, dan een screendump via
# de monitor terwijl QEMU nog draait (het meetinstrument van 19-07).
SHOT_OK=""
if [ "$GUI" != 0 ]; then
	i=0
	while ! has "HOPOS_FB_CONSOLE" && [ "$i" -lt 50 ] && kill -0 "$QPID" 2>/dev/null; do
		sleep 0.1
		i=$((i + 1))
	done
	sleep 1 # een tik van de meetregels (of de klok van de app) op het glas
	# Met GUI=display houdt de app in slot 2 het glas: daar hoort hij te
	# staan, niet de console.
	SHOT_MODE=console
	[ "$GUI" = display ] && SHOT_MODE=app
	if monitor shot; then
		SHOT_OK="$(shot_check "$SHOT_MODE")" || SHOT_OK="ROOD screendump unreadable"
	else
		SHOT_OK="ROOD screendump failed"
	fi
	[ -n "${SHOT_KEEP:-}" ] && cp "$SHOT" "$SHOT_KEEP"
fi
qemu_stop

fail=0
marks "$NEED" "$SLOT_MARKS"
if twice; then
	echo "   ok  de tweede ronde (de herstart door Hop): $TWICE"
else
	echo "   ROOD de tweede ronde ontbreekt: $TWICE"
	fail=1
fi
case "$POSTED" in
*" HTTP 2"*) echo "   ok  POST /v1/jobs:$POSTED" ;;
*)
	echo "   ROOD POST /v1/jobs: ${POSTED:-nooit gedaan}"
	fail=1
	;;
esac
probes_report
[ -n "$ACTS" ] && echo "${ACTS#?}"
case "$ACTS" in
*ROOD*) fail=1 ;;
esac
if [ "$next_act" -le "$nacts" ]; then
	echo "   ROOD monitor: $((nacts - next_act + 1)) van de $nacts acties nooit gedaan (moment niet gezien)"
	fail=1
fi
if [ "$GUI" != 0 ]; then
	marks "HOPOS_FB_UP|HOPOS_FB_CONSOLE"
	echo "   screendump: $SHOT_OK"
	case "$SHOT_OK" in ok*) ;; *) fail=1 ;; esac
fi
if has "HOPOS_PANIC|HOPOS_EXCEPTION"; then
	echo "   ROOD panic of exception"
	fail=1
fi
verdict qemu
echo "qemu-poort groen"
