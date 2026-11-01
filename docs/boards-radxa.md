# Radxa Zero 3E (RK3566) op HopOS v3

Alles hieronder is op de host getest en bouwt voor het target, maar niets
ervan heeft in v3 op ijzer gedraaid. De metingen op dit bord (05-08 tot
21-09) staan in het commentaar van de code; deze checklist zegt per stap
wat de console moet tonen en wat een afwijking betekent.

## Wat er is

| Deel | Waar | Getest |
| --- | --- | --- |
| Board: DTB uit x0 naar de heap, geheugen en cores uit de DTB, UART2 (ns16550, stap 4), GIC-600, de klok, het plan, de NIC-probe | `board/rk3566` | 28 host-tests (31 met `gui`): plan, pool uit de banken min de gaten, CRU-, GRF- en iomux-woorden, aff1-nummering, de map-tellingen, de zes van de initrd hieronder, en die van de vier stukken hieronder |
| Watchdog: de DW-WDT gewapend op TOP 15 (89,5 s), geaaid door de watchdog-taak van de kern, de erfenis over een flip (staat ENABLE al, dan alleen TORR en een kick); geen knop om hem uit te zetten (O2) | `board/rk3566/src/watchdog.rs`, `hopos/src/watchdog.rs` | host-tests op een nep-WDT en nep-CRU in RAM: de TOP-tabel, wapenen (timeout, kick, enable, response mode 0), de erfenis één keer, geen kick zonder pclk |
| TRNG: `rockchip,rk3568-rng` op 0xFE38_8000 als bron van de DRBG van de kern, gezaaid in `discover` | `driver/rkrng`, `board/rk3566/src/rng.rs`, de klokken en de reset in `soc.rs` | 9 host-tests op een nep-blok met hiword-masker: hele rondes, START maskeert alleen zichzelf, een dood blok is nul en geen entropie, de continue toets, de timeout zet de ring uit |
| Klok: de cores tussen 816 MHz (850 mV) en 1800 MHz (1150 mV) onder het klokbeleid van de kern. De klok is `SCMI_CLK_CPU` van de TF-A (SMC 0x82000010, shmem 0x0010_f000), zoals in mainline Linux; vdd_cpu is de rk860x op i2c0 0x40 (niet de RK817), via een gepolde rk3x-I2C. Omhoog eerst de spanning, omlaag eerst de klok, nooit een klok boven de teruggelezen spanning | `board/rk3566/src/clock.rs`, `i2c.rs`, `hopos/src/telemetry.rs` | host-tests: de werkpunten van rk3566.dtsi en hun VSEL-codes, welke klok een spanning draagt, het plafond, de ramp, de volgorde, een buck die stopt, een stand of klok zonder antwoord, de I2C-woorden |
| TSADC: temperatuur (warmste van CPU en GPU) op de tik en de heartbeat | `board/rk3566/src/tsadc.rs`, `hopos/src/telemetry.rs` | host-tests: de rk3568-codetabel met interpolatie, nul is geen meting, het delerwoord is het teruggelezen `0x1715` |
| usbdrd30: klokken (CLKGATE_CON(10) bit 8..10), reset (SRST_USB3OTG0) en de OTG-poort van usb2phy0 (`phy_sus`), met de gates van usbhost30 als vangrail | `board/rk3566/src/usb.rs` (alleen met `gui`) | host-tests: de gate- en resetwoorden uit clk-rk3568.c en rk3568-cru.h, de vangrail |
| Hop als bewoner: de initrd draagt `hopos.cfg` én het image (de container), `discover` haalt hem naar de heap en splitst hem, de rol uit `hopos.stage` | `board/rk3566/src/initrd.rs`, `slots.rs`; `image/radxa-initrd.py` | host-tests: de container uit het script (`testdata/mini.ird`), de oude kale config, zonder image, elk fout getal, de rolcodes, de grens |
| SoC-glue: klokgates, bronkeuze en snelheidsdeler (CRU), AXI-reset, RGMII-modus (GRF), de gmac1m1-pinmux, de PHY-reset op GPIO3 PC0, de klokken en de reset van het TRNG | `board/rk3566/src/soc.rs` | idem |
| Identity map: kern-RAM Normal, DMA Normal-NC, de rest Device | `board/rk3566/src/mmu.rs` | const-asserties tegen het plan |
| DWMAC4: registerblok, descriptors, MTL, de ops-tabel op de gedeelde stmmac-kern (reset, MDIO-master, ringen, `netdev::Device`, IRQ-ack en rearm), de MMC-interrupts dicht (anders stormt de lijn na 2 GiB binnen, 03-10) | `driver/nic/stmmac` (feature `dwmac4`) | 12 host-tests voor de DWMAC4, 12 voor de kern op een nagebootste core, op een nep-registerblok en nep-DMA in RAM |
| MDIO: scan, autonegotiatie, RTL8211F-delays | `driver/nic/mdio` | 10 host-tests (samen met de Pi-port) |
| Linkscript: Image op 0x0220_0000, tekst op 0x0221_0000 | `hopos/link-rk3566.ld` | het image-script leest het ELF terug |
| Kaart-image: build, ELF naar arm64-Image, Hop (of een app), de initrd, donor, MBR plus FAT16 | `image/radxa-zero3.sh` | levert het kaart-image en leest het terug: `hopos.img`, `extlinux.conf`, `hopos.cfg` en `hop.elf` komen met hun sha256 uit de FAT (30-09 ook door macOS gemount: dezelfde hashes) |

## Bouwen en flashen

```sh
sh image/radxa-zero3.sh                  # Hop als bewoner; of CFG=~/node.cfg, NODE=radxa-2
APP=appspike sh image/radxa-zero3.sh     # de kale app-rol: appspike twee keer
APP= sh image/radxa-zero3.sh             # zonder image (HOPOS_SLOT_NONE)
HOP_DIR=~/Git/hop sh image/radxa-zero3.sh   # een andere hop-repo (standaard ../hop/hop)
GUI=1 sh image/radxa-zero3.sh            # de gui-smaak (de beeldketen, docs/gui.md)
diskutil unmountDisk /dev/diskN
sudo dd if=target/radxa-zero3/hopos-radxa-zero3.img of=/dev/rdiskN bs=4m
```

Op de FAT staan `hopos.img` (de kern, met de config in zijn venster),
`hopos.ird` (de initrd: een lege `hopos.cfg` plus `hop.elf`) en
`extlinux/extlinux.conf`, met in de APPEND-regel
`hopos.node=radxa-1 hopos.stage=hop`. Het script bouwt `agentd-hopos` uit
`HOP_DIR` met `tools/hop-build.sh`, stript hem (19 MB naar 1,5 MB,
30-09) en weigert een initrd boven 16 MB (`board_rk3566::INITRD_MAX`). Aan het eind leest het de kaart
terug en vergelijkt elk bestand met wat erin ging; `FOUT` daar is een kapot
kaart-image, niet een kapot board.

De config zit in het venster van `hopos.img` ([boards.md](boards.md), "Het
config-venster"), zoals op elk board. Drie wegen om hem te veranderen:
herbouw met `CFG=`, `hop image` op het image of de kaart, of een sleutel in
de APPEND-regel van `extlinux/extlinux.conf` (die blijft tekst). Let op:
een sleutel in het venster wint van dezelfde sleutel in de APPEND
(`Board::boot_param`); de standaardconfig zet geen `hopos.node`,
dus die komt uit de APPEND. Een kaart met een kern zonder gevuld venster
leest `hopos.cfg` uit de initrd, zoals vóór 04-10.

## Hop als bewoner: waarom één initrd

De kern heeft geen SD-driver, dus hij leest Hop niet zelf van de kaart; de
Pi's krijgen Hop van hun firmware (`initramfs hop.elf 0x0f200000`), de
UEFI-boards van de stub (`hopos-stage.elf`), de LicheeRV bakt een app in de
kern. Voor de Radxa zijn drie wegen gewogen (30-09):

- **`initrd /hopos.cfg,/hop.elf` in extlinux**: valt af. De U-Boot van de
  donor is "U-Boot latest-2023.10-8-eed05a18" (de string in
  `donor-boot.bin`), en `boot/pxe_utils.c` van v2023.10 geeft
  `label->initrd` als één pad aan `get_relfile_envaddr`. De komma wordt
  deel van de bestandsnaam, de load faalt, en U-Boot slaat het hele label
  over: geen boot. Een U-Boot die wél een lijst kent, plakt de bestanden
  zonder grenzen achter elkaar; de kern moest dan toch splitsen.
- **Hop in de kern bakken** (de LicheeRV-weg): elke kern-build hangt dan aan
  de hop-repo, elke kern-flip draagt Hop als dood gewicht mee, en een nieuwe
  Hop is een nieuwe kern.
- **Eén container als initrd** (gekozen): de drie kanalen die op 05-08
  GEMETEN werkten (kernel, initrd, append) blijven precies wat ze waren;
  alleen de inhoud van het initrd-bestand groeit. Een initrd zonder de
  magic is de oude kale `hopos.cfg`, dus een oude kaart boot als vroeger.

De vorm staat in `board/rk3566/src/initrd.rs` en `image/radxa-initrd.py`:
magic `HOPOSIRD`, versie, lengte en tekst van de config, opgevuld tot 8,
lengte en bytes van het image, en het bestand eindigt daar precies (U-Boot
zet `initrd-end` op begin plus bestandsmaat). `discover` kopieert de hele
initrd woordgewijs naar de heap, zoals al met de config gebeurde (het DRAM
van U-Boot staat als Device in de map, en de ELF-lezer leest ongealigneerd),
en `slots::staged_image` geeft het image-deel. Een container met één fout
getal telt helemaal niet (`HOPOS_STAGE_REFUSED`): geen config en geen
bewoner is beter dan een half gelezen config. De grens van `fw::fdt::initrd`
ging daarvoor van 2 MB (de DTB-grens) naar 64 MB (`MAX_INITRD`); onder de
oude grens viel een groter image stil weg, ook op de Pi's.

De donor (idbloader en u-boot.itb, LBA 64 tot 32767 van het Radxa
b6-image) staat in `image/firmware/radxa/donor-boot.bin` (herkomst in de
`LEESMIJ.txt` ernaast); hij wordt bij elke build gehasht. De kaart bouwt
`tools/mkcard` (tot 02-10 een eigen python-kaartbouwer in het script). Console: USB-UART op de 40-pins header, pin 8 TX, 10 RX,
6 GND, 1500000 8N1.

## De checklist, in bootvolgorde

1. **U-Boot vindt de kern.** Verwacht: `Retrieving file: /hopos.img`,
   `Retrieving file: /hopos.ird` (ongeveer 1,5 MB met Hop), dan
   `## Flattened Device Tree blob at ...` en `Starting kernel ...`. Ziet
   U-Boot `extlinux.conf` niet, dan leest hij onze FAT niet (de partitie is
   actief, type 0x0C, LFN voor `extlinux.conf`): mount de kaart op de Mac en
   kijk of de drie bestanden er staan. `Skipping hopos for failure
   retrieving initrd`: `hopos.ird` ontbreekt of heet anders dan in
   `extlinux.conf`.
2. **Het Image landt op 0x0220_0000.** U-Boot kan `Moving Image from X to
   0x2200000` zeggen; dat is goed. Een ander doeladres betekent dat
   `bi_dram[0].start` niet 0x20_0000 is (gemeten: 0x20_0000): pas
   `DRAM_BASE` in het image-script aan, zodat text_offset klopt.
3. **De bunny op de UART.** Stilte na `Starting kernel` en het goede
   adres: de MMU-map of de UART. De UART is 0xFE66_0000 in het
   Device-gebied van de map; het image draait uit Normal op 0x0221_0000.
4. **`boot: ... EL2`.** `booti` levert op EL2 af (gemeten). EL1 geeft
   `HOPOS_BOOT_EL` en parkeren.
5. **`fdt: N bytes at 0x7c......, bootargs "hopos.node=radxa-1
   hopos.stage=hop", hopos.cfg M bytes`.** Dit is de kopie van DTB en initrd
   naar de heap; M is de config ín de container (de standaard is ~170
   bytes), niet de hele initrd. Staat er `HOPOS_RAM_CHECK_SKIPPED`, dan lag
   x0 niet in het DRAM of was de header geen FDT; noteer x0. `GIC differs
   from the board plan` betekent dat de DTB andere GIC-adressen heeft dan
   0xFD40_0000 en 0xFD46_0000. Direct erna: **`stage: 1500 KB image from
   the initrd at 0x..., role hop`** (het adres ligt in de heap, onder
   0x0620_0000). `stage: initrd ... refused: ... HOPOS_STAGE_REFUSED` noemt
   het getal dat niet klopt (en dan is ook `hopos.cfg 0 bytes`); `stage: no
   image in the initrd` is een kaart zonder Hop (`APP=`, of een oude kaart
   met een kale `hopos.cfg`).
6. **`fb: none from U-Boot`.** U-Boot laat hier geen scherm achter (gemeten).
   Staat er toch een framebuffer, noteer de geometrie: er is nog geen
   framebuffer-console in v3.
7. **`trng: rk3568-rng at 0xfe388000 online (...), the kernel DRBG is
   seeded from rk3568-rng HOPOS_RNG_RK3566_UP`.** Direct na de stage-regel,
   nog in `discover`. Het blok staat niet in de DTB van U-Boot; het adres
   komt uit rk356x-base.dtsi en werkte op v2 (06-08). Stilte hier, na de
   stage-regel en vóór `boot: HopOS`: het blok houdt de bus vast (de
   klokgates CLKGATE_CON(9) bit 10 en 11 gaan er vlak vóór open). `trng:
   WARNING rk3568-rng ...: <reden> ... HOPOS_RNG_INSECURE` noemt de reden:
   `a round of 32 zero bytes` (geen klok), `START still set after 10 ms`,
   of `two equal rounds` (een vastgelopen bron). De node draait dan op de
   jitter-seed.
8. **`boot: HopOS v3.0.0 on rk3566, EL2, 4 cores (big), 2046 MB DRAM
   HOPOS_BOOT`.** Het aantal cores en MB komen uit de DTB.
9. **De watchdog, de thermiek.** Vlak na de `irq:`-regel van stap 10,
   als de taken starten:
   `watchdog: hardware reset armed (DW-WDT at 0xfe600000, TOP 15 = 89.4 s
   (measured in CCVR at 24 MHz, fixed-top true)) - boot guard: ...
   HOPOS_WD_ARMED`. `from the table, CCVR did not load` in plaats van
   `measured` betekent dat de teller na de kick nul las (v2 las 2^31).
   Zodra het net op is en Hop slaat: `watchdog: liveness proven ...
   HOPOS_CANARY_LIVE`. Na een warme flip vanaf een kern die hem al
   wapende zegt de `HOPOS_WD_ARMED`-regel `already armed by the previous
   kernel`. Er is geen knop om hem uit te zetten (O2 van 01-11). Daarna
   de sensor: `hwmon: TSADC at 0xfe710000 cpu 41.5C gpu
   40.2C (raw N/M) ... HOPOS_TSADC_UP`, en `temp=41.5C` in de tik. Op v2
   converteerde deze sensor op dit bord nooit (06-08, drie hypothesen
   weggestreept); gebeurt dat hier ook, dan: `hwmon: TSADC at 0xfe710000
   gives no valid code (raw cpu 0 gpu 0, ...; user_con .. auto_con ..
   clksel51 .. grf_tsadc_con ..) ... HOPOS_TSADC_NONE`, en de tik houdt
   `temp=-`. Noteer de vier registers: v2 las `0x8fc0`, `0x10033`,
   `0x1715` en `0x107`.
   Daarna de klok: `dvfs: rk3566 vdd_cpu at i2c0 0x40 id1 0x88: vsel0
   0x8f = 900 mV (running), vsel1 0x97 = 1000 mV (suspend), i2c clkdiv
   0x3e003d; core 816 MHz by SCMI, apll 816 MHz by the CRU HOPOS_RK_VDD`
   (gemeten 03-10), `HOPOS_CLOCK_UP` en de boot-flank `dvfs: rk3566 core
   816 -> 1800 MHz (apll 816 -> 1800), vdd_cpu 900 -> 1150 mV
   HOPOS_RK_DVFS`; na 30 s stil `1800 -> 816 ..., vdd_cpu 1150 -> 850 mV`.
   `HOPOS_CLOCK_NONE` met `vdd_cpu at i2c0 0x40: i2c0 ...` of
   `SCMI_CLK_CPU via the TF-A: ...` noemt waarom er geen knop is; de klok
   blijft dan waar U-Boot hem liet. `short of the asked operating point`
   in de regel: de buck kwam niet op zijn spanning en de klok bleef eronder.
   Met `GUI=1` komt na het net (stap 13) de USB: `usb: usbdrd30 clocked (clkgate10
   0x...0700 -> 0x...0000, softrst9 was .., otg phy_sus .. -> 0x0,
   pipe-grf usb3otg0_con1 ..); it is the USB-C power input: ...` en, als de
   core nu antwoordt, `usb: 2 controller(s) up ... HOPOS_USB_UP`.
   `HOPOS_USB_OTG_REFUSED` betekent dat de gates van usbhost30 of de
   PIPE-klokken dicht lazen terwijl usbhost30 draait: dan klopt de kaart uit
   clk-rk3568.c niet voor dit silicium en is usbdrd30 niet aangeraakt.
   Zegt de DWC3-regel daarna nog steeds `GSNPSID names no DWC3 core`, dan
   was het de klok niet; noteer clkgate10 en softrst9 uit de regel.
10. **`irq: ...` en daarna `HOPOS_TICK 1`, `2`, ...** De tik loopt alleen als
   de slaap wekt. Het board slaapt met WFI op de timer-PPI 30 (bewezen op
   QEMU, niet op dit silicium). Blijft de tik staan: zet in
   `board/rk3566/src/lib.rs` `sleeper()` op `Mode::Wfe`. `irq(timer=...)`
   moet oplopen. `irq: FAIL ... HOPOS_IRQ_FAIL` noemt de reden
   (redistributor-frame, slapende redistributor).
11. **De NIC-keten.** Elke stap faalt met een eigen regel en `HOPOS_NIC_FAIL`:
    - `no DWMAC4 at GMAC1 (version 0x0...)`: de pclk staat dicht; de regel
      geeft clksel33, clkgate17 en softrst14.
    - `DMA soft reset does not clear (bus mode 0x00000001)`: de AXI-kant
      staat in reset (de meting van 05-08); de AXI-reset-puls ging mis.
    - `no PHY on the MDIO bus (... mdc/mdio mux 3/3 ...)`: de mux moet 3/3
      zijn; anders landde de pinmux niet. Een PHY op adres 0 en 1 met
      dezelfde id is normaal: de kern kiest 1 (de DTS).
    - `RTL8211F at 1 rgmii delays (tx, rx) Ok((..)) -> Ok((true, true))`:
      ná de schrijf moeten beide `true` zijn. Zonder TX-delay komt er een
      link, werkt RX en verdwijnt elk verzonden frame (06-08).
    - `phy: no link within 8000 ms`: kabel, of de PHY-reset.
12. **`net: dwmac4 at 0xfe010000 version 0x3051, PHY 1, link 1000 Mbps full
    duplex, intid 64`** en een diag-regel met `rxdesc[0] 0xc1000000`
    (OWN|IOC|BUF1V: de ring staat klaar). Dan `HOPOS_NIC_UP mac=02:48:4f:50:..`.
    Het adres volgt uit `hopos.node` (net::nodemac); `HOPOS_MAC_FIXED`
    betekent dat er geen node-naam was.
13. **DHCP en de lijn.** De kern vraagt een lease (net.rs). In
    `HOPOS_TICK` moet `nic=` oplopen bij verkeer: de lijn is SPI 32 (INTID
    64), NIE op bit 15 (de 4.10-indeling; 20-09 bewezen). Blijft `nic=0`
    terwijl er frames binnenkomen, dan komt er niets meer door: de pomp
    pollt niet en heeft geen vangrail meer (01-11, O3).
14. **Doorvoer.** v2 haalde met Normal-NC op de NIC-DMA 56,6 MB/s in en 99 uit
    (was 15,5 op Device). De map zet die regio vanaf de eerste instructie
    Normal-NC; meet met de netmeter als DHCP er is.
15. **De slots.** De kooi (`HOPOS_CAGE_UP`) staat in het Device-venster op
    0x0622_0000. De app-cores heten 0x100, 0x200, 0x300 (aff1, gemeten
    05-08). Vóór het netwerk al: `system: privilege minted for slot 1 (Hop)
    HOPOS_PRIVILEGE` (alleen met rol hop). Met `APP=` verwacht
    `slots: no staged image, nothing placed HOPOS_SLOT_NONE`; met
    `APP=appspike` twee keer `HOPOS_SLOT_START` en
    `HOPOS_APPSPIKE_DONE pass=9 fail=0`.
16. **De kick van de OS-core.** Het board gebruikt SGI 7, niet SGI 8 zoals QEMU
    virt: TF-A op Rockchip houdt SGI 8 tot en met 15 als Secure. De zelftest
    van de OS-core (`HOPOS_OS_SELFTEST`) moet `kick=` met een tijd tonen;
    `HOPOS_OS_SELFTEST_FAIL` met een lege kick is dit punt.

## De eerste boot met Hop

Hop woont op de OS-core naast de kern (slot 1, core 0) en wacht eerst op een
luisterend net (de DHCP-lease, zonder termijn: komt hij niet, dan reset de
guard van 120 s de node; zonder NIC `HOPOS_HOP_NO_UPLINK` en meldt Hop zijn
slot-adres). Daarna, in deze volgorde:

17. **`HOPOS_HOP_START slot=1 core=0 cpu=0 entry=0x... part=0x...+0x4000000
    image=N env=M`.** N is de maat van `hop.elf` uit het script (`wc -c
    target/radxa-zero3/hop.elf`, 1536920 op 30-09); een ander getal is een
    andere initrd dan je denkt. De regel `slots: Hop env: ...
    HOPOS_HOP_ENV` ervoor toont de env met `HOPOS_NODE_IP` (geheimen als
    lengte). `slots: role hop but no staged image, Hop not started
    HOPOS_HOP_FAIL`: stap 5 zei al dat er geen image was.
    `Hop placement:`, `Hop env:` of `Hop not started: elf: ...` met
    `HOPOS_HOP_FAIL`: de regel noemt het getal.
18. **`slot 1: 2 port(s) published tcp+udp on the uplink: :8080 :9080
    HOPOS_SLOT_PUBLISH`** (vóór `HOPOS_HOP_START`): de switch zet de twee
    poorten van de node (agent en leader) door naar Hop.
19. **`slot 1: hop: agent up node=radxa-1 cluster=hopos agent=:8080
    leader=:9080 cores=3 HOP_UP`**, en kort daarna `HOP_LEADER` (één node is
    zijn eigen leader). `cores=3`: de drie app-cores; Hop zelf deelt core 0
    met de kern. De API staat open (`hopos.insecure=1` uit de vaste config,
    luid als `HOPOS_API_INSECURE`). Zonder schijf weigert de kern elke
    bestandscall van Hop (geen hopfs, zie hieronder): een weigering over
    `/hop` in zijn log is verwacht, maar `HOP_UP` moet er toch komen.
20. **`curl http://<ip>:8080/health`** vanaf de laptop, met het adres uit de
    lease (`HOPOS_NODE_IP` in `HOPOS_HOP_ENV`, of de DHCP-server).
21. **welcome, de pagina.** Bouw en serveer hem op de laptop:

    ```sh
    cargo build --release --target aarch64-unknown-none-softfloat -p welcome
    mkdir -p /tmp/art
    rust-objcopy --strip-debug target/aarch64-unknown-none-softfloat/release/welcome /tmp/art/welcome.elf
    (cd /tmp/art && python3 -m http.server 8000)
    curl -X POST -d '{"name":"welcome","driver":"hop","artifacts":[{"url":"http://LAPTOP:8000/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}' http://<ip>:9080/v1/jobs
    curl http://<ip>/            # de bunny, node radxa-1, het slot, de uptime
    curl http://<ip>/health      # ok
    ```

    Op de console: `HOP_JOB_PLACED slot=2`, `HOPOS_SLOT_START slot=2 core=1
    cpu=1` (0x100: de eerste app-core, koud via PSCI CPU_ON), de poort 80
    naar slot 2, en `HOPOS_WELCOME_UP port=80`. Hangt het na `cage: slot 2
    built`: de PSCI-weg naar de app-core (stap 15 en 16). `curl -X DELETE
    http://<ip>:9080/v1/jobs/welcome` stopt hem en sluit poort 80 weer.

## Niet gedaan

- Geen schijf: de SD/eMMC-controller heeft geen v3-driver. `probe_disk` geeft
  `Ok(None)`, dus `HOPOS_DISK_NONE` en geen hopfs. Hop draait zonder volume:
  zijn staat en de jobs overleven geen herstart, en `/hop/init-jobs.json`
  bestaat niet (`hopos.init[]` in de config werkt wel, via de env).
- Hop komt uit de initrd, dus een nieuwe Hop is een nieuwe kaart (of een
  nieuwe `hopos.ird` op de FAT). Een koude kern-flip start Hop uit dezelfde
  initrd (U-Boot's DTB en initrd blijven gaten in de pool); op ijzer nog
  niet geprobeerd.
- Op ijzer is Hop op de Radxa nog nooit gestart: de container, de splitsing
  en de kaart zijn op de host bewezen, niet op dit silicium (QEMU heeft
  geen RK3566).
- Geen framebuffer-console en geen VOP2/HDMI-scanout.
- De TSADC converteerde op v2 nooit (zie stap 9); de Rust-init is die van
  Linux, en of hij hier wel meet, zegt de eerste boot. De
  hardware-thermal-shutdown blijft uit tot er een meting is.
- usbdrd30 is de USB-C, en dat is ook de voedingsingang: in hostmodus
  levert hij geen VBUS (de boost van de RK817, `OTG_SWITCH`, zit achter
  I2C; v3 praat op i2c0 alleen met vdd_cpu, `board/rk3566/src/i2c.rs`). Een apparaat dat van de bus leeft, heeft daar
  een hub met eigen voeding nodig. De USB3-poort van deze core bestaat op
  de RK3566 niet (geen combphy0); `pipe-grf usb3otg0_con1` in de regel
  zegt hoe U-Boot hem liet.
- Geen temperatuurrem op de klok: de TSADC geeft geen code, dus de cores
  blijven onder last op 1800 MHz. Linux remt de rk3566 bij 85 C (passief)
  en de hardware schakelt bij 95 C uit; hier geen van beide. `hopos.mhz=`
  klemt het plafond (1416, 1608) als het bord zonder koeling warm wordt.
- De pool eindigt op 0xF000_0000: een bord met 8 GB gebruikt alleen de onderste
  3,75 GB.
- `hopos/src` noemt het board nog `board_qemuvirt`; main.rs laat die naam
  naar `board_rk3566` wijzen, net als voor de Pi's. Een neutrale naam in
  slots.rs, cage.rs en flip.rs is de nette vervolgstap.
