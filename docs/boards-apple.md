# Mac mini M4 op HopOS v3

De Mac mini M4 (Apple t8132, J773g, Mac16,10: 6 E-cores "sawtooth" in
cluster 0, 4 P-cores "everest" in cluster 1, 24 GB vanaf 1 TiB, een Broadcom
57762 achter een Apple-PCIe-rootpoort, de SSD achter de ANS-coprocessor).
Alles hieronder is op de host getest en bouwt voor het target
(`image/apple-m4.sh` levert het raw image). Er is geen QEMU-model van de M4;
niets ervan heeft in v3 op ijzer gedraaid. De gedateerde lessen (28-08 tot
04-09) staan in het commentaar van de code.

## Wat er is

| Deel | Waar | Getest |
| --- | --- | --- |
| ADT-lezer: nodes, properties, paden, `-`/`_` gelijk, de ranges-vertaling, diepte begrensd | `fw/src/adt.rs` | 9 host-tests (plus afkapping op elke byte en een te diepe boom) |
| boot_args-lezer: RAM-contract, framebuffer, het ADT-adres met de virt-naar-fys-omrekening modulo 2^64 (30-08) | `fw/src/xnuboot.rs` | 3 host-tests met de getallen van 29-08 en de twee vormen van virt_base |
| GPT: tabel lezen, het grootste gat TUSSEN of NA de partities | `fw/src/gpt.rs` | 5 host-tests (de echte M4-tabel van 30-08) |
| AIC als `cpu::irq::Controller` (claim = ack, complete = masker open, het 4-bit doel), plus de fast IPI (kick, ack) | `driver/aic` | 6 host-tests op een nep-registerblok |
| tg3 (BCM57762): reset, MAC, MDIO/PHY, ringen, `netdev::Device`, INTx met tagged status (ack = mailbox dicht, heropenen op de tag bij een lege ring, HOSTCC_MODE_NOW) | `driver/nic/tg3` | 24 host-tests, o.a. de drie lessen van 29-08 (INDIR_ACCESS, PCI_COMMAND, NIC_ADDR van de std-ring), FCS, en ack/heropenen op de tag |
| RTKit-mailbox (ASC) en de SART v3 | `driver/rtkit` | 16 host-tests tegen een nep-coprocessor |
| SMC: sleutels, floats, sensoren, de warmste | `driver/smc` | 7 host-tests |
| ANS-NVMe: het ANS-transport onder de NVMe-core (lineaire SQ, TCB's, NVMMU, coprocessor), de GPT als capaciteit, het schrijfvenster, nette shutdown | `driver/nvme/src/apple.rs` | 7 host-tests op de nep-controller van de core: de vier ANS-lessen, secure-bar, CID en TCB per slot met de invalidatie, weigering buiten of zonder venster, 512-naar-4K, een omgevallen coprocessor; tickets en read-ahead in de core-toetsen |
| Board: bootstub, VHE-ingang, 48-bit-map met PXN, console, boot_args en ADT, cores met klasse, de CPU_ON-haak, AIC, timer-FIQ, fast IPI, PCIe-bring-up, tg3 met zijn INTx-lijn, ANS, SMC, watchdogs, p-states en hun wachter, het config-venster, slot-plan met voorproef | `board/apple` | 22 host-tests (map en PXN, plan, pool uit het contract, tunables, os-core-keuze, config-venster, watchdog-alarm, p-state-plafond, `hopos.cages`) |
| OS-core-rotatie voor `AppleVhe`: de fast IPI als bel, de ack op EL2, de FIQ als `Back::Timer`/`Back::Ipi`; de Apple-kick in de switcher; de CPU_ON-haak en de SCTLR-wis in `cpu::smp`; de VHE-bewuste event-stream | `cpu/src/el2`, `cpu/src/smp.rs`, `cpu/src/idle.rs` | host-tests (het kick-woord, de vectorindex, de haak, de stream-bits); de gedeelde paden op QEMU (virt, UEFI, UEFI+VHE) |
| Linkscript: raw image op 0x101_0000_0000, stub op 0, kern op 0x10000 | `hopos/link-apple.ld` | link-asserties; `image/apple-m4.sh` toetst het parameterblok |
| Loader (m1n1-proxy), meetcyclus, console, installer | `image/apple/` | de meetbank |

## Het plan

| Bereik | Wat | Mapping |
| --- | --- | --- |
| 0x100_0000_0000 tot `top_of_kernel_data` | iBoot: boot_args, ADT, trust cache, m1n1 met zijn spin-table | Device (2 MB) |
| `top_of_kernel_data` tot 0x101_0000_0000 | pool, laag deel | Device (2 MB) |
| 0x101_0000_0000 + 256 MB | kern-RAM: stub, scratch 0xE000, param-blok 0xE100, config 0xF000, kern 0x10000, BSS (met de tabellen), stack, heap | Normal WB |
| + 16 MB | DMA: tg3 8 MB, ANS 4 MB + RTKit-buffers 3 MB + SMC 1 MB | Normal-NC |
| + 16 MB | control-pages, kooien, flip-recorder, zwarte doos | Device |
| + 64 MB | boot-scratch, handoff, staging (met magic) | Normal WB |
| 0x101_1600_0000 tot `phys_base + mem_size` | pool, hoog deel | Device (2 MB) |
| alles onder 512 GB | MMIO (AIC, dockchannel, APCIe, ANS, PMGR) | Device (1 GB) |

Nooit een 1 GB-blok boven 2^40: dat geeft op dit silicium een "address size
fault, level 0" (28-08). De pool komt uit iBoot's contract (29-08: 23,7 GB in
twee regio's); zonder boot_args 1 GB boven het venster, luid.

## Bouwen en laden

```sh
sh image/apple-m4.sh                         # target/apple-m4/hopos-apple.img
CFG=hopos-m4.cfg sh image/apple-m4.sh        # met hopos.cfg in het venster van het image
APP=appspike sh image/apple-m4.sh            # plus stage.elf voor de staging
PYTHON=~/Git/m1n1/venv/bin/python3 CFG=hopos-m4.cfg \
  sh image/apple/boot-cycle.sh target/apple-m4/hopos-apple.img 90
STAGE=target/apple-m4/stage.elf ROLE=app ...  # een image in de staging
AT=0x10080000000 ...                          # de stub laten verplaatsen
image/apple/console.py /dev/cu.kis-100000-ch-0 60   # zonder m1n1
```

De meetbank (dossier, "Meetbank-recept"): `macvdmtool reboot serial`, laden
over m1n1's USB-gadget, `macvdmtool debugusb`, booten en de console over de
dockchannel (`/dev/cu.kis-100000-ch-0`). Chrome en Spotify houden het
Debug-USB-device vast: dicht voor je begint (29-08). De loader springt met
`P_VECTOR` naar offset 0x800 met x0 = het échte boot_args-blok: dezelfde
overdracht als na `kmutil configure-boot --raw --entry-point 2048`.

`hopos.cfg`-sleutels van dit board: `hopos.oscore=small|big|N`,
`hopos.pstate=off|E,P` (het plafond per cluster, standaard 5,6),
`hopos.disk=off`, `hopos.smc=1`, `hopos.cages=on|off` (zonder waarde:
kooien als de voorproef slaagt).

De config reist in het image, in het venster van elke kern
([boards.md](boards.md), "Het config-venster"): `CFG=hopos-m4.cfg sh
image/apple-m4.sh` of `hop image hopos-apple.img --config hopos-m4.cfg`.
Zonder loader (na de installatie) is dat de enige config. De loader zet
zijn `CFG=` in hetzelfde venster en als tekst op 0xF000; `BARE=1` laat
alles staan (zo boot de node na `kmutil`). 0xF000 is de terugval bij een
leeg venster: de tekst van de loader, of het venster van 4 KB dat
`image/apple-m4.sh` daar tot 04-10 bakte. De bootregel zegt waar de config
vandaan kwam: `cfg: hopos.cfg from the window in the image`, `... from the
0xF000 window`, `... from the loader`, of `cfg: no hopos.cfg ...
HOPOS_CFG_NONE`.
`APP=hop` zonder `CFG=` is luid (25-09: een agent zonder config draaide
als `hopos-<random>` met een open API).

Een kern-flip legt de nieuwe kern plat over het image, en een bundel
draagt normaal een leeg venster. Daarom geeft de draaiende kern zijn
venster mee aan het gestagede beeld (`board::cfgwin::carry`,
`HOPOS_FLIP_CFG`), en daarnaast 0xF000 (`fwinfo::carry_config`); zonder
dat booten geflipte kernen zonder `hopos.pstate=off` en adopteren ze niet
(01-10). Een kern die dat nog niet kan (alles vóór 01-10), krijgt de
config ín de bundel: `CFG=hopos-m4.cfg sh image/flip-bundle.sh apple`.

De p-state-tune draait in `start_interrupts` (niet in `discover`: daar
komen de regels niet op 5555) als recept in stappen met na elke stap een
SError-toets; bij de eerste SError stopt hij (`HOPOS_APPLE_PSTATE_SERROR`).
Op de t8132 laat de schrijf naar P-cluster +0x440f8 een SError achter die
blijft hangen (en +0x48400 bestaat niet); E neemt het woord wel. Zonder die
twee: E 5/8 (2172 MHz), P 6/20 (2352 MHz), geen SError (01-10, stempel M7).

## Wat de kern voor Apple draagt (29-09)

Alles hieronder is gebouwd en op de host getest, en de gedeelde paden
(`cpu::el2::oscore`, `cpu::idle`, `cpu::smp`) zijn op QEMU bewezen tegen
regressie (virt, UEFI, en UEFI onder VHE met neoverse-n1: `HOPOS_OS_SELFTEST
ok`). Op de M4 zelf heeft het nog niet gedraaid: dat is de checklist.

1. **De EL2-smaak in de lijm.** `hopos/src/cage.rs` kiest `AppleVhe` op dit
   board en toetst bij het bouwen dat `board_apple::FLAVOR` dezelfde is.
2. **`OsCore` voor `AppleVhe`** (`cpu/src/el2/oscore.rs`): de kick is
   `Bell::apple(mpidr)`: het IPI_RR-doel `core | cluster << 16` met bit 63
   als "scherp" (het doel van E-core 0 is 0, en 0 is "niet kicken"). De
   switcher van een app-core wist dat bit en schrijft `msr
   s3_5_c15_c0_1` in plaats van ICC_SGI1R (bij een yield en bij HVC #6).
   De peek (`pending`) leest IPI_SR_EL1 bit 0 zonder ack; de kern ackt
   zelf, op EL2, als de beurt als `Back::Ipi` terugkomt (04-09). De
   timer en de kick komen als FIQ: `fiq/lower-a64` (index 10) keert als
   `Back::Timer` (de CNTHP ging af) of `Back::Ipi` terug, een IRQ (de AIC)
   als `Back::Irq`. FMO, IMO en AMO staan in het HCR van elke bewoner.
3. **Een CPU_ON van het board in `cpu::smp`.** `cpu::smp::set_cpu_on`
   (gezet in `discover`) laat `start_one` (de verhuizing naar de OS-core)
   en de koude start van een kooi (`cage.rs`) via `board_apple::cores::cpu_on_mpidr` lopen: m1n1's
   spin-table, of PMGR plus de brievenbus als wij het bootobject zijn. De
   ingangen die zo'n core krijgt (`hopos_smp_entry`, en de trampolines van
   de Apple-switcher) zetten eerst DAIF dicht en SCTLR_EL2.M/C/I uit, zonder
   set/way-onderhoud: m1n1 roept een vrijgegeven core aan als functie met
   zijn MMU nog aan (29-08).
4. **De voorproef.** Het slot-plan (`board_apple::slots::plan`) draait bij
   zijn eerste aanroep, op de OS-core en vóór er één kooi bestaat, dezelfde
   drie beurten als de zelftest van de kern (de CNTHP, een yield, de fast
   IPI naar zichzelf): `slots: OS-core preflight ... HOPOS_APPLE_PREFLIGHT
   ok`. Rood (`HOPOS_APPLE_PREFLIGHT_FAIL`) = het plan weigert luid
   (`HOPOS_SLOT_PLAN`) en de node draait door zonder kooien, zoals vóór
   29-09. `hopos.cages=on` slaat de voorproef over, `hopos.cages=off`
   weigert altijd. De `hopos.cages=on`-rem van vóór 29-09 is weg.
5. **De keuze van de slaap per core** (onveranderd): het board meet de
   timer-FIQ (`HOPOS_APPLE_IDLE`) en kiest WFI of WFE. Een OS-core die de
   kern zelf opbracht (`hopos.oscore=small`), krijgt op t8132 geen
   timer-FIQ (29-08): daar zakt de voorproef op de timer-beurt, en dat is
   de bedoeling.
6. **`cpu::idle::ArmSleeper::new` onder E2H = 1** vervangt in CNTHCTL_EL2
   alleen de bits van de event-stream (`merge_event_stream`): EL1PCTEN en
   EL1PTEN blijven staan. Het herstel in het board is weg (board-uefi houdt
   het zijne, het is daar nu een no-op).
7. **Het PXN-gat.** `board/apple/src/mmu.rs` zet op elk blok dat XN is ook
   PXN (bit 53): onder E2H = 1 is bit 54 alleen UXN, en de kern mocht
   instructies halen uit Device (MMIO, de firmware, het kooi-venster) en
   uit de DMA-regio.
8. **De watchdog** (`hopos/src/watchdog.rs`, de `hw`-module van Apple):
   `board_apple::wdt::{arm, pet, off}` op de primaire van `/arm-io/wdt`,
   30 s op 24 MHz (de klok staat niet in de boom: de regel zegt het).
9. **De klok en de temperatuur** (`hopos/src/telemetry.rs`): de wachter op
   de p-states (`board_apple::wdt::PStateWatch`) meldt
   elke sprong (`HOPOS_APPLE_PSTATE`). De temperatuur staat met
   `hopos.smc=1` één keer in de bootlog (`HOPOS_APPLE_TEMP`); niet
   standaard, want op v2 kwam het INITIALIZE-antwoord nooit (31-08) en
   een half opgestarte RTKit-coprocessor die niemand pollt loopt vol. Op de
   tik staat hij niet (elke meting praat de SMC wakker en weer in slaap).
10. **De wekker van de app-cores** (02-10, `hopos/src/cage.rs`, Go's
    `kern/slots/waker.go`): een app-core idlet met een yield
    (`APP_IDLE_MODE = IDLE_YIELD`) en slaapt in de WFI van de switcher,
    want WFE op EL1 slaapt op de M4 niet (02-09). Elke ms kickt de kern
    elke draaiende app-core met een geyielde bewoner die due is (wektijd,
    kick of RX); een RX-schrijf van de switch kickt de core van het slot
    zelf (en de geyielde secundairen), niet meer mpidr 0. Marker
    `HOPOS_WAKER_UP`; met `hopos.tick=1` staan de tellers op de tik
    (`waker(rounds= seen= kicks= rx=)`). Go 02-09: een app-core van 74 %
    cpu en 1,3 M rondes/s naar 0 % en 47 wekken/s.

## Checklist voor de M4

Per stap wat de console (dockchannel) hoort te tonen, en wat een afwijking
betekent. Onder m1n1 eerst; pas daarna installeren. Bouw met een config:
`CFG=hopos-m4.cfg sh image/apple-m4.sh`.

1. **Eerste licht** vóór de MMU: `hopos x000001000xxxxxxx` (x0 = boot_args).
   Niets: de stub draaide niet (verkeerde offset in de loader) of de
   dockchannel staat elders. x0 = 0: m1n1 gaf geen boot_args (P_VECTOR).
2. **De bunny en `runtime ...`**: de MMU, VBAR en de heap staan. Stil na
   regel 1: de map (`mmu::build`) of het VHE-regime; een
   `exception: ... HOPOS_EXCEPTION` met FAR in het DRAM wijst naar de map.
   Nieuw 29-09: PXN op elk XN-blok; een instructie-abort (EC 0x21) met een
   PC buiten de kern-RAM is dan een sprong die vroeger stil "werkte".
3. `xnuboot: rev 3.x RAM 0x10001374000+24053 MB ... HOPOS_APPLE_BOOTARGS`
   en `adt: ... 10 cores (6 E + 4 P), this is core 6, serial ...`.
4. `watchdog: firmware watchdogs silenced: reg[0] ... (N window(s), ...)`.
   Zonder deze regel reset de node natief na 1:43 (31-08).
5. `cores: via m1n1's spin-table (...)` onder m1n1; `cores: ours` na de
   installatie. `cfg: hopos.cfg from the window in the image, N bytes
   HOPOS_CFG` (of `from the loader`); `HOPOS_CFG_NONE` = een leeg venster.
   `cpufreq: cluster 0 (E): pstate 1 (900 MHz) -> 5/...`. Met `hopos.smc=1`:
   `smc: die 41.5 C at boot HOPOS_APPLE_TEMP`; `HOPOS_APPLE_SMC_FAIL` of een
   `smc: ...`-fout is de meting van 31-08 opnieuw (geen shmem-adres), en
   dan hoort de boot gewoon door te gaan (hooguit ~8 s later).
6. **Met `hopos.oscore=small`**: `oscore: moving the kern from core 6 to
   core 0 ... HOPOS_OSCORE_MOVE`, dan `oscore: the kern runs on core 0
   HOPOS_OSCORE_UP`. Stil daarna: de nieuwe core kwam niet uit m1n1's
   spin-table of niet door `hopos_smp_entry` (SCTLR-wis, dan het regime);
   een `cores: cpu 0 ... not started` zegt waarom. Een "address size fault,
   level 0" in het lage DRAM op die core is het open raadsel van 29-08
   (alleen onder m1n1; opnieuw na de installatie). Zonder `hopos.oscore`
   blijft de kern op core 6 en is er geen regel.
7. `boot: HopOS v3.0.0 on apple-m4, EL2, 10 cores (big), 24576 MB DRAM
   HOPOS_BOOT`.
8. `disk: ans 0x481600000 nvmmu 0x485cc0000 nvme 0x4c5cc0000 sart
   0x485c50000 (v3) secure-bar true`, dan de GPT-regels
   (`iBootSystemContainer`, de APFS-container, `RecoveryOSContainer`), en
   `HOPOS_ANS_UP` met het venster, of `HOPOS_ANS_FULL` als macOS nog niet
   gekrompen is (dan geen hopfs, en dat is goed). Komt de ANS niet ready:
   `resetting the ANS power domain and retrying` hoort er één keer te staan.
9. `irq: AIC v... at 0x381000000: ... target N reaches this core, fast IPI
   and timer FIQ`. `no AIC target reaches this core`: de doel-meting via
   ISR_EL1 werkt niet op dit silicium; de node draait door op de vangrail.
10. `watchdog: hardware reset armed (Apple WDT at 0x..., 30000 ms timeout
    at 24 MHz ...) HOPOS_WD_ARMED`. `no /arm-io/wdt` of `refuses to arm`:
    onbewaakt, luid. Reset de node binnen 30 s na deze regel, dan aait de
    taak niet (de executor staat) of telt de teller niet op 24 MHz (dan
    schaalt de timeout: meet de tijd tot de reset). `dvfs: the APSC governs
    ... HOPOS_APPLE_PSTATE_WATCH`, en daarna alleen regels als een cluster
    springt (`cpufreq: cluster 1 (P) 6 -> 3 HOPOS_APPLE_PSTATE`). Geen enkele
    sprong in minuten idle: de APSC klokt niet terug (het antwoord op de
    vraag van 01-09).
11. `apcie: config 0x1cb0000000 ...`, per stap een regel (een uitgestelde
    abort landt bij de stap die hem veroorzaakte), `apcie: brought up by
    HopOS, 6 power gate(s), 263 tunable(s), 2 of 3 port(s)` (29-08), `apcie:
    link up, endpoint 02:00.0 14e4:1682`, `tg3: ASIC 0x57766 ...`, `tg3:
    LINK UP, 1000 Mb/s full duplex`, `net: tg3 INTx on AIC irq 1253 (port
    2 block 1249 + INTA 4), first interrupt after N us ... HOPOS_NIC_IRQ`,
    `HOPOS_NIC_UP mac=1c:f6:4c:54:fa:90`. Onder m1n1 kan de eerste regel
    `apcie: already up` zijn. `net: tg3 polled, AIC irq ...: the forced
    interrupt did not arrive within 50 ms, port INTSTAT ...`: de lijn kwam
    niet aan (bits 3:0 van INTSTAT zijn INTA tot INTD, Linux
    `PORT_INT_INTx`, gelezen vóór de NIC weer dichtgaat; staat bit 0, dan
    zag de poort INTA en klopt het AIC-nummer uit de ADT niet; een knop om
    een ander te proberen is er niet meer, `hopos.nicirq` is weg sinds
    01-11). De zelftest is Go's
    "gevonden door aflevering" (19-09). Komt de lijn niet, dan start de NIC
    niet: er is geen gepolde terugval meer (01-11, O3). Daarna: `HOPOS_NET_PUMP` zegt `the nic's irq line`.
    De schijf (ANS) hangt sinds 01-11 ook aan de AIC: `HOPOS_ANS_IRQ` noemt
    de lijn van de mailbox en die van de NVMe (woord 3 en het laatste van
    `/arm-io/ans` "interrupts", de volgorde van de M1). NOG NIET OP IJZER.
12. **DHCP** (`HOPOS_NET_UP`) en de tik: `HOPOS_TICK n sleeps=...`.
    Wekken per seconde rond 954 (29-08): `HOPOS_APPLE_IDLE` moet dan `Wfi`
    zeggen. Veel meer: WFE op de event-stream; controleer EVNTIS (ECV).
13. **De voorproef en de kooien**: `slots: OS-core preflight on mpidr
    0x80010100: timer=Some(Timer) yield=Some(Yield) kick=Some(Ipi)
    HOPOS_APPLE_PREFLIGHT ok`, dan `slots: cage up HOPOS_CAGE_UP ...` en
    `oscore: cpu 6 self-test timer=... yield=... kick=... HOPOS_OS_SELFTEST
    ok`. Wat een rode beurt betekent:
    - `timer=Some(Yield)`: de CNTHP-FIQ bereikte deze core niet (de spinner
      gaf na twee termijnen zelf op). Op een core die de kern zelf opbracht
      is dat verwacht (29-08); op core 6 onder m1n1 is het nieuw: kijk of
      FMO in HCR_EL2 staat en of `HOPOS_APPLE_IDLE` `fiq-at-core=true` zei.
    - `kick=Some(Timer)` of `Some(Yield)`: de fast IPI naar zichzelf kwam
      niet als FIQ binnen; `kick=Some(Irq)`: hij kwam, maar IPI_SR_EL1 bit
      0 stond niet (dan is de peek fout, niet de kick).
    - `yield=...` niet `Yield`: de overgang zelf (het `_EL12`-regime, VBAR
      van de os-vectoren); een `HOPOS_EXCEPTION` met ELR in
      `hopos_os_vhe_enter` is dat pad.
    - Een hang zonder regel: de beurt kwam nooit terug. Dan ging een FIQ
      niet naar EL2 (FMO) én gaf de spinner niet op (CNTVCT van EL1 trapt:
      CNTHCTL_EL2).
    Daarna de koude start van een app-core: `cage: slot 1 core 1 cold:
    CPU_ON mpidr=0x80000000 entry=... -> Ok(())`. `Err(Other(-9))` of
    `Err(InvalidParams)` met een `cores: cpu N ... not started`-regel: geen
    spin-table-adres voor die core (loader) of RVBAR niet van ons. Een app
    die niet start na `Ok`: de trampoline (SCTLR-wis, dan VBAR en stage-2);
    m1n1's exception-handler vangt dat onder de loader op de console.
14. **Installeren** (1TR, fysiek; `image/apple/install.sh`): `kmutil
    configure-boot -c hopos-apple.img --raw --entry-point 2048
    --lowest-virtual-address 0 -v "/Volumes/Macintosh HD"`. Daarna moet
    stap 5 `cores: ours` en `cfg: hopos.cfg from the window in the image` zeggen:
    dat zijn de metingen dat RVBAR naar ons image wijst en dat de config
    zonder loader meekwam. Stap 13 loopt dan via PMGR en de brievenbus.
    Terug: dezelfde regel met `m1n1.bin`.

## Op ijzer gezien op M1 (03-10)

- **De scrub van de Device-gemapte pool**: dezelfde Go-app (tamago,
  `cloudflared-arm64-tamago.elf` van de rollende release `apps`, 64 MiB,
  met een opzettelijk ongeldig `TUNNEL_TOKEN`) drie keer na elkaar
  geplaatst met een `DELETE` ertussen, elke keer in dezelfde partitie
  (`part=0x10068000000+0x4000000`, slot 4, core 4; de eerste keer `cold:
  CPU_ON`, daarna `dispatched to parked core 4`). Alle drie dezelfde regels
  (`mem: Go memory limit 27MB`, `appnet: RX doorbell served as
  interrupt`, de banner, `62 MB RAM, arm64 go1.26.4`) en hetzelfde einde:
  exit code 4294967295, de -1 van cloudflared's `UsageError` voor een
  ongeldig token. Geen `HOPOS_SLOT_FAULT`, geen panic. De laatste
  regels van de app (de reden) halen de console en de taaklog niet: de
  kern sluit het slot (`HOPOS_SYSTEM_EVICTED`) voor de servicer ze leegt.
- **De koude flip zonder PSCI**: niet op ijzer gevraagd. De kern weigert
  vroeg in `flip::prepare` (`HOPOS_FLIP_COLD_NO_PSCI`, voor de staging en
  het bevriezen), maar Hop stopt bij `"cold":true` eerst al zijn taken op
  de node (`HOP_FLIP_COLD_STOP`, `agentd-hopos/src/node.rs`) en vraagt de
  kern pas daarna; een weigering zet ze terug (`HOP_FLIP_COLD_BACK`). Met
  een live werklast op de node is dat geen toets zonder bewoners te
  stoppen.

## Niet gedaan

- Een mini-proxy in het image (elke iteratie na de installatie kost een
  1TR-bezoek).
- Hop op de OS-core: `hopos/src/slots.rs` `os_pool` sluit `AppleVhe` nog
  uit van de gedeelde OS-core (een ander spoor), dus Hop krijgt op de M4
  een app-core. Weghalen zodra stap 13 op ijzer groen is.
- `pmgr_reset` als herstelpad na een crash buiten de retry in
  `probe_disk` (die er wel is).
- De temperatuur op de tik: vraagt een SMC die wakker blijft en gepolld
  wordt (zijn syslog), niet een open-meet-slaap per seconde.
- Een core die in `cpu::el2::hold` wacht terwijl de kooien weigeren (een
  verhuizing met een rode voorproef), spint daar: WFE slaapt op de M4 niet.
- Een timer van een bewoner zelf (CNTV of CNTP op EL1 met de interrupt
  aan) is op Apple een FIQ die de kern als device ziet; de bewoners van
  HopOS slapen via de HVC-yield en zetten die niet aan.
- De framebuffer: iBoot laat er een achter, maar niemand scant hem uit
  (30-08); bewust uit.
