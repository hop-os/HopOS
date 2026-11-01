# De kern-flip

Een draaiende node vervangt zijn kern zonder herstart. De apps, Hop, hun
verbindingen door de switch, de NAT-flows en de volumes blijven; alleen de
kern wisselt. Dit is de procedure per board, de markers die erbij horen, de
faalmodi en wat de boot-guard doet. Daarnaast de koude flip: dezelfde
trigger met `"cold":true`, voor een kern met een andere switch-code (zie
"De koude flip"). De code: `hopos/src/flip.rs` (beleid),
`kern/src/kernflip.rs` (bundel, blob, recorder, de plek in
de staging),
`cpu/src/el2/chain.rs` (de sprong en de uit-stub van de koude flip),
`cpu/src/boot.rs` (de ingang: `FLIP_ENTRY`), `image/flip-bundle.sh` (de
bundel), de `FLIP_*`-blokken in `board/*/src/slots.rs` (de adressen) en
`board/uefi/src/flip.rs` (de feitenpagina van UEFI).

## Wat er overgaat, en wat niet

| Wat | Hoe |
| --- | --- |
| De bewoners (Hop en elke app) | Hun kooien, partities en cores blijven staan; het handoff-blob draagt de boekhouding en de nieuwe kern adopteert ze (`HOPOS_FLIP_ADOPT`). Hop wordt niet herstart. |
| De gepubliceerde poorten | De nieuwe kern publiceert ze opnieuw (`republish`, ook die van Hop: `slot 1: 2 port(s) published ... HOPOS_SLOT_PUBLISH`); een TCP-verbinding naar een gepubliceerde poort overleeft de sprong (DNAT is stateloos). |
| De NAT-flows (conntrack) | De switch-actor geeft een snapshot als waarde (`Command::SnapshotNat`) en zet daarmee de masquerade dicht; na de landing houdt de nieuwe switch de node-poorten vast vóór zijn eerste ronde, en na de adoptie komen de flows terug (`RestoreNat`). Een UITGAANDE TCP-verbinding van een app loopt zo door, over de slirp van QEMU heen: de toets FLIPCONN (hieronder). |
| De volumes (hopfs) | Vóór de sprong legt de actor de boom vast en neemt hij niets meer aan (`HOPOS_FS_FROZEN generation=N`); de nieuwe kern mount precies die generatie (`HOPOS_FS_UP fresh=0 generation=N`). |
| De switch-code van de app-cores | Blijft staan; de nieuwe kern adopteert haar alleen bij een gelijke som, en die som toetst de oude kern al vóór de sprong. |
| De config (`hopos.cfg`) | Staat op elk board in het venster van het kern-image (`board/src/cfgwin.rs`, [boards.md](boards.md)). Een bundel met een leeg venster krijgt dat van de draaiende kern vóór de sprong (`HOPOS_FLIP_CFG`); een bundel met een gevuld venster (`CFG=` van `image/flip-bundle.sh`, of `hop image`) houdt het zijne (`HOPOS_FLIP_CFG_OWN`). Een kern van vóór het venster geeft niets mee; de terugval (de ESP, de bootargs, de initrd, 0xF000 op de M4) blijft dan tellen, behalve op de LicheeRV: daar `CFG=`. |
| Niet: de staat van Hop in de kern (`STATE_PUT`) | Dat blob leeft in de heap van de system-API en sterft met kern A, zoals de fdstore van systemd met de manager. Hop draait door en hoeft het niet terug te lezen, maar tot zijn volgende `STATE_PUT` ligt er op kern B niets: een herstart van Hop in dat venster begint met een lege staat (`HOPOS_HOP_STATE kept=0`). Daarom hoort Hop zijn staat opnieuw te schrijven zodra zijn system-client de nieuwe kern bereikt (een nieuwe verbinding na de flip), niet pas bij de volgende wijziging. Het handoff-blob draagt het niet mee: dat is een vaste staart van 256 KiB (`kern/src/kernflip.rs` `HANDOFF_TAIL`, vooral de conntrack), de staat van Hop tot 1 MiB (`HOP_STATE_MAX`). |
| Niet: een image dat Hop voor zijn herstart gaf (`HOP_RESTART`) | Ook dat lag in de heap van kern A. Een herstart met hetzelfde image op kern B plaatst het gestagede (`HOPOS_HOP_IMAGE_STAGED`); is de staging naar de flip gegaan (`HOPOS_FLIP_STAGE_SHARED`), dan is er geen (`HOPOS_HOP_IMAGE_NONE`) en kan een herstart alleen met een nieuw image. Een Hop die faalt, laat de kern op elke generatie liggen: de watchdog reset de node en de koude boot start de Hop van het bootmedium. |
| Niet: de system-API-verbindingen | Die zijn van de kern; Hop's system-client bouwt ze opnieuw op. |
| Niet: file calls tussen bevriezing en sprong | Die krijgen `Busy` (`HOPOS_FS_FROZEN_CALL`); de aanroeper doet ze opnieuw op de nieuwe kern. |
| Niet: een nieuwe uitgaande verbinding tussen snapshot en sprong | Die krijgt geen flow; zijn SYN-retransmit krijgt er op de nieuwe kern een. |
| Niet: de next-hops (ook de gateway niet) | Die staan in de neighbour-tabel van de node-stack; de nieuwe vraagt ze opnieuw op het eerste frame, één ARP-ronde (een verkeerd overgenomen next-hop is erger). Het woord van de gateway-MAC in het blob blijft, altijd leeg. |

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

2. Zet hem op een webserver op de laptop:

   ```sh
   cd target && python3 -m http.server 8000
   ```

3. Vraag de flip aan:

   ```sh
   curl -X POST -H 'Content-Type: application/json' \
     -d '{"url":"http://LAPTOP:8000/hopos-<board>.flip","sha256":"<de som uit .sha256>"}' \
     http://NODE:8080/flip
   ```

   `202` betekent: de kern nam de bundel aan en springt over een halve
   seconde. `502` met een reden: de kern weigerde vóór de sprong en draait
   gewoon door. Een `502` met `switch code mismatch` vraagt de koude flip:
   dezelfde regel met `,"cold":true` in de body (zie "De koude flip").

4. Kijk naar de console, in deze volgorde:

   | Kern | Marker | Betekenis |
   | --- | --- | --- |
   | A | `HOP_FLIP` (slot 1) | Hop haalt de bundel |
   | A | `HOPOS_FLIP_SWITCHCODE_OK` | de switch-code van de bundel is die van de bewoners |
   | A | `HOPOS_FLIP_STAGED` | som getoetst, beeld plat en gerelokeerd naar het koude adres |
   | A | `HOP_FLIP_ACCEPTED` (slot 1) | Hop kreeg het ja |
   | A | `HOPOS_FS_FROZEN generation=N` | hopfs vastgelegd en dicht (alleen met een schijf) |
   | A | `HOPOS_FLIP_NAT_CAPTURED flows=K` | de conntrack gevangen, de masquerade dicht |
   | A | `HOPOS_FLIP_JUMP gen=G` | de laatste regel van kern A |
   | B | de bunny, `HOPOS_FLIP_BOOT gen=G` | het blob is gelezen |
   | B | `HOPOS_BOOT gen=G stamp=B` | de nieuwe kern met het nieuwe stempel |
   | B | `HOPOS_FS_UP fresh=0 generation=N` | dezelfde N als bij `HOPOS_FS_FROZEN` |
   | B | `HOPOS_FLIP_ADOPT n of n resident(s)` | alle bewoners terug |
   | B | `HOPOS_HOP_RESUMED` | Hop draait door, niet herstart; zijn supervisor loopt weer (`HOPOS_HOP_IMAGE_STAGED` of `HOPOS_HOP_IMAGE_NONE`: wat een herstart hier zou plaatsen) |
   | B | `HOPOS_FLIP_NAT restored=K of=K` | elke flow terug |
   | B | `HOPOS_FLIP_SETTLED` | de guard is tevreden: adoptie en net binnen de gratie |

   Daarna: `GET /tasks` op de agent-poort antwoordt, en een jobspec naar de
   leader wordt door kern B geplaatst.

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
- **Pi 4, Pi 5.** De nieuwe kern komt binnen op `_pi_start` met x0 = de
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

Vóór de sprong is elke fout een weigering, en draait kern A door. Wat de
kern weigert terwijl Hop nog wacht op zijn FLIP-antwoord, krijgt Hop terug
(`HOP_FLIP_FAIL`, `502`); wat daarna misgaat, staat alleen op de console.

| Marker | Waar | Wat er gebeurt |
| --- | --- | --- |
| `HOPOS_FLIP_REFUSED sha256 mismatch` | haak | de bytes zijn niet wat de som zegt |
| `HOPOS_FLIP_REFUSED bundle invalid` | haak | geen HOPRELO1-staart, een afgekapte stroom, een beeld buiten de grenzen, een relocatie die niet klopt |
| `HOPOS_FLIP_REFUSED bundle carries no switch code sum` | haak | een bundel van versie 1 (alpha.7 en ouder): die kan de switch-code niet laten toetsen |
| `HOPOS_FLIP_REFUSED flip ABI mismatch` | haak | een bundel met een andere flip-ABI (v2 was ABI 2) |
| `HOPOS_FLIP_REFUSED switch code mismatch` | haak | de EL2-switch-code van de nieuwe kern is een andere dan die waarin de bewoners draaien; zie "De koude flip" |
| `HOPOS_FLIP_REFUSED image too large` | haak | het beeld past niet in de staging of niet op het koude adres (UEFI: niet in het oude image) |
| `HOPOS_FLIP_REFUSED firmware DTB gone` | haak | op een DTB-board (virt, Pi, Radxa) staat op x0 van de firmware geen FDT-kop meer; de nieuwe kern zou zonder geheugenkaart landen |
| `HOPOS_FLIP_REFUSED cold flip without a staged image` | haak | koud, maar er ligt geen ELF in de staging om Hop uit te starten (de Radxa, of een eerdere warme flip die er overheen moest: `HOPOS_FLIP_STAGE_SHARED`) |
| `HOPOS_FS_FREEZE_FAIL` en `HOPOS_FLIP_FAIL` | flip-taak | de commit faalde of de actor antwoordde niet binnen 2 s; er is niets bevroren |
| `HOPOS_FLIP_FAIL` na `HOPOS_FS_FROZEN` | flip-taak | de conntrack, de bewoners, het blob of de indeling van de sprong faalde; hopfs ontdooit (`HOPOS_FS_THAWED`) en de masquerade gaat weer open |
| `HOPOS_FLIP_COLD_STOP_FAIL`, `HOPOS_FLIP_COLD_CORE`, dan `HOPOS_FLIP_FAIL` | flip-taak, koud | een bewoner stopte niet, of een app-core ging niet uit binnen een seconde (AFFINITY_INFO); hopfs ontdooit. Gestopte bewoners blijven gestopt (Hop plaatst ze opnieuw), een uitgezette core staat op "koud" in zijn mailbox, dus de volgende dispatch is weer een CPU_ON |
| `HOPOS_FLIP_COLD_NO_WAY_BACK`, dan `HOPOS_FLIP_REFUSED cold flip: CPU_OFF has no way back, ask warm` | haak, koud | de Pi 5: CPU_OFF is daar een deur zonder terugweg (10-07), dus koud kan alleen zolang geen app-core ooit draaide; Hop krijgt een 502 met `version 0x1, want 0x0` en herstart de taken die hij al stopte (`HOP_FLIP_COLD_BACK`). Tot 03-10 toetste pas de flip-taak dit, na een 202 (`HOPOS_FLIP_FAIL`) |
| `HOPOS_FLIP_REFUSED warm flip not on riscv64, ask cold` | haak | riscv64 flipt alleen koud (zie "riscv64" hieronder); Hop krijgt `version 0x0, want 0x1` |
| `HOPOS_FLIP_COLD_CORE`, `HOPOS_FLIP_CORE_BACK`, dan `HOPOS_FLIP_FAIL` | flip-taak, koud, riscv64 | een app-hart draaide nog een bewoner of bereikte de uit-stub niet binnen een seconde; elk hart dat al uit het image was, gaat terug de switcher in (`HOPOS_FLIP_CORE_BACK`) en hopfs ontdooit |

Na de sprong bestaat kern A niet meer, en is een koude herstart de enige
weg terug. Dat is geen geslaagde flip: de apps beginnen dan opnieuw (hopfs
houdt hun volumes).

| Marker | Wat er gebeurt |
| --- | --- |
| `HOPOS_FLIP_BLOB_BAD` | het paar was geldig maar het blob niet: er leven misschien bewoners, dus geen koude boot eroverheen maar een PSCI-reset (op ijzer: de watchdog) |
| `HOPOS_FLIP_STALE` | een firmware-boot (geen merkteken van de trampoline: x3 = `FLIP_ENTRY`, op UEFI de flip-ingang met x1 = 0; `cpu::boot::FLIP_ENTERED`) vond nog een geldig paar: de overdracht van een sprong die al landde of na de sprong stierf. Gewist en niet gelezen, de boot is koud (met `HOPOS_FLIP_LAST` en de doos). De overdracht is eenmalig: de landende kern wist het paar en veegt de nul meteen naar DRAM. Tot 04-10 bleef die nul in de cache, overleefde het paar de watchdog-reset, en adopteerde de stickkern van de O6N (v3.0.5) de bewoners van een flip die al geland was: `HOPOS_FLIP_ADOPT`, Hop dood, pas na de guard koud (2,5 minuut in plaats van 50 s). riscv64 heeft geen merkteken; daar is het wissen de bescherming, en een koud blob boot daar hoe dan ook koud |
| `HOPOS_FLIP_ADOPT_FAIL` | de nieuwe kern weigert de adoptie (switch-code, partities, cores); de guard reset na de gratie |
| `HOPOS_FLIP_NAT_FAIL` | de conntrack kwam niet terug; de bewoners draaien, verbindingen door de masquerade breken |
| `HOPOS_FLIP_GUARD` | geen adoptie of geen net binnen 120 s: PSCI-reset |
| niets na `HOPOS_FLIP_JUMP` | de nieuwe kern hangt vóór zijn console (op UEFI ook: geen feitenpagina, de core parkeert). Alleen een hardware-watchdog helpt; op QEMU virt de i6300esb, die elke ring heeft. De volgende koude boot zegt waar het stopte |
| `HOPOS_FLIP_LAST`, `HOPOS_FLIP_ARCHIVED` | op de volgende koude boot: de stap waar de laatste flip stopte, uit de vluchtrecorder (`FLIP_RECORDER_PA`), en de poging daarvoor (06-09: zonder archief schreef de flip die een gevallen node optilde zijn eigen stappen over het spoor) |
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

Een geflipte kern krijgt twee minuten gratie (`GRACE`: langer blind petten verbergt een kern die hangt, korter verliest
een trage DHCP). Binnen die tijd moeten de bewoners geadopteerd zijn én
moet de uplink een adres hebben. Dan `HOPOS_FLIP_SETTLED`, en gaat de
vluchtrecorder leeg: een latere koude boot is geen mislukte flip. Zo niet,
dan `HOPOS_FLIP_GUARD` en een PSCI SYSTEM_RESET: een halve kern met
levende bewoners is erger dan een koude boot. Alleen "de executor draait"
is geen voorwaarde (06-09 op de M4: een guard die dat toetste, liet een
kern zonder net twee minuten petten en dan toch vallen).

Na een koude flip is er niets te adopteren; daar is het net de enige
voorwaarde, en een koude landing zonder net valt ook terug op een koude boot
(het bootmedium, dus de kern van vóór de flip).

Wat de guard niet dekt: een kern die hangt vóór zijn executor draait. Dat
is de hardware-watchdog van het board.

## De koude flip

Past de switch-code niet (een wijziging in `cpu/src/el2/switch.rs` of de
blobs), dan weigert de warme flip vóór de sprong: de bewoners draaien in de
oude switch-code, en de nieuwe kern mag die alleen adopteren bij een gelijke
som. De weg is dan de koude flip, op dezelfde ene trigger:

```sh
curl -X POST -H 'Content-Type: application/json' \
  -d '{"url":"http://LAPTOP:8000/hopos-<board>.flip","sha256":"<som>","cold":true}' \
  http://NODE:8080/flip
```

Wat er gebeurt, in volgorde:

1. **Hop** haalt de bundel en stroomt hem de kern in, zoals warm. Pas dan
   stopt hij zijn eigen taken op deze node (`agent.hold_for_flip`, dezelfde
   Stop-acties als een preemptie: `HOP_FLIP_COLD_STOP stopped=N`); de jobs
   blijven in de agent-staat op hopfs. Dan de FLIP met `n` =
   `abi::systemapi::FLIP_COLD`. Een URL die niet werkt, stopt dus niets.
   De agent houdt de gestopte taken vast: weigert de kern de FLIP, of
   leeft Hop 30 s na een aangenomen FLIP nog (de sprong ging niet door),
   dan herstarten ze (`HOP_FLIP_COLD_BACK`).
2. **De haak** toetst de som en de bundel, maar niet de switch-code
   (`HOPOS_FLIP_COLD_ASKED`), weigert op een board waar CPU_OFF geen
   terugweg heeft zodra een app-core ooit draaide
   (`HOPOS_FLIP_COLD_NO_WAY_BACK`), en eist een ELF in de staging: de nieuwe
   kern start Hop daaruit. Het nieuwe beeld gaat ACHTER dat image
   (`kernflip::stage_slot`), niet eroverheen; ook bij een warme flip, zodat
   Hop na elke warme flip nog klaarligt voor een latere koude. Geen extra
   kopie van Hop, en geen plek in het kernvenster erbij.
3. **De flip-taak** legt hopfs vast (`HOPOS_FS_FROZEN`), stopt elke
   bewoner die niet op de OS-core woont en die Hop liet staan
   (`HOPOS_FLIP_COLD_STOP slot=N`), en zet elke geparkeerde app-core uit:
   `cpu::el2::chain::send_off` stuurt hem via zijn park-mailbox naar een
   uit-stub op de plek van de trampoline, die "koud" in de mailbox schrijft
   en PSCI CPU_OFF doet (weigert de firmware, dan terug de parkeerlus in).
   De taak wacht tot AFFINITY_INFO voor elke app-core OFF zegt
   (`HOPOS_FLIP_COLD stopped=N cores_off=K`). Dan het blob: alleen de vlag
   "koud", de generatie en de som, geen bewoners en geen conntrack; de
   sprong is die van warm (`HOPOS_FLIP_JUMP`).
4. **De nieuwe kern** leest het blob (`HOPOS_FLIP_BOOT gen=G
   HOPOS_FLIP_COLD_BOOT`) en boot verder als een koude kern: eigen
   switch-code (`HOPOS_CAGE_UP` met de nieuwe som), PSCI CPU_ON voor elke
   app-core die hij gebruikt, dezelfde hopfs-generatie
   (`HOPOS_FS_UP fresh=0`), en Hop koud uit de staging (`HOPOS_HOP_START`,
   `HOP_UP`). Hop leest zijn staat terug van hopfs en plaatst de jobs
   opnieuw. De guard eist alleen het net binnen de gratie
   (`HOPOS_FLIP_SETTLED`).

Waarom CPU_OFF en geen parkeerlus die blijft (29-09): de nieuwe kern
installeert zijn switch-code en parkeerlus opnieuw en start elke core met
CPU_ON; een core die nog in de oude parkeerlus staat, geeft ALREADY_ON en
staat midden in code die net overschreven wordt. Op de Pi 5-stockfirmware
komt een uitgezette core niet terug (10-07): daar weigert de koude flip
zodra een app-core ooit draaide.

Wat de koude flip niet meeneemt: de verbindingen (Hop en de apps
herstarten), de conntrack, en elke taak die Hop niet opnieuw plaatst. De
volumes blijven (hopfs). Na de sprong is de koude flip even onherroepelijk
als de warme; vóór de sprong laat elke fout een kern achter die doordraait.

De koude installatie (het image op het bootmedium en een herstart) blijft
de weg voor een node zonder staging van Hop (de Radxa) en voor een kern
die zelf niet opkomt.

### riscv64

Op riscv64 is er alleen de koude flip (02-10). De switch-code draait daar
uit het kern-image (niet uit een kopie in de plan-regio zoals op arm64),
dus de nieuwe kern kan niemand adopteren (`cage_riscv::adopt`), en een warme
flip weigert vóór de sprong (`flip::WARM`). Dezelfde weg als hierboven,
met drie verschillen:

- **Het app-hart uit het image** (`flip::cores_off`, `slots::park_for_flip`):
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

- `sh tools/qemu-test-flip.sh`: virt, alles hierboven, plus een
  TCP-verbinding van de host naar Hop die op kern A opengaat en door kern B
  beantwoord wordt, hopfs- en conntrack-getallen die aan beide kanten
  gelijk zijn, en FLIPCONN: een appspike met `ROLE=FLIPCONN` opent vóór de
  flip een UITGAANDE TCP-verbinding naar een echo op de host (10.0.2.2,
  door de masquerade en de slirp van QEMU) en stuurt elke seconde een
  regel. De echo zet er `A` voor, en `B` zodra de console
  `HOPOS_FLIP_BOOT` toont. Groen bij `HOPOS_APPSPIKE_FLIPCONN ok before=N
  after=M` met M > N, zonder fout van de stack in de app en met precies één
  verbinding bij de echo. Gemeten 29-09: `before=3 after=6`, 3 antwoorden
  via kern A en 3 via kern B, op virt en onder EDK2. Slirp laat het toe: de
  host-kant van de flow is de socket van slirp, en die ziet alleen een
  paar seconden stilte op dezelfde vier-tupel.
  Daarna (virt) de zwarte doos: een `system_reset` over QMP, de QEMU-vorm
  van een watchdog-reset (het RAM blijft). Kern A boot koud op dezelfde
  ring (`HOPOS_CONSOLE_CARRIED` met meer dan 0 bytes), en tcp/5555 speelt
  de laatste `HOPOS_APPSPIKE_DONE` van kern B af vóór de bootregel van
  kern A.
  In elke modus behalve `MISMATCH` draagt kern A een config in zijn
  venster en de bundel een leeg: `HOPOS_FLIP_CFG` vóór de sprong en
  `HOPOS_CFG_WINDOW` na de landing. Onder EDK2 staat er dan geen
  `hopos.cfg` op de ESP, dus koud start Hop op kern B alleen met de
  meegegeven config (groen 04-10, warm en koud).
- `MISMATCH=1 sh tools/qemu-test-flip.sh`: dezelfde bundel met een andere
  switch-code-som; groen alleen bij een weigering vóór de sprong.
- `COLD=1 sh tools/qemu-test-flip.sh`: de koude flip met die bundel. Eerst
  een appspike die blijft (`HOLD=1`, een bewoner op een app-core), dan warm
  (geweigerd, 502), dan koud (202): `HOP_FLIP_COLD_STOP`,
  `HOPOS_FLIP_COLD cores_off>=1`, `HOPOS_FLIP_BOOT gen=2`,
  `HOPOS_FLIP_COLD_BOOT`, dezelfde hopfs-generatie, `HOP_UP` ná de landing
  (twee `HOP_BOOT` in de console), en daarna weer werk op een core die
  kern A uitzette (CPU_ON). Gemeten 29-09: `stopped=0 cores_off=1` (Hop
  stopte de appspike zelf, de kern zette de core uit), en de koud
  herstarte Hop plaatste de job van vóór de flip vanzelf opnieuw. Het pad
  waarin de KERN een bewoner stopt die Hop liet staan
  (`HOPOS_FLIP_COLD_STOP slot=N`), loopt in deze toets dus niet.
- `OSCORE=1 sh tools/qemu-test-flip.sh` (ook met `COLD=1`): de kern op
  core 1; kern B komt daar door `_start` (`oscore: cpu 1 self-test` na de
  landing, geen `HOPOS_OSCORE_MOVE`). Dezelfde ingang als de Radxa en, via
  `_pi_start`, de Pi's.
- `BOARD=rpi4 sh tools/qemu-test-flip.sh`: de flip-ingang van de Pi op QEMU
  raspi4b (geen net, dus geen flip): core 1 met de registers van de
  trampoline komt tot kmain met dezelfde DTB; zonder merkteken parkeert hij.
- `BOARD=uefi sh tools/qemu-test-flip.sh`: hetzelfde als de eerste onder EDK2,
  met de PIE-basis en de feitenpagina; `COLD=1` ervoor is de koude flip
  onder EDK2 (Hop koud uit `hopos-stage.elf`, dat de feitenpagina terugwijst).
  Groen 29-09, beide.
- `sh tools/qemu-riscv-test-flip.sh`: de koude flip op QEMU virt riscv64,
  met de hop-CLI als trigger. Kern A met Hop op hart 0 en een appspike die
  blijft (een init-job) op hart 1; `hop flip` warm wordt geweigerd,
  `hop flip --cold` springt (`cores_off=1`), kern B landt koud, de
  kooi-zelftest op hart 1 slaagt na de sprong (het hart kwam uit de
  uit-stub), Hop start koud, dezelfde hopfs-generatie, en de appspike komt
  terug op hart 1. Groen 02-10.
- Host: `net` (`de_conntrack_overleeft_de_flip_via_de_actor`), `kern`
  (`a_new_kernel_carries_the_ring_and_every_reader_goes_on` in
  `conport.rs`,
  `a_version_2_bundle_carries_its_switch_code_sum`,
  `freeze_commits_first_and_names_the_generation`,
  `a_cold_handoff_carries_its_flag_and_nothing_to_adopt`,
  `the_new_image_goes_behind_hop_in_the_staging`, en in `system` de vlag
  tot aan de haak), `cpu` (`chain`: `only_a_parked_core_is_sent_to_the_off_stub`;
  `boot`: `only_core_zero_boots_cold_and_any_core_lands_a_flip`), `sync`
  (`one_place_holds_one_and_never_spins`), `board-licheerv`
  (`the_window_goes_along_only_into_an_image_without_one`).

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
