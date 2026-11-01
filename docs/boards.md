# Boards: de config van elke node, en de UEFI-machines (O6N en Altra)

Eerst de config die elk board deelt (de gedeelde bestanden en het
config-venster in de kern), dan de UEFI-boards. Stand 29-09-2026, nacht
(interrupts over PCI, watchdog, klok en thermiek erbij). Wat er voor de
Radxa Orion O6N en de Ampere Altra
in v3 gebouwd is, wat je bij de eerste boot op de console hoort te zien, wat
er nog niet is, en wat je meet. De lat is die van v2 ([measurements.md](measurements.md)):
een v3-board is pas klaar als het op elk gemeten punt minstens doet wat v2 deed.

## De config (alle boards)

De config van een node is drie lagen uit `image/cfg`, aaneengeplakt in het
venster van de kern, in deze volgorde; de laatste waarde van een sleutel
wint (`fw::bootcfg::get`), een herhaalde sleutel (`hopos.init[]`) telt alle
lagen:

| Laag | Bestand | Wat erin staat |
| --- | --- | --- |
| 1, de default | `image/cfg/default.cfg` | voor elke node: `hopos.cluster=hopos`, `hopos.insecure=1` en `hopos.console=1` (open op het eigen LAN), `hopos.cages=on`, `hopos.storage=stateless` (een device begint schoon en vergeet alles; `stateful` in een eigen laag laadt de boom van de schijf en laat Hop's staat op `/hop/` een herstart overleven; `HOPOS_FS_STATELESS` op de console zegt dat de boot leeg begon) |
| 2, het bord | `image/cfg/<board>.cfg` (`rpi4`, `rpi5`, `radxa`, `o6n`, `altra`, `apple`, `licheerv`) | alleen wat per bord afwijkt, de plaatsing van Hop: `hopos.hop.sharegroup=system` op de Pi's en de Radxa (Hop op de OS-core naast de kern; een storm van RPC's van apps kan hem daar uithongeren), `hopos.hop.sharegroup=hop` met `hopos.hop.core-class=small` op de O6N, de Altra en de M4 (een eigen app-core, een zuinige waar het bord klassen heeft: de A520 op de O6N, een E-core op de M4; de Altra kent geen klassen), en `hop` zonder klasse op de LicheeRV (de C906L) |
| 3, de smaak | `image/cfg/headless.cfg` of `image/cfg/headfull.cfg` | alleen init-jobs: headless geen, headfull welcome op poort 80 van de release `apps`, en de display-app als regel met een hekje |

Elk image-script stapelt de drie zelf (de smaak volgt `GUI=1` of
`MEDIA=1`), `tools/release.sh` per board en smaak, en
`CFG=headless|headfull sh image/flip-bundle.sh <board>` in een bundel. Een
eigen `CFG=` is een lijst bestanden in die volgorde, bijvoorbeeld de drie
lagen plus een eigen bestand erachter (een node buiten het eigen LAN: met
`hopos.apikey` en `hopos.insecure=0`). `hop image --config` neemt één
bestand: `cat default.cfg <board>.cfg <smaak>.cfg eigen.cfg >node.cfg`.

Geen `hopos.node` erin: zonder heet een node naar zijn board en de laatste
twee bytes van zijn uplink-MAC (`rpi4-4c54`, `o6n-1a2b`; de regel
`HOPOS_NODE_DEFAULT` op de console), zodat dezelfde lagen op elke node
passen. De media-node heeft zijn eigen `jobs/hopos-media-o6n.cfg`.

**Het config-venster** (`board/src/cfgwin.rs`, zoals v2 met `image/hopcfg`
en `mkcard -cfgwindow`): elke kern draagt een venster van 16 KiB in
`.data.hopcfg`, op een 4 KiB-grens: de kopregel `#HOPCFG1 window=16384
len=0000000432`, de config, en `#`-regels als padding. Elk image-script zet
`CFG=` (of de drie lagen van `image/cfg`) daarin met `image/hopcfg.py`: in
`kernel8.img` van de Pi's, `hopos.img` van de Radxa, `BOOTAA64.EFI` van de
UEFI-boards, het bootobject van de M4, `monitor.bin` van de LicheeRV, en
met `CFG=` van `image/flip-bundle.sh` in een flipbundel. Een gevuld venster
is het configbestand van de node: het wint van `hopos.cfg` op de ESP, in de
initrd van de Radxa, van de m1n1-loader op 0xF000 en van de bootargs
(`cmdline.txt`, de APPEND), die blijven als terugval bij een leeg venster
(`HOPOS_CFG_WINDOW` op de console zegt dat het venster telde). Een flip
geeft het venster van de draaiende kern mee aan een bundel met een leeg
venster (`HOPOS_FLIP_CFG`); een bundel met een eigen config houdt die
(`HOPOS_FLIP_CFG_OWN`). Een eigen config in een release-image, een bundel
of op een kaart zet `hop image` (de hop-repo): `hop image <img> --config
mijn.cfg --write /dev/rdiskN` schrijft de kaart in één keer, `hop image
<img|bundel|/dev/rdiskN>` toont wat erin staat.

Kanttekening: de Radxa en de LicheeRV leiden hun MAC-adres af van
`hopos.node` (anders een vaste MAC, `HOPOS_MAC_FIXED`). De Radxa zet daarom
`hopos.node` in de APPEND van extlinux (`NODE=`, standaard `radxa-1`); op de
LicheeRV hoort een eigen `CFG=` met `hopos.node` of `hopos.mac` zodra er
twee op één LAN staan.

## Bouwen

```sh
BOARD=uefi  sh image/uefi-run.sh   # target/uefi-esp-uefi/: het generieke image, meteen op QEMU/EDK2 (BUILD_ONLY=1: alleen bouwen)
BOARD=o6n   sh image/uefi-run.sh   # target/uefi-esp-o6n/EFI/BOOT/BOOTAA64.EFI
BOARD=altra sh image/uefi-run.sh   # target/uefi-esp-altra/EFI/BOOT/BOOTAA64.EFI
GUI=1 BOARD=o6n sh image/uefi-run.sh   # de gui-smaak (GOP); MEDIA=1 voor de VPU (docs/media.md)
```

Op een stick: een FAT32-partitie met die boom erop (de config staat in het
venster van `BOOTAA64.EFI`: de lagen van `image/cfg`, of `CFG=`), Secure Boot uit. Een dd-bare stick
maakt `tools/release.sh` (`hopos-o6n-headless.img.gz` en zo). Beide boards zijn het UEFI-board
(`board-uefi`; het kernvenster van de O6N is `window-8000` op 0x8800_0000,
dat van de Altra `window-a000` op 0xA000_0000) plus een eigen crate:
`board-o6n` en `board-altra`. De binary kiest met `--features board-o6n` of
`board-altra`.

**Stand van de build:** alle drie de UEFI-images bouwen (`tools/gate.sh`),
en `tools/qemu-uefi-test.sh` boot het generieke image op QEMU/EDK2 mét
interrupts over PCI. Toets vóór je naar het board loopt: die test groen, en
`BOARD=o6n` en `BOARD=altra` leveren een `BOOTAA64.EFI`.

## Interrupts over PCI, watchdog, klok en thermiek (alle UEFI-boards)

MSI-X. Een PCIe-device krijgt, in deze
volgorde (`board_uefi::irq`, `hopos.nicirq=auto`):

1. **MSI-X via de GICv3-ITS** (`driver_gicv3::its`): de ITS uit de MADT, zijn
   tabellen en de LPI-tabellen in de laatste MB van de NIC-DMA-helft
   (`ITS_DMA`, Normal-NC), één collectie op de redistributor van de
   OS-core, per device `MAPD`/`MAPTI`, vector 0 in de MSI-X-tabel. De
   DeviceID komt uit de IORT (root-complex, eventueel door een SMMU, naar
   de ITS-groep). Noemt die groep een andere ITS dan de eerste uit de MADT
   (de Altra: acht, een per root-complex), dan komt die ITS erbij op
   (`HOPOS_ITS_MORE`; eigen tabellen in `ITS_MORE_DMA`, de LPI-configuratie
   en de redistributor gedeeld, een eigen stuk LPI-nummers) en krijgt het
   device zijn doorbell: een MSI naar de doorbell van een andere ITS komt
   nooit aan (A7g, 03-10). Zonder IORT-weg geen MSI-X (een verkeerde
   DeviceID is stil: de ITS gooit de schrijf weg), tenzij
   `hopos.nicirq=msix` de gok DeviceID = requester-id afdwingt.
2. **INTx uit de `_PRT`** van de host-bridge (`fw::aml`: een minimale lezer
   die alleen statische `_PRT`-pakketten leest en een methode of een
   link-device luid weigert), na de swizzle door de bridges. Op de O6N zegt
   de DSDT precies wat de device tree zegt (host-test tegen
   de echte DSDT: 477 voor bus 0x30, en de vier andere root-poorten).
3. **Geen lijn** is een fout, met de reden op de regel: het device start
   niet (01-11, O3). Pollen bestaat niet meer.

`hopos.nicirq=` in `hopos.cfg`: `auto` (standaard), `msix`, `intx`, of een
INTID (bordkennis die de DSDT niet heeft). De virtio-net en de virtio-blk
van QEMU gaan altijd op MSI-X.

De verwachte regels (QEMU/EDK2, 29-09):

```
irq: ITS IIDR 0x43b TYPER 0x1f0001efb1, 16 DeviceID bits, device table flat (4 KB pages), doorbell 0x8090040, LPIs enabled on the redistributor at 0x80a0000 HOPOS_ITS_UP
net: virtio-net-pci at 00:02.0, MSI-X via the ITS, LPI 8192 (DeviceID 0x10), queue 256 HOPOS_NIC_IRQ
net: pump on the nic's irq line, uplink queues 2x128 HOPOS_NET_PUMP
HOPOS_TICK 2 sleeps=1120 polls=2070 irq(timer=0 nic=17 other=0) os(...) temp=-
```

Zonder ITS: `HOPOS_ITS_NONE`; LPI's die al aan stonden met andermans tabel:
`HOPOS_ITS_FAIL` (EnableLPIs is eenmaal gezet vaak niet terug te zetten).

**De watchdog** (`hopos::watchdog`, het beleid in `kern::watchdog` met zijn
policy-tests): de SBSA-watchdog uit de GTDT, 12 s
gevraagd, aaien elke 2 s. Fase 1 blind tot het levensteken, op een
flip-boot hoogstens twee minuten op de rauwe teller; fase 2 alleen op
bewijs. Het levensteken is de canary van Go: elke ronde een nieuwe
TCP-verbinding over de node-stack (3 s geduld, hooguit 15 s oud), naar
Hop's agent op 10.100.0.2:8080 als Hop op de node woont (door de switch en
Hop's accept-laag, plus zijn heartbeat), anders naar de eigen system-poort
op het uplink-adres. `hopos.wd=off` zet hem uit (ook een die de vorige kern
wapende). Regels: `HOPOS_WD_ARMED`, `HOPOS_CANARY_LIVE`,
`HOPOS_CANARY_MISS`, `HOPOS_BOOT_GUARD_EXPIRED` (geen levensteken binnen de
guard van 120 s, voor elke boot),
`HOPOS_RESET_REQUESTED`, en per wissel van de dial `HOPOS_WD_CANARY_OK` of
`HOPOS_WD_CANARY_FAIL` (ook zonder blok, dus ook op QEMU); QEMU heeft er
geen: `watchdog: no SBSA watchdog in the GTDT (QEMU?) - node liveness is
UNGUARDED HOPOS_WD_NONE`. Let op een teller van 1 GHz
(Armv8.6+, de O6N): WOR is 32 bits, dus de timeout wordt 8,6 s en de
armed-regel zegt dat.

**De thermiek** (`hopos::telemetry`): elke seconde op de tik (`temp=41.5C`,
`-` zonder sensor) en op de control-page van Hop (`CTRL_TEMP`, milligraden;
`applib::Ctrl::temp_milli_c`), voor zijn heartbeat.

**De willekeur** (`cpu::drbg`, `hopos::seed`, `applib::rand`): het board
zaait de DRBG van de kern in `discover` uit de standaardbronnen van de CPU
(`cpu::drbg::seed_from_cpu`): RNDR op de O6N (Cortex-A720, FEAT_RNG), de
SMCCC TRNG van TF-A op de Altra, en anders jitter (EDK2 op QEMU). Tot
30-09 zaaide dit board niets: de O6N bootte zonder één `trng:`-regel en
de DRBG bleef ongeseed. De verwachte regels:

Een UEFI-board zonder FEAT_RNG en zonder SMCCC-TRNG (de O6N: de Cix heeft geen van beide, 30-09) zegt eerst wat de SMCCC-TRNG-probe zag (`HOPOS_SMCCC_TRNG` of `HOPOS_SMCCC_NO_TRNG` met de reden: geen EL3-monitor, SMCCC 1.0, of TRNG_VERSION NOT_SUPPORTED) en kan met de feature `efi-rng` (aan voor de O6N) in hopos.cfg de TRNG achter de firmware gebruiken: de stub vraagt vóór ExitBootServices 64 bytes aan het EFI_RNG_PROTOCOL (wat Linux' `efi_get_random_bytes` ook doet) en de kern zaait zijn DRBG daarmee als bron `efi-rng` (`HOPOS_RNG_EFI_UP`). Alleen op verzoek: op 13-07 bleef een firmware in dat protocol hangen. Na een flip is de firmware weg: de vertrekkende kern legt dan 64 verse bytes uit zijn DRBG achter de feitenpagina (`HOPOS_FLIP_SEED`; Linux legt bij kexec zo een `rng-seed` in de DTB van de nieuwe kern), en de nieuwe kern zaait daaruit, met dezelfde bron `efi-rng` (`HOPOS_RNG_EFI_CARRIED`). Tot 03-10 zaaide een geflipte O6N hier uit jitter.

```
trng: rndr online, the kernel DRBG is seeded from rndr (FEAT_RNG) HOPOS_RNG_RNDR_UP                  (O6N)
trng: smccc-trng online, the kernel DRBG is seeded from the SMCCC TRNG (DEN 0098) HOPOS_RNG_SMCCC_UP (Altra)
trng: WARNING the kernel DRBG is seeded from timer jitter, not hardware entropy: <reden> (EL3 monitor: yes|no); ... HOPOS_RNG_INSECURE
rng: every slot gets 32 bytes from the kernel DRBG (rndr) on its control page at build and every second HOPOS_RNG_SLOTS source=hardware
slot 1: applib: rng seeded from the kernel (rndr, gen 2) and 256 jitter samples HOPOS_APP_RNG source=hardware kind=rndr
slot 1: hop: TLS randomness from the kernel seed (rndr, hardware) mixed with timer jitter (512 samples) HOP_TLS_ENTROPY_HW source=rndr
```

Elk slot krijgt 32 bytes uit die DRBG op zijn control-page: bij de bouw
van de kooi, na een flip-adoptie, en elke seconde vers vanuit de
telemetrie-tik. Het blok ligt onder het EL1-fault-rapport, naar beneden
groeiend (`abi::hopabi`, 30-09):

| Offset | Woord | Betekenis |
| --- | --- | --- |
| 0xFA8 tot 0xFC8 | `CTRL_RNG_SEED` | 32 bytes zaad, eigen bytes per slot |
| 0xFA0 | `CTRL_RNG_GEN` | seqlock: 0 geen zaad, oneven de kern schrijft, even en niet 0 geldig; per page monotoon, ook over een flip |
| 0xF98 | `CTRL_RNG_SOURCE` | `0xC0DE5EED` in de bovenste 32 bits, de bron in de onderste byte: 1 jitter, 2 rndr, 3 smccc-trng, 4 SoC-blok |

De env-ruimte (`CTRL_ENV_MAX`) ging daarvoor van 0xEA8 naar 0xE78 bytes.
Additief: een oude app leest het blok niet; een nieuwe app op een oude kern
ziet generatie 0 (of een bronwoord zonder de magic) en meldt
`HOPOS_APP_RNG_NONE`, en leest een env van een oude kern nog tot 0xEA8
bytes (`CTRL_ENV_LEGACY_MAX`). De app gebruikt het zaad nooit rauw:
`applib::rand::Rng` mengt het met eigen jitter in een ChaCha20-DRBG, en
herzaait vanzelf als er een nieuwe generatie ligt. Waarom geen system-op:
het zaad is nodig vóór er een verbinding is (de ISS van de netstack, de
eerste TLS-handshake van Hop), en een woord op de page is de vorm van de
temperatuur en de wandklok.

**De klok** (`driver_dvfs::run` op de OS-core): de last van de node is de
kern (de slaap van zijn executor) plus elk slot (`CTRL_IDLE`, alle cores).
Sample elke 10 ms, oordeel over 50 ms: mist een bewoner meer dan 30% van één
core aan idle (de kern: 70%, zijn UART-console is gepold), dan vol; omlaag
na 30 s stil. Elke 10 s een meetregel `dvfs: clock <MHz> (full|quiet), temp
<C>, busy <bron> HOPOS_CLOCK` (`busy slot 3 (0 permille idle)`, of `none
(last slot 3, 14 s ago)`), en een regel per flank (`HOPOS_CLOCK_EDGE`, met
de bron: `(full, busy: slot 3 (...))`). `hopos.clock=dvfs|max|quiet|firmware`
pint, `hopos.mhz=` klemt het plafond. De knoppen: de Pi's de ARM-klok via de
mailbox, de O6N de `_CPC`-woorden, de Radxa `SCMI_CLK_CPU` van de TF-A met
vdd_cpu over I2C (`board_rk3566::clock`, 816 tot 1800 MHz); de rest houdt de
klok van de firmware.

## Radxa Orion O6N

### Wat er is

| Onderdeel | Crate | Hoe getest |
| --- | --- | --- |
| NIC: RTL8126A (5G, O6) en RTL8125B/D/CP/BP (2,5G, O6N), chip-XID kiest de variant | `driver-rtl8126` | 12 host-tests tegen een nep-chip: reset tot `hw_start`, ringen, MAC-terugval, EPHY-tabel en PHY-stappen van de 8125B, link via PHYSR, RX-grenzen, TX-padding en eigendom, IRQ-mask/rearm |
| NVMe: één core (admin- en I/O-queue, identify, zestien tickets met eigen pagina's en PRP-lijst, een ticket boven de MDTS als meer opdrachten, read-ahead, flush, 512-byte-sectoren boven 4K-namespaces) met het PCI-transport | `driver-nvme` (`lib.rs`, `pci.rs`) | 17 host-tests op de core met een nagebootst transport (ring en lineair) en 2 voor PCI (CAP, CC, stride), tegen een nep-controller die echte SQ's/CQ's en PRP's uitvoert; time-out, vreemde CID en een dood transport maken de driver dood |
| Thermometer: SCP via SCMI op 0x065d0000 (het kanaal van de DSDT-`_TMP`) | `driver-scmi`, `board-o6n::thermal` | 6 + 2 host-tests; kiest CPU-sensoren, anders alle Celsius |
| Klok: `_CPC`-scanner (AML zonder interpreter), domeinen, `CpcKnob` | `board-o6n::{cpc, clock}`, `driver-dvfs` | 3 + 3 host-tests; het beleid 8 host-tests |
| Core-klassen: MADT, anders `_CPC` (25%-clustering), anders vast per MPIDR (aff1 0-3 small) | `board-o6n::class` | 4 host-tests, waaronder de meting van 17-09 (2232 ×4, 8192/7876/7246/6931 ×2 = small ×4, big ×8) |
| PCIe-zoektocht: eerste Realtek, eerste NVMe, INTx per root-poort | `board-o6n::probe` | 3 host-tests op een nep-config-space, en de INTx-tabel tegen de `_PRT` van de echte DSDT |
| NIC-lijn: MSI-X via de ITS als de IORT de DeviceID kent, anders INTx 477 uit de `_PRT` | `board-uefi::irq`, `driver-gicv3::its` | ITS: 5 host-tests (commando's, plat en twee niveaus, een route); `_PRT`: 5 host-tests; op QEMU/EDK2 bewezen met virtio-net |
| Klok: `CpcKnob` uit DSDT en SSDT's, na de OEM-toets `CIXTEK` | `board-o6n::machine`, `driver-dvfs::run` | de taak op de host: zakken na 30 s, opklokken op last, de meetregel |

### EL2: de hele kern onder VHE (E2H = 1)

Op de Cortex-A720 stierf een EL1 onder nVHE binnen een halve seconde
(17-09), dus de O6N draait VHE, en niet alleen in de switcher van de
app-cores: de hele kern draait onder E2H = 1 (`board-o6n` zet altijd
`board-uefi/vhe` aan, `board/uefi/src/el2.rs`). De ingang zet E2H met de
MMU nog uit en schrijft TCR_EL2 en SCTLR_EL2 in de vorm van TCR_EL1 en
SCTLR_EL1, CPTR_EL2 in de CPACR-vorm en CNTHCTL_EL2 in de VHE-lay-out
(EL1PCTEN/EL1PTEN op 10/11); de map zet PXN naast XN. De OS-core-rotatie
(Hop op de kern-core) en de switcher van de app-cores (`hopos_el2_vhe`)
gebruiken de `_EL12`-encoderingen voor het EL1-regime van hun bewoners, en
die bestaan alleen onder E2H = 1: een VHE-switcher op een nVHE-kern gaf op
29-09 een `exception: sync/el2 ESR=0x2000000 (EC 0x0)` in
`hopos_os_vhe_enter` bij de zelftest van de OS-core. De build weigert die
combinatie nu (`hopos/src/cage.rs`). Onder E2H = 1 is de timer van de kern
de CNTHP (PPI 26: `cntp_*_el0` vanaf EL2), dezelfde timer die de OS-core
op de deadline zet; ze wisselen elkaar af op dezelfde core.

**Bewezen op QEMU, niet op de A720.** `FEATURES=vhe CPU=neoverse-n1 sh
tools/qemu-uefi-test.sh` (EDK2, 4 cores, de hele appspike-keten in twee
slots en de toetsen van buiten) is drie keer groen, ook met de kern
verhuisd naar core 2 (`hopos.oscore=2`), en `FEATURES=vhe CPU=neoverse-n1
BOARD=uefi sh tools/qemu-test-flip.sh` (Hop als bewoner van de OS-core, de flip
met de som over de vhe-blobs) is groen. De kale nVHE-weg (`sh
tools/qemu-uefi-test.sh`, cortex-a57) is ongewijzigd. Het kernvenster van
de O6N staat sinds 29-09 echt op 0x8800_0000: `window-8000` werd tot dan
door niemand aangezet.

### Wat je hoort te zien (in deze volgorde)

- `uefi: booted at EL2 ... window 0x88000000+0x14000000 ... OEM "CIXTEK"`,
  meteen gevolgd door `uefi: kern under E2H=1 (VHE), HCR_EL2 0x480000038
  HOPOS_UEFI_VHE` (HCR teruggelezen: RW, IMO/FMO/AMO en E2H). Staat daar
  `HOPOS_UEFI_VHE_FAIL`, dan nam de core E2H niet aan. Dan `pcie: ...` per
  functie (van `board-uefi`). Kijk of beide Realteks en de NVMe erin staan,
  met hun BAR's.
- `slots: cage up HOPOS_CAGE_UP ... hash=0x...` en `oscore: cpu 0 self-test
  timer=Some((Timer, ~1000+)) yield=Some((Yield, ..)) kick=Some((Ipi, ..))
  (back, us) HOPOS_OS_SELFTEST ok`: de rotatie onder VHE op de kern-core
  (op QEMU timer ~1300, yield ~20, kick ~10 µs).
- `o6n: 11 app cores, classes from Mpidr - small 3, mid 0, big 8` (of
  `Madt`). De kern-core is core 0; is dat een A520, dan staat er small 3.
- `boot: HopOS v3.0.0 on o6n, EL2, 12 cores (...) HOPOS_BOOT`.
- `irq: ITS IIDR 0x... ... HOPOS_ITS_UP` (de GIC-700 heeft een ITS; de regel
  zegt DeviceID-bits en plat of twee niveaus).
- `net: RTL8125? 10ec:8125 at 31:00.0 xid 0x... link 2500 Mbps full duplex, MSI-X via the ITS, LPI 8192 (DeviceID 0x...) (the DT table said INTID 477) HOPOS_NIC_IRQ`,
  of zonder IORT-weg `..., INTx on INTID 477 (SPI 445) ...`, en dan
  `HOPOS_NIC_UP mac=...` en na DHCP `HOPOS_NET_UP`. In de tik moet `nic=`
  oplopen met het verkeer.
- `dvfs: 5 _CPC domains, policy Auto, ... HOPOS_CLOCK_UP` en `dvfs: -> 2600
  MHz (fastest domain) (full, boot) HOPOS_CLOCK_EDGE`; na 30 s stil de flank
  naar quiet, en elke 10 s `HOPOS_CLOCK` met de temperatuur.
- `watchdog: hardware reset armed (SBSA watchdog, 8.5 s ...) ...
  HOPOS_WD_ARMED`, na DHCP (en Hop) `HOPOS_CANARY_LIVE`.
- `disk: nvme <model> at ... HOPOS_NVME_UP`, dan `HOPOS_DISK_UP` en
  `HOPOS_FS_UP` (hopfs op de NVMe; het hele device is van HopOS, stateful).
  Na de interrupts de lijn: `disk: nvme at ... MSI-X via the ITS, LPI N
  (DeviceID ...), vector 0 of N, first interrupt after N us, ...
  HOPOS_NVME_IRQ`, of `polled (<reden>) HOPOS_NVME_IRQ` met daarvoor
  `HOPOS_NVME_IRQ_DIAG`. De schijf telt in de tik onder `other=`, niet
  onder `nic=`; `queue_polls=` in `HOPOS_DISK_STATS` zegt hoe vaak de
  wachter naar de CQ keek.
- Hop met de bordlaag `image/cfg/o6n.cfg`: `slots: Hop in the sharegroup
  hop (hopos.hop.sharegroup) HOPOS_HOP_GROUP`, `slots: Hop prefers a small
  core for his group (hopos.hop.core-class) HOPOS_HOP_CLASS` en
  `HOPOS_HOP_START slot=1 core=N cpu=N ... class=small` (een A520). Een
  warme flip neemt Hop mee op zijn oude
  core (`HOPOS_HOP_RESUMED`); de nieuwe plaatsing geldt vanaf de volgende
  start van Hop (koude boot of koude flip).
- Bij de eerste temperatuurvraag: `hwmon: N SCMI sensors of M (first ...)`.

Gaat het mis, dan zegt de regel welke stap: `rtl8126: <stap> timed out (reg
0x..=0x..)`, `rtl8126: unsupported chip XID 0x...`, `nvme: CSTS.RDY never
became 1`, `HOPOS_NIC_FAIL`, `HOPOS_NVME_FAIL`.

De kick van de OS-core is SGI 1 (`board_uefi::KICK_SGI`), niet 8: TF-A
houdt SGI 8 tot en met 15 als secure, en een niet-beveiligde kick daar
verdwijnt stil (O6N 30-09: `kick=(Timer, 100000 us, try 2)`). Zegt de boot
`irq: kick SGI 1 is not ours (GICR_IGROUPR0 ...) HOPOS_KICK_SECURE`, dan
leest de redistributor de groep of de enable als 0; zegt de zelftest
`irq: kick SGI 1 did not arrive: ICC_SGI1R 0x... to MPIDR 0x..., GICR_IGROUPR0
... ISENABLER0 ... ISPENDR0 ..., ICC_HPPIR1 ... HOPOS_KICK_LOST`, dan ging
het woord de deur uit maar stond de SGI na 1 ms niet pending.

### Wat er nog niet is

- **VHE op de A720 is onbewezen.** QEMU's neoverse-n1 is VHE zonder de
  eigenaardigheden van de A720; wat de O6N van E2H = 1 vindt, zegt de
  eerste boot (`HOPOS_UEFI_VHE`, de zelftest van de OS-core, en of een
  bewoner langer dan een halve seconde leeft: Hop's `HOP_UP` en de tik).
- **MSI-X op ijzer is onbewezen.** Of de Cix-IORT de root-complexen naar
  de ITS afbeeldt, en of de RTL8125 op MSI-X zijn IntrStatus-flank zo geeft
  als op INTx, zegt de eerste boot. Zonder IORT-weg valt hij terug op INTx
  477 (L80: één interrupt per frame, rtt p50 156-201 µs). Met
  `hopos.nicirq=intx` meet je de terugval los.
- **De NVMe-lijn op ijzer is onbewezen.** Vector 0 (de admin-CQ en de
  I/O-CQ, IEN) via de ITS van zijn root-complex, in `probe_nvme` (de
  interrupts staan sinds 01-11 vóór de opslag), met een zelftest (een Get
  Features op de admin-CQ): wat niet aankomt, is een schijf zonder lijn en
  de kern start niet (`HOPOS_DISK_FAIL`). De wachter van `blkdev::Queue`
  slaapt op de bel, met de termijn van 5 s (`blkdev::TERM`).
- **De `_CPC`-klassenbron**: nu MADT, anders de MPIDR-tabel.
- **De header-UART** (0x040d0000) als spiegel: de console is die van de SPCR.
- **De kern-core in het klokbeleid**: zijn idle-tijd staat in de slaper van
  de executor en die leest geen taak; Hop (die de core deelt) telt wel mee,
  als slot.
- Twee poorten tegelijk (de eerste ondersteunde wint), VPU en USB (media/gui).

### Wat je meet (lat: L74-L83 van v2)

1. Boot tot `HOPOS_NET_UP` en `HOPOS_FS_UP`; noteer de variant (XID) en de
   linksnelheid.
2. Netwerk gepold: rtt p50 (v2 gepold 160-168 µs) en doorvoer M4 ↔ O6N (v2
   met IRQ 112-117 MB/s). Pomp-rondes per seconde idle (v2 gepold 3333/s).
3. NVMe: 1 GiB schrijven en teruglezen met positie-afhankelijke inhoud;
   MB/s beide kanten; `slowest_ns`. Herstart: hopfs herstelt (`restored`).
4. Core-klassen: klopt de indeling met het ijzer (4× A520, 8× A720)? Is
   core 0 een A520 of een A720?
5. Temperatuur: `temp=` op de tik en `HOPOS_CLOCK`, en of die bij
   belasting stijgt.
6. Interrupts: `nic=` in de tik tegen de pakketten; rtt p50 met MSI-X tegen
   INTx (`hopos.nicirq=intx`); geen `stray`-regel.
7. Klok: `HOPOS_CLOCK_EDGE` naar quiet na 30 s idle, en binnen ~20 ms terug
   naar vol onder last (v2 L83: 867 tegen 267 Msteps/s).
8. Watchdog: armed-regel (de echte timeout), `HOPOS_CANARY_LIVE`; dan de
   kabel eruit tot `HOPOS_CANARY_MISS` en de reset (de O6N-refresh: WRR
   werkt daar niet, WOR opnieuw schrijven wel).

## Ampere Altra

### Wat er is

| Onderdeel | Crate | Hoe getest |
| --- | --- | --- |
| NIC: Intel igb (I210 8086:1533 en familie; QEMU's 82576 8086:10c9), advanced descriptors, RX 256 / TX 64, doorbell per burst | `driver-igb` | 11 host-tests tegen een nep-igb: reset (met Go's pauze van 10 ms), MAC uit RAL0/RAH0, ringen, RDT pas na RXDCTL.ENABLE, link via MDIC en STATUS, RX-grenzen, zelf-flush per 32, TX-eigendom en de TDT≠TDH-rem |
| NVMe | `driver-nvme` | zie de O6N |
| Thermometer: SMpro via PCC-kanaal 14 (xgene-hwmon), PCCT-parser | `driver-smpro` | 4 host-tests, waaronder "een onafgemaakt commando laat de buffer van de firmware" |
| Klassen: homogeen big | `board-altra` | host-test |
| PCIe-zoektocht over tot acht segmenten, hoge BAR's via `map_device` | `board-altra` | host-test op twee nep-segmenten |

### Wat je hoort te zien

- Vóór de exit, op de firmware-console: `uefi: window 0xa0000000+0x14000000,
  ... MB DRAM, N map entries, M page tables`. N is het aantal descriptors
  (Go: duizenden); de buffer is 256 KB, en vraagt de firmware meer, dan
  eerst `... HOPOS_UEFI_MAP_BIG`. Past de identity map niet in 1024
  tabellen: `HOPOS_UEFI_TABLES` en terug naar de firmware.
- Is 0xA000_0000 bezet: `kernel window ... is taken`, de vrije regio's,
  Go's zes kandidaten elk met `free` of `taken`, en
  `HOPOS_UEFI_WINDOW_FREE` met de eerste vrije (herbouw met dat venster).
  Eén venster per build: de indeling, `slots` en de kern-flip rekenen met
  constanten, de kern is wel een PIE.
- `uefi: booted at EL2 ...`, `pcie: segment ...` voor elk segment.
- `slots: pool ... MB in N regions, largest ... MB`. Valt er iets buiten
  de pool (een volle lijst, het plafond van 64 regio's):
  `HOPOS_UEFI_MAP_DROPPED` met het aantal stukken en de MB's.
- `boot: HopOS v3.0.0 on altra, EL2, 128 cores (big) ... HOPOS_BOOT` (of 80,
  afhankelijk van de SKU).
- `irq: ITS IIDR ... doorbell 0x100100130040, for 01:00.0 (the IORT's ITS for its root complex), collection on the redistributor at ... HOPOS_ITS_MORE`
  (de igb hangt aan ITS 7 van de acht),
- `net: igb 8086:1533 at ... link 1000 Mbps full duplex, MSI-X via the ITS, LPI 8224 (DeviceID 0x70100), first interrupt after N us HOPOS_NIC_IRQ`,
  dan `HOPOS_NIC_UP`, `HOPOS_NET_PUMP` met `the nic's irq line` en
  `HOPOS_NET_UP`. Komt de afgevuurde vector niet aan:
  `no line: the forced interrupt (EICS) did not arrive within 50 ms`, en
  dan start de NIC niet (01-11, O3: geen gepolde terugval).
  Daarvoor dan één regel `net: igb MSI-X diag: ... HOPOS_NIC_IRQ_DIAG`: de
  DeviceID en de IORT-weg (SMMUv3 met CR0 en GBPA, de ITS die de groep
  noemt tegen de onze uit de MADT), of MAPD/MAPTI/INV/SYNC in dit kernleven
  liepen, de MSI-X-control, het command-register, entry 0 en de PBA zoals
  de functie ze teruggeeft, GPIE/IVAR0/EIMS/EICR van de igb, en een `INT`
  vanuit de ITS zelf: komt die wel, dan werkt de ITS-kant en haalt de
  schrijf van de igb de ITS niet; blijft hij stil, dan zit het in de ITS,
  de redistributor of de collectie.
- `hwmon: SoC 45.2C (SMpro, PCC channel 14)` en daarna `temp=` op de tik.
- `watchdog: hardware reset armed (SBSA watchdog, 12.0 s ...) ... HOPOS_WD_ARMED`
  (servers zijn braaf SBSA; de eerste echte proef van dit pad).
- `dvfs: server clocks are firmware domain on this board HOPOS_CLOCK_NONE`.
- `disk: nvme ... HOPOS_NVME_UP`, `HOPOS_DISK_UP`, `HOPOS_FS_UP`, en na de
  interrupts `disk: nvme at ... MSI-X via the ITS, LPI N ... HOPOS_NVME_IRQ`
  (de NVMe heeft een eigen root-complex en dus misschien een eigen ITS:
  `HOPOS_ITS_MORE` voor hem, vóór die van de igb).

### Wat er nog niet is

- **De igb op MSI-X is op ijzer nog niet gezien.** Standaard
  `hopos.nicirq=msix`: vector 0 via de ITS, de driver in de MSI-X-modus van
  Linux `igb_configure_msix` met één vector (GPIE, IVAR0 voor RX-queue 0,
  EIAC/EIAM/EIMS; ack EIMC, de pomp heropent bij een lege ring), en een
  zelftest met EICS: wat niet aankomt, is een NIC zonder lijn en die start
  niet (01-11, O3). Nooit
  INTx: de `_PRT`-INTx doodt de SoC (L83, 19-09, UART-bewijs); `intx` en
  een INTID weigert het board (`board_altra::nic_irq_mode`).
- **De NVMe-lijn**: zoals op de O6N, nooit INTx (de functie staat vanaf de
  probe met INTx uit).
- Het geheugenplan is dat van `board-uefi`: of de pool de ~300 GB boven de
  512 GB haalt (v2 15-07: 1,62 GB zonder de hoge map), zegt de bootregel van
  `board-uefi`.

### Wat je meet

1. Boot tot `HOPOS_NET_UP`/`HOPOS_FS_UP`; de igb-variant en de link.
2. Doorvoer en rtt p50/p99 op MSI-X (netmeter en bench pull).
3. NVMe zoals op de O6N; de BAR ligt hoog: klopt `map_device`?
4. Het aantal cores en de pool-grootte tegen de firmware-memory-map.

## QEMU `-device igb`

De igb-driver is QEMU-testbaar (82576), MSI-X inbegrepen. Met de hand
(03-10): de ESP van `BOARD=altra image/uefi-run.sh` op de QEMU-regel van
dat script, met `-cpu max` en `-device igb,netdev=n0,romfile=` in plaats van
virtio-net. Dan `MSI-X via the ITS, LPI 8192 (DeviceID 0x10), first
interrupt after ~120 us`, DHCP en `HOPOS_NET_UP` over de lijn, en de
NIC-interrupts lopen op met het verkeer. Nog
geen script (een `NIC=igb`-variant van `tools/qemu-uefi-test.sh`).
