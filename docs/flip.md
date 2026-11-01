# De kern-flip

Een draaiende node vervangt zijn kern zonder herstart. Alleen warm (Derek,
01-11): de apps en Hop draaien door in hun kooien, de volumes blijven, de
lease blijft; alleen de kern wisselt. Een verbinding mag breken: wie hem
had, krijgt een fout of een time-out van zijn stack en verbindt opnieuw
("internet is toch gemaakt om te herconnecten"). Fout flippen is fataal,
zonder opvangmechanismen tot blijkt dat ze nodig zijn.

**De vorm (§14.4): de flip is een gewone boot plus een handoff.** Kern B
loopt dezelfde bootreeks als een koude boot; per stap begint hij leeg, of
neemt hij het stuk van kern A over. Elk onderdeel kent alleen zijn eigen
stuk: kern A vangt het (`capture`), kern B neemt het over (`restore`).
Alleen de flip zelf (`hopos/src/flip.rs`, en `setup` in
`hopos/src/main.rs` dat de stukken uitdeelt) weet dat er een flip is.

De code: `hopos/src/flip.rs` (de haak, de vangst, de sprong en de landing),
`kern/src/kernflip.rs` (het blob, de bundel, de recorder),
`cpu/src/el2/chain.rs` (de sprong), de ingangen (`cpu/src/boot.rs`,
`board/uefi/src/entry.rs` en `flip.rs`, `board/raspi/src/arch.rs`,
`board/apple/src/head.rs`), `image/flip-bundle.sh` (de bundel) en de
`FLIP_*`-blokken in `board/*/src/slots.rs` (de adressen).

## De reeks, met per stap leeg of uit de handoff

| Stap | Leeg (koude boot) | Uit de handoff | Wie |
| --- | --- | --- | --- |
| klok | de vaste wandklok tot SNTP | `wall_off` van kern A | `clock::restore` in `flip::land` |
| opslag | `hopos.storage` zegt stateless of stateful | de generatie die kern A vastlegde (`keep`) | `storage::start` |
| hok (claims) | `cage::new`: eigen switch-code, CPU_ON | `cage::adopt`, per bewoner `adopt_slot`, `Lifecycle::adopt` | `slots::start` (in `setup`) |
| lease | DHCP | uit de handoff, geen DHCP (H1) | `Node::run` |
| uplink, stack, listener | `SetUplink`, de stack, de listen-socket vóór de lus van poort 0, dan `NET_READY` | idem | `Node::run` |
| slots (net) | | de poorten van de bewoners open (`Lifecycle::republish`) na `NET_READY` | de lifecycle-actor |
| Hop | plaatsen uit de staging, na `NET_READY` | de supervisor terug (`HOPOS_HOP_RESUMED`), na `NET_READY` | `HopNode::supervise` |

De claims van het hok staan synchroon in `setup`, vóór de executor draait:
er draaien cores in de switch-code, en `OsCore::new` en de eerste
plaatsing mogen hun partities, cores en sched-blok niet opnieuw uitdelen.
Wat het hok op het net zet (de poorten), komt pas na de listener. De
ringen sluiten wel meteen aan (`Attach`): een frame van een app vóór de
uplink is er gaat verloren, en dat mag.

**Wat een flip niet overleeft:** de conntrack (een verbinding door de
masquerade breekt; de app krijgt een RST van zijn peer, gemeten op QEMU:
`connection reset by peer`, en verbindt opnieuw), de open verbindingen van
de node-stack (de system-API: de ene retry van applib, O9), de
next-hops, de staat van Hop in de kern (`STATE_PUT`) en een image van
`HOP_RESTART`. Een DNAT naar een gepubliceerde poort is stateloos: een
verbinding naar Hop loopt door zodra zijn poort weer open staat.

**De masquerade begint per boot ergens anders** (`net::nat::masq_start`,
de klok, RFC 6056). Met een vaste start gaf kern B de node-poorten van
kern A opnieuw uit; de peer (de slirp van QEMU) had het oude tupel nog
open en antwoordde de nieuwe SYN met een RST, en Hop kwam niet meer terug
(01-11, twee van drie runs).

**Na de 202 is er geen weg terug (§13.7).** Een vangst die faalt (hopfs
bevriest niet binnen 2 s, `snapshot` weigert, bijvoorbeeld bij een lopende
stroom, het blob past niet), of een landing die een stuk niet terugkrijgt
(het blob, de cage, een bewoner, de NIC): `HOPOS_FATAL`, en de koude boot
meldt waar het stopte (`HOPOS_FLIP_LAST`). Dat is de `fatal()` van het
fundament, `cpu::fatal!`: één regel, de core parkeert, de pets stoppen en
de watchdog reset. Wat vooraf te toetsen is, toetst de haak:
de som, de bundel, de switch-code, de maat, de staging en de indeling van
de sprong (`Jump::check`).

**riscv64 flipt niet** (`flip::FLIPS`, de haak weigert vóór de 202: `no
flip on riscv64`). De switch-code draait daar uit het kern-image; kern B
zou hem onder de app-harts overschrijven. Wat ontbreekt voor een warme
flip: een kopie van de M-mode-switcher buiten het image (zoals de
plan-regio op arm64), `mtvec` van elk app-hart op die kopie, en een
`adopt` in `cage_riscv`. De M-mode-trampoline in `cpu::el2::chain` staat
er nog, zonder gebruiker.

## Het blob

`kern::kernflip::Handoff`, versie 7 (`FLIP_ABI` 4), op `FLIP_HANDOFF_PA`
(direct onder het staging-maatwoord), met het pointer/magic-paar als
laatste op de boot-scratch. De kop is 80 bytes:

| Woord | Wat |
| --- | --- |
| 0, 1 | magic `HOPHAND1`, versie 7 |
| 2 | het aantal bewoners |
| 3 | de generatie van kern B |
| 4 | de wandklok (`wall_off`) |
| 5 tot 9 | de lease: ip en masker, gateway en dns, server en looptijd, T1 en T2, de tellerstand van de ACK; ip 0 = geen lease |

Daarachter de bewoners (`SlotState`: partitie, core, SMP-breedte, groep,
poorten, job, volumes). Versie 7 kwam op 01-11: de conntrack, de koude
vlag en de nulwoorden van oudere vormen gingen eruit, net als de vier
recorder-stappen zonder schrijver. `FLIP_ABI` 4 laat een kern van 3.0.20
(ABI 3) zo'n bundel weigeren vóór de 202; zo'n node komt naar deze vorm
via zijn bootmedium, niet via een flip.

## Inventaris: wie weet van de flip (01-11)

Gemeten met een grep op `flip|handoff|adopt` in code (geen commentaar,
geen toetsen): 45 bestanden op de start van deze ronde (4461670), 43 na
de ronde, buiten `flip.rs` en `kernflip.rs` (weg: `net/src/nat.rs`,
`switch.rs`, `kern/src/nodecfg.rs`; erbij: `hopos/src/net.rs`, de lease); weggelaten zijn de woorden
die iets anders betekenen (`cpu/src/smp.rs` `Handoff`, de `adopt_*` van
applib) en de toetsrol FLIPCONN van appspike. Het getal zakt weinig: de
meeste plekken zijn de adressen van het board (`FLIP_*` in elk
`slots.rs`) en de adoptie van het hok, en die horen erbij. Wat weg is, zijn
de takken: geen `flip::landed()`, geen `adopted_ok`, geen koude weg, geen
conntrack.

| Waar | Wat | Kant |
| --- | --- | --- |
| `kern/src/system.rs` | `PrivOp::Flip` (n moet 0 zijn), `FlipBundle`, `FLIP_BUNDLE_JOB`, `MAX_FLIP_PATH` bij een start | de haak |
| `abi/src/systemapi.rs` | `FLIP_BUNDLE_JOB`; `FLIP_COLD` alleen nog voor de import van Hop | de haak |
| `abi/src/layout` | `FLIP_HANDOFF_LEN`, `HANDOFF_*_OFF`, `flip_handoff_pa` | adressen |
| `board/*/src/slots.rs`, `board/uefi/src/flip.rs`, `facts.rs` | `FLIP_*`, `BLACK_BOX`, de feitenpagina van UEFI | adressen, ingang |
| `board/*` | `flip_carry_config` (Apple), `flip_carry_seed` (UEFI) | vangst |
| `cpu/src/boot.rs`, `board/raspi/src/arch.rs`, `board/apple/src/head.rs`, `board/uefi/src/entry.rs` | x3 = `FLIP_ENTRY`, `FLIP_ENTERED`, x1 = 0 op UEFI | ingang (O6, nog niet) |
| `cpu/src/el2/chain.rs`, `dispatch.rs` | `relocate`, `chain`, `installed_hash`, `adopt` | sprong, hok |
| `hopos/src/storage.rs` | `freeze_for_flip`; `start(keep)` | opslag |
| `hopos/src/usb.rs`, `telemetry.rs`, `watchdog.rs` | `quiesce`, de klok vol, een aai | vangst |
| `hopos/src/net.rs` | `lease()`; `start(lease)`, `NET_READY` | lease |
| `kern/src/slots.rs` | `snapshot`, `validate_adoption`, `adopt`, `republish`, `Life::adopted` | hok |
| `kern/src/partmem.rs`, `pool.rs`, `grants.rs`, `gui/fbgrant` | `adopt` | hok |
| `hopos/src/slots.rs`, `kooi.rs`, `cage.rs`, `cage_riscv.rs` | `start(adopt)`, `adopt_slot`, `cage::adopt`, `supervise(Some)` | hok |
| `hopos/src/main.rs` | `setup` deelt de stukken uit, `hop_adopted` | de reeks |
| `kern/src/rpc.rs` | `Freeze` | opslag |

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
dezelfde HMAC als een jobspec (met `hopos.insecure=1` zonder).

1. Bouw de bundel van dezelfde bron als de draaiende kern, met een eigen
   stempel: `HOPOS_STAMP=B sh image/flip-bundle.sh <board>` (board: `virt`,
   `uefi`, `o6n`, `altra`, `rpi4`, `rpi5`, `radxa`, `apple`). Uitvoer:
   `target/hopos-<board>.flip`, `.sha256` (het vertrouwensanker) en
   `.switch` (de som van de switch-code).
2. Zet hem op een webserver en vraag de flip aan:

   ```sh
   curl -X POST -H 'Content-Type: application/json' \
     -d '{"url":"http://LAPTOP:8000/hopos-<board>.flip","sha256":"<de som uit .sha256>"}' \
     http://NODE:8080/flip
   ```

   `202`: de kern nam de bundel aan en springt over een halve seconde; er
   is geen weg terug. `502` met een reden: de kern weigerde en draait door.
   `switch code mismatch`: deze bundel kan niet warm; de weg is het
   bootmedium.

3. Kijk naar de console, in deze volgorde:

   | Kern | Marker | Betekenis |
   | --- | --- | --- |
   | A | `HOP_FLIP` (slot 1) | Hop haalt de bundel |
   | A | `HOPOS_FLIP_SWITCHCODE_OK` | de switch-code van de bundel is die van de bewoners |
   | A | `HOPOS_FLIP_STAGED` | som getoetst, beeld plat en gerelokeerd, de sprong past |
   | A | `HOP_FLIP_ACCEPTED` (slot 1) | Hop kreeg het ja (de 202) |
   | A | `HOPOS_FS_FROZEN generation=N` | hopfs vastgelegd en dicht (alleen met een schijf) |
   | A | `HOPOS_FLIP_JUMP gen=G` | de laatste regel van kern A |
   | B | de bunny, `HOPOS_FLIP_BOOT gen=G` | het blob is gelezen, met de lease |
   | B | `HOPOS_BOOT gen=G stamp=B` | de nieuwe kern met het nieuwe stempel |
   | B | `HOPOS_FS_UP fresh=0 generation=N` | dezelfde N als bij `HOPOS_FS_FROZEN` |
   | B | `HOPOS_FLIP_ADOPT n of n resident(s)` | alle bewoners terug |
   | B | `HOPOS_DHCP_CARRIED`, `HOPOS_NET_UP`, `HOPOS_SYSTEM_UP` | de lease van kern A, de stack en de listener, zonder DHCP |
   | B | `HOPOS_FLIP_SETTLED` | het net luistert binnen de gratie |
   | B | `HOPOS_SLOT_PUBLISH` (per bewoner), `HOPOS_HOP_RESUMED` | de poorten weer open, Hop draait door |

### Per board

| Board | Bundel | Koud adres | Staging, blob, recorder, trampoline | Bewezen |
| --- | --- | --- | --- | --- |
| QEMU virt | `flip-bundle.sh virt` | `0x4020_0000` (link.ld) | de staging van QEMU op `0xB020_0000`, de boot-scratch-pagina's op `0xB000_0000` | `sh tools/qemu-test-flip.sh`, ook `MISMATCH=1` en `OSCORE=1` |
| UEFI (EDK2) | `flip-bundle.sh uefi` | de basis die de firmware koos (`__efi_head`), binnen `SizeOfImage` | de loader-regio van het kernvenster (`0x5000_0000` + 256 MB) | `BOARD=uefi sh tools/qemu-test-flip.sh` |
| Orion O6N | `flip-bundle.sh o6n` | idem | idem, venster op `0x8800_0000` | nog niet op ijzer |
| Ampere Altra | `flip-bundle.sh altra` | idem | idem, venster op `0xB000_0000` | nog niet op ijzer |
| Pi 4, Pi 5 | `flip-bundle.sh rpi4` / `rpi5` | `0x80000` (link-raspi.ld) | achter de initramfs van Hop op `0x0F20_0000`, de boot-scratch op `0x0F10_0000` | bundels gebouwd; de ingang op QEMU raspi4b (`BOARD=rpi4 sh tools/qemu-test-flip.sh`); nog niet op ijzer |
| Radxa Zero 3E | `flip-bundle.sh radxa` | `0x0221_0000` (link-rk3566.ld) | het staging-venster op `0x0780_0000`, de recorder op `FLIP_SCRATCH_PA`, de trampoline op de bovenste pagina van de kern-RAM | bundel gebouwd; de ingang is `_start`, dezelfde als virt (`OSCORE=1`); nog niet op ijzer |
| Mac mini M4 | `flip-bundle.sh apple` | `0x101_0000_0000` (link-apple.ld `KERN_BASE`, het RAM-venster) | de staging in de loader-regio (`LOADER` + 2 MB) met het blob vlak onder de stagingkop; de recorder op `ADMIN` + `0xc0_0000`, buiten de boot-scratch (iBoot legt het bootobject daar bij elke boot terug); de trampoline op de bovenste pagina van de kern-RAM (`board/apple/src/slots.rs`) | op ijzer geflipt tot 3.0.20 (24-10, ALLES.md), met het blob van versie 6 |

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
  blijft liggen voor een herstart van Hop op de nieuwe kern.
- **Radxa.** De ingang is `_start` zelf (U-Boot `booti`), met dezelfde
  poort als virt: `OSCORE=1 sh tools/qemu-test-flip.sh` bewijst een flip
  vanaf core 1 op precies die code. De DTB en de initrd (`hopos.cfg`)
  liggen in gaten van de pool (`FW_HOLES`), dus ook daar is de DTB de
  feitenpagina.
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
| `bundle carries no switch code sum`, `flip ABI mismatch` | een bundel van een andere vorm (een kern van ABI 3 weigert een bundel van ABI 4, en omgekeerd) |
| `switch code mismatch` | de EL2-switch-code is een andere dan die waarin de bewoners draaien |
| `image too large` | het beeld past niet in de staging of op het koude adres |
| `jump layout` | de trampoline, de vensters of de kopie van de sprong kloppen niet (`Jump::check`) |
| `firmware DTB gone` | op een DTB-board staat op x0 van de firmware geen FDT-kop meer |
| `no flip on riscv64` | riscv64 flipt niet |
| `a flip is already under way` | een tweede FLIP (`HOPOS_FLIP_BUSY`) |
| `Version { have: n, want: 0 }` (system-API) | een vlag in `n`, zoals de koude flip van een oudere Hop |

Na de 202 is elke fout `HOPOS_FATAL` en een koude boot (de bewoners
beginnen opnieuw, hopfs houdt hun volumes):

| Marker na `HOPOS_FATAL` | Waar |
| --- | --- |
| `HOPOS_FS_FREEZE_FAIL`, `HOPOS_FLIP_FAIL` | kern A: de vangst of de sprong |
| `HOPOS_FLIP_BLOB_BAD` | kern B: het paar was geldig maar het blob niet |
| `HOPOS_FLIP_ADOPT_FAIL` | kern B: de cage, een bewoner of de lifecycle weigert de adoptie |
| `HOPOS_NIC_FAIL`, `HOPOS_NET_FAIL` | kern B: het net komt niet terug |

En verder:

| Marker | Wat er gebeurt |
| --- | --- |
| `HOPOS_FLIP_STALE` | een firmware-boot vond nog een geldig paar: gewist en niet gelezen, de boot is koud |
| niets na `HOPOS_FLIP_JUMP` | kern B hangt vóór zijn console; alleen een hardware-watchdog helpt. De volgende koude boot zegt waar het stopte |
| `HOPOS_FLIP_LAST`, `HOPOS_FLIP_ARCHIVED` | op de volgende koude boot: de stap waar de laatste flip stopte, en de poging daarvoor |
| `HOPOS_CONSOLE_CARRIED` | bij elke boot, als eerste regel: hoeveel bytes van een vorige kern nog in de console-ring staan; `nc NODE 5555` speelt ze af vóór de nieuwe boot (de zwarte doos, hieronder) |

## De zwarte doos

De recorder zegt wáár een geflipte kern stierf, de zwarte doos wat hij
daarvoor zei. Aanleiding: de Pi 5 zonder UART (30-09). Een geflipte kern
stierf daar vlak na de landing; de koude boot erna meldde alleen
`HOPOS_FLIP_LAST` op "landed", want de TCP-console (`hopos/src/conport.rs`)
komt pas na DHCP en zijn ring ligt in de BSS van de dode kern.

Sinds 01-11 is de zwarte doos de console zelf: één ring op een vaste plek
naast de recorder (`kern/src/conport.rs`, `hopos/src/conport.rs`), met de
UART, tcp/5555 en `NEXT_LOG` 0 van Hop als lezers. De kop (magic
`HOPCON01`, de ringmaat, de schrijfpositie en de posities van de UART en
van Hop) en elke schrijf gaan meteen naar DRAM, want een watchdog-reset
spoelt geen cache. Een kern opent de ring die er ligt in `kmain`, vóór de
bunny, en schrijft verder: na een flip of een reset staat de console van
de vorige kern dus vóór die van de nieuwe op 5555, zet de UART eerst wat
er nog niet op de draad stond (hoogstens 8 KiB), en leest Hop verder waar
hij was. Alleen een andere magic of maat is een verse ring. De flip weet
er niets van.

| Board | Doos | Waarom die een reset overleeft |
| --- | --- | --- |
| Pi 4, Pi 5 | `0x0F1B_7000` (het blob min 32 KiB) | het laadvenster, in het gat van de boot-scratch tussen de trampoline (`0x0F10_2000`) en het blob (`0x0F1B_F000`), met de recorder (`0x0F10_1000`); de firmware schrijft er alleen de DTB (`0x0F00_0000`) en de initramfs (`0x0F20_0000`) |
| QEMU virt | `0xB00B_8000` | hetzelfde gat onder `0xB00C_0000`; een QEMU-reset laadt alleen de ROM's terug (de kern op `0x4020_0000`, de staging) |
| UEFI, O6N, Altra | loader-regio + `0xB_8000` (virt: `0x600B_8000`) | hetzelfde gat, boven de feitenpagina van de stub (+ `0x3000`, 8 KiB) |
| Radxa Zero 3E | `0x0630_8000` (`slots::BLACK_BOX`, 32 KiB) | het plan had de doos al, naast de recorder op `0x0630_1000`, Device |
| Mac mini M4 | ADMIN + `0xc0_8000` (`slots::BLACK_BOX`, 32 KiB) | idem, naast de recorder; niet op de boot-scratch, want iBoot legt daar het bootobject terug (01-09) |
| riscv64 | geen: de ring staat in .bss | nog geen plek die bewezen een reset overleeft |

De plek staat per board in zijn `slots.rs` (`BLACK_BOX`), met daar de
`const`-asserties: op virt en de Pi's boven de trampoline en de recorder,
op UEFI boven de feitenpagina, op de M4 achter de recorder en binnen het
kooi-venster, op de Radxa binnen zijn structuurvenster. Op virt, de Pi's
en UEFI ligt hij per constructie direct onder het blob (het blob min 32
KiB). `hopos/src/conport.rs` leest die plek en toetst alleen dat de doos
op 64 bytes ligt.

## De boot-guard

De flip heeft geen eigen guard meer: de ene guard van 120 s van de
watchdog (O1, `kern::watchdog::GUARD_SECS`) geldt voor elke boot, ook na
een landing. Komt kern B niet tot een levensteken (een self-dial die
verbindt, en Hop die slaat), dan houdt de watchdog zijn pets in
(`HOPOS_BOOT_GUARD_EXPIRED`) en reset de node koud. Luistert het net
(`net::NET_READY`: de lease, de stack en de system-listener), dan zegt
`flip::settle` `HOPOS_FLIP_SETTLED` en gaat de vluchtrecorder leeg.

## Toetsen

- `sh tools/qemu-test-flip.sh`: virt warm. Hop en FLIPCONN (een appspike
  met een uitgaande TCP-verbinding door de masquerade naar een echo op de
  host) leven door; FLIPCONN krijgt na de sprong een fout van zijn stack
  (`HOPOS_APPSPIKE_FLIPCONN_BROKE read: ... connection reset by peer`),
  verbindt opnieuw en krijgt drie antwoorden via kern B
  (`HOPOS_APPSPIKE_FLIPCONN ok ... breaks=1`). Verder: dezelfde
  hopfs-generatie, de lease (`HOPOS_DHCP_CARRIED`, geen
  `HOPOS_DHCP_BOUND` na de landing), één `HOP_BOOT`, een jobspec op kern B,
  en de zwarte doos na een `system_reset` over QMP: kern A boot koud op
  dezelfde ring (`HOPOS_CONSOLE_CARRIED` met meer dan 0 bytes), en
  tcp/5555 speelt de laatste `HOPOS_APPSPIKE_DONE` van kern B af vóór de
  bootregel van kern A. Een TCP-verbinding naar
  Hop die op kern A opengaat, mag breken (alleen gemeld); gemeten liep hij
  drie van drie keer door. Rood is ook `HOPOS_FATAL`.
- `MISMATCH=1`: een andere switch-code-som; groen alleen bij een weigering
  vóór de sprong.
- `OSCORE=1`: de kern op core 1; kern B komt daar door `_start`.
- `BOARD=rpi4`: de flip-ingang van de Pi op QEMU raspi4b.
- `BOARD=uefi`: onder EDK2, met de PIE-basis en de feitenpagina.
- Host: `kern` (`kernflip`: `handoff_round_trip` met de lease,
  `a_lease_of_zero_is_no_lease`, de recorder, de bundel; `slots`:
  `adoption_separates_cages_from_smp_cores` met `republish`), `net`
  (`the_first_masquerade_port_moves_with_the_clock_and_stays_in_range`),
  `cpu` (`chain`, `boot`).

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
