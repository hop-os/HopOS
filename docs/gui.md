# Het gui-vlak

Wat HopOS v3 met een scherm, een toetsenbord en een muis doet, en wat niet.

## De beslissingen

1. **Geen GPU-driver.** De kern schrijft pixels in een beeld dat al loopt:
   van de firmware (UEFI GOP, de VideoCore-mailbox van de Pi), van QEMU
   (`ramfb`), of op de Radxa van onze eigen scanout-keten (VOP2 en HDMI,
   want U-Boot laat daar geen beeld achter, gemeten 05-08).
2. **De console is van de kern, het glas van de display-app.** Bij boot
   staat de console op het scherm: de bunny als vaste kop, rechts drie
   meetregels (kern-heap, datum, tijd, elke seconde), de log eronder. Vraagt
   een job het glas, dan krijgt de display-app het venster in zijn kooi en
   gaat de console eraf; stopt het slot (ook een crash), dan komt de console
   terug. Eén houder tegelijk; de eerste wint.
3. **Een grant alleen voor een device zonder DMA** (gui-ontwerp §7). Een
   framebuffer is een pixelbuffer: geen registers, geen DMA, dus hij mag in
   een kooi. Een xHCI is een bus-master en er is geen IOMMU: die blijft van
   de kern, en de app krijgt de invoer als stroom (06-08).
4. **Kaal is kaal.** Zonder de feature `gui` linkt de binary geen regel
   display-code en is elk board headless.

## Bouwen

`--features gui` naast het board, bijvoorbeeld
`cargo build --release --target aarch64-unknown-none-softfloat -p hopos --features board-rk3566,gui`.
De feature zet `gui` aan op het gekozen board (`board-x?/gui`) en linkt
`driver-fb`, `gui-fbgrant`, de invoer naar het glas (`gui-usbin`) en de
bouwsteen `usb` met zijn HID-kant (`usb/hid`, `driver-hid`).

De bouwsteen `usb` (de crate `usb`, `driver-xhci`, `driver-dwc3`,
`hopos/src/usb.rs` en de `usb`-feature van het board) is geen keuze op
zich: `gui` trekt hem mee voor toetsenbord en muis, `media` voor de
optische drive, zonder de ander (docs/sequence-hopos.md §14.3). Wat een
build niet heeft, staat als stub in `hopos/src/off.rs`.

| Crate | Wat het bezit |
| --- | --- |
| `driver/fb` (`driver-fb`) | de console op een lineaire framebuffer: `Desc`, `Console` (kop, log, `header_status`), geen allocatie |
| `kern::grants` | het `DeviceGrant`-primitief: één venster, één houder, `WindowMap` naar de stage-2, de adoptie na een flip, de haken (`Grants`) |
| `gui/fbgrant` (`gui-fbgrant`) | het beleid: wie het glas krijgt, de `FB_*`-env, de console eraf en terug |
| `gui/rkscan` (`gui-rkscan`) | de RK3566-keten: PD_VO, VOP2, DW-HDMI |
| `driver/usb/xhci`, `usb` | de bus (bouwsteen `usb`): host, enumeratie, de scan, de opslag; met `usb/hid` de rapporten van boot-HID (`driver/usb/hid`) |
| `gui/usbin` (`gui-usbin`) | de bezorging van toetsen en muis aan de display-app: de rij, de cursor, de JSON-regels |
| `driver/usb/dwc3` (`driver-dwc3`) | een Synopsys DWC3-core in hostmodus (de RK3566), vóór de xHCI |
| `board/<x>/src/usb.rs`, `Board::usb_hosts` | welke controllers een board heeft: venster, lijn, soort (xHCI of DWC3), en een eigen stuk DMA-geheugen; PCIe en firmware-handshake zijn dan gedaan |
| `hopos/src/gui.rs` | de bedrading: console op het glas, meetregels, de invoer van de USB-taak (`UsbInput`), de grant-haken |
| `hopos/src/usb.rs` | de USB-taak (bouwsteen `usb`): de controllers, de ronde van de manager, de stop voor een flip |
| `hopos/src/net/input.rs` (`net::input`) | de input-listener op `10.100.0.1:7879`: de regels naar de houder van het glas |
| `applib/src/fb.rs` (`applib::fb`) | de app-kant: de `FB_*`-env, het venster Normal-NC in de stage-1 van de app (`applib::mmu`), de regellezer van `INPUT_ADDR` |
| `apps/display` | de display-app: achtergrond, klok, de laatste invoer en de cursor op het glas |

## Per board

| Board | Beeld | Waar het vandaan komt | Status |
| --- | --- | --- | --- |
| QEMU virt | `-device ramfb` | `board/qemuvirt/src/ramfb.rs`: fw_cfg `etc/ramfb`, 1280x800 op 0xC400_0000 | `GUI=1 sh tools/qemu-test.sh`: de console op het glas, screendump getoetst |
| UEFI (EDK2, QEMU) | GOP | `board/uefi/src/gop.rs`, vóór `ExitBootServices`; met `-device ramfb` geeft EDK2 een GOP | `GUI=1 sh tools/qemu-uefi-test.sh`: GOP 800x600 op 0xfc7a_0000, de console erop, de hele keten groen |
| Orion O6N | GOP van de eigen firmware | die van het UEFI-board (`board_uefi::On`, `board/uefi/src/gop.rs`) | nooit op ijzer |
| Ampere Altra | GOP (BMC-VGA) | idem | nooit op ijzer |
| Raspberry Pi 4 en 5 | HDMI via de firmware | `board/raspi/src/vcfb.rs`: DTB-simplefb, anders `FB_ALLOC` over de mailbox (1920x1080) | nooit op ijzer |
| Radxa Zero 3E | HDMI via onze eigen keten | `board/rk3566/src/display.rs` over `gui-rkscan` | nooit op ijzer |

De USB-invoer per board (`Board::usb_hosts`, `board/<x>/src/usb.rs`):

| Board | Controllers | Hoe het board ze vindt | DMA | Status |
| --- | --- | --- | --- | --- |
| QEMU virt | `qemu-xhci` | PCIe-host van virt (ECAM 0x3f00_0000, `highmem-ecam=off`), klasse 0x0c0330 op bus 0; de BAR wijzen we zelf toe uit het 32-bit MMIO-venster (geen firmware) | bovenste 2 MB van `BLK_DMA`, 0x4fe0_0000 | `GUI=display sh tools/qemu-test.sh` groen |
| UEFI (EDK2, QEMU) | elke xHCI op de MCFG-segmenten | klasse 0x0c0330, BAR 0 van de firmware, boven 1 TB via `map_device` | bovenste 2 MB van `BLK_DMA`, gelijk verdeeld | handmatig 29-09: twee xHCI's (de ESP-stick en `qemu-xhci`), toetsenbord, muis, opslag |
| Orion O6N | tien native xHCI's (XHC0..5, USB0..3) | platformapparaten in de DSDT (`PNP0D10`), aan/uit en host/device uit het variabelen-RAM van de firmware (`GNVA`, `GNVL`); `usb_firmware.rs` leest de exacte vorm | bovenste 2 MB van `BLK_DMA` (de NVMe neemt de onderste 4), tien vaste stukken | nooit op ijzer |
| Raspberry Pi 5 | twee xHCI's in de RP1 (`rp1-usb0`, `rp1-usb1`) | achter de PCIe-link die de NIC-probe traint; zonder link geen controller | 0x14a0_0000 (2 MB, `board_raspi::usb::USB_DMA`), `bus_off` 0x10_0000_0000 | nooit op ijzer |
| Raspberry Pi 4 | de VL805 | de BCM2711-RC op (gen 2), BAR 0 op PCIe 0xf800_0000 = CPU 0x6_0000_0000 (gigabyte 24 als Device erbij), met de endpoint dicht, dan `NOTIFY_XHCI_RESET` over de mailbox als config 0x50 nul is, wachten tot er een versie staat (hoogstens 1 s, twee ketens), en pas dan memory-decode aan | 0x14a0_0000 (2 MB) | nooit op ijzer |
| Radxa Zero 3E | twee DWC3-cores (`usbdrd30`, `usbhost30`) | vaste SoC-adressen; hostmodus via `driver-dwc3`, de klokken en PHY's van U-Boot | `USB_DMA` van het plan (0x06c0_0000), gehalveerd | nooit op ijzer |
| Ampere Altra | (xHCI op PCIe, zoals UEFI) | niet bedraad: `board/altra` geeft `usb_hosts` niet door | | |

### Wat je op het scherm hoort te zien

- **Radxa (HDMI).** 1920x1080p60 uit de eigen keten: PD_VO aan via de
  PMU, de HPLL en de VOP2-klokken, de VOP-IOMMU uit, VP0 met Smart0 scant
  `FB_RAM` (0x0700_0000, 8 MB, Normal-NC, het PA-plan van het board), dan
  DW-HDMI met de PHY en de frame composer (DVI, geen infoframes).
  De keten drijft altijd 1080p60 en leest geen EDID. De
  console krijgt 16x16-cellen (120x67). Regels: `display:
  1920x1080p60 on HDMI (sink attached: true) HOPOS_DISPLAY_UP`, dan `fb:
  console on 1920x1080 ... HOPOS_FB_CONSOLE`. Faalt een laag, dan zegt de
  regel welke, met de registers van die laag (`HOPOS_DISPLAY_FAIL`); de
  console tekent dan in een buffer die niemand uitscant. `HOPOS_DISPLAY_NOLATCH` = VP0 nam de
  config-done niet over.
- **Pi 4 en Pi 5 (HDMI via de firmware).** De firmware zet het beeld aan;
  wij vragen er een buffer van via de mailbox. Let op de diepte: GEMETEN
  11-07 op de Pi 5 bleef de scanout op 16 bpp terwijl de depth-tag 32 zei;
  de pitch beslist (`vcfb::from_mbox`). De buffer ligt buiten de
  `/memory`-banken; `vcfb::plan_map` zet zijn 2 MB-blokken Normal-NC in de
  tabel van de eerste gigabyte (alleen lege regels). Regels: `fb:
  VideoCore framebuffer ... HOPOS_FB_UP`, `HOPOS_FB_CONSOLE`. Zonder
  HDMI-kabel bij boot kan de firmware weigeren: dan `HOPOS_FB_NONE` en de
  node draait headless (de ontdekking is één keer per boot, 19-07).
- **O6N (GOP).** Wat de firmware in de GOP zet, meestal de resolutie van
  het BIOS-scherm. Regels: `uefi: GOP WxH stride S format F @ ...
  HOPOS_UEFI_GOP` vóór de exit, `HOPOS_FB_CONSOLE` erna. De buffer gaat
  Normal-NC in de identity map (`gop::map`, op 4 KB precies). Nooit op
  ijzer gezien; de O6N-console na de exit is zelf nog open (docs/boards.md).

## De grant

Een job vraagt het glas met `GUI=display` in zijn env; dat is wat de kern
leest (`FB=1` werkt ook). Hop's `StartSpec` draagt geen eigen
`gui`-veld (hop/runner, 29-09): de jobspec zet het met
`"env":{"GUI":"display"}`, en Hop geeft de env ongewijzigd door. De kern
geeft de houder in de env:

| Sleutel | Waarde |
| --- | --- |
| `FB_BASE` | het IPA van de buffer in de kooi: `0x2000_0000` plus de offset in zijn 2 MB-blok (de buffer mag fysiek boven 4 GB liggen: de ramfb-vondst van 19-07, 0x1_bc7a_0000) |
| `FB_WIDTH`, `FB_HEIGHT` | pixels |
| `FB_STRIDE` | bytes per regel |
| `FB_BPP` | 32 of 16 |
| `FB_SWAP` | `1` als rood en blauw ruilen (GOP-formaat RGB) |
| `INPUT_ADDR` | waar HOP de invoer uitserveert, `10.100.0.1:7879`; alleen als het board USB-controllers noemt (en ingetrokken als er geen opkomt) |

De sleutels, de poort, de pixelregel (`FB_SWAP`, r5g6b5 bij 16 bpp) en de
JSON-regels van de invoer staan als contract in `abi::glass`; de grant,
de deliverer, de console en applib lezen ze daar.

De haken in de lifecycle (`kern::grants::Grants`, geïmplementeerd door
`hopos::gui::GuiGrants`):

| Stap | Haak | Plek |
| --- | --- | --- |
| na de claim, vóór de env op de control-page | `env` | `Request::Env` aan de lifecycle-actor, vlak vóór `Stream::put_env` in `Stream::arm` (`kern/src/system.rs`), voor een start van Hop en voor de boot-plaatsing (`kern::system::place`) |
| na de kooibouw, vóór de dispatch | `arm` | `Lifecycle::arm` (`kern/src/slots.rs`); faalt hij, dan is het een startfout: poorten dicht, grant terug |
| bij de adoptie na een flip | `adopt` | `Lifecycle::adopt`, na alle eigendomsclaims en vóór de servicers |
| na een bevestigde stop, en bij een abort | `release` | `Lifecycle::stop` en `Lifecycle::abort` |

De actor bezit de aanbieder (`Lifecycle<…, G: Grants>`): `hopos/src/slots.rs`
geeft `GuiGrants` mee in de gui-smaak en `NoGrants` kaal. Past de env met de
`FB_*`-regels niet op de control-page (`CTRL_ENV_MAX`), dan gaat de grant
terug en draait de app headless (`HOPOS_FB_ENV` of `HOPOS_GRANT_ENV`).

Op QEMU: `GUI=display sh tools/qemu-test.sh` laat Hop de display-app
plaatsen (een job met `"env":{"GUI":"display","DISPLAY_QUIT":"27"}`; de kern
plaatst sinds 01-11 alleen Hop, O7), met de USB-apparaten erbij, en loopt de
hele levensloop: per ronde `HOPOS_FB_GRANT` en `HOPOS_FB_ARM`, de app op het
glas en aan de invoer, en na Escape `HOPOS_FB_RELEASE` (de console terug) en
een tweede houder: Hop herstart de service (zie de checklist).

## De display-app

`apps/display` is een gewone app op `applib`, teruggebracht tot wat het glas en de invoer bewijzen: een
achtergrond, een klok, de tellers, de laatste tien invoerregels en een
cursor. Geen compositor, geen `/screen.png`, geen `/kvm`. Wat hij van
`applib` gebruikt (`applib::fb`):

1. **De `FB_*`-env lezen** (`Glass::from_env`) en weigeren wat niet klopt:
   een stride kleiner dan breedte maal bytes per pixel, een BPP anders dan
   16 of 32, een venster buiten het glasvenster van de kooi.
2. **Het venster in de stage-1 van de app** (`fb::map`): sinds 30-09 heeft
   elke app een stage-1 (`applib::mmu`, gezet in `_start`: de
   RAM-declaratie Normal-WB, de control-page Normal-NC, de ringen
   Normal-WB, SCTLR M/C/I), en `map` zet het glas er als **Normal-NC** op
   4 KB bij (`FB_BASE` staat niet op 2 MB), in dezelfde tabellen in de
   64 KB onder het image (`LINK_TEXT_OFF`). Tot 30-09 legde `fb.rs` een
   eigen tabel met SCTLR.C uit en de staart Device; waarom dat niet meer
   hoeft en waarom de control-page NC blijft, staat in de moduledoc van
   `applib::mmu`. De switcher bewaart het EL1-regime per bewoner, dus een
   yield verliest de map niet. Zonder stage-1 (`HOPOS_APP_NO_MMU`) tekent
   de app via Device, trager maar correct (`HOPOS_DISPLAY_NOMAP`); staat
   de MMU aan maar kwam het glas er niet in, dan tekent hij niet.
3. **De invoer**: een TCP-client naar `INPUT_ADDR` die regels leest
   (`LineReader`, zonder allocatie), één JSON-event per regel, precies het
   object dat `POST /input` van de browser-KVM aanneemt
   (`{"k":"key","c":..,"v":..}`, `{"k":"move","x":..,"y":..}`,
   `{"k":"btn",...}`, `{"k":"wheel",...}`, `Input::parse`); een lege regel
   is een keepalive (elke 5 s). Drie gemiste keepalives of een verbroken
   stroom is opnieuw bellen met een oplopende pauze (na een kern-flip).
4. **Een eigen tekenaar** (`apps/display/src/paint.rs`) in het formaat van
   de env (16 of 32 bpp, `FB_SWAP`), met het font van de console van de
   kern (`driver_fb::glyph`, `Desc::encode`).

Twee taken met elk één eigenaar: de tekentaak bezit het glas (wacht op een
gebeurtenis of op de volgende seconde van de klok), de invoertaak de
verbinding; ertussen een SPSC-rij met waarden (vol is weggooien en
tellen). De app blijft leven; alleen met `DISPLAY_QUIT=<code>` in de env
stopt hij op die toets (voor de QEMU-poort, die zo de hele levensloop van
de grant toetst; een jobspec zet hem niet).

Markers: `display: window 0x20000000+... mapped Normal-NC in stage-1`,
`display: glass WxH ... HOPOS_DISPLAY_UP w=W h=H`, `display: input stream
from 10.100.0.1:7879 HOPOS_DISPLAY_CONN`, en bij veranderde tellers
(hooguit vijf keer per seconde) `display: keys=.. moves=.. buttons=..
wheel=.. dropped=.. HOPOS_DISPLAY_INPUT keys=K moves=M`.

Bouwen en draaien:

```sh
cargo build --release --target aarch64-unknown-none-softfloat -p display
# QEMU met een venster en Hop: klik erin en typ, na de job hieronder.
GUI=display ARTIFACT=display image/qemu-run.sh
```

De jobspec (die van welcome, met het glas erbij):

```json
{"name":"display","driver":"hop",
 "artifacts":[{"url":"http://LAPTOP:8000/display.elf"}],
 "memory_limit":33554432,"env":{"GUI":"display"}}
```

## De USB-invoer

HOP bezit de controllers (een xHCI is een DMA-master en er is geen
IOMMU), leest boot-protocol-rapporten van toetsenbord en muis en levert ze
als JSON-regels aan de houder van het glas. De display-app belt, HOP
luistert (`INPUT_ADDR` reist mee in de grant): zo hoeft HOP de app niet te
vinden, en controleert hij op accept (en per regel opnieuw) dat de beller
het slot van de houder is (de switch weigert een frame met andermans
bron-MAC, 06-08).

De keten, na het netwerk (`hopos::usb::start`):

1. `Board::usb_hosts` noemt de controllers: het venster, de lijn (nog
   ongebruikt: de driver pollt), het soort (xHCI of DWC3) en een eigen stuk
   DMA-geheugen (Normal-NC, buiten de kern-RAM). PCIe-link, BAR's en
   firmware-handshake zijn dan al gedaan. Kaal bestaat de vraag niet.
2. Het invoeradres gaat meteen in de grant (`FbGrant::use_input`), vóór
   de USB-taak spawnt en dus vóór de eerste plaatsing: de bring-up slaapt
   op de executor, en een display-app die intussen het glas kreeg, moet
   het adres al hebben. Komt er geen controller op, dan trekt de taak het
   weer in (`stop_input`, `HOPOS_USB_NONE`).
3. De USB-taak bezit de `usb::Manager`. `Registry::bring_up` zet
   eerst élke controller stil (probe, reset) en start ze pas daarna, zodat
   een controller van de firmware of van de vorige kern nooit schrijft in
   een stuk dat een ander net opbouwt; een DWC3 gaat eerst in hostmodus
   (`driver-dwc3`). Dan de ronde van `Manager::step`: scannen (elke 500
   ms), rapporten ophalen (elke 4 ms), slapen op het timerwiel.
4. Elke wachtende hardwarestap van de driver (een poortreset tot 2 s, een
   commando tot 1 s, de DWC3-reset van 225 ms) is `async` en slaapt op de
   `Timer` van de taak (`sync::Timer`, in de binary het timerwiel van de
   executor, `executor::ExecTimer`). Tot 29-09 spinde de driver op de klok en hield een
   insteek de hele executor van de kern vast.
5. Wat de taak leest, gaat als waarde door een SPSC-rij (256, vol is
   weggooien en tellen, `gui::UsbInput`) naar de input-listener in
   `net/input.rs`. Die luistert
   op `10.100.0.1:7879` zodra de node-stack er is, laat alleen de houder
   binnen, laat een nieuwe verbinding van de houder de oude verdringen, en
   schrijft elke regel met een deadline van 1 s. Zonder verbinding vallen
   de regels weg en tellen ze (`HOPOS_INPUT_DROP`, de eerste drie keer een
   regel).

De les van 29-09 op QEMU: de driver schreef 64-bit registers hoog-eerst
(omdat het lage woord laat latchen), maar `qemu-xhci` latcht
CRCR en ERSTBA op het hoge woord (Linux schrijft laag-dan-hoog). De
controller haalde zijn eerste commando van adres 0 en zette HCE; elke
Enable Slot bleef zonder completion. `write64` schrijft nu hoog, laag,
hoog: beide soorten latchen op het volledige adres.

## Checklist per board

Elke stap met de consoleregel die erbij hoort.

**QEMU virt**

- [x] `GUI=1 sh tools/qemu-test.sh`: `fb: ramfb 1280x800 @ 0xc4000000
  HOPOS_FB_UP`, `fb: console on 1280x800 ... (80x50 cells)
  HOPOS_FB_CONSOLE`, en de screendump (via de monitor) heeft ruim 170.000
  tekst- en 849.000 achtergrondpixels: de bunny, de meetregels en de log.
  `SHOT_KEEP=pad.ppm` bewaart hem.
- [x] USB (`GUI=display sh tools/qemu-test.sh`, 29-09, twee keer groen):
  `usb: xHCI 1b36:000d at 00:01.0 on PCIe, registers 0x10000000+0x4000`,
  `usb: qemu-xhci xHCI 1.0, 64 slots, 8 ports`, `HOPOS_USB_UP`,
  `usb: qemu-xhci port 5: keyboard 0627:0001`, `port 6: mouse`,
  `input: listening on 10.100.0.1:7879 ... HOPOS_INPUT_UP`.
- [x] De display-app: `slot 1: fb granted ... HOPOS_FB_GRANT`,
  `HOPOS_FB_ARM`, `slot 1: display: window 0x20000000+0x3e8000 mapped
  Normal-NC in stage-1`, `HOPOS_DISPLAY_UP w=1280 h=800`, `input:
  10.100.0.2 connected ... HOPOS_INPUT_CONN`, `HOPOS_DISPLAY_CONN`; dan
  `sendkey a` en `mouse_move 40 30` via de monitor: `HOPOS_DISPLAY_INPUT
  keys=1 moves=1`; de screendump heeft ruim 1.012.000 pixels in de
  achtergrond van de app, 8.500 tekst, 55 cursor en geen console.
- [x] De levensloop: `sendkey esc`, `HOPOS_DISPLAY_QUIT`, `HOPOS_SLOT_DONE
  slot=1 exit=0`, `HOPOS_FB_RELEASE`; de kern plaatst hem opnieuw in slot
  2: `HOPOS_FB_GRANT`, `input: 10.100.0.3 connected`, en na een toets en
  een beweging `slot 2: ... HOPOS_DISPLAY_INPUT keys=1 moves=1`. Sinds
  01-11 plaatst Hop de app (O7): beide rondes in slot 2, de tweede is de
  herstart van de service door Hop.

**UEFI op QEMU (EDK2)**

- [x] `GUI=1 sh tools/qemu-uefi-test.sh` (`image/uefi-run.sh` geeft QEMU dan
  `-device ramfb`): `uefi: GOP 800x600 stride 3200 format 1 @ 0xfc7a0000
  HOPOS_UEFI_GOP` vóór de exit, `HOPOS_FB_CONSOLE` erna. De buffer valt
  niet op 2 MB, dus `cpu::memattr` laat hem staan zoals `gop::map` hem zette
  (Normal-NC op 4 KB); de regel `fb: left as the board mapped it` is dan
  verwacht. Nog geen screendump-toets in dit script.
- [x] USB, met de hand (29-09): `GUI=1 BOARD=uefi sh image/uefi-run.sh
  -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device
  usb-mouse,bus=xhci.0`: `usb: xhci0: xHCI 1b36:000d at 00:01.0 on PCIe,
  registers 0x800000c000+0x4000` (de ESP-stick) en `xhci1` (de
  toetsenbord en muis), `HOPOS_USB_UP`, `xhci0 port 1: mass storage`,
  `xhci1 port 5: keyboard`, `port 6: mouse`, `HOPOS_INPUT_UP`. Nog geen
  toets in `qemu-uefi-test.sh`.

**Orion O6N**

- [ ] HDMI of DP aan, boot van de stick met een gui-image:
  `HOPOS_UEFI_GOP` met een plausibele resolutie.
- [ ] De bunny en de log op het scherm; de tijd rechtsboven loopt.
- [ ] Na de exit: loopt het beeld door? (De GOP-buffer is van de firmware;
  een firmware die zijn scanout bij de exit stopt, is een bevinding.)
- [ ] USB: `usb: N of 10 native xHCI hosts enabled by the firmware`, en
  per host die uit staat een regel (`disabled by the firmware, or a
  device-role port`). Weigert de parser de DSDT, dan zegt de regel
  waarom (`unsupported CIX USB firmware description: ...`): een nieuwe
  firmware is dan een nieuwe meting.
- [ ] Per host: `usb: XHCn xHCI ...` en een PORTSC-regel; een toetsenbord
  in een USB-A-poort geeft `keyboard ...` op de poort waar hij zit.
- [ ] De display-app met Hop (`"env":{"GUI":"display"}`):
  `HOPOS_DISPLAY_UP`, `HOPOS_DISPLAY_CONN`, typen geeft
  `HOPOS_DISPLAY_INPUT`, en de cursor volgt de muis op het scherm.

**Raspberry Pi 4 en 5**

- [ ] HDMI in poort 0 vóór de stroom: `fb: VideoCore framebuffer ...
  HOPOS_FB_UP` met 16 of 32 bpp.
- [ ] De bunny en de log op het scherm; geen strepen (strepen = een
  diepte-verwisseling, 11-07).
- [ ] Geen bus-fault bij de eerste veeg (de freeze-jacht van 04-08: een
  verkeerd gelezen adres).
- [ ] Pi 5, USB: zonder `usb: the RP1 link is down` (de NIC-probe traint
  hem); `usb: rp1-usb0 xHCI ...` en `rp1-usb1`, hun PORTSC-regels, en een
  toetsenbord (06-08: een Logi Bolt gaf toetsenbord én muis op één
  dongle) als `keyboard` en `mouse`.
- [ ] Pi 4, USB, KOUD geboot: `usb: vl805 firmware 0x... loaded by the
  VideoCore after N us (attempt 1, reply 0x...)` (of `already loaded` na
  een warme flip), dan `usb: vl805 on PCIe (status 0xb0)`, de zelftest
  `ok`, `usb: vl805 xHCI ...` en de PORTSC-regel. Een fout is
  `HOPOS_USB_VL805` (de versie bleef 0; de endpoint blijft dan dicht en de
  xHCI-registers onaangeroerd) en een SError zegt zijn stap met
  `HOPOS_USB_SERROR`; de hele lijst staat in docs/boards-pi.md, stap 21.
- [ ] De display-app met Hop: `HOPOS_DISPLAY_UP`, typen geeft
  `HOPOS_DISPLAY_INPUT`; op 16 bpp (de Pi 5-diepte van 11-07) klopt de
  kleur van de achtergrond (donkergroen).

**Radxa Zero 3E**

- [ ] HDMI aan: `HOPOS_DISPLAY_UP` en `sink attached: true`.
- [ ] Een beeld op 1080p60; de bunny en de log.
- [ ] Zonder monitor: `sink attached: false`, geen hang.
- [ ] USB: per core `usb: usbdrd30 dwc3 id=5533.... gctl=...` (GSNPSID nul
  = niet geklokt: de CRU-gates en de PHY's zijn van U-Boot, en dan is dat
  de meting), dan de xHCI-regel en PORTSC. Een toetsenbord in de USB-A
  (`usbhost30`) geeft `keyboard`.

**Elk board, met Hop**

- [ ] Een job met `"env":{"GUI":"display"}`: `HOPOS_FB_GRANT` en de console
  gaat van het glas; met de display-app `HOPOS_DISPLAY_UP` en, met USB,
  `HOPOS_DISPLAY_CONN`.
- [ ] De job stoppen: `HOPOS_FB_RELEASE` en de bunny komt terug; de
  input-listener sluit de stroom van het oude slot bij de volgende regel of
  keepalive (binnen 5 s), of zodra een nieuwe houder belt
  (`HOPOS_INPUT_GONE`).

## Niet gedaan

- Alles buiten QEMU is gebouwd en niet gedraaid: de O6N-parser toetst
  tegen de vastgelegde DSDT, de Pi's en de Radxa alleen tegen hun
  datasheet.
- Interrupts: elke controller heeft een `irq` in zijn `UsbHost`, maar de
  driver pollt (elke 4 ms); de lijnen (RP1-MSI-X via de MIP, de
  GIC-SPI's van de Radxa, de `_CRS` van de O6N) zijn nog niet bedraad.
- Opslag op USB: de manager meldt een drive (`mass storage vid:pid`), maar
  de binary heeft nog geen verzoekenrij voor `BulkReq` en geen
  optical-driver erachter (`Sink::storage_attached` is leeg).
- De toets van `INPUT_ADDR` na een kern-flip op ijzer: de display belt na
  15 s stilte opnieuw; op QEMU is alleen de wissel van houder getoetst.
- `tools/qemu-uefi-test.sh` heeft nog geen `GUI=display`-stand; de
  UEFI-keten is met de hand gedraaid (zie de checklist).
- De O6N-toets dat het variabelen-RAM van de firmware in de ACPI-RAM van de
  EFI-geheugenkaart ligt; het UEFI-board geeft die kaart
  niet door.
