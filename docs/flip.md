# De kern-flip

Een draaiende node vervangt zijn kern zonder herstart. De apps, Hop, hun
verbindingen door de switch, de NAT-flows, de lease en de volumes blijven;
alleen de kern wisselt.

**De vorm (§14.4, 01-11): de flip is een gewone boot plus een handoff.**
Kern B loopt dezelfde bootreeks als een koude boot. Per stap één tak: leeg
beginnen, of het stuk van kern A overnemen. Elk onderdeel kent alleen zijn
eigen stuk: kern A vangt het (`capture`), kern B neemt het over
(`restore`). Alleen de flip zelf (`hopos/src/flip.rs`, en `setup` in
`hopos/src/main.rs` dat de stukken uitdeelt) weet dat er een flip is.

De code: `hopos/src/flip.rs` (de haak, de vangst, de sprong en de landing),
`kern/src/kernflip.rs` (het blob, de bundel, de recorder, de zwarte doos),
`cpu/src/el2/chain.rs` (de sprong en de uit-stub van de koude flip), de
ingangen (`cpu/src/boot.rs`, `board/uefi/src/entry.rs` en `flip.rs`,
`board/raspi/src/arch.rs`, `board/apple/src/head.rs`), `image/flip-bundle.sh`
(de bundel) en de `FLIP_*`-blokken in `board/*/src/slots.rs` (de adressen).

## De reeks, met per stap leeg of uit de handoff

| Stap | Leeg (koude boot) | Uit de handoff (warm) | Wie |
| --- | --- | --- | --- |
| klok | de vaste wandklok tot SNTP | `wall_off` van kern A | `clock::restore` in `flip::land` |
| opslag | `hopos.storage` zegt stateless of stateful | de generatie die kern A vastlegde (`keep`) | `storage::start` |
| switch | leeg | de uplink, de gepubliceerde poorten en de flows, vóór de eerste ronde | `net::start` met `net::Carried`, `Switch::restore` |
| hok | `cage::new`: eigen switch-code, CPU_ON | `cage::adopt`, per bewoner `adopt_slot`, dan `Lifecycle::adopt` | `slots::start` |
| lease | DHCP, dan `SetUplink` | uit de handoff, geen DHCP en geen `SetUplink` (H1) | `Node::run` |
| stack, listener | | idem; de listen-socket gebonden vóór de lus van poort 0, dan `NET_READY` | `Node::run` |
| Hop | plaatsen uit de staging, na `NET_READY` | de supervisor terug (`HOPOS_HOP_RESUMED`), na `NET_READY` | `HopNode::supervise` |

**Koud** is een handoff zonder hok (`Handoff::slots` is `None`): kern A
stopt de bewoners en zet de app-cores uit, kern B begint het hok leeg en
start Hop uit de staging, met de lease, de klok en de opslag van kern A.
**Mismatch** (een andere switch-code) weigert warm vóór de 202; Hop vraagt
dan koud (`HOPOS_COLD_FLIP` in zijn env zegt vooraf of dat kan).

**Waarom de switch en het hok vóór de lease, en niet erna.** In de tijd
staan ze vóór de eerste ronde van de executor (synchroon in `setup`), de
lease, de stack en de listener in de eerste ronde (`Node::run`, zonder
`.await` ertussen als de lease uit de handoff komt). Omgekeerd valt een
antwoord van een oude peer bij de nieuwe node-stack, en die doodt een
levende verbinding met een RST (9bf3c74); en een flow naar een slot dat
nog niet aangesloten is, vindt geen ring. Zo hoeft de switch niets vast te
houden (weg: `HoldAdoption`, `FinishAdoption`, `RestoreNat`, de
herstel-taak).

**Wat een warme flip niet overleeft:** de open TCP-verbindingen van de
node-stack (de system-API, de console over TCP). Daarvoor is de ene retry
van applib (O9). De next-hops van de node-stack ook niet: één ARP-ronde.
De staat van Hop in de kern (`STATE_PUT`) en een image van `HOP_RESTART`
leven in de heap van kern A en sterven met hem; Hop schrijft zijn staat
opnieuw zodra zijn system-client kern B bereikt.

**Na de 202 is er geen weg terug (§13.7).** Een vangst die faalt (hopfs
bevriest niet binnen 2 s, de switch antwoordt niet, `snapshot` weigert, het
blob past niet, de indeling van de sprong), een koude flip die een bewoner
of core niet stil krijgt, of een landing die een stuk niet terugkrijgt
(het blob, de cage, een bewoner, de NIC): `HOPOS_FATAL`, en de koude boot
meldt waar het stopte (`HOPOS_FLIP_LAST`). Tot de `fatal()` van het
fundament er is, is dat `main::fatal` (PSCI-reset, anders de watchdog op
zijn kortst, anders parkeren).

## Het blob

`kern::kernflip::Handoff`, versie 6, op `FLIP_HANDOFF_PA` (direct onder
het staging-maatwoord), met het pointer/magic-paar als laatste op de
boot-scratch. De kop is 128 bytes:

| Woord | Wat |
| --- | --- |
| 0, 1 | magic `HOPHAND1`, versie 6 |
| 2 tot 5 | nul (het venster van een oudere vorm, niet gelezen) |
| 6 | het aantal bewoners |
| 7 | de generatie van kern B |
| 8 | nul (de som van de bundel van een oudere vorm) |
| 9 | de vlaggen: bit 0 = geen hok (koud) |
| 10 | de wandklok (`wall_off`) |
| 11 tot 15 | de lease: ip en masker, gateway en dns, server en looptijd, T1 en T2, de tellerstand van de ACK; ip 0 = geen lease |

Daarachter de bewoners (`SlotState`: partitie, core, SMP-breedte, groep,
poorten, job, volumes), de conntrack (alleen flows van een bewoner die
meegaat) en een nulwoord. De lease kwam er op 01-11 bij in de vrije
kopwoorden: een kern die hem niet kent, schrijft er nul, en dan haalt kern
B een lease met DHCP en zet de switch zijn uplink na DHCP. Geen nieuwe
versie, dus een flip vanaf 3.0.20 landt (met DHCP).

## Inventaris: wie weet van de flip (01-11)

Uit de code (geen commentaar), met waar het in de nieuwe vorm heen ging.
**Vet** is veranderd in deze ronde.

### De haak, vóór de 202 (kern A)

| Plek | Wat | Nieuwe vorm |
| --- | --- | --- |
| `kern/src/system.rs` | `PrivOp::Flip`, `FlipBundle`, de reservering `FLIP_BUNDLE_JOB`, `Hooks::flip`, `MAX_FLIP_PATH` bij een start | blijft: de ene trigger |
| `hopos/src/flip.rs` `prepare` | som, bundel, `FLIP_ABI`, switch-code, staging, `relocate`, de config mee | blijft; **de indeling van de sprong (`Jump::check`) nu hier** |
| `kern/src/kernflip.rs` | `Bundle`, `flatten`, `stage_slot`, `sum64` | blijft |
| `kern/src/nodecfg.rs` | `ColdFlip` als `HOPOS_COLD_FLIP` voor Hop | blijft: Hop beslist warm of koud vooraf |
| `board/*` | `flip_carry_config` (Apple), `flip_carry_seed` (UEFI) | blijft: stukken van het board |

### De vangst, na de 202 (kern A): `capture`

| Plek | Stuk | Nieuwe vorm |
| --- | --- | --- |
| `hopos/src/storage.rs` `freeze_for_flip` | opslag | blijft; **`thaw_after_flip` weg** |
| **`hopos/src/net.rs` `capture`** | lease en conntrack | **nieuw: de lease die de keeper houdt (`LEASE`) en `Command::CaptureNat`** |
| `net/src/nat.rs`, `switch.rs` | conntrack | **`Nat::capture` zet zelf de masquerade dicht (`closed`); weg: `hold_adoption`, `finish_adoption`, `MAX_ADOPT`, de drie commando's** |
| `hopos/src/gui.rs` `quiesce_usb` | USB | blijft |
| `kern/src/slots.rs` `snapshot`, `validate_adoption` | hok | blijft |
| `hopos/src/flip.rs` `stop_residents`, `cores_off`; `cpu/src/el2/chain.rs` `send_off`; `hopos/src/cage_riscv.rs` `park_for_flip` | het hok leeg (koud) | blijft; **`cores_back`, `unpark_after_flip` weg** |
| `hopos/src/telemetry.rs`, `watchdog.rs` | de klok vol, een aai | blijft |
| `kern/src/kernflip.rs` | het blob, het paar, de recorder | **de lease erbij; `bundle_sum`, `window`, `total`, `old_*` weg uit de struct** |
| `cpu/src/el2/chain.rs` `chain` | de sprong, x3 = `FLIP_ENTRY` | zie "De ene ingang" (nog niet) |

### De ingang (kern B)

| Plek | Merkteken | Nieuwe vorm |
| --- | --- | --- |
| `cpu/src/boot.rs` `_start` | x3 = `FLIP_ENTRY`: elke core door, `FLIP_ENTERED` | zie "De ene ingang" (nog niet) |
| `board/raspi/src/arch.rs` `_pi_start` | x3 bewaard in x22 | idem |
| `board/apple/src/head.rs` | x22 | idem |
| `board/uefi/src/entry.rs`, `flip.rs`, `facts.rs` | x1 = 0, de feitenpagina, `hopos_efi_flip_facts` | idem: dit wordt de vorm voor elk board |
| `cpu/src/riscv/boot.rs` | geen; `jumped()` altijd waar | idem |

### De landing (kern B): `restore`

| Plek | Stuk | Nieuwe vorm |
| --- | --- | --- |
| `hopos/src/flip.rs` `land` | het paar, het blob | **geeft een `Landing` (hok, net) aan `setup`; de klok neemt hier zijn stuk over** |
| `hopos/src/main.rs` `setup` | alle stukken | **de enige die de handoff uitdeelt; de NIC die niet terugkomt is `HOPOS_FATAL`** |
| `hopos/src/storage.rs` `start` | opslag | **`keep` in plaats van `flip::landed()`** |
| `hopos/src/net.rs` `start`, `Node::run` | lease, uplink, poorten, flows | **`net::Carried`, `Switch::restore` vóór de eerste ronde, geen DHCP (H1), de listen-socket vóór de lus, `NET_READY`** |
| `hopos/src/slots.rs` `start` | hok | **een bewoner die niet terugkomt is `HOPOS_FATAL`; `Lifecycle::adopt` vóór de actor draait; weg: `republish`, `REPUBLISH`, `Request::Republish`, `carry_hop_ports` (compat van vóór 04-10)** |
| `hopos/src/kooi.rs` `adopt_slot`, `cage.rs` `adopt`, `kern/src/partmem.rs`, `pool.rs`, `grants.rs`, `gui/fbgrant` | hok | blijft |
| `hopos/src/slots.rs` `supervise(Some)` | Hop | blijft (`HOPOS_HOP_RESUMED`); **wacht op `NET_READY` in plaats van een poll van 50 ms** |
| `hopos/src/main.rs` `hop_adopted` | het token van Hop | blijft (een stuk van het hok) |
| `kern/src/slots.rs` `Life::adopted` | de root van een geadopteerde app blijft (F2) | blijft |
| `hopos/src/seed.rs` `plant` | vers zaad per bewoner | blijft |
| `hopos/src/watchdog.rs` | `flip::generation() > 1` wapent de boot-guard | **nog niet**: de ene guard (agent D) |
| `hopos/src/flip.rs` `guard` | de gratie | **wacht op `NET_READY`, zonder poll; adoptie is geen voorwaarde meer (die faalt hard)**; gaat weg met de ene guard |
| `applib/src/sys.rs` | de ene retry (O9) | blijft |

## De ene ingang (O6): nog niet gebouwd

Nu zijn er drie merktekens: x3 = `FLIP_ENTRY` (de DTB-boards en Apple),
x1 = 0 plus de feitenpagina (UEFI), en geen (riscv). Het merkteken doet
twee dingen: het laat een andere core dan core 0 door (de OS-core), en het
zegt "dit is een sprong, geen firmware-boot" (een oud paar na een
watchdog-reset adopteert nooit, O6N 04-10).

De vorm die we zoeken: **x0 = de feitenpagina, op elk board.**

1. Elk board heeft een feitenpagina op een vaste plek naast de recorder
   (zoals `FLIP_FACTS_PA` op UEFI): magic, versie, wat de firmware gaf (de
   DTB, de ADT van Apple, op UEFI de tabellen en de memory map), het
   venster van het koude beeld, en één veld meer: **waar de handoff ligt,
   of nul**.
2. Bij stroom aan bouwt de ingang (of `kmain`, vóór `discover`) die pagina
   uit wat de firmware gaf, met handoff nul. UEFI doet dat al (`prepare`
   en `go`).
3. Kern A zet het handoff-veld en springt met x0 = het adres van de
   pagina, x1 tot x3 nul.
4. De ingang toetst x0 tegen het vaste adres (een register-vergelijking,
   geen lees op een adres dat de firmware gaf, met de MMU uit). Gelijk: een
   sprong, elke core mag door, en `discover` leest de pagina. Anders: de
   firmware, alleen core 0, en de pagina wordt opnieuw gebouwd (een oude
   handoff gaat dan vanzelf weg: geen `Stale` meer nodig).
5. Het pointer/magic-paar op de boot-scratch vervalt: het handoff-veld is
   het paar.

Wat het kost: de vijf ingangen (asm), de trampoline, `discover` per board
(de DTB uit de pagina), een `FLIP_ABI`-stap (een kern A van nu springt met
x3, en een kern B van de nieuwe vorm zou dat als firmware-boot lezen, over
levende bewoners heen; `FLIP_ABI` 4 laat kern A zo'n bundel weigeren vóór
de 202), en `BOARD=rpi4` krijgt een andere stub. Ongetoetst blijft de M4
(geen QEMU) en het ijzer van de Pi 5, de O6N en de Altra. Daarom niet in
deze ronde: eerst de vraag aan Derek (hieronder).

## De procedure, voor elk board

De flip heeft één trigger: `POST /flip` op de agent-API van Hop, achter
dezelfde HMAC als een jobspec (met `hopos.insecure=1` zonder). Er is geen
console-commando en geen config-sleutel (Derek, 01-09: één weg, geen
opties).

1. Bouw de bundel van dezelfde bron als de draaiende kern, met een eigen
   stempel zodat je de twee op de console uit elkaar houdt:

   ```sh
   HOPOS_STAMP=B sh image/flip-bundle.sh <board>
   ```

   Board: `virt`, `uefi`, `o6n`, `altra`, `rpi4`, `rpi5`, `radxa`, en voor
   riscv64 (alleen koud) `virt-riscv` en `licheerv`. Uitvoer:
   `target/hopos-<board>.flip`, `.sha256` (het vertrouwensanker) en
   `.switch` (de som van de switch-code, ter controle).

2. Zet hem op een webserver op de laptop (`cd target && python3 -m
   http.server 8000`) en vraag de flip aan:

   ```sh
   curl -X POST -H 'Content-Type: application/json' \
     -d '{"url":"http://LAPTOP:8000/hopos-<board>.flip","sha256":"<de som uit .sha256>"}' \
     http://NODE:8080/flip
   ```

   `202`: de kern nam de bundel aan en springt over een halve seconde; er
   is geen weg terug. `502` met een reden: de kern weigerde en draait door.
   `switch code mismatch` vraagt de koude flip: `,"cold":true` in de body.

3. Kijk naar de console, in deze volgorde:

   | Kern | Marker | Betekenis |
   | --- | --- | --- |
   | A | `HOP_FLIP` (slot 1) | Hop haalt de bundel |
   | A | `HOPOS_FLIP_SWITCHCODE_OK` | de switch-code van de bundel is die van de bewoners |
   | A | `HOPOS_FLIP_STAGED` | som getoetst, beeld plat en gerelokeerd, de sprong past |
   | A | `HOP_FLIP_ACCEPTED` (slot 1) | Hop kreeg het ja (de 202) |
   | A | `HOPOS_FS_FROZEN generation=N` | hopfs vastgelegd en dicht (alleen met een schijf) |
   | A | `HOPOS_FLIP_COLD stopped=S cores_off=K` | (koud) het hok leeg |
   | A | `HOPOS_FLIP_NAT_CAPTURED flows=K` | de conntrack van de bewoners die meegaan, de masquerade dicht |
   | A | `HOPOS_FLIP_JUMP gen=G` | de laatste regel van kern A |
   | B | de bunny, `HOPOS_FLIP_BOOT gen=G` (koud ook `HOPOS_FLIP_COLD_BOOT`) | het blob is gelezen, met de lease |
   | B | `HOPOS_BOOT gen=G stamp=B` | de nieuwe kern met het nieuwe stempel |
   | B | `HOPOS_FS_UP fresh=0 generation=N` | dezelfde N als bij `HOPOS_FS_FROZEN` |
   | B | `HOPOS_NET_CARRIED flows=K of=K` | de switch nam de lease, de poorten en de flows over, vóór zijn eerste ronde |
   | B | `HOPOS_FLIP_ADOPT n of n resident(s)` | (warm) alle bewoners terug |
   | B | `HOPOS_NET_UP`, `HOPOS_SYSTEM_UP` | de stack en de listener, zonder DHCP |
   | B | `HOPOS_FLIP_SETTLED` | het net luistert binnen de gratie |
   | B | `HOPOS_HOP_RESUMED` (warm) of `HOP_UP` (koud) | Hop draait door, of start koud |

### Per board

| Board | Bundel | Koud adres | Staging, blob, recorder, trampoline | Bewezen |
| --- | --- | --- | --- | --- |
| QEMU virt | `flip-bundle.sh virt` | `0x4020_0000` (link.ld) | de staging van QEMU op `0xB020_0000`, de boot-scratch-pagina's op `0xB000_0000` | `sh tools/qemu-test-flip.sh`, ook `MISMATCH=1`, `COLD=1` en `OSCORE=1` |
| UEFI (EDK2) | `flip-bundle.sh uefi` | de basis die de firmware koos (`__efi_head`), binnen `SizeOfImage` | de loader-regio van het kernvenster (`0x5000_0000` + 256 MB) | `BOARD=uefi sh tools/qemu-test-flip.sh`, ook `COLD=1` |
| Orion O6N | `flip-bundle.sh o6n` | idem | idem, venster op `0x8800_0000` | nog niet op ijzer |
| Ampere Altra | `flip-bundle.sh altra` | idem | idem, venster op `0xB000_0000` | nog niet op ijzer |
| Pi 4, Pi 5 | `flip-bundle.sh rpi4` / `rpi5` | `0x80000` (link-raspi.ld) | achter de initramfs van Hop op `0x0F20_0000`, de boot-scratch op `0x0F10_0000` | bundels gebouwd; de ingang op QEMU raspi4b (`BOARD=rpi4 sh tools/qemu-test-flip.sh`); nog niet op ijzer |
| Radxa Zero 3E | `flip-bundle.sh radxa` | `0x0221_0000` (link-rk3566.ld) | het staging-venster op `0x0780_0000`, de recorder op `FLIP_SCRATCH_PA`, de trampoline op de bovenste pagina van de kern-RAM | bundel gebouwd; de ingang is `_start`, dezelfde als virt (`OSCORE=1`); nog niet op ijzer |
| QEMU virt riscv64 | `flip-bundle.sh virt-riscv` | `0x8000_0000` (link-riscv.ld) | achter Hop in de staging van QEMU op `0xA820_0000`; recorder, trampoline en de uit-stub van hart 1 op de boot-scratch (`0xA800_1000`, `+0x2000`, `+0x3000`) | alleen koud: `sh tools/qemu-riscv-test-flip.sh` (02-10) |
| LicheeRV Nano | `flip-bundle.sh licheerv` (Hop en desgewenst `CFG=` in het image) | `0x8400_0000` (RUNADDR) | de staging op `0x8690_0000` (13 MB, tussen de DMA-regio van 1 MB en pool C); recorder en trampoline op de staart (`0x8FE0_1000`, `+0x2000`) | alleen koud; bundel gebouwd (9,2 MB beeld); nog niet op ijzer |
| Mac mini M4 | `flip-bundle.sh apple` | `0x101_0000_0000` (link-apple.ld `KERN_BASE`, het RAM-venster) | de staging in de loader-regio (`LOADER` + 2 MB) met het blob vlak onder de stagingkop; de recorder op `ADMIN` + `0xc0_0000`, buiten de boot-scratch (iBoot legt het bootobject daar bij elke boot terug); de trampoline op de bovenste pagina van de kern-RAM (`board/apple/src/slots.rs`) | alleen warm: zonder EL3 geen PSCI, dus een koude flip weigert vóór de staging (`HOPOS_FLIP_COLD_NO_PSCI`, Hop krijgt `HOPOS_COLD_FLIP=no`); op ijzer warm geflipt tot 3.0.20 (24-10, ALLES.md) |

Wat per board eerst te kijken is, morgen op het ijzer:

- **UEFI, O6N, Altra.** Een geflipte kern komt binnen zonder firmware:
  `_start_efi` ziet x1 = 0 en leest de feiten van de koude stub uit de
  feitenpagina (`FLIP_FACTS_PA`, `board/uefi/src/flip.rs`). Die pagina
  schrijft alleen een stub van déze versie. Boot de node dus eerst koud van
  een stick met deze code; een kern van vóór de flip-ingang (tag v3.0.0-alpha.7 en ouder) heeft geen feitenpagina,
  en een flip vanaf zo'n kern parkeert de nieuwe kern zonder één regel
  (zie de faalmodi). Na de flip staat er geen `HOPOS_UEFI_STUB` op de
  console: er is geen ConOut meer, de eerste regel is de bunny. De nieuwe
  kern moet in de maat van de oude passen (`image too large` anders).
- **Pi 4, Pi 5.** (De ingang van nu; zie "De ene ingang" voor de vorm
  die we zoeken.) De nieuwe kern komt binnen op `_pi_start` met x0 = de
  DTB-pointer die de firmware de eerste kern gaf, x1 = x2 = 0 en x3 =
  `cpu::boot::FLIP_ENTRY` ("HOPFLIPE"), op de core waar de oude kern
  draaide. De ingang aanvaardt een andere core dan core 0 alleen met dat
  merkteken: `_start` laat bij een koude boot alleen affiniteit 0 door
  (een firmware die alle cores loslaat, krijgt één kern), en `_pi_start`
  houdt x3 vast over zijn kladwerk heen (x22). De feitenpagina van de Pi
  is de DTB zelf: de firmware legde hem in het laadvenster
  (`device_tree_address`), buiten de kern-RAM en de pool, en de oude kern
  toetst vóór de sprong dat daar nog een FDT-kop staat
  (`HOPOS_FLIP_REFUSED firmware DTB gone`). Bewezen op QEMU raspi4b
  (29-09, `BOARD=rpi4 sh tools/qemu-test-flip.sh`): core 1 met het
  merkteken komt door `_pi_start` en `_start` tot kmain en leest dezelfde
  DTB een tweede keer (`fdt:`, `mem:`, `vcmail:`); zonder merkteken
  parkeert hij na `P2`. De Pi houdt zijn kern op core 0
  (`board_raspi::os_core`, de SPI-route van de GIC-400), dus op ijzer landt
  een flip altijd op core 0; een kern die elders landt, verhuist terug
  (`HOPOS_OSCORE_MOVE`). Op ijzer eerst kijken: de 'P2' en de bunny na
  `HOPOS_FLIP_JUMP`, en `fdt:` met hetzelfde adres. De staging is de plek
  van het initramfs van Hop; het nieuwe beeld gaat erachter, zodat Hop er
  blijft liggen voor een koude flip.
- **Radxa.** De ingang is `_start` zelf (U-Boot `booti`), met dezelfde
  poort als virt: `OSCORE=1 sh tools/qemu-test-flip.sh` bewijst een flip
  vanaf core 1 op precies die code. De DTB en de initrd (`hopos.cfg`)
  liggen in gaten van de pool (`FW_HOLES`), dus ook daar is de DTB de
  feitenpagina. Er is geen staging van Hop, dus een koude flip weigert
  (`cold flip without a staged image`).
  De trampoline staat in de kern-RAM (de bovenste heap-pagina):
  het enige uitvoerbare RAM buiten de pool. `cpu::el2::chain` staat dat toe
  zolang het beeld hem niet raakt. Eerst bewijzen dat de heap die pagina op
  het moment van de flip niet gebruikt. De heap heeft sinds 29-09 vrije
  lijsten en vult een gat vóór de staart ([heap.md](heap.md),
  `holes_are_reused_before_the_tail`), dus die pagina raakt hij pas als
  zijn gebied bijna vol is; ook dan weigert de flip niet, maar overschrijft
  hij die pagina vlak voor de sprong, en daarna leest niemand hem meer.

## De faalmodi

Vóór de 202 is elke fout een weigering (`HOPOS_FLIP_REFUSED <reden>`,
`HOP_FLIP_FAIL` en `502` bij Hop), en kern A draait door:

| Reden | Wat |
| --- | --- |
| `sha256 mismatch` | de bytes zijn niet wat de som zegt |
| `bundle invalid` | geen HOPRELO1-staart, een afgekapte stroom, een beeld buiten de grenzen, een relocatie die niet klopt |
| `bundle carries no switch code sum`, `flip ABI mismatch` | een bundel van een andere vorm |
| `switch code mismatch` | warm, maar de EL2-switch-code is een andere dan die waarin de bewoners draaien: vraag koud |
| `image too large` | het beeld past niet in de staging of op het koude adres |
| `jump layout` | de trampoline, de vensters of de kopie van de sprong kloppen niet (`Jump::check`; tot 01-11 pas na de 202) |
| `firmware DTB gone` | op een DTB-board staat op x0 van de firmware geen FDT-kop meer |
| `cold flip without a staged image` | koud, maar er ligt geen ELF in de staging om Hop uit te starten |
| `cold flip needs PSCI`, `cold flip: CPU_OFF has no way back` | de M4; de Pi 5 zodra een app-core draaide |
| `warm flip not on riscv64` | riscv flipt alleen koud |
| `a flip is already under way` | een tweede FLIP (`HOPOS_FLIP_BUSY`) |

Na de 202 is elke fout `HOPOS_FATAL` en een koude boot (de bewoners
beginnen opnieuw, hopfs houdt hun volumes):

| Marker na `HOPOS_FATAL` | Waar |
| --- | --- |
| `HOPOS_FS_FREEZE_FAIL`, `HOPOS_FLIP_FAIL` | kern A: de vangst of de sprong |
| `HOPOS_FLIP_COLD_STOP_FAIL`, `HOPOS_FLIP_COLD_CORE` | kern A, koud: een bewoner of core niet stil |
| `HOPOS_FLIP_BLOB_BAD` | kern B: het paar was geldig maar het blob niet |
| `HOPOS_FLIP_ADOPT_FAIL` | kern B: de cage, een bewoner of de lifecycle weigert de adoptie |
| `HOPOS_NIC_FAIL`, `HOPOS_NET_FAIL` | kern B: het net van kern A komt niet terug |
| `HOPOS_FLIP_GUARD` | kern B: geen luisterend net binnen 120 s |

En verder:

| Marker | Wat er gebeurt |
| --- | --- |
| `HOPOS_FLIP_STALE` | een firmware-boot vond nog een geldig paar (de overdracht van een sprong die al landde of na de sprong stierf): gewist en niet gelezen, de boot is koud |
| niets na `HOPOS_FLIP_JUMP` | kern B hangt vóór zijn console; alleen een hardware-watchdog helpt. De volgende koude boot zegt waar het stopte |
| `HOPOS_FLIP_LAST`, `HOPOS_FLIP_ARCHIVED` | op de volgende koude boot: de stap waar de laatste flip stopte, uit de vluchtrecorder, en de poging daarvoor |
| `HOPOS_FLIP_BLACKBOX` ... `HOPOS_FLIP_BLACKBOX_END` | op de volgende koude boot: wat de dode kern zei (hieronder) |

## De zwarte doos

De recorder zegt wáár een geflipte kern stierf, de zwarte doos wat hij
daarvoor zei. Aanleiding: de Pi 5 zonder UART (30-09). Een geflipte kern
stierf daar vlak na de landing; de koude boot erna meldde alleen
`HOPOS_FLIP_LAST` op "landed", want de TCP-console (`hopos/src/conport.rs`)
komt pas na DHCP en zijn ring ligt in de BSS van de dode kern.

Elke consoleregel gaat daarom ook naar een ring op een vaste plek naast de
recorder (de tee in `conport.rs`, eerst de doos, dan de UART). De vorm (`kern::kernflip::box_open`): een kop van één
cacheregel met de magic `HOPBOX02`, de generatie van de schrijver, de
schrijfpositie (monotoon) en de ringmaat, daarachter 16 KiB ring. Elke
schrijf gaat eerst met de bytes en dan met de positie naar DRAM (clean en
invalidate tot het point of coherency), want een watchdog-reset spoelt
geen cache.

- Een koude boot leest de doos in `flip::land`, vóór hij er zelf in
  schrijft, en drukt na `HOPOS_FLIP_LAST` de staart af: een kopregel
  `flip: the console of the dead kernel (generation N), last B bytes
  HOPOS_FLIP_BLACKBOX`, de regels met `  | ` ervoor (alleen afdrukbaar
  ASCII, de halve eerste regel weg), en een slotregel
  `... HOPOS_FLIP_BLACKBOX_END`. Alleen bij de magic, de eigen ringmaat en
  een generatie; daarna is de doos leeg. Stond er niets:
  `HOPOS_FLIP_BLACKBOX_EMPTY`, zodat "geen post-mortem" te onderscheiden is van "gewist". Dan pas
  begint hij een verse doos met zijn eigen generatie (1): ook een koude
  dood staat zo op de volgende koude boot, en een kern drukt nooit zijn
  eigen regels af.
- Een landende kern (warm of koud geflipt) begint een verse doos met zijn
  generatie, zonder af te drukken: de kern die sprong, stierf niet. Wat
  hij vóór de landing zei (de bunny, `discover`), staat er niet in.
- `HOPOS_FLIP_BLOB_BAD` opent de doos niet: de koude boot erna drukt dan
  de console van de kern die sprong af.

| Board | Doos | Waarom die een reset overleeft |
| --- | --- | --- |
| Pi 4, Pi 5 | `0x0F1B_7000` (het blob min 32 KiB) | het laadvenster, in het gat van de boot-scratch tussen de trampoline (`0x0F10_2000`) en het blob (`0x0F1B_F000`), met de recorder (`0x0F10_1000`); de firmware schrijft er alleen de DTB (`0x0F00_0000`) en de initramfs (`0x0F20_0000`) |
| QEMU virt | `0xB00B_8000` | hetzelfde gat onder `0xB00C_0000`; een QEMU-reset laadt alleen de ROM's terug (de kern op `0x4020_0000`, de staging) |
| UEFI, O6N, Altra | loader-regio + `0xB_8000` (virt: `0x600B_8000`) | hetzelfde gat, boven de feitenpagina van de stub (+ `0x3000`, 8 KiB) |
| Radxa Zero 3E | `0x0630_8000` (`slots::BLACK_BOX`, 32 KiB) | het plan had de doos al, naast de recorder op `0x0630_1000`, Device |
| Mac mini M4 | ADMIN + `0xc0_8000` (`slots::BLACK_BOX`, 32 KiB) | idem, naast de recorder; niet op de boot-scratch, want iBoot legt daar het bootobject terug (01-09) |
| riscv64 | geen (`HOPOS_FLIP_BLACKBOX_NONE`) | nog geen plek die bewezen een reset overleeft |

De plek staat per board in zijn `slots.rs` (`BLACK_BOX`), met daar de
`const`-asserties: op virt en de Pi's boven de trampoline en de recorder,
op UEFI boven de feitenpagina, op de M4 achter de recorder en binnen het
kooi-venster, op de Radxa binnen zijn structuurvenster. Op virt, de Pi's
en UEFI ligt hij per constructie direct onder het blob (het blob min 32
KiB). `hopos/src/flip.rs` (`mod black_box`) leest die plek en toetst
alleen dat de doos past en op 64 bytes ligt.

## De boot-guard

Een geflipte kern moet binnen twee minuten (`flip::GRACE`) een luisterend
net hebben (`net::NET_READY`: de lease, de stack en de system-listener).
Dan `HOPOS_FLIP_SETTLED`, en gaat de vluchtrecorder leeg. Zo niet, dan
`HOPOS_FLIP_GUARD` en `HOPOS_FATAL`. De adoptie is geen voorwaarde meer:
die faalt hard op het moment zelf. Tijdelijk: de ene boot-guard van 120 s
in de watchdog (O1, agent D) neemt dit over, voor elke boot.

## De koude flip

Past de switch-code niet (een wijziging in `cpu/src/el2/switch.rs` of de
blobs), dan weigert de warme flip vóór de sprong. De weg is dan de koude
flip, op dezelfde trigger met `"cold":true`:

1. **Hop** haalt de bundel en stroomt hem de kern in, zoals warm. Pas dan
   stopt hij zijn eigen taken (`agent.hold_for_flip`,
   `HOP_FLIP_COLD_STOP stopped=N`); weigert de kern, of leeft Hop 30 s na
   een aangenomen FLIP nog, dan herstarten ze (`HOP_FLIP_COLD_BACK`).
2. **De haak** toetst de som en de bundel, niet de switch-code
   (`HOPOS_FLIP_COLD_ASKED`), en eist een ELF in de staging: kern B start
   Hop daaruit. Het nieuwe beeld gaat ACHTER dat image
   (`kernflip::stage_slot`), ook bij een warme flip.
3. **De flip-taak** legt hopfs vast, stopt elke bewoner die niet op de
   OS-core woont (`HOPOS_FLIP_COLD_STOP slot=N`), zet elke geparkeerde
   app-core uit (`cpu::el2::chain::send_off`: een uit-stub op de plek van
   de trampoline met PSCI CPU_OFF, en wachten op AFFINITY_INFO OFF), vangt
   de lease, en springt met een blob zonder hok.
4. **Kern B** boot als een koude kern met de stukken van kern A: eigen
   switch-code (`HOPOS_CAGE_UP` met de nieuwe som), CPU_ON per app-core,
   dezelfde hopfs-generatie, de lease (`HOPOS_NET_CARRIED`, geen DHCP), en
   Hop koud uit de staging (`HOPOS_HOP_START`, `HOP_UP`).

Waarom CPU_OFF en geen parkeerlus die blijft (29-09): kern B installeert
zijn switch-code en parkeerlus opnieuw en start elke core met CPU_ON; een
core die nog in de oude parkeerlus staat, geeft ALREADY_ON midden in code
die net overschreven wordt.

### riscv64

Op riscv64 is er alleen de koude flip (02-10). De switch-code draait daar
uit het kern-image (niet uit een kopie in de plan-regio zoals op arm64),
dus de nieuwe kern kan niemand adopteren (`cage_riscv::adopt`), en een warme
flip weigert vóór de sprong (`flip::WARM`). Dezelfde weg als hierboven,
met drie verschillen:

- **Het app-hart uit het image** (`flip::cores_off`, `slots::park_for_flip`;
  lukt het niet, dan `HOPOS_FATAL`, er is geen weg terug na de 202):
  de C906L van de LicheeRV gaat in reset (het resetblok, zoals een harde
  intrekking), en de nieuwe kern haalt hem eruit zoals bij elke boot
  (`start_app_hart`). Een hart zonder resetblok (QEMU) krijgt in zijn
  sched-blok `SCHED_OFF_PC` en de bel; de switcher springt aan het begin
  van zijn volgende ronde naar de uit-stub (`cpu::riscv::switch::off_stub`,
  een kopie op `FLIP_PARK_PA`, buiten het image), die zijn D-cache veegt,
  bevestigt in `SCHED_MBOX_CTX` en slaapt tot de bel. Die komt van de
  nieuwe kern (`boot::start_hart`), en dan gaat het hart diens `_start` in,
  de parkeerlus van het postvak. `cores_off=1` op de console.
- **De sprong** is de M-mode-trampoline van `cpu::el2::chain`: interrupts
  dicht, op de C906 de hele D-cache naar DRAM (`th.dcache.ciall`), de
  kopie, opnieuw vegen, de I-cache leeg, en naar `_start` met a0 = 0 en a1
  = wat de firmware de eerste kern gaf (QEMU: de DTB, die de oude kern
  vóór de sprong toetst).
- **De LicheeRV draagt Hop in het image.** De bundel bakt Hop erin
  (`STAGE=` of de Hop van `tools/hop-build.sh`); de nieuwe kern start díe
  Hop. De config gaat mee zoals op elk board (hieronder). De staging (13 MB op
  `0x8690_0000`) ligt tussen de DMA-regio (van 8 naar 1 MB) en pool C (van
  16 naar 10 MB): de heap van de kern bleef heel.

De reservering van de bundel in Hop kreeg daarbij de plaatsing van Hop
zelf (de OS-core, `kern::system`): een bundel draait nooit, en met één
app-hart bezet faalde de reservering ("no free run of 1 app core(s)").

Na de koude flip plaatst Hop wat hij uit de init-jobs (`hopos.init[]`) of
van de leader krijgt; de huidige Hop bewaart zijn eigen jobs niet over een
herstart (`GET /tasks` is na een koude flip zonder init-jobs leeg, ook op
arm64, 02-10).

## Toetsen

- `sh tools/qemu-test-flip.sh`: virt warm, met een TCP-verbinding van de
  host naar Hop die op kern A opengaat en door kern B beantwoord wordt,
  hopfs- en conntrack-getallen die aan beide kanten gelijk zijn
  (`HOPOS_FLIP_NAT_CAPTURED flows=K`, `HOPOS_NET_CARRIED flows=K of=K`),
  de lease (geen `HOPOS_DHCP_BOUND` na `HOPOS_FLIP_BOOT`), en FLIPCONN: een
  appspike met een UITGAANDE TCP-verbinding door de masquerade die over de
  flip heen doorloopt. Daarna de zwarte doos na een `system_reset` over
  QMP. Rood is ook `HOPOS_FATAL`.
- `MISMATCH=1`: een andere switch-code-som; groen alleen bij een weigering
  vóór de sprong.
- `COLD=1`: de koude flip, met de lease (`HOPOS_NET_CARRIED flows=0`) en
  weer werk op een core die kern A uitzette (CPU_ON).
- `OSCORE=1` (ook met `COLD=1`): de kern op core 1; kern B komt daar door
  `_start`.
- `BOARD=rpi4`: de flip-ingang van de Pi op QEMU raspi4b (geen net, dus
  geen flip): core 1 met de registers van de trampoline komt tot kmain.
- `BOARD=uefi` (ook met `COLD=1`): onder EDK2, met de PIE-basis en de
  feitenpagina.
- `sh tools/qemu-riscv-test-flip.sh`: de koude flip op QEMU virt riscv64.
- Host: `net` (`de_conntrack_overleeft_de_flip_via_de_actor`:
  `CaptureNat` op de oude switch, `Switch::restore` op de nieuwe;
  `capture_closes_the_masquerade`,
  `a_restored_flow_keeps_the_port_the_peer_sees`), `kern` (`kernflip`:
  `handoff_round_trip` met de lease,
  `a_handoff_without_the_hok_is_cold_and_keeps_the_rest`, de doos, de
  recorder; `slots`: `adoption_separates_cages_from_smp_cores`), `cpu`
  (`chain`, `boot`).

Ongetoetst (geen QEMU): de M4, en het ijzer van de Pi 4, de Pi 5, de Radxa,
de O6N en de Altra.

## Lessen

- 29-09: de eerste flip met een app op een app-core (FLIPCONN) hing de
  OS-core van kern B, zonder regel, zodra Hop daarna een taak plaatste. De
  `Ack` van de switch is een brievenbus met één plaats, en de adoptie
  stuurt twee `Attach`-en met dezelfde `Ack` vóór de switch draait. Bij één
  plaats is het volgnummer "gevuld" van de ene ronde hetzelfde getal als
  "leeg" van de volgende, dus het tweede resultaat won het slot opnieuw en
  daarna draaide elke `try_recv` voor altijd (gevonden met `info registers`
  over QMP: de PC in `cage::attach`, in `Mailbox::try_recv`). De fix zit in
  `sync::mpsc`: de producer toetst `enq - deq < N` vóór de claim. Een flip
  met alleen Hop (op de OS-core) zag het nooit.
