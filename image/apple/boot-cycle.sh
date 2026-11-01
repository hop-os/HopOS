#!/bin/sh
# boot-cycle.sh — één volledige meetcyclus op de Mac mini: herstart in
# debugusb-mode (macvdmtool, vereist sudo zonder wachtwoord voor precies dat
# binary), wacht op de dockchannel-poort, laad het image via load.py en
# vang de console N seconden op.
#
#   image/apple/boot-cycle.sh target/apple-m4/hopos-apple.img [seconden] [logbestand]
#
# MODE=split (default): `reboot serial` → laden over m1n1's USB-gadget (~9MB/s)
#   → `debugusb` → booten én console over de dockchannel (kis ch-0). Dit is de
#   volgorde die de kis-poorten betrouwbaar laat verschijnen (gemeten 28-08:
#   `reboot debugusb` en de toggle serial→debugusb lieten ze weg).
# MODE=debugusb: alles over de dockchannel (traag: ~10KB/s laden).
# MODE=serial: proxy over m1n1's USB-gadget (usbmodem, snel), console op
# /dev/cu.debug-console @1500000 (uart0) — de terugval als de kis-driver van
# de laptop na een reboot niet meer enumereert (gemeten 28-08: dan helpt
# alleen een kabel-herplug).
#
# STAGE=pad: het image van Hop in de staging (image/apple/load.py).
#
# Omgeving: MACVDMTOOL (default ~/Git/macvdmtool/macvdmtool), PYTHON (de venv
# met pyserial+construct), M1N1 (zie load.py).
set -u
IMG="$1"; SECS="${2:-90}"; LOG="${3:-/tmp/boot-$(date +%H%M%S).log}"
# De root-eigendom kopie (sudoers wijst daarop; een binary in $HOME zou
# effectief wachtwoordloos root zijn) — anders de build in ~/Git.
MACVDMTOOL="${MACVDMTOOL:-$([ -x /usr/local/sbin/macvdmtool ] && echo /usr/local/sbin/macvdmtool || echo $HOME/Git/macvdmtool/macvdmtool)}"
PYTHON="${PYTHON:-python3}"
MODE="${MODE:-split}"
DIR="$(cd "$(dirname "$0")/../.." && pwd)"

wait_kis() {
	i=0
	until [ -e /dev/cu.kis-100000-ch-0 ]; do
		i=$((i+1)); [ $i -ge 40 ] && { echo "== kis niet verschenen na 40s; nog een keer debugusb"; sudo -n "$MACVDMTOOL" debugusb >/dev/null; i=0; ATTEMPT=$((${ATTEMPT:-0}+1)); [ $ATTEMPT -ge 3 ] && { echo "== kis blijft weg: kabel aan de laptopkant herpluggen"; return 1; }; }
		sleep 1
	done
	echo "== kis na ${i}s"
}

if [ "$MODE" = split ]; then
	echo "== reboot serial"
	sudo -n "$MACVDMTOOL" reboot serial || { echo "macvdmtool: sudo zonder wachtwoord ontbreekt"; exit 2; }
	i=0
	until ls /dev/cu.usbmodem*1 >/dev/null 2>&1; do
		i=$((i+1)); [ $i -ge 120 ] && { echo "== geen usbmodem na 120s"; exit 3; }; sleep 1
	done
	DEV="$(ls /dev/cu.usbmodem*1 | head -1)"
	echo "== $DEV na ${i}s; m1n1 laten opkomen"; sleep 6
	cd "$DIR"
	echo "== laden over $DEV"
	PHASE=load M1N1TIMEOUT=6 M1N1DEVICE="$DEV" "$PYTHON" image/apple/load.py "$IMG" 2>&1 | tee "$LOG" | grep -v "^TTY> " | grep -v "^  cpu" | cut -c1-300
	echo "== debugusb"
	sudo -n "$MACVDMTOOL" debugusb >/dev/null
	wait_kis || exit 4
	sleep 2
	echo "== booten + console over kis → $LOG"
	PHASE=boot M1N1TIMEOUT=6 CONSOLE=proxy CONSOLE_SECONDS="$SECS" M1N1DEVICE=/dev/cu.kis-100000-ch-0 \
		"$PYTHON" image/apple/load.py "$IMG" 2>&1 | tee -a "$LOG" | grep -v "^TTY> " | cut -c1-300
	exit 0
fi

echo "== reboot $MODE"
sudo -n "$MACVDMTOOL" reboot "$MODE" || { echo "macvdmtool: sudo zonder wachtwoord ontbreekt"; exit 2; }
if [ "$MODE" = serial ]; then
	i=0
	until ls /dev/cu.usbmodem*1 >/dev/null 2>&1; do
		i=$((i+1)); [ $i -ge 120 ] && { echo "== geen usbmodem na 120s"; exit 3; }; sleep 1
	done
	DEV="$(ls /dev/cu.usbmodem*1 | head -1)"; CONSOLE=/dev/cu.debug-console
else
	DEV=/dev/cu.kis-100000-ch-0; CONSOLE=proxy
	i=0
	until [ -e "$DEV" ]; do
		i=$((i+1)); [ $i -ge 90 ] && { echo "== $DEV niet verschenen na 90s; nog een keer debugusb"; sudo -n "$MACVDMTOOL" debugusb; i=0; ATTEMPT=$((${ATTEMPT:-0}+1)); [ $ATTEMPT -ge 3 ] && exit 3; }
		sleep 1
	done
fi
echo "== $DEV na ${i}s; m1n1 laten opkomen"
sleep 6
echo "== laden + booten → $LOG (console $CONSOLE)"
cd "$DIR"
M1N1TIMEOUT=6 CONSOLE="$CONSOLE" CONSOLE_SECONDS="$SECS" M1N1DEVICE="$DEV" \
	"$PYTHON" image/apple/load.py "$IMG" 2>&1 | tee "$LOG" | grep -v "^TTY> " | grep -v "^  cpu" | cut -c1-300
