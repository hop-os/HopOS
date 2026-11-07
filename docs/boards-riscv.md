# RISC-V op HopOS v3: QEMU virt riscv64 en de LicheeRV Nano

De tweede architectuur. QEMU virt riscv64 is de proefbank, de Sipeed
LicheeRV Nano (SG2002, XuanTie C906) het board. De metingen op dit board
(30-07 tot 19-08) staan in het commentaar van
de code; deze pagina zegt wat er staat, hoe je het bouwt, en per stap wat de
console op het ijzer moet tonen.

## Machine mode, en waarom

HopOS draait op RISC-V in machine mode. De kooi is een
PMP-whitelist (begrenzen) plus een Sv39-tabel (verplaatsen), en PMP
programmeren kan alleen machine mode. Het image neemt daarom de plek van
OpenSBI in:

| | QEMU virt | LicheeRV Nano |
| --- | --- | --- |
| Wie laadt ons | QEMU `-bios none -kernel hopos` | de vendor-FSBL, MONITOR-slot van `fip.bin` |
| Waar | 0x8000_0000, elk hart | 0x8400_0000 (RUNADDR; lager boot niet, 19-08) |
| a0 / a1 | hart-id / DTB (bovenin het RAM) | hart-id / geen DTB |
| Timebase | 10 MHz | 25 MHz (40 ns per tik) |
| Cache-coherent | ja | nee: `dev::push`/`pull` doen `th.dcache.cpa`/`cipa` (feature `thead`) |

Onder OpenSBI (`-bios default`) landt de kern in S-mode en is de PMP van de
firmware: dan is er geen kooi, en `kmain` weigert net als "HopOS eist EL2" op
ARM (`board.privilege(3)`). SBI HSM voor de harts is daarmee ook geen weg: in
machine mode is er niemand onder ons om het te vragen. Een hart start via de
parkeerlus van de boot-stub (`cpu::riscv::boot::start_hart`: postvak plus
`msip`) of, op de LicheeRV, via het resetblok van de C906L; de C906B van
de LicheeRV heeft geen bel en geen resetblok en pollt zijn postvak.

De tabel ARM naar RISC-V (`cpu/src/riscv/mod.rs`):

```text
ARM                     RISC-V
EL2                     machine mode         (de kern)
EL1-app                 supervisor mode      (een slot)
HVC-yield               ecall-yield          (a0 = wektijd, a7 = 0/1)
stage-2 (VTTBR)         Sv39 (satp) + PMP    (verplaatsen + begrenzen)
ERET                    mret
PSCI CPU_ON             msip op een geparkeerd hart; resetblok op de C906L;
                        de C906B pollt zijn postvak
GIC                     PLIC + CLINT
```

## Wat er is

| Deel | Waar | Getest |
| --- | --- | --- |
| Boot-stub: `_start` in M-mode, stack, BSS, `mtvec`, FPU, de parkeerlus van de andere harts (eerst kijken, dan `wfi`), de reset-ingang voor een hart dat de kern uit reset haalt (`reset_pc`: het hart-id uit `set_reset_hart`, niet uit `mhartid`), het T-Head-regime (`mxstatus`, `mcor`, `mhint`, `mhcr`) op elk hart | `cpu/src/riscv/boot.rs` | QEMU; host-test van de start |
| Trap-ingang van de kern: vlag, bron dicht in `mie`, dispatcher wekken; exceptions luid | `cpu/src/riscv/trap.rs` | QEMU; host-tests |
| CLINT: `mtimecmp` in 32-bit spec-volgorde, `msip`, de probe | `cpu/src/riscv/clint.rs` | QEMU; host-tests |
| Slaap en klok: `wfi` op de eigen `mtimecmp` met MIE dicht, TIME-CSR | `cpu/src/riscv/idle.rs` | QEMU; host-tests |
| PLIC als `cpu::irq::Controller` | `cpu/src/riscv/plic.rs` | QEMU (virtio-net-lijn); host-tests |
| TRNG: geen, luid (`HOPOS_RNG_INSECURE`) | `cpu/src/riscv/trng.rs` | |
| PMP-whitelist als TOR | `cpu/src/riscv/pmp.rs` | 5 host-tests |
| Sv39-relocatie in 2 MB-blokken, T-Head-attributen | `cpu/src/riscv/sv39.rs` | 4 host-tests |
| De M-mode-switcher: yield met wektijd, exit, fault, park op de CLINT, de kick als wek, de deurbel (RX), de kill-tick, de intrekking van een slaper, de hercontrole van de lijst bij een koude boot | `cpu/src/riscv/switch.rs` | QEMU: zelftest en appspike |
| De kooi: `kern::cage::{Cage, Cores}` als beleid voor beide architecturen (`Kooi`, `KooiCores`), met de riscv-kant over PMP, Sv39 en de switcher (`Rv`) | `hopos/src/kooi.rs`, `hopos/src/cage_riscv.rs` | QEMU: appspike twee keer door de lifecycle |
| De OS-core: bewoners op het hart van de kern, in zijn idle (de overgang M naar S en terug, de rotatie over sched-blok 0, de wekker op de deadline) | `cpu/src/riscv/oscore.rs`, `idle.rs` (`RvSleeper::host`) | QEMU: zelftest bij elke boot, Hop als bewoner |
| De zelftest van de kooi (yield, exit, escape, kill-tick, slaper) | `board/qemuvirt-riscv/src/cage.rs` | QEMU, elke boot |
| applib op riscv64: `_start` in S-mode, de paniek, de timebase van de control-page (`CTRL_TIMEBASE_HZ`) | `applib/src/rt.rs`, `arch.rs`, `clock.rs` | QEMU: appspike 9 van 9 |
| T-Head-cache-onderhoud in `dev::push`/`pull` | `dev/src/lib.rs` (feature `thead`) | bouwt |
| Board QEMU virt riscv64: ns16550, CLINT, PLIC, virtio-mmio net en blk | `board/qemuvirt-riscv` | `tools/qemu-riscv-test.sh` |
| Board LicheeRV: ns16550 (stride 4), dwmac plus ePHY-recept, CLINT, PLIC, de DW-watchdog (probe, wapenen, aaien), `hopos.cfg` uit een venster in het image, de C906L via het resetblok, appspike in het image | `board/licheerv` | bouwt; host-tests |
| dwmac (DWMAC1000, 3.x) met elke descriptor en buffer op een eigen cacheline, als ops-tabel op de gedeelde stmmac-kern | `driver/nic/stmmac` (feature `dwmac1000`) | 12 host-tests voor de DWMAC1000, 12 voor de kern |
| Linkscript | `hopos/link-riscv.ld` (build.rs zet de basis per board) | |
| FIP uit de donor, gehasht, met `hopos.cfg` en appspike erin | `image/licheerv-agent.sh` | niet op ijzer |
| De kern-flip | geen: de switch-code draait uit het kern-image, dus riscv64 flipt (nog) niet; de haak weigert vóór de 202 (`hopos/src/flip.rs` `FLIPS`, docs/flip.md) | |

## Bouwen en draaien

```sh
cargo build --target riscv64gc-unknown-none-elf -p hopos --features board-qemuvirt-riscv
cargo build --target riscv64gc-unknown-none-elf -p appspike
sh tools/qemu-riscv-test.sh               # boot, kooi-zelftest, appspike in slot 1 en 2, de toets van buiten
APP= sh tools/qemu-riscv-test.sh          # alleen de boot-poort
sh tools/qemu-riscv-test-hop.sh           # de kring: Hop op hart 0, een jobspec, appspike op hart 1, de herstart

sh image/licheerv-agent.sh                                  # target/licheerv/fip-licheerv.bin
CFG=~/lrv.cfg APP=appspike sh image/licheerv-agent.sh /dev/diskN   # met config en appspike, op de kaart
```

Twee builds, niet één: de feature `thead` (in `dev` en `cpu`) komt alleen mee
met `board-licheerv`. Bouw daarom per board met `-p hopos --features ...`,
nooit beide boards in één cargo-aanroep: dan unificeert cargo de feature en
draagt het QEMU-image T-Head-instructies die daar illegal zijn.

De QEMU-regel (`tools/qemu-riscv-test.sh`):

```sh
qemu-system-riscv64 -M virt -m 1G -smp 2 -bios none -nographic \
  -kernel target/riscv64gc-unknown-none-elf/release/hopos \
  -global virtio-mmio.force-legacy=false \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:10100-:10100 -device virtio-net-device,netdev=n0 \
  -drive file=disk.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 \
  -device loader,file=appspike.stripped,addr=0xa8200000,force-raw=on \
  -device loader,addr=0xa8100000,data=<maat>,data-len=8 \
  -device loader,addr=0xa8100008,data=0,data-len=8
```

Gemeten 29-09 (QEMU 11.0.2), de markers van de poort, twee keer groen:

```text
cage: riscv switcher on hart 1: yield resumed at 0x5000000c, exit state 4, escape state 4 with mcause 7 mtval 0x80000000 vec 1, spinner state 4 after the kill tick (1210 us), sleeper state 2 -> 4 HOPOS_RV_CAGE_UP
boot: HopOS v3.0.0 on qemuvirt-riscv, EL3, 2 cores (big), 1024 MB DRAM HOPOS_BOOT gen=1 stamp=dev
cage: hart 1 (core 1) in the switcher: wake 0x2004008, bell 0x2000004, sleep cap 100000 ticks, kill tick 100000 ticks, reset false HOPOS_RV_HART_UP
cage: slot 1 built: part 0xa6000000+0x2000000 -> va 0x50000000, satp 0x80000000000a7e12, pmp 4 entries cfg 0x8000f00, ctrl 0xa7e00000, core 1 (hart 1)
cage: slot 1 dispatched to hart 1 (BootPending + msip) HOPOS_RV_DISPATCH
HOPOS_SLOT_START slot=1 core=1 cpu=1 entry=0x50010000 part=0xa6000000+0x2000000
slot 1: HOPOS_APPSPIKE_TIMER ok slept_ns=10521000
slot 1: HOPOS_APPSPIKE_NET ok ip=10.100.0.2 dial_us=5441 tx_kicks=4 log_us=540 flush_us=1679
slot 1: HOPOS_APPSPIKE_FS ok file=hallo.txt size=62 list=1 us=12855
slot 1: HOPOS_APPSPIKE_DONE pass=9 fail=0
HOPOS_SLOT_DONE slot=1 exit=0
slot 1: stopped, partition and core released HOPOS_SLOT_STOPPED
HOPOS_SLOT_START slot=2 core=1 cpu=1 entry=0x50010000 part=0xa6000000+0x2000000
slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0
HOPOS_SLOT_DONE slot=2 exit=0
slot 2: stopped, partition and core released HOPOS_SLOT_STOPPED
```

"EL3" op de boot-regel is de privilege-modus: 3 = machine mode. De
`dial_us` (5,4 ms) is hoger dan op arm64 (2,9 ms): een app kan de kern-hart
nog niet bellen (geen HVC #6-tegenhanger), dus een TX wachtte op de failsafe
van de switch. (Sinds 01-11 is de failsafe weg en belt de app met de
kick-ecall, a7 = 2.)

### De donor van de LicheeRV

`image/licheerv-agent.sh` heeft twee vendor-bestanden nodig (geen eigen
werk): `image/firmware/licheerv/donor-fip.bin` en
`image/firmware/licheerv/fiptool.py`. Beide komen van Sipeed, en beide zijn
op 30-09 teruggevonden en gehasht:

| Bestand | Herkomst | sha256 |
| --- | --- | --- |
| `donor-fip.bin` (440832 bytes) | Release `20260114` van [sipeed/LicheeRV-Nano-Build](https://github.com/sipeed/LicheeRV-Nano-Build/releases/tag/20260114), asset `2026-01-14-16-03-d4003f.tar.xz`; daarin `2026-01-14-16-03-d4003f.img`, partitie 1 (FAT16, type 0x0C, LBA 1 tot 32768, 16 MiB), het bestand `fip.bin` in de root. Naast hem staan daar `boot.sd`, `ver` (`2026-01-22-14-17-d4003f.img`) en de vlagbestanden. | `d85e68836f57a9fcb1bbfba3c1ccf93d1b062ac168305d7f0cc83a72a796c6b9` |
| `fiptool.py` (25165 bytes) | Dezelfde repo, branch `main`: `fsbl/plat/cv181x/fiptool.py` (niet de `cv180x`-versie ernaast, die een ander hash heeft). | `cc1d37d0d8fbcb3e6180c0403bcf4fa5c7038306be13779f3b8b948915084bf5` |

Dit is de donor; sinds 02-10 staat hij in de repo
(`image/firmware/licheerv/`), en het script vindt hem daar zelf:

```sh
LICHEERV_DONOR_SHA256=d85e68836f57a9fcb1bbfba3c1ccf93d1b062ac168305d7f0cc83a72a796c6b9 sh image/licheerv-agent.sh
```

Opnieuw maken: pak de tar uit (1,7 GB image), en haal `fip.bin` van
partitie 1, bijvoorbeeld op de Mac met
`hdiutil attach -readonly -imagekey diskimage-class=CRawDiskImage 2026-01-14-16-03-d4003f.img`
en een `cp` vanaf het gemounte `boot`-volume; of zonder mount met
`dd if=2026-01-14-16-03-d4003f.img of=p1.img bs=512 skip=1 count=32768` en
een FAT-lezer (`mcopy -i p1.img ::fip.bin donor-fip.bin`).

Niet elke Sipeed-release geeft dezelfde bytes. De `fip.bin` van release
`20251230` (`2025-12-30-20-00-6073d5.img.xz`) is even groot, met dezelfde
FSBL (bouwtijd `2025-12-29T19:21:03+08:00`), DDR-parameters en OpenSBI,
maar een andere U-Boot in het LOADER_2ND-deel (29832 bytes verschil vanaf
offset 0x28208); sha256
`ead936116224c25467917859594d6f5f9af2fa883b52e19be86c780ff770494d`. Die
U-Boot wordt nooit gestart (wij zijn de monitor), maar de FSBL pakt hem wel
uit naar 0x8020_0020, onder RUNADDR; gemeten is alleen de donor hierboven.
Zet daarom `LICHEERV_DONOR_SHA256`, zodat het script een andere donor
weigert.

## De kooi

Op QEMU bewezen (29-09) door de lifecycle van de kern: appspike in slot 1
en slot 2, na elkaar op hart 1, elk in zijn eigen partitie achter PMP plus
Sv39, met al zijn toetsen groen, exit 0 en de bevestigde stop. Het beleid
(bouwen, starten, stoppen, intrekken, stil) is voor beide architecturen
hetzelfde (`hopos/src/kooi.rs`); per ISA verschillen alleen de registers en
de tabellen (`hopos/src/cage_riscv.rs` naast `cage.rs` van arm64), en de
bewonerslijst van elke core is één implementatie (`cpu::el2::roster`):

```text
ARM (cage.rs)                     RISC-V (cage_riscv.rs)
stage-2-tabel + VMID              PMP-whitelist (TOR) + Sv39 in de staart
trampoline, x0 = control-page     koude boot door de switcher, a0 = page
mailbox + SEV, koud PSCI CPU_ON   BootPending in de lijst + msip
revoke: tabel nul + TLBI          CTX_REVOKE: yield, kill-tick, rotatie;
                                  of het resetblok (C906L)
OS-core (Hop naast de kern)       dezelfde vorm: `cpu::riscv::oscore`
SMP-eenheden, sharegroepen        één bewoner per app-hart
```

- **Bouwen** (`build`): eerst uit elke bewonerslijst en het ctx-blok leeg,
  dan de control-page (entry, slot, `CTRL_IDLE_MODE` = yield, de wandklok,
  `CTRL_TIMEBASE_HZ`), de ringen, de Sv39-tabel in `ABI_MAP_OFF` van de
  staart (het app-RAM rwx, de staart als device zonder T-Head-cachebits;
  een bewoner van de OS-core deelt de L1 met de kern en krijgt de staart
  gecachet: 03-10 de pull over de draad van 3,9 naar 9,8 MB/s),
  één TOR-venster over de claim plus de deny-all, het regime in
  `CTX_REGIME`, en de RX-kop voor de deurbel (`CTX_RING_HEAD_PA`). Het
  app-venster is 768 MB: de staart draagt een wortel plus één tabel
  (`ABI_MAP_PAGES` = 2), dus het venster blijft in de gigabyte van
  `LINK_BASE`.
- **Starten** (`dispatch`): het slot in de lijst van zijn hart, staat
  BootPending, de bel. De switcher leest bij een koude boot de lijst NÁ de
  staat nog eens: een slot dat naar een ander hart verhuisde, start nooit
  op twee.
- **Slapen**: een app-hart idlet met de yield (a0 = wektijd); een `wfi` van
  een bewoner wekt nooit. De switcher slaapt voor hem op de CLINT, en wordt
  wakker op de wektijd, de bel (dan krijgt elke slaper één beurt), of de
  deurbel (de RX-kop groeide voorbij de drempel op `CTRL_RX_DOOR`, zoals
  `el2::rx_due` op ARM).
- **Stoppen**: de kill-vlag op de control-page en de bel; de app ziet hem in
  zijn heartbeat (50 ms) en exit. Weigert hij, dan `CTX_REVOKE`: de switcher
  leest het bij elke yield, bij elke kill-tick (10 ms, alleen MTIE aan
  terwijl een bewoner draait; een tick hervat dezelfde bewoner, geen
  preemptie), en bij elke ronde over een slaper. Een hart met een resetblok
  en zonder tick gaat in reset en opnieuw de switcher in; de C906L heeft
  sinds 02-10 de tick (de tijdschijf van zijn gedeelde hart), dus daar is
  de reset niet meer het mes.
- **Stil** (`quiet`): de ctx-staat is Empty of Dead. Dead schrijft de
  switcher pas na de volledige cache-veeg van het hart (de teardown), dus
  dan is de partitie echt terug.

Wat de zelftest van het board (`board/qemuvirt-riscv/src/cage.rs`) bij elke
boot bewijst, vóór de lijm: de koude boot met relocatie, de yield en de
hervatting op `mepc + 4`, de exit, de whitelist (een store naar de kern:
mcause 7), de kill-tick (een bewoner die nooit yieldt, dood 1,2 ms na zijn
intrekking bij een tick van 1 ms), en de intrekking van een slaper (Saved
naar Dead zonder hervatting).

## De OS-core

Het hart van de kern is deelbaar (PORT.md beslissing 2), zoals op arm64:
een core als elke andere, als sharegroep `system` (`kern::pool::SYSTEM_GROUP`),
met de kern als vaste bewoner met voorrang. Een job met de tag
`"sharegroup":"system"` woont erbij, en een job zonder groep die geen vrije
app-core vindt ook (`HOPOS_PLACE_SYSTEM`, docs/apps.md). Hop woont er naast
de kern (`Placement::hop`, sharegroep `hop`) als het board dat kiest
(`HOP_ON_OS_CORE`: QEMU wel, de LicheeRV niet), en de idle van de executor
is een beurt voor de bewoners (`cpu/src/riscv/oscore.rs`,
`RvSleeper::host`). De overgang is een functie: `enter` bewaart ra, sp, gp,
tp en s0..s11 van de kern, zet `mtvec` op een eigen trap-ingang en de kooi
van de bewoner (PMP met teruglezen, `satp`), en `mret`'t. Elke trap van de
bewoner (een yield, een exit, een fault, of een machine-interrupt: de PLIC,
de kick, de wekker op de deadline van de executor, die in S-mode altijd
genomen worden) bewaart hem in zijn ctx-blok en keert terug in de kern
alsof `enter` klaar was. De interrupt blijft pending en de kern neemt hem
zodra hij zijn masker opent, zoals na een `wfi`. De beurt is hooguit 10 ms
(`TURN_CAP_NS`). Wie er aan de beurt is, beslist `cpu::el2::next` op riscv64
en arm64, met de regel van de switcher van een gedeelde app-core (wie werk
heeft, een idle is een yield, RX achter de deurbel maakt een slaper meteen
due, de core slaapt pas als niemand werk heeft); de kern is zelf geen
bewoner en gaat altijd voor. Een intrekking op de OS-core is een aanvraag
plus een kick (`CTX_REVOKE` en `CTX_KICK_PENDING`, `el2::recall`), en
`el2::next` is de enige plek die er een dode bewoner van maakt, vóór zijn
volgende beurt en zonder dat hij nog één instructie draait: de terugweg
(`settle`) leest de intrekking niet, op beide architecturen. Zo doet KVM
het ook (`KVM_REQ_VM_DEAD`, gelezen in `vcpu_enter_guest`).

De kern gebruikt geen f-registers (0 f-instructies in het image, getoetst
door `tools/qemu-riscv-test.sh`), dus de overgang bewaart f0..f31 niet: wat
een bewoner erin had, staat er bij zijn volgende beurt nog. Hop zelf
gebruikt ze wel (38 f-instructies in `agentd-hopos`, gemeten 29-09).

Gemeten 29-09 op QEMU:

```text
oscore: hart 0 self-test timer=Some((Timer, 1645)) yield=Some((Yield, 75)) kick=Some((Ipi, 64)) exit=Some((Exit, 71)) (back, us) HOPOS_OS_SELFTEST ok
HOPOS_HOP_START slot=1 core=0 cpu=0 entry=0x50010000 part=0xa4000000+0x4000000 image=3614256 env=149
slot 1: hop: agent up node=hopos-qemu cluster=hopos agent=:8080 leader=:9080 cores=1 HOP_UP
HOPOS_SLOT_START slot=2 core=1 cpu=1 entry=0x50010000 part=0xa2000000+0x2000000
slot 2: HOPOS_APPSPIKE_DONE pass=9 fail=0
slot 1: hop: adopted 1 running cage(s) from the saved state HOP_ADOPTED
os(in=803 irq=5 ipi=0 timer=44 yield=754 exit=0 fault=0 idle=6218 res_ms=153 kicks=0)
```

`tools/qemu-riscv-test-hop.sh` bouwt `agentd-hopos` met `tools/hop-build.sh`,
net als de aarch64-toetsen: tegen de applib, abi en sync van deze werkboom.

Wie wat schrijft (de regel van de cachelines, de C906 is niet coherent,
30-07): van het sched-blok schrijft de kern alleen regel 1..3 (lijst,
lengte, wekker, bel, slaap, tick), de switcher regel 0 (lopend slot,
rotor); van het ctx-blok schrijft de kern alles vóór BootPending en daarna
alleen `CTX_REVOKE` (een eigen regel), de switcher de rest.

## De checklist voor de LicheeRV, in bootvolgorde

Console: de UART0-pinnen van de LicheeRV (GND, TX, RX), 115200 8N1 (de FSBL
zet hem; HopOS doet geen init). Bouw met
`CFG=~/lrv.cfg APP=appspike sh image/licheerv-agent.sh /dev/diskN`, met in
`lrv.cfg` minstens `hopos.node=<naam>` (dan een eigen MAC).

1. **De FSBL springt ons in.** Verwacht na de FSBL-regels meteen de bunny en
   `runtime ... none/riscv64`. Niets na de FSBL: RUNADDR klopt niet met
   KERN_BASE (het script weigert een ELF-entry die niet 0x8400_0000 is), of
   de donor zet LOADER_2ND nog op een runaddr (het script nult het
   param2-blok; "0 param2 blocks" is een andere donor).
2. **`boot: LicheeRV Nano (SG2002), machine mode monitor`.** Daarna
   `board: CLINT: mtimecmp writable`. `HOPOS_CLINT_FAIL` betekent dat de
   c900-CLINT de 32-bit-schrijf niet vasthoudt: de kern pollt dan (werkt,
   verbruikt meer). Nooit een lees op 0xbff8 (`mtime`): die bestaat niet
   (bus-fout, 30-07).
3. **`trng: WARNING ... HOPOS_RNG_INSECURE`.** Hoort er altijd te staan: er
   is geen hardware-TRNG. Geen geheimen van waarde op deze node.
4. **`watchdog: probing the DW-WDT at 0x3010000`**, dan `DW-WDT answers, the
   watchdog task may arm it`. Stilte na de probe-regel is een bus-fout op het
   WDT-blok: de probe eruit en melden.
5. **`config: hopos.cfg from the window in the image, N bytes HOPOS_CFG_UP`**
   (het venster van elke kern, [boards.md](boards.md)). `HOPOS_CFG_NONE`:
   een leeg venster (gebouwd met `CFG=` zonder pad). Later in de boot
   `HOPOS_CFG_BAD` als het venster kapot is of geen UTF-8 (`image/hopcfg.py`
   en `hop image` weigeren dat al bij het schrijven).
6. **`HOPOS_BOOT`** met `licheerv, EL3, 2 cores`. Een `exception: illegal
   instruction` vóór deze regel is het T-Head-regime (`mxstatus`
   THEADISAEE): vergelijk `mxstatus` met de gemeten FSBL-waarde 0xc0638000.
7. **`HOPOS_TICK 1..3`**: de slaap op de CLINT. Ticks die haperen of
   `sleeps` die niet oplopen: de `wfi` wekt niet op `mtimecmp` (de
   01-08-klasse); de vangrail van 2 ms houdt de node dan wel levend.
8. **De watchdog**: `watchdog: hardware reset armed (DW-WDT at 0x3010000, TOP
   13 (21474 ms), reset on expiry) ... HOPOS_WD_ARMED`, dan na de lease
   `HOPOS_CANARY_LIVE`. Onomkeerbaar tot de reset, en er is geen knop om
   hem uit te zetten (O2 van 01-11). Een node die na ~21 s
   herstart zonder `HOPOS_CANARY_MISS` ervoor: de pets komen niet aan (de
   restart-schrijf naar CRR).
9. **Het net**: `net: dwmac version 0x1037, PHY 0 id 0043:5649, link
   pending (autoneg)`, `net: dwmac irq 31 on the PLIC (RX and TX done)
   HOPOS_NIC_IRQ`, dan `HOPOS_NIC_UP`, `net: link up at 100 Mbps after N
   ms HOPOS_LINK_UP` (de probe wacht niet meer, 01-11) en `HOPOS_NET_UP`. Zonder `hopos.mac` en
   `hopos.node`: `HOPOS_MAC_FIXED` (een tweede LicheeRV op hetzelfde LAN
   botst). `version reads 0`: de klokgates (REG_CLK_EN_0 bit 25/26). `no PHY
   on the MDIO bus`: het ePHY-recept kwam niet aan (volgorde niet
   veranderen). Geen lease: kijk naar `rx_bad_len` en de diag-regel van de
   dwmac; een ring die na een paar frames stilvalt is de les van 30-07 (een
   invalidate die een CPU-schrijf weggooit), en de dwmac legt daarom alles op
   eigen cachelines.
10. **Geen opslag**: `disk: none` is juist; er is geen SD-driver.
11. **De rolverdeling** (sinds 03-10): de kern blijft op de C906B (1 GHz),
    waar de FSBL hem start: `the kern on hart 0 (C906B), app hart 1
    (C906L)` in de boot-regel. Alleen de PLIC van de C906B heeft de dwmac
    (bron 31, zie "Niet gedaan"), dus `net: dwmac irq 31 on the PLIC
    HOPOS_NIC_IRQ` en `HOPOS_NET_PUMP` met de lijn in plaats van de pomp
    van 300 µs. De kern slaapt er op `mtimecmp` met `wfi` (twee weken
    productie in Go); CLINT-index en PLIC-context zijn 0 (de CLINT en de
    PLIC zijn per core, elke core is voor zichzelf hart 0). Hij deelt zijn
    core als groep `system`: welcome (geen tag, geen vrije core) komt
    erbij met `HOPOS_PLACE_SYSTEM`. Hop heeft de C906L voor zich
    (`HOP_ON_OS_CORE` = false op dit board): `HOPOS_HOP_START slot=1
    core=1 cpu=1`.

    De loterij van 03-10 ochtend is dezelfde dag weer verwijderd: de PLIC
    van de C906L heeft geen ethernetbron, de kern hoort op de C906B.
12. **Het app-hart**: `cage: hart 1 (core 1) in the switcher: wake
    0x74004000, bell 0x0, sleep cap 0 ticks, kill tick 250000 ticks, reset
    true HOPOS_RV_HART_UP`. De C906L komt uit reset op de reset-ingang en
    haalt zijn eerste werk uit het postvak. Elk hart neemt het T-Head-regime (I-cache aan: anders
    ~77x trager, gemeten 18-08) en gaat de switcher in. Hij SPINT (geen
    slaap): op de C906L is slapen nooit bewezen, de C906B droeg in Go twee
    weken `wfi` maar niet met deze switcher; er is geen bel van de kern
    naar hem (de CLINT is per core). Zijn comparator (`mtimecmp(0)`, per
    core) draagt de kill-tick van 10 ms: die vuurt alleen terwijl een
    bewoner draait en is de tijdschijf van de apps die een hart delen; op
    de C906L is het resetblok het tweede mes. Op ijzer te zien:
    `tools/qemu-riscv-test-share.sh` op QEMU, en op het board een `BURN=1`
    naast welcome die op :80 blijft antwoorden.
12. **appspike, twee keer**: `HOPOS_RV_DISPATCH` met "picked up by the
    spinning switcher", `HOPOS_SLOT_START slot=1 core=1 cpu=1`, dan de regels
    van de app. Verwacht `HOPOS_APPSPIKE_DONE pass=8 fail=1`: de FS-toets
    faalt eerlijk (geen schijf), en dus `exited with 1 HOPOS_SLOT_EXIT_FAIL`
    en daarna toch de stop en slot 2. Wat hier het eerste echte bewijs is:
    - de klok: de app meet zichzelf met zijn eigen klok, dus de toets is
      de meetregel van de kern bij de exit, `slot 1: +N ms app=3 ... beat=B`:
      de heartbeat slaat elke 50 ms app-tijd, dus B hoort bij N/50 (QEMU:
      +352 ms, beat=7). B bij N/20 is een app die met 10 MHz rekent op de
      25 MHz van de SG2002: `CTRL_TIMEBASE_HZ` kwam niet aan;
    - de kooi: een fault met `vec=0 esr=0x7` in de eerste regels is de
      TOR-whitelist die op dit silicium nooit geldig gemeten is (30-07);
      `esr=0xc`/`0xd`/`0xf` is de Sv39-tabel (de T-Head-bits, 31-07);
      `vec=64016` (`FAULT_CAGE_VERIFY` 0xFA11, min 1 op de regel) is een
      `pmpcfg0` die anders terugleest dan de kern hem schreef;
    - de caches: een app die hangt na READY of de NET-toets niet haalt, is
      een regel die de kern of de switcher niet veegde (de C906L is niet
      coherent met de C906B).

## Niet gedaan

- **Hop op de LicheeRV, op ijzer**: sinds 3.0.1 bakt de release Hop
  (riscv64, `tools/hop-build.sh`) in de kern als eerste bewoner
  (`STAGE=...` van het image-script, `HOPOS_EMBED` in build.rs; sinds
  01-11 is een gebakken image altijd Hop, zonder rol). De kring op QEMU virt is groen, en
  sinds 03-10 (R3) ook op het board: Hop in slot 1 met `HOPOS_PRIVILEGE` (op
  dit board in een partitie van 10 MiB, `HOP_MEM` in hopos/src/slots.rs;
  Hop meet er 0,5 MB), sinds R11 op de C906L voor zich (core 1),
  welcome in slot 2, en na een koude flip zaait Hop welcome opnieuw uit
  `hopos.init` (`HOP_INIT_SEEDED`).
- **De kick van een app naar de kern op de LicheeRV**: de vorm staat sinds
  02-10 (`ecall` met a7 = 2; de switcher schrijft een 1 op `SCHED_OS_BELL`,
  de rotatie van de OS-core neemt hem als een yield naar nu; de app kickt
  alleen met `IDLE_KICK` op zijn control-page). Op QEMU virt is de bel
  `msip` van hart 0: appspike `dial_us` van mediaan 7,5 en 9,6 ms naar 4,4
  en 4,0 ms (tien runs per kant, TCG). De C906L heeft geen bel naar de C906B
  (de CLINT is per core); de kandidaat is de mailbox van de CV181x
  (0x0190_0000, PLIC-bron 101 in de vendor-DTS; Linux
  `drivers/mailbox/cv1800-mailbox.c`: `MBOX_EN_REG(cpu)`, dan `MBOX_SET_REG`
  0x60 met het kanaalbit, en de ontvanger wist met `MBOX_SET_CLR_REG`).
  Tot die bewezen is, hoort de kern een app na elke slaap van hooguit 2 ms
  (`cpu::riscv::idle::WFI_CAP_NS`, docs/description.md §4.11). De 300
  µs-poll van de pomp is weg (01-11): een vastgehouden frame wacht op de
  TX-completion van de dwmac, op dezelfde lijn.
- **De NIC-interrupt op de LicheeRV bestaat niet op de C906L.** De TRM
  (sophgo-doc, SG200X, "Interrupt number and Interrupt source mapping for
  Slave RISCV C906 @ 700Mhz") kent op de PLIC van de C906L geen ethernetbron
  (31 is daar UART1), en de vendor-FreeRTOS voor dat hart zet
  `ETH0_SBD_INTR_O` op NA; daarom staat de kern op de C906B. Daar hangt de
  dwmac aan PLIC-bron 31 (Linux `cv180x.dtsi`
  `SOC_PERIPHERAL_IRQ(15)` = 15 + 16, vendor-DTS 31, TRM "Master RISCV
  C906" 31 Ethnet0). Wil de PLIC hem niet, dan start de NIC niet (01-11,
  O3). Een app op de C906L hoort de kern na elke slaap van hooguit 2 ms
  (`WFI_CAP_NS`), want er is geen bel van de C906L naar de C906B.
  `HOPOS_NIC_IRQ` zegt het bij boot; NOG NIET OP IJZER (de rolverdeling
  van punt 11 is de eerste kaart die het toont).
- **SMP-apps** op riscv64: één core per bewoner. Meerdere bewoners op één
  hart kan wel (sinds 3.0.3 telt een kooi niet als core): de switcher
  bewaart sinds 02-10 f0..f31 en `fcsr`, en de kill-tick is op een gedeeld
  hart de tijdschijf (10 ms), de enige preemptie in HopOS
  (`tools/qemu-riscv-test-share.sh`).
- De kern-flip op riscv64: de switch-code draait uit het kern-image (op
  arm64 staat een kopie in de plan-regio), dus `cage_riscv::adopt` weigert,
  en er is geen koude flip meer (01-11). De haak weigert vóór de 202; de
  trampoline in machine mode ging eruit op 07-11 (F2, docs/flip.md). Een
  zwarte doos voor riscv64 ook niet.
- De LicheeRV: slapen in de switcher (de C906B droeg in Go twee weken
  `wfi`; een soak met deze switcher, dan een slaapgrens in `app_hart`), en een SD-driver (dan `hopos.cfg` naast
  `fip.bin` in plaats van in het image).
