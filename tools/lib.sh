# De hulpfuncties van de QEMU-toetsen in tools/ en van de image-scripts,
# één keer. Een script zet $DIR (de werkboom) en leest ze dan in met
# `. "$DIR/tools/lib.sh"`, vóór zijn `cd "$DIR"`. De functies rekenen op
# $LOG (de console), en waar ze het zeggen op $TARGET, $ART, $QPID, $RED,
# $TIMEOUT en de poorten.

HOP_DIR="${HOP_DIR:-$DIR/../hop/hop}"

# De apps van deze werkruimte, één lijst: tools/gate.sh bouwt ze,
# tools/release.sh levert ze uit als <app>-<arch>.elf (de rollende release
# `apps`, jobs/README.md), met decode (het media-vlak, apps/decode) erbij;
# voor riscv64 alleen APPS_RISCV64.
APPS="appspike welcome bench display vitals syncprobe"
APPS_RISCV64="appspike welcome"

# De staging van virt, virt-riscv en de Pi's (board/*/src/slots.rs
# STAGE_MAX): 14 MiB, het image rauw met zijn debug-info eraf.
STAGE_MAX=14680064

# --- Poorten

# Een host-poort: de gevraagde als hij vrij is, anders een vrije van het OS
# (met een regel op stderr), zodat een toets naast een andere QEMU of
# server draait. `port 0` is altijd een vrije van het OS.
port() {
	python3 - "$1" "${2:-}" <<'PY'
import socket, sys
want, name = int(sys.argv[1]), sys.argv[2]
s = socket.socket()
try:
    s.bind(("127.0.0.1", want))
    print(s.getsockname()[1])
except OSError:
    s.close()
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    got = s.getsockname()[1]
    print(f"   {name} {want} is taken, using {got}", file=sys.stderr)
    print(got)
s.close()
PY
}

# ports NAAM...: de host-poorten van een toets (SYS, AGENT, LEADER, ART,
# WEB, S3, NTP, ECHO worden $SYSPORT, $AGENTPORT, ...). Zonder waarde uit de
# omgeving een vrije van het OS: zo botsen twee toetsen, of twee runners
# (tools/qemu-all.sh), nooit op een vaste poort; met een waarde die, en is
# hij bezet dan een vrije, luid gemeld. $QFWD krijgt de hostfwd's van QEMU's user-net
# voor de poorten met een kant in de gast: SYS :10100 (de system-API),
# AGENT :8080 en LEADER :9080 (Hop), WEB :80.
ports() {
	for p in "$@"; do
		eval "v=\${${p}PORT:-0}"
		v="$(port "$v" "${p}PORT")"
		eval "${p}PORT=$v"
		case "$p" in
		SYS) g=10100 ;;
		AGENT) g=8080 ;;
		LEADER) g=9080 ;;
		WEB) g=80 ;;
		*) continue ;;
		esac
		QFWD="${QFWD:+$QFWD,}hostfwd=tcp:127.0.0.1:$v-:$g"
	done
}

# --- Werk en opruimen

QPID=""
PIDS=""

# scratch NAAM: de console $LOG, een werkmap $ART met de plek van de schijf
# ($DISK), een trap die bij elke uitgang QEMU ($QPID), de helpers op de
# achtergrond ($PIDS) en beide opruimt, en de SNTP-server van de host
# (sntp, $NTPPORT).
scratch() {
	LOG="$(mktemp -t "hopos-$1.XXXXXX")"
	ART="$(mktemp -d -t "hopos-$1-art.XXXXXX")"
	DISK="$ART/disk.img"
	trap cleanup EXIT INT TERM
	sntp
}
cleanup() {
	[ -n "$QPID" ] && kill "$QPID" 2>/dev/null
	[ -z "${KEEP_ALL:-}" ] || cp "$LOG" "$KEEP_ALL/$(date +%s)-$(basename "$LOG")" 2>/dev/null
	for p in $PIDS; do kill "$p" 2>/dev/null; done
	rm -rf "$LOG" "$ART"
	true
}

# qemu_stop: QEMU weg, en gewacht tot hij weg is.
qemu_stop() {
	kill "$QPID" 2>/dev/null || true
	wait "$QPID" 2>/dev/null || true
	QPID=""
}

# --- Bouwen en serveren

# find_objcopy: rust-objcopy van de toolchain in $OBJCOPY (leeg als hij er
# niet is). Na de `cd "$DIR"`: de toolchain is die van de werkboom.
find_objcopy() {
	[ -n "${OBJCOPY+x}" ] || OBJCOPY="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/rust-objcopy 2>/dev/null | head -1)"
}

# need_objcopy NAAM: zonder rust-objcopy stopt NAAM.
need_objcopy() {
	find_objcopy
	[ -n "$OBJCOPY" ] || {
		echo "$1: rust-objcopy missing (rustup component add llvm-tools)" >&2
		exit 1
	}
}

# strip_elf IN UIT: de ELF zonder debug-info (de release-profielen dragen
# `debug = true` voor de zwarte doos); de symbolen blijven, want de
# plaatsing leest __hopos_* uit de symbooltabel. Zonder rust-objcopy een
# kopie.
strip_elf() {
	find_objcopy
	if [ -n "$OBJCOPY" ]; then "$OBJCOPY" --strip-debug "$1" "$2"; else cp "$1" "$2"; fi
}

# fits ELF NAAM: past het image op de staging? Anders FAIL en stop. $SIZE
# wordt zijn maat.
fits() {
	SIZE=$(wc -c <"$1" | tr -d ' ')
	[ "$SIZE" -le "$STAGE_MAX" ] || {
		echo "FAIL: $2 is $SIZE bytes, the staging holds $STAGE_MAX" >&2
		exit 1
	}
}

# apps APP...: de apps voor $TARGET (één cargo-aanroep), zonder debug-info
# in $ART/<app>.elf.
apps() {
	cargo build --quiet --release --target "$TARGET" $(printf ' -p %s' "$@")
	for a in "$@"; do strip_elf "$DIR/target/$TARGET/release/$a" "$ART/$a.elf"; done
}

# hop_elf: Hop (agentd-hopos uit $HOP_DIR) voor $TARGET, tegen de applib
# van deze werkboom (tools/hop-build.sh); de ELF in $HOP_ELF.
hop_elf() {
	HOP_ELF="$(HOP_DIR="$HOP_DIR" sh "$DIR/tools/hop-build.sh" "$TARGET")"
}

# pick_app STANDAARD: het image voor de staging volgens $APP (zonder APP:
# STANDAARD), zoals elk image-script het kiest. Leeg is geen image; `hop` is
# Hop (hop_elf: tegen de applib van deze werkboom, want de stage-1 en de
# vectortabel van applib moeten in Hop zitten, de eerste Pi 5-boot van
# 30-09; HOP_PATCH=0 bouwt de tag van de hop-repo); een pad is die ELF;
# anders een app uit deze werkruimte. Zet $IMAGE (leeg = geen) en $ROLE
# (zonder ROLE: hop, behalve voor een app uit deze werkruimte). De kern
# plaatst alleen Hop (01-11, O7): een image met de rol app blijft liggen.
pick_app() {
	APP="${APP-$1}"
	IMAGE=""
	case "$APP" in
	"") ;;
	hop)
		hop_elf
		IMAGE="$HOP_ELF"
		ROLE="${ROLE:-hop}"
		;;
	*/*)
		IMAGE="$APP"
		ROLE="${ROLE:-hop}"
		;;
	*)
		(cd "$DIR" && cargo build --quiet --release --target "$TARGET" -p "$APP")
		IMAGE="$DIR/target/$TARGET/release/$APP"
		ROLE="${ROLE:-app}"
		;;
	esac
}

# serve: de artifact-server over $ART op 127.0.0.1:$ARTPORT (voor de gast
# http://10.0.2.2:$ARTPORT/), zijn log in $ART/http.log.
serve() {
	(cd "$ART" && exec python3 -m http.server "$ARTPORT" --bind 127.0.0.1) >"$ART/http.log" 2>&1 &
	PIDS="$PIDS $!"
}

# sntp: de tijd van de host als SNTP-server op 127.0.0.1:$NTPPORT/udp, voor
# de gast hopos.ntp=10.0.2.2:$NTPPORT: Hop zet zijn klok bij de start
# verplicht (lukt de eerste meting niet, dan exit en een reset), en een ring
# hangt zo niet aan pool.ntp.org. Elke ring krijgt hem in scratch, op
# $NTPPORT uit de omgeving of een vrije (port), en exporteert die; de
# QEMU-starts (qemu_virt en qemu_rv via bootargs, image/uefi-run.sh in de
# config) geven hem de gast mee, vóór een eigen hopos.ntp van de ring, dus
# die wint. image/qemu-run.sh en uefi-run.sh nemen een gezette $NTPPORT als
# een server die er al is (die van de ring), en starten anders hun eigen
# (los, of uit tools/qemu-test-sync.py). Modus 4, stratum 2,
# het transmit-veld van de vraag als originate (dat toetst Hop), zoals
# hop/tools/qemu-test-cluster.sh; zijn log in $ART/ntp.log. Hij stopt met
# $PIDS (cleanup), en anders met het proces dat hem startte: in
# qemu-run.sh doet dat exec, dus dan is het QEMU.
sntp() {
	NTPPORT="$(port "${NTPPORT:-0}" NTPPORT)"
	export NTPPORT
	ntplog=/dev/null
	[ -z "${ART:-}" ] || ntplog="$ART/ntp.log"
	python3 - "$NTPPORT" >"$ntplog" 2>&1 <<'PY' &
import os, socket, struct, sys, time
parent = os.getppid()
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.settimeout(0.5)
print("ntp up", flush=True)
def stamp(t):
    t += 2208988800
    return struct.pack("!II", int(t), int((t % 1) * (1 << 32)))
while os.getppid() == parent:
    try:
        req, addr = s.recvfrom(512)
    except socket.timeout:
        continue
    if len(req) < 48:
        continue
    now = time.time()
    resp = bytes([0x24, 2, 6, 0xEC]) + bytes(8) + b"LOCL" + stamp(now) + req[40:48] + stamp(now) + stamp(time.time())
    s.sendto(resp, addr)
    print("ntp answered", addr, flush=True)
PY
	PIDS="$PIDS $!"
}

# bootargs: de bootparameters voor -append (qemu_virt, qemu_rv): de
# SNTP-server van sntp, dan $BOOTARGS; de laatste waarde wint
# (kern::nodecfg), dus een hopos.ntp in BOOTARGS gaat voor.
bootargs() {
	printf '%s' "${NTPPORT:+hopos.ntp=10.0.2.2:$NTPPORT${BOOTARGS:+ }}${BOOTARGS:-}"
}

# --- QEMU

# hop_virt [VAR=waarde...] [-- QEMU-argumenten]: image/qemu-run.sh op de
# achtergrond, met Hop ($HOP_ELF) in slot 1, de poorten en de schijf van de
# toets, en de VAR=waarde-paren erachter (SMP, BOOTARGS, DISKLAT, CFG); de
# console in $LOG, QEMU
# in $QPID (qemu-run.sh doet exec).
hop_virt() {
	(
		export SYSPORT="$SYSPORT" AGENTPORT="$AGENTPORT" LEADERPORT="$LEADERPORT" WEBPORT="${WEBPORT:-}" \
			HOP_DIR="$HOP_DIR" APP="${HOP_ELF:-}" ROLE=1 DISK="$DISK"
		while [ $# -gt 0 ] && [ "$1" != -- ]; do
			export "$1"
			shift
		done
		[ $# -eq 0 ] || shift
		exec sh "$DIR/image/qemu-run.sh" "$@"
	) </dev/null >"$LOG" 2>&1 &
	QPID=$!
}

# qemu_virt SMP [QEMU-argumenten]: QEMU -M virt met EL2 (HopOS eist een
# EL2-boot: de stage-2-kooi is een invariant), GICv3, cortex-a53 en 3 GiB
# (het PA-plan van virt legt de slot-pool tot voorbij 0xC000_0000), de
# console op stdio; virtio-net modern (force-legacy=false: transportversie
# 2) op de mmio-bus (virt zet hem anders op PCIe) met $QFWD, en virtio-blk
# op de mmio-bus achter drive disk0 (die geeft de aanroeper: -drive of
# -blockdev), net als het scherm, de monitor en de kern; altijd de
# i6300esb met een reset als hij afloopt (O2 van 01-11: de watchdog is
# verplicht, ook op QEMU). De regel van image/qemu-run.sh. Doet exec. $QNETDEV gaat achter de virtio-net (de
# trage draad van WIRE=1), $QNETOBJ is een extra -object (de vertraging).
# De bootparameters (bootargs: de SNTP-server en $BOOTARGS) in één
# -append: QEMU legt ze in /chosen/bootargs van de DTB, en een tweede
# -append zou de eerste vervangen.
qemu_virt() {
	smp="$1"
	shift
	args="$(bootargs)"
	[ -z "$args" ] || set -- -append "$args" "$@"
	# shellcheck disable=SC2086 # QNETOBJ is een lijst argumenten
	exec qemu-system-aarch64 -M virt,gic-version=3,highmem-ecam=off,virtualization=on \
		-cpu cortex-a53 -smp "$smp" -m 3G -serial stdio \
		-global virtio-mmio.force-legacy=false \
		-device "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.0${QNETDEV:+,$QNETDEV}" \
		-netdev "user,id=n0,$QFWD" ${QNETOBJ:-} \
		-device virtio-blk-device,drive=disk0,bus=virtio-mmio-bus.1 \
		-device i6300esb -action watchdog=reset \
		"$@"
}

# qemu_rv IMAGE ROL [QEMU-argumenten]: QEMU virt riscv64 in machine mode
# (twee harts, -bios none) met de kern $KERNEL, virtio-net met $QFWD en
# virtio-blk op $DISK. IMAGE (leeg = geen) rauw op de staging
# (board/qemuvirt-riscv/src/slots.rs: het image op STAGE_PA, zijn maat op
# STAGE_HDR_PA, de rol op STAGE_ROLE_PA: 0 = app, 1 = Hop). Doet exec: op
# de achtergrond aanroepen.
qemu_rv() {
	img="$1"
	role="$2"
	shift 2
	if [ -n "$img" ]; then
		set -- -device "loader,file=$img,addr=0xa8200000,force-raw=on" \
			-device "loader,addr=0xa8100000,data=$(wc -c <"$img" | tr -d ' '),data-len=8" \
			-device "loader,addr=0xa8100008,data=$role,data-len=8" "$@"
	fi
	# De bootparameters (bootargs) in /chosen/bootargs van de DTB, zoals
	# qemu_virt op arm64 (het board leest ze als config-laag).
	args="$(bootargs)"
	[ -z "$args" ] || set -- -append "$args" "$@"
	exec qemu-system-riscv64 -M virt -m 1G -smp 2 -bios none -nographic \
		-kernel "$KERNEL" \
		-global virtio-mmio.force-legacy=false \
		-netdev "user,id=n0,$QFWD" -device virtio-net-device,netdev=n0 \
		-drive "file=$DISK,if=none,format=raw,id=d0" -device virtio-blk-device,drive=d0 \
		"$@"
}

# qemu_edk2 CPU VARS ESP [QEMU-argumenten]: QEMU virt met EDK2 (EL2,
# GICv3, vier cores, 3 GiB, of $SMP en $MEM), de firmware-variabelen in
# VARS en de ESP als USB-stick (bootindex=0, zodat de BDS niet eerst PXE
# probeert); NIC en schijf geeft de aanroeper mee. De regel van
# image/uefi-run.sh. Doet exec.
qemu_edk2() {
	cpu="$1"
	vars="$2"
	esp="$3"
	shift 3
	exec qemu-system-aarch64 -M virt,gic-version=3,virtualization=on \
		-cpu "$cpu" -smp "${SMP:-4}" -m "${MEM:-3G}" \
		-nographic -monitor none -serial stdio \
		-drive "if=pflash,format=raw,readonly=on,file=${QEMU_SHARE:-/opt/homebrew/share/qemu}/edk2-aarch64-code.fd" \
		-drive "if=pflash,format=raw,file=$vars" \
		-device qemu-xhci \
		-drive "file=fat:$esp,format=raw,if=none,id=esp,readonly=on" \
		-device usb-storage,drive=esp,bootindex=0 \
		"$@"
}

# --- De console

# Staat het patroon (grep -E) op de console in $LOG?
has() { tr -d '\r' <"$LOG" | grep -q -E "$1"; }

# Hoe vaak staat het patroon op de console?
count() { tr -d '\r' <"$LOG" | grep -c -E "$1" || true; }

# Staan alle patronen van een |-lijst op de console?
all() {
	(
		IFS='|'
		for m in $1; do has "$m" || exit 1; done
	)
}

# De eerste regel van de console met het patroon.
first() { tr -d '\r' <"$LOG" | grep -m1 -E "$1"; }

# --- De wachtlus

# started: de klok van de toets, vanaf nu ($START; $elapsed in seconden).
started() {
	START=$(date +%s)
	elapsed=0
}
step() {
	sleep 0.2
	elapsed=$(($(date +%s) - START))
}
# alive: niets uit $RED op de console, QEMU leeft, en $TIMEOUT is niet om.
alive() {
	! has "$RED" && kill -0 "$QPID" 2>/dev/null && [ "$elapsed" -lt "$TIMEOUT" ]
}

# post_job JSON: de jobspec naar de leader van Hop (via de hostfwd en de
# DNAT van de kern). $POSTED wordt het antwoord met " HTTP nnn" erachter;
# faalt curl, dan "ROOD curl: ..." en faalt de functie. $POST_MAX is de
# tijd van curl (standaard 20 s).
post_job() {
	if out="$(curl -s -m "${POST_MAX:-20}" -w ' HTTP %{http_code}' -X POST \
		-H 'Content-Type: application/json' -d "$1" \
		"http://127.0.0.1:$LEADERPORT/v1/jobs" 2>&1)"; then
		POSTED="$out"
	else
		POSTED="ROOD curl: $out"
		return 1
	fi
}

# job_loop JSON KLAAR: wacht op $BOOT_MARKS, POST dan de job, en wacht tot
# de functie KLAAR slaagt; of tot alive faalt, of de POST.
job_loop() {
	POSTED=""
	started
	while alive; do
		if [ -z "$POSTED" ]; then
			if all "$BOOT_MARKS"; then post_job "$1" || break; fi
		elif "$2"; then
			break
		fi
		step
	done
}

# running NAAM: GET /tasks van Hop toont een taak van job NAAM als running
# ($TASKS wordt het antwoord, met "(nog niet running)" ervoor zolang niet).
# Een exit bestaat daar niet als staat: Hop herstart een service, dus de
# toets pollt.
running() {
	t="$(curl -s -m 5 "http://127.0.0.1:$AGENTPORT/tasks" 2>&1 || true)"
	if printf '%s' "$t" | python3 -c '
import json, sys
tasks = json.load(sys.stdin)
sys.exit(0 if any(t.get("job_name") == sys.argv[1] and t.get("state") == "running" for t in tasks) else 1)
' "$1" 2>/dev/null; then
		TASKS="$t"
		return 0
	fi
	TASKS="(nog niet running) $t"
	return 1
}

# --- De toets van buiten (de system-API via de hostfwd)

# probe: verbinden via de hostfwd en wachten tot de kern de verbinding
# sluit (EOF). Slaagt alleen als de gast antwoordt.
probe() {
	python3 - "$SYSPORT" <<'PY'
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
s.settimeout(5)
n = 0
while True:
    b = s.recv(4096)
    if not b:
        break
    n += len(b)
print(f"connected to 127.0.0.1:{sys.argv[1]}, closed by the kernel after {n} bytes")
PY
}

refusals() { grep -c "HOPOS_SYSTEM_REFUSED" "$LOG" || true; }

# probe_at MOMENT: één toets van buiten: EOF van de host én een nieuwe
# weigeringsregel van de kern (tot 3 s na de EOF, want de console loopt via
# QEMU's stdio iets achter).
probe_at() {
	before=$(refusals)
	if out="$(probe 2>&1)"; then
		i=0
		while [ "$(refusals)" -le "$before" ] && [ "$i" -lt 30 ]; do
			sleep 0.1
			i=$((i + 1))
		done
		if [ "$(refusals)" -gt "$before" ]; then
			echo "ok  extern na '$1': $out"
		else
			echo "ROOD extern na '$1': EOF maar geen nieuwe HOPOS_SYSTEM_REFUSED"
		fi
	else
		echo "ROOD extern na '$1': $(echo "$out" | tail -1)"
	fi
}

# De toetsen van buiten op hun momenten ($PROBE_AT, een |-lijst van grep
# -E-patronen, in volgorde), de keten loopt intussen door. probe_due doet
# de volgende als zijn moment er is (en slaagt dan); probes_left zegt of er
# nog een wacht; probes_report meldt ze.
PROBES=""
next_probe=1
probe_due() {
	[ -n "${PROBE_AT:-}" ] && probes_left || return 1
	at=$(echo "$PROBE_AT" | cut -d'|' -f"$next_probe")
	has "$at" || return 1
	PROBES="$PROBES
   $(probe_at "$at")"
	next_probe=$((next_probe + 1))
}
probes_left() {
	[ -n "${PROBE_AT:-}" ] && [ "$next_probe" -le "$(echo "$PROBE_AT" | awk -F'|' '{print NF}')" ]
}
probes_report() {
	if [ -n "$PROBES" ]; then echo "${PROBES#?}"; fi
	case "$PROBES" in
	*ROOD*) fail=1 ;;
	esac
	if probes_left; then
		n=$(echo "$PROBE_AT" | awk -F'|' '{print NF}')
		echo "   ROOD extern: $((n - next_probe + 1)) van de $n toetsen nooit geprobeerd (moment niet gezien)"
		fail=1
	fi
}

# --- Het rapport (fail=1 is rood)

# marks LIJST...: per patroon van de |-lijsten "ok  patroon: de eerste
# regel" of "ROOD patroon ontbreekt".
marks() {
	IFS_WAS="$IFS"
	IFS='|'
	for m in $*; do
		if has "$m"; then
			echo "   ok  $m: $(first "$m")"
		else
			echo "   ROOD $m ontbreekt"
			fail=1
		fi
	done
	IFS="$IFS_WAS"
}

# posted: de POST van post_job.
posted() {
	case "$POSTED" in
	*"HTTP 2"*) echo "   ok  POST /v1/jobs: $POSTED" ;;
	"")
		echo "   ROOD POST /v1/jobs nooit gedaan (Hop niet op tijd op)"
		fail=1
		;;
	*)
		echo "   ROOD POST /v1/jobs: $POSTED"
		fail=1
		;;
	esac
}

# served NAAM: de artifact-server gaf NAAM uit.
served() {
	if grep -q "GET /$1" "$ART/http.log" 2>/dev/null; then
		echo "   ok  artifact-server: $(grep -c "GET /$1" "$ART/http.log") download(s) van $1"
	else
		echo "   ROOD artifact-server: nooit gevraagd"
		fail=1
	fi
}

# reds: iets uit $RED op de console.
reds() {
	if has "$RED"; then
		echo "   ROOD $(first "$RED")"
		fail=1
	fi
}

# took [WAT]: de tijd sinds started.
took() { echo "   tijd: $(($(date +%s) - START)) s na ${1:-de start van QEMU}"; }

# verdict NAAM [STAART]: rood bewaart de console (een eigen tijdelijk
# bestand), drukt hem af (of zijn laatste STAART regels) en stopt met 1;
# groen bewaart hem in $KEEP_LOG als die gezet is.
verdict() {
	if [ "$fail" != 0 ]; then
		KEEP="$(mktemp -t "hopos-$1-rood.XXXXXX")"
		tr -d '\r' <"$LOG" >"$KEEP"
		echo "== console bewaard in $KEEP"
		if [ -n "${2:-}" ]; then
			echo "== console (staart):"
			tail -"$2" "$KEEP"
		else
			echo "== console:"
			cat "$KEEP"
		fi
		exit 1
	fi
	if [ -n "${KEEP_LOG:-}" ]; then tr -d '\r' <"$LOG" >"$KEEP_LOG"; fi
}
