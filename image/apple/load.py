#!/usr/bin/env python3
# load.py — zet een HopOS-image via m1n1's proxy op een Apple-silicon-
# machine en start hem (v3: board/apple, image/apple-m4.sh). De
# "firmware-rol" van dit script: wat U-Boot/booti op de Radxa doet, plus het
# param-blok (board_apple::fwinfo) met wat alleen de loader weet: m1n1's
# spin-table en de config-tekst. Overgenomen van de Go-meetbank
# (image/apple/load-probe.py op tag v2.2.8, bewezen 28-08 t/m 04-09); nieuw in v3: de
# staging (STAGE=pad, het image van Hop) in de loader-regio, met een magic.
#
#   M1N1DEVICE=/dev/cu.usbmodem<serienr>1 python3 image/apple/load.py target/apple-m4/hopos-apple.img
#
# Omgeving: M1N1 = pad naar de m1n1-clone (default ~/Git/m1n1), M1N1DEVICE =
# de proxy-poort. Na de sprong leest het script /dev/cu.debug-console
# (CONSOLE=... om te kiezen, CONSOLE=none om over te slaan) op 1500000 baud
# en echoot wat de probe zegt.
import os, sys, struct, time, subprocess, pathlib

M1N1 = os.path.expanduser(os.environ.get("M1N1", "~/Git/m1n1"))
sys.path.append(os.path.join(M1N1, "proxyclient"))
from m1n1.setup import *  # noqa: E402

# Pariteit met board/apple/src/lib.rs, fwinfo.rs en slots.rs.
RAM_BASE   = 0x101_0000_0000

# AT: waar het image NEERGEZET wordt. Standaard meteen op zijn linkadres, maar
# het hoeft niet meer — sinds het image een bootstub vooraan draagt (board/apple/src/head.rs) (offset 0
# secundaire cores, 0x800 de boot-core) verplaatst het zichzelf. AT= is dus de
# proef op die stub zónder installatie: zet hem 2GB lager neer en kijk of HopOS
# alsnog op 0x101_0000_0000 wakker wordt.
#
#   AT=0x10080000000 image/apple/boot-cycle.sh target/apple-m4/hopos-apple.img
LOAD_AT    = int(os.environ.get("AT", hex(RAM_BASE)), 0)
STUB_ENTRY = 0x800  # pariteit: bootstub.s, en de --entry-point van kmutil
PARAM_BASE = LOAD_AT + 0xE100
PARAM_MAGIC = 0x454C505041504F48  # "HOPAPPLE"
PARAM_VERSION = 5
MAX_CPUS = 16
SPIN_TABLE_ENTRY = 64  # sizeof(struct spin_table): mpidr, flag, target, args[4], retval
SPIN_TARGET_OFF = 16
# De staging (board_apple::slots): in de loader-regio van het venster, op
# de LINKadressen (die regio hoort niet bij het image, de stub verplaatst
# hem niet). Zonder magic is er geen staging.
LOADER_BASE = RAM_BASE + 0x1200_0000
STAGE_HDR = LOADER_BASE + 0x10_0000
STAGE_PA = LOADER_BASE + 0x20_0000
STAGE_MAX = LOADER_BASE + 0x0400_0000 - STAGE_PA
STAGE_MAGIC = 0x4547415453504F48  # "HOPSTAGE"

img_path = pathlib.Path(sys.argv[1])
img = img_path.read_bytes()
# CFG=pad: de config in het venster van het image (board/src/cfgwin.rs,
# image/hopcfg.py), want dat wint van de tekst op 0xF000 hieronder. Een image
# van vóór het venster heeft er geen: dan alleen 0xF000, zoals voorheen.
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
import hopcfg  # noqa: E402
WINDOWS = hopcfg.windows(img)
if os.environ.get("CFG") and len(WINDOWS) == 1:
    at = WINDOWS[0]
    size = hopcfg.head(img, at)[0]
    img = img[:at] + hopcfg.window(pathlib.Path(os.environ["CFG"]).read_bytes(), size) + img[at + size:]

# PHASE=all (default) doet alles over één lijn. De snelle meetbank splitst:
# PHASE=load over m1n1's USB-gadget (serial-mode, ~9MB/s), dan macvdmtool
# debugusb, dan PHASE=boot over de dockchannel (kis ch-0) die daarna de console
# is. m1n1 blijft tussendoor gewoon draaien, dus wat PHASE=load in RAM legde
# (image + param-blok) staat er nog.
PHASE = os.environ.get("PHASE", "all")

# BARE=1: booten alsof er geen loader is — géén param-blok en géén config in
# het geheugen, dus het board moet alles uit boot_args en de device tree halen.
# Precies wat er na een `kmutil configure-boot` gebeurt. Doe dit over de
# dockchannel (MODE=split in boot-cycle.sh): m1n1's USB-gadget verdwijnt met
# hem mee, de dockchannel blijft.
BARE = os.environ.get("BARE") == "1"

def boot_and_console():
    u.msr(DAIF, 0x3C0)
    print("Booting at %#x (stub entry, image at %#x), x0 = boot_args %#x ..." % (
        LOAD_AT + STUB_ENTRY, LOAD_AT, u.ba_addr))
    try:
        # P_VECTOR en niet kboot_boot: m1n1 geeft het stokje dan door zoals
        # iBoot dat doet — x0 = het échte boot_args-blok — in plaats van als
        # bootloader met zijn eigen FDT. Sinds HopOS de PCIe-controller zelf
        # opbrengt (board/apple/src/pcie.rs) is er niets meer dat kboot_boot voor
        # ons deed, en dan is één bootroute beter dan twee: deze is dezelfde
        # die na de installatie geldt.
        #
        # Rechtstreeks en niet via p.reload: die wacht daarna op een
        # proxy-handshake die nooit komt (wij zijn geen m1n1) en LEEST
        # ondertussen de lijn leeg — precies de bytes die wij willen zien.
        p.request(p.P_VECTOR, LOAD_AT + STUB_ENTRY, u.ba_addr, 0, 0, 0, no_reply=True)
    except Exception as e:
        # De proxy sluit zijn USB-gadget vóór de sprong; een afgebroken antwoord
        # hier is normaal.
        print("(proxy closed: %s)" % e)
    console = os.environ.get("CONSOLE", "proxy")
    if console == "none":
        return
    import serial
    limit = float(os.environ.get("CONSOLE_SECONDS", "0")) or None
    if console == "proxy":
        s = iface.dev
        s.timeout = 1
    else:
        s = serial.Serial(console, 1500000, timeout=1)
    print("--- console %s (%s) ---" % (console, "%.0fs" % limit if limit else "Ctrl-C to stop"))
    t0 = time.time()
    try:
        while limit is None or time.time() - t0 < limit:
            d = s.read(4096)
            if d:
                sys.stdout.write(d.decode("utf-8", "replace"))
                sys.stdout.flush()
    except KeyboardInterrupt:
        pass

if PHASE == "boot":
    boot_and_console()
    sys.exit(0)

# Het param-blok. Sinds 29-08 draagt het alleen nog wat de loader ÁLS ENIGE
# weet; al het andere (UART-bases, DRAM, framebuffer, opslag, cores) leest het
# board zelf uit iBoot's boot_args en de device tree (board_apple::fwinfo).
p.smp_start_secondaries()
# Zijn wachtstand op WFE zetten. m1n1's secundaire cores slapen standaard in
# WFI en willen een IPI; onze Release() stuurt een SEV, zoals Linux'
# cpu-release-addr voorschrijft. kboot_boot zette dit vroeger voor ons om, maar
# die weg gebruiken we niet meer (zie boot_and_console) — dus doen we het hier,
# waar de rest van de m1n1-kennis ook staat.
p.smp_set_wfe_mode(True)
ncpus = len(list(u.adt["/cpus"]))
my_mpidr = u.mrs(MPIDR_EL1) & 0xFFFFFF
release = [0] * MAX_CPUS
mpidr = [0] * MAX_CPUS
boot_cpu = 0xFFFF

# m1n1's spin-table: het symbool uit de ELF van precies deze m1n1-build,
# gerelokeerd naar waar m1n1 nu draait (u.base). Een verkeerde offset zou een
# core naar een willekeurig woord laten kijken, dus we controleren: de
# flag-woorden van de gestarte cores moeten 1 zijn.
def spin_table_addr():
    # m1n1.bin is de RAW link (m1n1-raw.ld): dezelfde code 0x4000 lager dan in
    # m1n1.elf. Symbolen dus uit m1n1-raw.elf, anders lees je naast de tabel.
    elf = os.path.join(M1N1, "build", "m1n1-raw.elf")
    out = subprocess.check_output(["aarch64-elf-nm", elf]).decode()
    sym = [l for l in out.splitlines() if l.endswith(" spin_table")]
    base = [l for l in out.splitlines() if l.endswith(" _base")]
    return u.base + int(sym[0].split()[0], 16) - int(base[0].split()[0], 16)

# De spin-table is optioneel: lukt de lookup niet, dan boot HopOS zonder
# app-cores — eerste licht gaat vóór de cores.
try:
    spin = spin_table_addr()
    print("u.base %#x, spin_table %#x, my mpidr %#x" % (u.base, spin, my_mpidr))
    for cpu in range(min(ncpus, MAX_CPUS)):
        ent = spin + cpu * SPIN_TABLE_ENTRY
        m, flag = p.read64(ent), p.read64(ent + 8)
        mpidr[cpu] = m
        if m == my_mpidr:
            boot_cpu = cpu
        elif flag:
            release[cpu] = ent + SPIN_TARGET_OFF
    if boot_cpu == 0xFFFF:
        raise RuntimeError("boot cpu not in spin table (symbol offset wrong?)")
    print("spin table: boot cpu %d, alive %r" % (boot_cpu, [c for c in range(ncpus) if release[c]]))
except Exception as e:
    print("spin table unavailable (%s) — no app cores" % e)
    release = [0] * MAX_CPUS
    boot_cpu = 0xFFFF

# De hop van de Go-kern (cpuinit.s verhuisde vóór de eerste Go-instructie)
# bestaat in v3 niet in de loader: de keuze is `hopos.oscore=small` in de
# config, en de kern verhuist zelf (docs/boards-apple.md). De velden blijven
# leeg, zodat het blok byte voor byte versie 5 is.
hop_cpu, hop_mpidr, hop_rel = 0xFFFF, 0, 0

params = struct.pack("<6Q", PARAM_MAGIC, PARAM_VERSION, boot_cpu,
                     hop_cpu, hop_mpidr, hop_rel)
params += struct.pack("<16Q", *release)
assert len(params) == 0xB0

# De dockchannel (debugusb-mode) is een WOORD-kanaal: een write waarvan de
# lengte geen veelvoud van 8 is, verliest zijn staart tot er meer bytes komen
# (GEMETEN 28-08: 3038 bytes faalt altijd, 3040 werkt; m1n1's writemem stuurt
# alles in één keer en wacht dan op één ack die nooit komt). Daarom: elke
# write op 8 bytes gepad, in chunks met eigen ack+CRC; sporadisch verlies
# (~1 op 130KB gezien) wordt hersteld door de proxy eerst "vol te maken" met
# nullen tot hij antwoordt (unwedge), te hersyncen met een nop, en de chunk
# opnieuw te sturen. Over de USB-gadget (usbmodem) is dit overbodig maar
# onschadelijk.
import gzip
def pad8(b):
    return b + b"\0" * (-len(b) % 8)

def unwedge():
    dev = iface.dev
    old = dev.timeout
    dev.timeout = 0.05
    for _ in range(20000):
        dev.write(b"\0" * 64)
        if dev.read(64):
            break
    time.sleep(0.4)
    dev.reset_input_buffer()
    dev.timeout = old
    for _ in range(5):
        try:
            iface.nop()
            return
        except Exception:
            time.sleep(0.2)
            dev.reset_input_buffer()

def robust_writemem(addr, data, chunk=int(os.environ.get("CHUNK", "8192"))):
    data = pad8(data)
    assert chunk % 8 == 0
    off, retries, t0 = 0, 0, time.time()
    while off < len(data):
        piece = data[off:off + chunk]
        try:
            iface.writemem(addr + off, piece)
            off += len(piece)
            sys.stdout.write("."); sys.stdout.flush()
        except Exception as e:
            retries += 1
            sys.stdout.write("!(%s)" % type(e).__name__); sys.stdout.flush()
            if retries > 40:
                raise
            unwedge()
    dt = time.time() - t0
    print("\n%d bytes in %.1fs (%.0f KB/s), %d retries" % (len(data), dt, len(data) / dt / 1024, retries))

payload = gzip.compress(img, compresslevel=6)
print("Loading %d bytes (gz %d) to %#x..%#x%s" % (len(img), len(payload), LOAD_AT, LOAD_AT + len(img),
    "" if LOAD_AT == RAM_BASE else " (the stub relocates to %#x)" % RAM_BASE))
gz_addr = u.malloc(len(payload) + 8)
robust_writemem(gz_addr, payload)
tmo = iface.dev.timeout
iface.dev.timeout = None
n = p.gzdec(gz_addr, len(payload), LOAD_AT, len(img))
iface.dev.timeout = tmo
if n != len(img):
    raise SystemExit("gzdec: %d != %d — image corrupt on target" % (n, len(img)))
print("gzdec OK: %d bytes at %#x" % (n, LOAD_AT))
if not BARE:
    robust_writemem(PARAM_BASE, params)

# Platform-config als tekst op CFG_BASE (board_apple::fwinfo::CFG_PA): het
# hopos.cfg-bestand uit CFG=pad, voor een kern met een leeg venster of een
# van vóór het venster (die met een venster kreeg CFG= hierboven al in zijn
# venster). Zonder CFG blijft staan wat het image draagt: het venster dat
# image/apple-m4.sh met CFG= vulde, of nullen (gzdec schreef net het hele
# bestand, dus er liggen geen resten van een vorige boot). BARE=1 laat het
# venster ook staan: na een `kmutil configure-boot` is het venster de enige
# config.
CFG_BASE, CFG_SIZE = LOAD_AT + 0xF000, 0x1000
cfg = ""
if os.environ.get("CFG"):
    cfg = pathlib.Path(os.environ["CFG"]).read_text()
cfgb = cfg.encode()
if len(cfgb) >= CFG_SIZE:
    raise SystemExit("config too large: %d >= %d" % (len(cfgb), CFG_SIZE))
baked = img[0xF000:0xF000 + 16] == hopcfg.MAGIC or (
    len(WINDOWS) == 1 and hopcfg.head(img, WINDOWS[0])[2] > 0)
if BARE:
    robust_writemem(PARAM_BASE, b"\0" * 0xB0)
    print("bare: no param block%s; the board reads the firmware itself" % (
        ", the baked config window stays" if baked else ", no config"))
elif os.environ.get("CFG"):
    robust_writemem(CFG_BASE, cfgb.ljust(CFG_SIZE, b"\0"))
    print("config: %d bytes from %s%s" % (len(cfgb), os.environ["CFG"],
        " (over the baked window)" if baked else ""))
else:
    print("config: %s" % ("the window baked into the image" if baked else "none"))
# De staging: image, maat, een leeg woord (waar tot 01-11 de rol stond), en
# het magic als laatste (zonder magic leest de kern de loader-regio niet als
# image).
if os.environ.get("STAGE"):
    stage = pathlib.Path(os.environ["STAGE"]).read_bytes()
    if len(stage) > STAGE_MAX:
        raise SystemExit("stage too large: %d > %d" % (len(stage), STAGE_MAX))
    robust_writemem(STAGE_PA, stage)
    robust_writemem(STAGE_HDR, struct.pack("<3Q", len(stage), 0, STAGE_MAGIC))
    print("stage: %d bytes at %#x" % (len(stage), STAGE_PA))
else:
    robust_writemem(STAGE_HDR, struct.pack("<3Q", 0, 0, 0))
p.dc_cvau(LOAD_AT, len(img) + 0x10000)
p.ic_ivau(LOAD_AT, len(img) + 0x10000)


if PHASE == "load":
    print("Loaded; not booting (PHASE=load). Switch the port and run PHASE=boot.")
    sys.exit(0)
boot_and_console()
