#!/bin/sh
# De kern-flip op QEMU: een draaiende node vervangt zijn kern zonder
# herstart, en Hop (slot 1) draait door.
#
# Kern A (stempel A) boot met Hop, zoals tools/qemu-test-hop.sh. Kern B is
# dezelfde bron met stempel B, gebouwd als flip-bundel
# (image/flip-bundle.sh: twee links, de relocatietabel uit het verschil).
# Van buiten gaat POST /flip naar Hop; Hop haalt de bundel van de
# artifact-server op de host (10.0.2.2), stroomt hem rauw de kern in en
# vraagt PrivOp::FLIP. Groen alleen als:
#
#   kern A      HOPOS_BOOT gen=1 stamp=A, Hop op (HOP_UP, de poorten door);
#   FLIPCONN    vóór de flip plaatst Hop een appspike met ROLE=FLIPCONN: één
#               UITGAANDE TCP-verbinding van het slot naar een echo op de
#               host (10.0.2.2, door de masquerade van de kern en de slirp
#               van QEMU), elke seconde een regel. De echo zet er een fase
#               voor (A, en B zodra de console HOPOS_FLIP_BOOT toont); groen
#               bij "HOPOS_APPSPIKE_FLIPCONN ok before=N after=M" met M > N:
#               de verbinding mag over de flip breken (de conntrack gaat
#               niet mee, 01-11), maar de app zit niet vast: hij krijgt een
#               fout van zijn stack (HOPOS_APPSPIKE_FLIPCONN_BROKE) en
#               verbindt opnieuw;
#   de flip     POST /flip geeft 202; HOPOS_FLIP_STAGED (som getoetst,
#               beeld gerelokeerd) en HOPOS_FLIP_JUMP gen=2;
#   kern B      HOPOS_BOOT gen=2 stamp=B, HOPOS_FLIP_BOOT gen=2,
#               "HOPOS_FLIP_ADOPT 2 of 2 resident(s)" (Hop en FLIPCONN),
#               HOPOS_HOP_RESUMED en HOPOS_FLIP_SETTLED (de guard: een
#               luisterend net binnen de gratie);
#   de lease    (warm en koud) kern B neemt de lease van kern A over
#               (HOPOS_DHCP_CARRIED met het adres) en doet geen DHCP: geen
#               HOPOS_DHCP_BOUND na HOPOS_FLIP_BOOT (H1, 01-11);
#   dezelfde Hop  precies één "HOP_BOOT" en één HOPOS_HOP_START in de hele
#               console (Hop is niet herstart, zijn kooi is geadopteerd), en
#               GET /tasks antwoordt na de flip;
#   werk        daarna een jobspec naar de leader: HOP_JOB_PLACED,
#               HOPOS_SLOT_START en "HOPOS_APPSPIKE_DONE pass=9 fail=0",
#               allemaal ná de landing: de nieuwe kern plaatst.
#   de som      HOPOS_FLIP_SWITCHCODE_OK: de switch-code van de bundel is die
#               van de bewoners, getoetst vóór de sprong;
#   de config   kern A draagt een config in het venster van zijn image
#               (board/src/cfgwin.rs; virt: CFG= van image/qemu-run.sh,
#               uefi: CFG= van image/uefi-run.sh, en dan staat er geen
#               hopos.cfg op de ESP), de bundel een leeg venster: kern A
#               geeft het zijne mee (HOPOS_FLIP_CFG vóór de sprong) en kern
#               B leest het (HOPOS_CFG_WINDOW na de landing; ook koud);
#   de groep    (virt, warm) kern A zet Hop in de sharegroup hop (een eigen
#               app-core), de bundel draagt een eigen venster met
#               hopos.hop.sharegroup=system (HOPOS_FLIP_CFG_OWN). Een warme
#               flip verplaatst Hop niet: kern B zegt "Hop in the sharegroup
#               hop (carried over the flip)" en in één regel dat de config
#               pas bij een koude start geldt (HOPOS_HOP_GROUP_COLD), en de
#               jobspec van daarna (met "sharegroup":"hop") landt op de core
#               van Hop;
#   hopfs       HOPOS_FS_FROZEN generation=N vóór de sprong, en na de landing
#               HOPOS_FS_UP fresh=0 generation=N: dezelfde generatie, dus de
#               staat van Hop overleeft via de schijf én (Hop draait door)
#               via de adoptie;
#   een verbinding  een TCP-verbinding van de host naar de agent-poort van
#               Hop, geopend vóór de sprong en pas ná de landing afgemaakt
#               (GET /tasks in twee helften). Mag breken; alleen gemeld.
#   de zwarte doos  (virt, warm) daarna een system_reset over QMP, zoals een
#               watchdog-reset: het RAM blijft, de ROM's (kern A, de staging)
#               komen terug. Kern A boot koud (HOPOS_BOOT gen=1 stamp=A) op
#               dezelfde console-ring (kern/src/conport.rs): "N bytes from
#               before this boot" HOPOS_CONSOLE_CARRIED met N > 0, en
#               tcp/5555 speelt de laatste HOPOS_APPSPIKE_DONE van kern B af
#               vóór de bootregel van kern A. De rest van de toets kijkt
#               alleen naar de console van vóór de reset.
#   eenmalig    (virt, warm) de reset van de O6N (04-10): daar bleef de
#               overdracht na de landing in DRAM staan, en de stickkern
#               adopteerde de bewoners van een flip die al geland was. Een
#               loader zet bij ELKE reset (ook bij de eerste boot) de handoff
#               in de feitenpagina op de boot-scratch, naast het echte blob
#               van de sprong. Groen alleen als kern A beide keren koud boot
#               (HOPOS_BOOT gen=1: x0 van de firmware is niet de pagina, dus
#               geen sprong, en de pagina komt er opnieuw), na de reset geen
#               HOPOS_FLIP_BOOT, HOPOS_FLIP_ADOPT, HOPOS_HOP_RESUMED of
#               HOPOS_HOP_EXIT zegt, en Hop weer opkomt (HOP_UP).
#
# Rood is ook: HOPOS_PANIC, HOPOS_EXCEPTION, een fout van Hop, en elke
# flip-weigering (HOPOS_FLIP_REFUSED, _FAIL, _BLOB_BAD,
# _ADOPT_FAIL, HOP_FLIP_FAIL). Rood bewaart de console en drukt hem af.
#
#   tools/qemu-test-flip.sh                 TIMEOUT=120 standaard, in seconden
#   BOARD=uefi tools/qemu-test-flip.sh      dezelfde flip onder EDK2: de kern als
#                                           BOOTAA64.EFI (image/uefi-run.sh), Hop
#                                           als hopos-stage.elf op de ESP, de
#                                           bundel van image/flip-bundle.sh uefi,
#                                           TIMEOUT=180; plus wat alleen daar kan
#                                           misgaan: de feitenpagina van de stub
#                                           (board/uefi/src/flip.rs), de
#                                           PIE-basis (docs/flip.md) en het
#                                           zaad over de flip (efi-rng via
#                                           virtio-rng, HOPOS_RNG_EFI_CARRIED)
#   MISMATCH=1 tools/qemu-test-flip.sh      de weigering: dezelfde bundel met een
#                                           andere switch-code-som (en dus een
#                                           andere sha256). Groen alleen als de
#                                           kern hem vóór de sprong weigert
#                                           ("HOPOS_FLIP_REFUSED switch code
#                                           mismatch"), Hop HOP_FLIP_FAIL meldt
#                                           en een 5xx geeft, kern A gewoon
#                                           doordraait (GET /tasks) en er geen
#                                           sprong, geen bevriezing en geen
#                                           landing op de console staat.
#   OSCORE=1 tools/qemu-test-flip.sh        de kern op core 1 (hopos.oscore): de
#                                           flip springt vanaf core 1, en de
#                                           nieuwe kern moet op een andere core
#                                           dan 0 door `_start` (cpu::boot
#                                           admits: x0 = de feitenpagina;
#                                           dezelfde ingang als de Pi's en de
#                                           Radxa). Samen te nemen
#                                           met BOARD=uefi (daar
#                                           in hopos.cfg op de ESP, de weg van
#                                           de O6N).
#   BOARD=rpi4 tools/qemu-test-flip.sh      de flip-INGANG van de Pi op QEMU
#                                           raspi4b (daar is geen net, dus geen
#                                           flip): kernel8.img van board-rpi4
#                                           boot drie keer. Koud op core 0 (de
#                                           DTB-plek); dan op core 1 met de
#                                           registers van de chain-trampoline
#                                           (x0 = de feitenpagina op de
#                                           boot-scratch 0x0F10_0080, met de
#                                           DTB erin en geen handoff, x1 tot x3
#                                           nul, core 0 in een WFE-lus): groen
#                                           als die core door _pi_start en
#                                           _start tot kmain komt en de DTB een
#                                           tweede keer leest, uit de pagina;
#                                           en dezelfde sprong met x0 = de DTB,
#                                           zoals de firmware: die moet na "P2"
#                                           parkeren (de koude-boot-poort).
#   KEEP_LOG=pad tools/qemu-test-flip.sh    bewaart ook een groene console (en
#                                           die van na de reset in pad.reset)
#   SYSPORT/AGENTPORT/LEADERPORT/ARTPORT/ECHOPORT  de host-poorten
#                                           (standaard vrije van het OS)
#   HOP_DIR=pad                             de hop-repo (standaard ../hop/hop)
#   FEATURES=vhe CPU=neoverse-n1 BOARD=uefi ...  de flip met de kern onder
#                                           E2H = 1 (image/uefi-run.sh en
#                                           image/flip-bundle.sh nemen FEATURES
#                                           mee; alleen BOARD=uefi kent CPU)
set -eu

DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "$(dirname "$0")/lib.sh"
BOARD="${BOARD:-virt}"
case "$BOARD" in
virt) TIMEOUT="${TIMEOUT:-120}" ;;
uefi) TIMEOUT="${TIMEOUT:-180}" ;;
rpi4) ;;
*)
	echo "BOARD=$BOARD: virt, uefi of rpi4" >&2
	exit 64
	;;
esac

# De ingangsproef van de Pi (zie boven): geen net, geen Hop, drie boots.
if [ "$BOARD" = rpi4 ]; then
	T=aarch64-unknown-none-softfloat
	W="$(mktemp -d -t hopos-flip-pi.XXXXXX)"
	trap 'rm -rf "$W"' EXIT INT TERM
	DTB="${DTB:-$DIR/image/firmware/rpi4/bcm2711-rpi-4-b.dtb}"
	cd "$DIR"
	cargo build --quiet --release --target "$T" -p hopos --features board-rpi4
	cargo build --quiet --release --target "$T" -p appspike
	need_objcopy qemu-test-flip
	strip_elf "$DIR/target/$T/release/appspike" "$W/app.elf"
	"$OBJCOPY" -O binary "$DIR/target/$T/release/hopos" "$W/kernel8.img"
	# De stub op core 1, met de hand gecodeerd (geen assembler nodig): van
	# EL3 (zo komt een core van raspi4b uit de reset als QEMU hem niet zelf
	# start) naar EL2, dan precies wat de trampoline van cpu::el2::chain de
	# nieuwe kern geeft: x0 = $2 (de feitenpagina, of de DTB als de
	# firmware), x1 tot x3 nul, en de sprong naar 0x80000 (_pi_start). Core
	# 0 krijgt een WFE-lus.
	stub() {
		python3 - "$1" "$2" <<'STUB'
import struct, sys
out, x0, mark = sys.argv[1], int(sys.argv[2], 16), 0
code = [
    0xd280a029,  # mov x9, #0x501          SCR_EL3: RW, HCE, NS
    0xd51e1109,  # msr scr_el3, x9
    0xd2807929,  # mov x9, #0x3c9          EL2h, DAIF dicht
    0xd51e4009,  # msr spsr_el3, x9
    0x10000069,  # adr x9, +0xc
    0xd51e4029,  # msr elr_el3, x9
    0xd69f03e0,  # eret
    0x580000e0,  # ldr x0, x0
    0xaa1f03e1,  # mov x1, xzr
    0xaa1f03e2,  # mov x2, xzr
    0x580000c3,  # ldr x3, mark
    0x580000e9,  # ldr x9, entry
    0xd61f0120,  # br x9
    0xd503201f,  # nop (uitlijning)
]
b = b"".join(struct.pack("<I", w) for w in code) + struct.pack("<QQQ", x0, mark, 0x80000)
open(out, "wb").write(b)
open(out + ".park", "wb").write(struct.pack("<II", 0xd503205f, 0x17ffffff))
STUB
	}
	# -no-reboot zoals tools/qemu-rpi4-test.sh: QEMU raspi4b modelleert het
	# PM-blok alleen als reset-knop (een FULL_RESET reset meteen), en de
	# watchdog wapent sinds 01-11 meteen na `discover`, vóór HOPOS_BOOT; er
	# is geen knop om hem uit te zetten (O2). Een kern die zo ver komt, stopt
	# QEMU dus met die reset; de ingang, de bunny en de DTB staan ervoor.
	boot() { # $1 = de console, daarna extra QEMU-argumenten
		con="$1"
		shift
		qemu-system-aarch64 -M raspi4b -display none -monitor none -serial "file:$con" \
			-kernel "$W/kernel8.img" -no-reboot "$@" 2>"$W/qemu.err" &
		qp=$!
		sleep "${SECS:-7}"
		kill "$qp" 2>/dev/null || true
		wait "$qp" 2>/dev/null || true
		tr -d '\r' <"$con" >"$con.txt"
	}
	set --
	[ -f "$DTB" ] && set -- -dtb "$DTB" -initrd "$W/app.elf"
	boot "$W/cold.log" "$@"
	AT="$(sed -n 's/^fdt: .* bytes at \(0x[0-9a-f]*\).*/\1/p' "$W/cold.log.txt" | head -1)"
	echo "== koud op core 0: DTB op ${AT:-geen}, $(grep -c '^P2' "$W/cold.log.txt") keer P2 (de reset van de PM-watchdog stopt QEMU)"
	# De feitenpagina (abi::layout::facts_pa van board_raspi::map
	# BOOT_SCRATCH_PA): de handoff (0: geen bewoners om te adopteren) en de
	# DTB, zoals kern A hem achterlaat.
	FACTS=0x0F100080
	stub "$W/flip.bin" "$FACTS"
	stub "$W/none.bin" "${AT:-0}"
	for k in flip none; do
		boot "$W/$k.log" "$@" \
			-device "loader,file=$W/$k.bin,addr=0x0F800000,force-raw=on" \
			-device "loader,file=$W/$k.bin.park,addr=0x0F801000,force-raw=on" \
			-device loader,addr=0x0F100080,data=0,data-len=8 \
			-device "loader,addr=0x0F100088,data=${AT:-0},data-len=8" \
			-device loader,addr=0x0F800000,cpu-num=1 -device loader,addr=0x0F801000,cpu-num=0
	done
	fail=0
	if [ -n "$AT" ] && [ "$(grep -c '^P2' "$W/cold.log.txt")" = 1 ]; then :; else
		echo "   ROOD de koude boot kwam niet tot discover, of niet tot de reset van de PM-watchdog"
		fail=1
	fi
	# Met de pagina: P2 (de ingang op EL2), de bunny (kmain), en dezelfde DTB.
	for m in "^P2" "the Rust-only OS" "fdt: .* bytes at ${AT:-0x}"; do
		if grep -q "$m" "$W/flip.log.txt"; then
			echo "   ok  core 1 met de feitenpagina: $(grep -m1 "$m" "$W/flip.log.txt")"
		else
			echo "   ROOD core 1 met de feitenpagina: '$m' ontbreekt"
			fail=1
		fi
	done
	# Met de DTB: de koude-boot-poort van _start parkeert hem na de Pi-ingang.
	if grep -q "^P2" "$W/none.log.txt" && ! grep -q "Rust-only" "$W/none.log.txt"; then
		echo "   ok  core 1 met de DTB in x0: 'P2' en dan niets (geparkeerd in _start)"
	else
		echo "   ROOD core 1 met de DTB in x0 kwam verder dan _start, of niet tot P2"
		fail=1
	fi
	grep -m1 "HOPOS_OSCORE" "$W/flip.log.txt" | sed 's/^/   (de Pi houdt zijn kern op core 0: /; s/$/)/' || true
	if [ "$fail" != 0 ]; then
		for k in cold flip none; do
			echo "== $k:"
			cat "$W/$k.log.txt"
		done
		exit 1
	fi
	echo "qemu-flip groen (rpi4, de ingang)"
	exit 0
fi
MISMATCH="${MISMATCH:+1}"
# De modus: warm (met FLIPCONN) of mismatch (alleen de weigering).
MODE=warm
[ -n "$MISMATCH" ] && MODE=mismatch
TARGET=aarch64-unknown-none-softfloat
scratch qemu-flip
ports SYS AGENT LEADER ART ECHO CON

cd "$DIR"
echo "== bouwen ($BOARD, $MODE): kern A (stempel A), bundel B (stempel B), appspike, agentd-hopos"
apps appspike
hop_elf
if [ "$BOARD" = uefi ]; then
	# De ESP van kern A met Hop als gestagede bewoner (rol hop). Alleen virt
	# zet voor Hop zelf QEMU_CFG achter de config (kern::nodecfg, 01-10);
	# hier staat de insecure-regel in het venster van BOOTAA64.EFI (en geen
	# hopos.cfg op de ESP), anders weigert Hop zijn API. Kern B krijgt hem
	# alleen over de flip mee.
	ESP="$ART/esp"
	# De feature efi-rng (zoals de O6N): kern A zaait uit het EFI_RNG_PROTOCOL
	# van EDK2 (de virtio-rng hieronder), kern B uit het zaad dat A meegaf.
	printf 'hopos.insecure=1\n' >"$ART/hopos.cfg"
	[ -n "${OSCORE:-}" ] && printf 'hopos.oscore=%s\n' "$OSCORE" >>"$ART/hopos.cfg"
	HOPOS_STAMP=A BUILD_ONLY=1 ESP="$ESP" APP="$HOP_ELF" ROLE=hop CFG="$ART/hopos.cfg" \
		FEATURES="${FEATURES:+$FEATURES,}board-uefi/efi-rng" \
		sh "$DIR/image/uefi-run.sh" 2>&1 | sed 's/^/   /'
	[ -e "$ESP/EFI/BOOT/BOOTAA64.EFI" ] || {
		echo "ROOD: geen BOOTAA64.EFI"
		exit 1
	}
else
	HOPOS_STAMP=A cargo build --quiet --release --target "$TARGET" -p hopos --features board-qemuvirt
fi
# De groep over de flip (virt, warm): kern A zet Hop op een eigen app-core,
# de bundel vraagt in zijn eigen venster de OS-core.
GROUPFLIP=""
[ "$BOARD:$MODE" = virt:warm ] && GROUPFLIP=1
BCFG=""
if [ -n "$GROUPFLIP" ]; then
	printf 'hopos.hop.sharegroup=system\n' >"$ART/kern-b.cfg"
	BCFG="$ART/kern-b.cfg"
fi
HOPOS_STAMP=B CFG="$BCFG" sh "$DIR/image/flip-bundle.sh" "$BOARD"
BUNDLE="hopos-$BOARD.flip"
cp "$DIR/target/$BUNDLE" "$ART/$BUNDLE"
SHA="$(cat "$DIR/target/$BUNDLE.sha256")"
if [ "$MODE" != warm ]; then
	# De som van de switch-code staat op kop + 56 (kern::kernflip::Bundle,
	# versie 2); één bit om, en de sha256 opnieuw: de bundel is verder echt.
	SHA="$(python3 - "$ART/$BUNDLE" <<'TAMPER'
import hashlib, struct, sys
p = sys.argv[1]
b = bytearray(open(p, "rb").read())
head = struct.unpack_from("<Q", b, len(b) - 16)[0]
v = struct.unpack_from("<Q", b, head + 56)[0]
struct.pack_into("<Q", b, head + 56, v ^ 1)
open(p, "wb").write(b)
print(hashlib.sha256(b).hexdigest())
TAMPER
)"
	echo "   $MODE: de switch-code-som van de bundel is omgezet, sha256 $SHA"
fi
cat >"$ART/half.py" <<'HALF'
import os, socket, sys, time
port, base = int(sys.argv[1]), sys.argv[2]
out = open(base + ".out", "w")
try:
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    s.sendall(b"GET /tasks HTTP/1.1\r\n")
    open(base + ".open", "w").close()
    deadline = time.time() + 120
    while not os.path.exists(base + ".go") and time.time() < deadline:
        time.sleep(0.1)
    s.sendall(b"Host: node\r\nConnection: close\r\n\r\n")
    s.settimeout(20)
    got = b""
    while True:
        b = s.recv(4096)
        if not b:
            break
        got += b
    out.write(got.decode("latin-1").split("\r\n", 1)[0])
except Exception as e:
    out.write("ERROR " + repr(e))
out.close()
open(base + ".done", "w").close()
HALF
# De echo van FLIPCONN: elke regel terug met de fase ervoor (A, of B zodra
# het bestand .landed er is), elke verbinding geteld in .conns en elke
# regel met zijn fase in .lines.
cat >"$ART/echo.py" <<'ECHO'
import os, socket, sys, threading
port, base = int(sys.argv[1]), sys.argv[2]
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(4)
def serve(c, n):
    buf = b""
    try:
        while True:
            b = c.recv(4096)
            if not b:
                break
            buf += b
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                phase = b"B" if os.path.exists(base + ".landed") else b"A"
                with open(base + ".lines", "ab") as f:
                    f.write(phase + b" " + str(n).encode() + b" " + line + b"\n")
                c.sendall(phase + b" " + line + b"\n")
    except Exception as e:
        with open(base + ".lines", "ab") as f:
            f.write(b"E " + str(n).encode() + b" " + repr(e).encode() + b"\n")
    c.close()
n = 0
while True:
    c, _ = srv.accept()
    n += 1
    with open(base + ".conns", "w") as f:
        f.write(str(n))
    threading.Thread(target=serve, args=(c, n), daemon=True).start()
ECHO
serve
python3 "$ART/echo.py" "$ECHOPORT" "$ART/echo" >"$ART/echo.log" 2>&1 &
PIDS="$PIDS $!"

echo "== booten op QEMU $BOARD met Hop, kern A (tot ${TIMEOUT}s; agent :$AGENTPORT, leader :$LEADERPORT, artifacts :$ARTPORT, echo :$ECHOPORT)"
if [ "$BOARD" = uefi ]; then
	# De QEMU-regel van image/uefi-run.sh, met de hostfwd's van Hop erbij
	# (dat script zet alleen de system-API door).
	dd if=/dev/zero of="$ART/vars.fd" bs=1048576 count=64 2>/dev/null
	dd if=/dev/zero of="$DISK" bs=1048576 count=0 seek=64 2>/dev/null
	qemu_edk2 "${CPU:-cortex-a57}" "$ART/vars.fd" "$ESP" \
		-device virtio-net-pci,netdev=n0,romfile= \
		-netdev "user,id=n0,$QFWD" \
		-drive "if=none,format=raw,file=$DISK,id=disk0" \
		-device virtio-blk-pci,drive=disk0 \
		-device virtio-rng-pci \
		</dev/null >"$LOG" 2>&1 &
	QPID=$!
else
	# QMP voor de reset van de zwarte doos (hieronder); qemu-run.sh geeft
	# zijn argumenten door aan QEMU. Warm ook de oude handoff ("eenmalig"
	# hierboven): het adres van het blob (flip_handoff_pa: de staging-kop
	# 0xB010_0000 min 256 KiB) in de feitenpagina op de boot-scratch
	# 0xB000_0000 + 0x80 (board/qemuvirt/src/slots.rs, abi::layout
	# facts_pa), bij elke reset.
	# Kern A met een config in zijn venster; de bundel heeft een leeg.
	if [ -n "$GROUPFLIP" ]; then
		printf 'hopos.hop.sharegroup=hop\n' >"$ART/kern-a.cfg"
	else
		printf 'hopos.hop.sharegroup=system\n' >"$ART/kern-a.cfg"
	fi
	set -- -qmp "unix:$ART/q.sock,server=on,wait=off"
	[ "$MODE" = warm ] && set -- "$@" \
		-device loader,addr=0xb0000080,data=0xb00c0000,data-len=8
	hop_virt HOPOS_STAMP=A CFG="$ART/kern-a.cfg" CONPORT="$CONPORT" -- "$@"
fi

# Alleen wat ná de landing van kern B op de console kwam.
after() { tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | grep -q -E "$1"; }
all_after() {
	(
		IFS='|'
		for m in $1; do after "$m" || exit 1; done
	)
}
echoes() {
	n="$(grep -c "^$1 " "$ART/echo.lines" 2>/dev/null || true)"
	echo "${n:-0}"
}

A_MARKS="HOPOS_BOOT gen=1 stamp=A|HOPOS_HOP_START slot=1|slot 1: 2 port\\(s\\) published tcp\\+udp on the uplink: :8080 :9080 HOPOS_SLOT_PUBLISH|slot 1: .*HOP_UP"
WORK_MARKS="slot 1: .*HOP_JOB_PLACED slot=[2-9]|HOPOS_SLOT_START slot=[2-9]|slot [0-9]+: HOPOS_APPSPIKE_DONE pass=9 fail=0"
BASE_RED="HOPOS_PANIC|HOPOS_FATAL|HOPOS_EXCEPTION|HOPOS_HOP_FAULT|HOPOS_HOP_EXIT|HOPOS_HOP_FAIL|HOPOS_FLIP_BLOB_BAD|HOPOS_FS_FREEZE_FAIL|HOPOS_CAGE_FAIL|HOPOS_CFG_BAD|HOPOS_FLIP_CFG_NONE|HOPOS_FLIP_CFG_OWN"
case "$MODE" in
warm)
	FLIP_MARKS="HOPOS_FLIP_SWITCHCODE_OK|HOPOS_FLIP_STAGED|HOPOS_FS_FROZEN generation=|HOPOS_FLIP_JUMP gen=2|HOPOS_BOOT gen=2 stamp=B|HOPOS_FLIP_BOOT gen=2|HOPOS_FLIP_ADOPT 2 of 2 resident\\(s\\)|HOPOS_HOP_RESUMED|HOPOS_DHCP_CARRIED|HOPOS_FLIP_SETTLED"
	WORK_MARKS="$WORK_MARKS|slot [0-9]+: HOPOS_APPSPIKE_FLIPCONN ok before="
	RED="$BASE_RED|HOPOS_FLIP_REFUSED|HOPOS_FLIP_FAIL|HOPOS_FLIP_ADOPT_FAIL|HOP_FLIP_FAIL|HOPOS_APPSPIKE_FLIPCONN FAIL"
	;;
mismatch)
	FLIP_MARKS="HOPOS_FLIP_REFUSED switch code mismatch|slot 1: .*HOP_FLIP_FAIL"
	WORK_MARKS=""
	RED="$BASE_RED|HOPOS_FLIP_STAGED|HOPOS_FS_FROZEN|HOPOS_FLIP_JUMP|HOPOS_FLIP_BOOT|HOPOS_FLIP_FAIL"
	;;
esac
if [ "$BOARD" = uefi ] && [ "$MODE" != mismatch ]; then
	# De TRNG achter de firmware is na de koude boot weg: de nieuwe kern
	# zaait uit het zaad van de oude (board/uefi/src/flip.rs, carry_seed).
	FLIP_MARKS="$FLIP_MARKS|HOPOS_RNG_EFI_UP|HOPOS_FLIP_SEED|HOPOS_RNG_EFI_CARRIED"
fi
AFTER_MARKS="${AFTER_MARKS:-}"
if [ -n "$GROUPFLIP" ]; then
	# De bundel houdt zijn eigen venster; Hop blijft in zijn groep.
	BASE_RED="$(printf '%s' "$BASE_RED" | sed 's/|HOPOS_FLIP_CFG_OWN//')"
	RED="$(printf '%s' "$RED" | sed 's/|HOPOS_FLIP_CFG_OWN//')"
	FLIP_MARKS="$FLIP_MARKS|flip: the bundle carries its own hopos.cfg in its window, ours stays behind HOPOS_FLIP_CFG_OWN"
	AFTER_MARKS="${AFTER_MARKS:+$AFTER_MARKS|}cfg: hopos.cfg from the window in the kernel image, [0-9]+ bytes HOPOS_CFG_WINDOW|slots: Hop in the sharegroup hop \\(carried over the flip\\) HOPOS_HOP_GROUP|slots: the config puts Hop in the sharegroup system \\(hopos.hop.sharegroup\\) from the next cold start, .* HOPOS_HOP_GROUP_COLD"
elif [ "$MODE" != mismatch ]; then
	# De config over de flip: mee vóór de sprong, gelezen na de landing.
	FLIP_MARKS="$FLIP_MARKS|flip: hopos.cfg carried into the new image HOPOS_FLIP_CFG"
	AFTER_MARKS="${AFTER_MARKS:+$AFTER_MARKS|}cfg: hopos.cfg from the window in the kernel image, [0-9]+ bytes HOPOS_CFG_WINDOW"
fi

JOB='{"name":"spike","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432}'
[ -n "$GROUPFLIP" ] && JOB='{"name":"spike","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"tags":{"sharegroup":"hop"}}'
# De bewoner vóór de flip: FLIPCONN (warm).
PRE_JOB=""
case "$MODE" in
warm) PRE_JOB='{"name":"flipconn","driver":"hop","artifacts":[{"url":"http://10.0.2.2:'"$ARTPORT"'/appspike.elf"}],"memory_limit":33554432,"env":{"ROLE":"FLIPCONN","FLIPCONN":"10.0.2.2:'"$ECHOPORT"'"}}' ;;
esac
FLIPREQ='{"url":"http://10.0.2.2:'"$ARTPORT"'/'"$BUNDLE"'","sha256":"'"$SHA"'"}'
PRE=""
[ -z "$PRE_JOB" ] && PRE="n.v.t."
FLIPPED=""
LASTPOST=""
TASKS=""
POSTED=""
started
flip() {
	curl -s -m 30 -w ' HTTP %{http_code}' -X POST -H 'Content-Type: application/json' \
		-d "$1" "http://127.0.0.1:$AGENTPORT/flip" 2>&1
}
while alive; do
	# De landing zet de fase van de echo om, zo vroeg als de console hem
	# toont.
	if [ ! -e "$ART/echo.landed" ] && has "HOPOS_FLIP_BOOT"; then
		: >"$ART/echo.landed"
	fi
	if [ -z "$PRE" ]; then
		if all "$A_MARKS"; then
			post_job "$PRE_JOB" || {
				PRE="$POSTED"
				POSTED=""
				break
			}
			PRE="$POSTED"
			POSTED=""
		fi
		step
		continue
	fi
	if [ -z "$FLIPPED" ]; then
		# Wacht op de bewoner van vóór de flip: drie echo's van FLIPCONN.
		[ "$MODE" = warm ] && { [ "$(echoes A)" -ge 3 ] || { step; continue; }; }
		all "$A_MARKS" || { step; continue; }
		if out="$(flip "$FLIPREQ")"; then
			FLIPPED="$out"
			# De verbinding over de flip: nu openen, met de eerste
			# helft van een GET /tasks; de rest pas na de landing.
			[ "$MODE" = warm ] && python3 "$ART/half.py" "$AGENTPORT" "$ART/half" &
		else
			FLIPPED="ROOD curl: $out"
			break
		fi
		step
		continue
	fi
	if [ -z "$TASKS" ]; then
		landed=1
		[ "$MODE" = mismatch ] || all_after "${AFTER_MARKS:-HOPOS_FLIP_BOOT}" || landed=""
		if all "$FLIP_MARKS" && [ -n "$landed" ]; then
			# Eerst de halve verbinding van vóór de sprong afmaken.
			if [ -e "$ART/half.open" ] && [ ! -e "$ART/half.go" ]; then
				: >"$ART/half.go"
				n=0
				while [ ! -e "$ART/half.done" ] && [ "$n" -lt 150 ]; do
					sleep 0.2
					n=$((n + 1))
				done
			fi
			# Hop antwoordt, over de uplink van de nieuwe kern.
			if t="$(curl -s -m 5 -w ' HTTP %{http_code}' "http://127.0.0.1:$AGENTPORT/tasks" 2>&1)"; then
				case "$t" in *"HTTP 200"*) TASKS="$t" ;; esac
			fi
		fi
		step
		continue
	fi
	[ "$MODE" = mismatch ] && break
	if [ -z "$POSTED" ]; then
		# Een curl die niets terugkrijgt, probeert het over een seconde
		# opnieuw: de leader van een koud herstarte Hop moet nog opkomen.
		if ! post_job "$JOB"; then
			LASTPOST="${POSTED#ROOD }"
			POSTED=""
			sleep 1
		fi
		step
		continue
	fi
	all_after "$WORK_MARKS" && break
	step
done
# De zwarte doos: na een groene warme flip op virt een system_reset (het RAM
# blijft, zoals na een watchdog-reset), en de console-ring van de koude kern
# A moet de console van kern B nog dragen. Alles vanaf de reset gaat naar een
# eigen log, en wat tcp/5555 dan afspeelt naar con.log.
RESET=""
CUT=""
if [ "$BOARD" = virt ] && [ "$MODE" = warm ] && [ -n "$POSTED" ] && all_after "$WORK_MARKS" && ! has "$RED"; then
	CUT="$(wc -c <"$LOG" | tr -d ' ')"
	RESET="$(python3 - "$ART/q.sock" <<'QMP' 2>&1
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
f = s.makefile("rw")
f.readline()
for cmd in ("qmp_capabilities", "system_reset"):
    f.write(json.dumps({"execute": cmd}) + "\n")
    f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            break
print(json.dumps(r))
QMP
)" || RESET="ROOD qmp: $RESET"
	# Tot Hop weer op is (of iets roods), hoogstens een minuut.
	n=0
	while [ "$n" -lt 300 ] && kill -0 "$QPID" 2>/dev/null; do
		tail -c +"$((CUT + 1))" "$LOG" | tr -d '\r' | grep -q -E "slot 1: .*HOP_UP|$RED" && break
		sleep 0.2
		n=$((n + 1))
	done
	python3 - "$CONPORT" >"$ART/con.log" 2>&1 <<'CON'
import socket, sys, time
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
s.settimeout(1)
end, out = time.time() + 3, b""
while time.time() < end:
    try:
        b = s.recv(65536)
    except socket.timeout:
        continue
    if not b:
        break
    out += b
sys.stdout.write(out.decode("utf-8", "replace"))
CON
fi
qemu_stop
if [ -n "$CUT" ]; then
	tail -c +"$((CUT + 1))" "$LOG" | tr -d '\r' >"$ART/reset.log"
	head -c "$CUT" "$LOG" >"$ART/before.log"
	cp "$ART/before.log" "$LOG"
fi

fail=0
marks "$A_MARKS" "$FLIP_MARKS"
IFS_WAS="$IFS"
IFS='|'
for m in $AFTER_MARKS $WORK_MARKS; do
	if after "$m"; then
		echo "   ok  na de flip: $(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | grep -m1 -E "$m")"
	else
		echo "   ROOD $m ontbreekt na de flip"
		fail=1
	fi
done
IFS="$IFS_WAS"
case "$MODE:$PRE" in
*:n.v.t.) ;;
*"HTTP 2"*) echo "   ok  de bewoner van vóór de flip: $PRE" ;;
*) echo "   ROOD de bewoner van vóór de flip: ${PRE:-nooit geplaatst}"; fail=1 ;;
esac
case "$MODE:$FLIPPED" in
mismatch:*"HTTP 5"*) echo "   ok  POST /flip geweigerd: $FLIPPED" ;;
mismatch:*) echo "   ROOD POST /flip had geweigerd moeten worden: ${FLIPPED:-nooit gedaan}"; fail=1 ;;
*:*"HTTP 202"*) echo "   ok  POST /flip ($MODE): $FLIPPED" ;;
*:) echo "   ROOD POST /flip nooit gedaan (kern A, Hop of de bewoner niet op tijd op)"; fail=1 ;;
*) echo "   ROOD POST /flip: $FLIPPED"; fail=1 ;;
esac
case "$TASKS" in
*"HTTP 200"*) echo "   ok  GET /tasks na de flip: $TASKS" ;;
*) echo "   ROOD GET /tasks na de flip: ${TASKS:-nooit beantwoord}"; fail=1 ;;
esac
case "$MODE:$POSTED" in
mismatch:*) ;;
*"HTTP 2"*) echo "   ok  POST /v1/jobs op kern B: $POSTED" ;;
*) echo "   ROOD POST /v1/jobs op kern B: ${POSTED:-${LASTPOST:-nooit gedaan}}"; fail=1 ;;
esac
if [ "$MODE" = warm ]; then
	HALF="$(cat "$ART/half.out" 2>/dev/null || true)"
	case "$HALF" in
	*" 200"*) echo "   ok  een verbinding over de flip: geopend op kern A, beantwoord door kern B: $HALF" ;;
	*) echo "   --  een verbinding over de flip brak (mag, 01-11): ${HALF:-geen antwoord}" ;;
	esac
	# FLIPCONN: de app zegt before=N after=M; een breuk is een fout van
	# de stack, waarna hij opnieuw verbond.
	FC="$(tr -d '\r' <"$LOG" | sed -n 's/.*HOPOS_APPSPIKE_FLIPCONN ok before=\([0-9]*\) after=\([0-9]*\).*breaks=\([0-9]*\).*/\1 \2 \3/p' | head -1)"
	CONNS="$(cat "$ART/echo.conns" 2>/dev/null || echo 0)"
	set -- $FC
	if [ -n "$FC" ] && [ "$2" -gt "$1" ]; then
		echo "   ok  FLIPCONN: $1 antwoord(en) via kern A, $(($2 - $1)) via kern B, $3 breuk(en) en $CONNS verbinding(en) bij de echo (A=$(echoes A) B=$(echoes B))"
	else
		echo "   ROOD FLIPCONN: before/after '${FC:-?}', verbindingen bij de echo ${CONNS}, A=$(echoes A) B=$(echoes B)"
		cat "$ART/echo.lines" 2>/dev/null | tail -5 | sed 's/^/        /'
		fail=1
	fi
fi
if [ -n "$GROUPFLIP" ]; then
	# De jobspec met de tag hop landt op de core van de geadopteerde Hop.
	HOPCORE="$(tr -d '\r' <"$LOG" | sed -n 's/.*Hop carried over the flip on core \([0-9]*\),.*/\1/p' | head -1)"
	JOBCORE="$(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | sed -n 's/.*HOPOS_SLOT_START slot=[2-9] core=\([0-9]*\) .*/\1/p' | head -1)"
	if [ -n "$HOPCORE" ] && [ "$HOPCORE" != 0 ] && [ "$JOBCORE" = "$HOPCORE" ]; then
		echo "   ok  de groep: Hop bleef op core $HOPCORE (sharegroup hop), de job met sharegroup hop landde ernaast"
	else
		echo "   ROOD de groep: Hop op core '${HOPCORE:-?}', de job met sharegroup hop op core '${JOBCORE:-?}'"
		fail=1
	fi
fi
# hopfs: de generatie die kern A vastlegde, is die welke kern B mount.
FROZEN="$(tr -d '\r' <"$LOG" | sed -n 's/.*HOPOS_FS_FROZEN generation=\([0-9]*\).*/\1/p' | head -1)"
MOUNTED="$(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | sed -n 's/.*HOPOS_FS_UP fresh=0 generation=\([0-9]*\).*/\1/p' | head -1)"
if [ "$MODE" = mismatch ]; then
	:
elif [ -n "$FROZEN" ] && [ "$FROZEN" = "$MOUNTED" ]; then
	echo "   ok  hopfs: generatie $FROZEN vastgelegd en bevroren door kern A, gemount door kern B (fresh=0)"
else
	echo "   ROOD hopfs: kern A bevroor generatie '${FROZEN:-?}', kern B mountte '${MOUNTED:-geen fresh=0}'"
	fail=1
fi
if [ "$MODE" != mismatch ]; then
	# De lease ging mee (H1): kern B deed geen DHCP.
	CARRIED="$(tr -d '\r' <"$LOG" | sed -n 's/.*dhcp: lease \([0-9.]*\)\/[0-9]* from the previous kernel.*/\1/p' | head -1)"
	if after "HOPOS_DHCP_BOUND"; then
		echo "   ROOD de lease: kern B deed DHCP ($(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | grep -m1 HOPOS_DHCP_BOUND))"
		fail=1
	elif [ -n "$CARRIED" ] && [ "$CARRIED" != 0.0.0.0 ]; then
		echo "   ok  de lease: $CARRIED van kern A overgenomen, geen DHCP op kern B"
	else
		echo "   ROOD de lease: niet overgenomen ('${CARRIED:-geen HOPOS_DHCP_CARRIED}')"
		fail=1
	fi
fi
boots="$(count 'slot 1: .*HOP_BOOT')"
starts="$(count 'HOPOS_HOP_START slot=1')"
want=1
if [ "$boots" = "$want" ] && [ "$starts" = "$want" ]; then
	echo "   ok  dezelfde Hop: 1x HOP_BOOT en 1x HOPOS_HOP_START over twee kernen"
else
	echo "   ROOD Hop: HOP_BOOT ${boots}x, HOPOS_HOP_START ${starts}x (verwacht ${want}x)"
	fail=1
fi
if grep -q "GET /$BUNDLE" "$ART/http.log" 2>/dev/null; then
	echo "   ok  artifact-server: de bundel is opgehaald"
else
	echo "   ROOD artifact-server: de bundel is nooit gevraagd"
	fail=1
fi
reds
if [ "$BOARD" = virt ] && [ "$MODE" = warm ]; then
	# De zwarte doos: de console van na de reset, en de ring op 5555.
	R="$ART/reset.log"
	case "$RESET" in
	*'"return"'*) echo "   ok  system_reset over QMP na de flip" ;;
	*) echo "   ROOD system_reset over QMP: ${RESET:-niet gedaan (de flip was niet groen)}"; fail=1 ;;
	esac
	for m in "HOPOS_BOOT gen=1 stamp=A" "console: [1-9][0-9]* bytes from before this boot .*HOPOS_CONSOLE_CARRIED" "slot 1: .*HOP_UP"; do
		if grep -q -E "$m" "$R" 2>/dev/null; then
			echo "   ok  na de reset: $(grep -m1 -E "$m" "$R")"
		else
			echo "   ROOD na de reset: '$m' ontbreekt"
			fail=1
		fi
	done
	# Eenmalig: de overdracht van de sprong is na de reset van niemand.
	if grep -q -E 'HOPOS_FLIP_BOOT|HOPOS_FLIP_ADOPT|HOPOS_HOP_RESUMED' "$R" 2>/dev/null; then
		echo "   ROOD na de reset een landing: $(grep -m1 -E 'HOPOS_FLIP_BOOT|HOPOS_FLIP_ADOPT|HOPOS_HOP_RESUMED' "$R")"
		fail=1
	else
		echo "   ok  na de reset koud: geen HOPOS_FLIP_BOOT, HOPOS_FLIP_ADOPT of HOPOS_HOP_RESUMED"
	fi
	# Kern B zei HOPOS_APPSPIKE_DONE vóór de reset; op 5555 staat het vóór
	# de laatste bootregel van kern A.
	if tr -d '\r' <"$ART/con.log" 2>/dev/null | awk '/HOPOS_APPSPIKE_DONE pass=9/ { d = 1 } /HOPOS_BOOT gen=1 stamp=A/ && d { ok = 1 } END { exit !ok }'; then
		echo "   ok  op 5555 na de reset: $(tr -d '\r' <"$ART/con.log" | grep -m1 -E 'HOPOS_APPSPIKE_DONE')"
	else
		echo "   ROOD de ring op 5555 draagt de laatste regels van kern B niet (HOPOS_APPSPIKE_DONE vóór HOPOS_BOOT stamp=A): $(wc -c <"$ART/con.log" 2>/dev/null) bytes"
		fail=1
	fi
	if grep -q -E "$RED" "$R" 2>/dev/null; then
		echo "   ROOD na de reset: $(grep -m1 -E "$RED" "$R")"
		fail=1
	fi
	if [ "$fail" != 0 ] && [ -s "$R" ]; then
		echo "== console na de reset:"
		cat "$R"
	fi
fi
if [ -n "${OSCORE:-}" ]; then
	# Kern A verhuisde naar de OS-core; kern B kwam daar binnen (zijn
	# zelftest van de OS-core draait op die fysieke core) en verhuisde niet.
	if has "HOPOS_OSCORE_UP" && after "oscore: cpu $OSCORE self-test" && ! after "HOPOS_OSCORE_MOVE"; then
		echo "   ok  OSCORE=$OSCORE: kern A op core $OSCORE, en kern B kwam daar door _start: $(tr -d '\r' <"$LOG" | awk '/HOPOS_FLIP_BOOT/ { f = 1 } f' | grep -m1 "oscore: cpu")"
	else
		echo "   ROOD OSCORE=$OSCORE: geen HOPOS_OSCORE_UP, of kern B niet op core $OSCORE"
		fail=1
	fi
fi
took
verdict qemu-flip
[ -n "${KEEP_LOG:-}" ] && [ -s "$ART/reset.log" ] && cp "$ART/reset.log" "$KEEP_LOG.reset"
echo "qemu-flip groen ($BOARD, $MODE)"
