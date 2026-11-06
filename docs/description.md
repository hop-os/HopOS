# HopOS v3: de omschrijving

Wat het systeem is, laag voor laag, uit de code gelezen en niet uit de
andere docs. Het doel is review: een omschrijving maakt zichtbaar wat twee
keer bestaat, wat op de verkeerde plek staat en wat geen eigenaar heeft. Het
eerste deel beschrijft; het laatste hoofdstuk, "Opgevallen bij het
schrijven", verzamelt wat dubbel of scheef lijkt, met paden.

Stand: main op 333d91e (04-10-2026), met het config-venster van a6ceae5.
Regelnummers verschuiven; de padnamen blijven.
Bijgewerkt op 24-10-2026 (main op 2d1ed1f) langs de code: wat sinds 04-10
anders is, staat in de tekst, en in §8 staat bij elk thema wat er opgelost
is. De volgorde in de tijd en de afwegingen per stap:
[sequence-hopos.md](sequence-hopos.md).

## 1. In één alinea

HopOS is één statische binary die het hele OS is. Core 0 (of de core die
`hopos.oscore` aanwijst) draait de kern op EL2 (riscv: machine mode). Elke
app is een ELF in een eigen fysieke partitie achter een hardware-kooi
(stage-2 op arm64, PMP plus Sv39 op riscv), op een eigen core of op een core
die hij expliciet deelt. Er is geen shell, geen libc, geen proces: een app
praat met de kern via een gedeelde pagina, twee ringen en één TCP-verbinding.
De kern is alleen mechanisme. Hop, de orchestrator, is de eerste bewoner:
een app met het ene recht om andere apps te plaatsen, te stoppen en te
vervangen. De kern bezit de kooi, de switch, de opslag en de klok; Hop bezit
het beleid.

## 2. De lagen

```
                 apps/  appspike/  go/ (tamago)           ← apps, elk in een kooi
                 applib/  (runtime, leannet, system-API-client)
 ─────────────────────────── abi/ (het contract) ───────────────────────────
 hopos/          de binary: kiest één board, spawnt alle taken, de lijm
 kern/  net/     lifecycle, system-API, hopfs, flip  |  switch, NAT, pomp
 gui/  media/    fb-grant, USB-invoer, VOP2  |  VPU-driver, optisch
 usb/            de bus: de bouwsteen van gui en media
 board/          het Board-contract + 11 boards
 driver/         één crate per device (± 35)
 cpu/  fw/       boot, vectoren, kooi, switcher, OS-core  |  FDT, ACPI, ADT
 executor/ sync/ heap/ bounded/ dev/ netdev/ blkdev/      ← het fundament
```

Omvang in regels (Rust plus asm en scripts, incl. toetsen): driver 35k,
board 25k, kern 21k, cpu 16k, apps 13k, applib 13k, hopos 11k, tools 9k,
net 7k, abi 7k, gui 5k, media 5k, fw 5k, image 3k, de rest samen 6k.

De afhankelijkheden lopen van boven naar beneden. kern hangt bewust niet af
van net, cpu of board: al het ijzer komt binnen als trait (`kern/src/cage.rs`).
net kent geen kern. Alleen hopos kent alles en verbindt het.

## 3. Afspraken die overal gelden

- **Eén eigenaar per staat**, en die eigenaar is een taak (het handboek,
  `rustdoc/README.md` §1). Wie iets wil, stuurt een bericht. Een tabel die
  één taak schrijft en velen kort lezen is een `Local<RefCell<T>>`. De twee
  toegestane sloten: de heap (`heap::HeapLock`) en de console
  (`cpu/src/console.rs`).
- **Alleen `dev` raakt device-geheugen.** Geen `read_volatile`, geen barrière
  en geen cache-onderhoud buiten `dev/` (geverifieerd met grep). Een driver
  krijgt een `dev::Pa` van het board en leest via `Reg<T>`/`regs`.
- **Een driver vindt niets zelf.** Het board ontdekt het device (FDT, ACPI,
  ADT, PCI-walk), mapt het, geeft de driver een registerbasis en een
  DMA-regio die alleen van die driver is.
- **De interruptketen** is overal gelijk: de controller claimt, een
  `IrqAck` van de driver maskeert het device, een `sync::Signal` gaat aan,
  en de eigenaar-taak doet het werk. Nooit werk in exception-context. Het
  masker gaat pas weer open als de ring leeg is.
- **Faalbaar alloceren.** `BoundedVec` geeft `Full`, `try_reserve` in plaats
  van `push`, de executor boxt met `try_box`.
- **Een gebeurtenis, geen klok.** Taken slapen op een `Signal` of een
  deadline, niet in een poll-lus (`docs/apps.md`). Wat toch pollt, staat met
  zijn reden in §4.11; verder pollt niets.

## 4. Van stroom aan tot een app op de draad

### 4.1 Boot

| Pad | Ingang | Wie |
| --- | --- | --- |
| arm64 nVHE (qemuvirt, Pi's, Radxa) | `cpu/src/boot.rs` `_start` (op de Pi eerst `_pi_start`) | core 0, of een core met de feitenpagina in x0 (`admits`) |
| UEFI (generiek, O6N, Altra; VHE of nVHE) | `board/uefi/src/entry.rs` `_start_efi`: zelf-relocatie, EFI-memmap, `ExitBootServices`, `uefi_enter_kernel` | de BSP |
| Mac mini M4 | `board/apple/src/head.rs` (eigen relocatie, VHE-only) | iBoot of m1n1 |
| riscv64 machine mode | `cpu/src/riscv/boot.rs` `_start`; andere harts in een postvak-lus | hart 0 |

De drie arm64-ingangen zetten stack, BSS, vectoren (`__hopos_vectors`),
de identity map en de MMU. Die van riscv zet stack, BSS, `mtvec` (de trap
`__hopos_trap`) en de FPU, zonder MMU: machine mode vertaalt niet. Alle
vier roepen `kmain(x0, el)` in `hopos/src/main.rs` aan. Een
flip-boot komt via dezelfde ingangen binnen, met x0 = de feitenpagina op de
boot-scratch (O6, `docs/flip.md`): de handoff en wat de firmware bij stroom
aan gaf. `kmain` leest de pagina vóór `discover` (`flip::facts`), en bouwt
hem bij stroom aan opnieuw, zonder handoff: een koude boot is een boot
zonder handoff. riscv64 flipt niet en heeft geen pagina.

`kmain` doet console, privilegetoets (EL2 of M-mode, anders parkeren), heap,
`discover`, en `move_to_os_core`: wijst `hopos.oscore` een andere core aan,
dan start `cpu::smp::start_one` die core en wordt de boot-core een app-core
(`cpu::el2::hold`). Daarna `setup` en `exec.run`.

`setup` (`hopos/src/main.rs`) zet in deze volgorde op: flip-landing,
klok, conport, wandklok, het zaad (`seed`), de rol van de staging plus het
`Privilege`-token voor slot 1, opslag (probe, hopfs-actor, committer),
codec, system-API, interrupts met een IRQ-taak, de taken tick, watchdog,
telemetrie en load, de node-config, de NIC (pomp, switch, poort 0, DHCP,
listeners), USB-invoer, flip en slots. Ten slotte Hop in slot 1 (64 MiB,
LicheeRV 10 MiB) of de adoptie van de bewoners na een flip, in beide gevallen
onder zijn supervisor (§4.3). Hop draait in de idle van de kern op de
OS-core (`sleeper.host`).

### 4.2 De OS-core en de app-cores

De OS-core is gastheer: als de executor van de kern niets te doen heeft,
geeft `ArmSleeper::sleep` (`cpu/src/idle.rs`) de core in modus Resident aan
bewoners op EL1 onder stage-2 (`cpu/src/el2/oscore.rs`, `OsCore::run`). De
kern neemt hem terug via IRQ, kick, CNTHP-timer of een yield. riscv volgt
dezelfde vorm (`cpu/src/riscv/oscore.rs`) en deelt de rotatieregel
(`round`, `next`, `due`) en de bewonerslijst (`cpu/src/el2/roster.rs`).

Een app-core draait de **switcher** (`cpu/src/el2/switch.rs`), assembly in
drie smaken (nvhe, vhe, apple) die positie-onafhankelijk naar de plan-regio
gekopieerd wordt (descriptor `HOPSWTC1` met FNV). Een app komt binnen via
`_tramp` (VTTBR, VTCR, HCR, ERET naar EL1) en verlaat de core alleen met een
HVC: #0 exit, #1 yield met wektijd, #4 een sibling wekken, #6 de OS-core
kicken. De wissel is coöperatief: round-robin over de bewoners die due zijn.
Op riscv heeft de switcher een kill-tick van 10 ms, de enige preemptie in
HopOS.

De **kooi** op arm64: stage-2 met 4 KB-granule, 39-bit IPA, 2 MB-blokken,
tabellen in het kooiblok (`cpu/src/el2/stage2.rs`). Intrekken is nullen,
vegen, `tlbi alle1is`. Op riscv: een TOR-whitelist in PMP plus een
Sv39-relocatie (`cpu/src/riscv/{pmp,sv39}.rs`), geschreven door
`hopos/src/cage_riscv.rs`; de switcher leest `pmpcfg0` terug en weigert de
bewoner als het niet klopt.

### 4.3 Een slot van plaatsing tot stop

Wie wat bezit:

| Eigenaar | Staat | Waar |
| --- | --- | --- |
| `Lifecycle` (actor) | partities, het kooi-geheugen, cores, bewoners, kooi, grants | `kern/src/slots.rs`, `kern/src/cageheap.rs` |
| `Servicers` (leesbare tabel) | per levend slot generatie, mounts, job, partitie | `kern/src/slots.rs` |
| servicer-taak per levensduur | de outbox-lezer van één levensduur | `kern/src/slots.rs` |
| `System` | lopende image-stromen, het Privilege-token, de staat van Hop | `kern/src/system.rs` |
| supervisor van Hop (taak) | Hop's spec, zijn image en het vorige | `hopos/src/slots.rs` |
| `FsActor` | de hopfs-boom | `kern/src/rpc.rs` |
| `StoreQueue` | 8 openstaande store-ops | `kern/src/store.rs` |
| `Switch` (actor) | poorten, ringhelften, NAT | `net/src/switch.rs` |
| `Pump` | de NIC | `net/src/pump.rs` |

Er is geen maximum aantal slots (01-11). Een slot bestaat van de plaatsing
tot de stop: de kern geeft het laagste vrije nummer (zoals een fd) en kent
alleen "past het". Zijn tabellen groeien met het hoogste nummer dat er ooit
tegelijk was (`kern::SlotVec`, zoals de fd-tabel van Linux); zijn kooi-blok,
zijn VMID en de groei van een bewonerslijst komen bij de bouw uit het
kooi-geheugen (chunks van 2 MiB uit de pool, `kern/src/cageheap.rs`) en gaan
bij de stop terug. Het plan is per core: cores zijn vast. Het slot-LAN is
10.100.0.0/16, het slot zestien bits (`abi::layout::SLOT_MAX`, een grens van
het adres). `PrivOp::Slots` geeft de bezette slots in stukken, zoals
`getdents`.

1. **Start.** Hop stuurt `START_SLOT` over de system-API. `System::start`
   toetst env, poorten en mounts en vraagt de lifecycle om een claim.
   `Lifecycle::claim` kiest het slot (het laagste vrije, behalve dat van
   Hop), cores (`kern/src/pool.rs`: dedicated, een sharegroep, of de
   OS-core als groep `system`), alloceert een partitie
   (`kern/src/partmem.rs`, best-fit op 2 MiB, levensloop als type
   `Partition<Free|Owned>`) en scrubt die in brokken.
2. **Stromen.** Hop stroomt de ELF met `STREAM_IMAGE`. `Placer`
   (`kern/src/system.rs`) schrijft elk segment rechtstreeks in de partitie,
   leest daarna de symbolen, en `abi::place::build` zet de vier woorden die
   de kern patcht (RamStart, RamSize, slotHint, ABI-stempel). Een Go-image
   (stempel 10) gaat door dezelfde weg.
3. **Wapenen.** `Lifecycle::arm`: poorten publiceren bij de switch, de kooi
   bouwen (het kooi-blok, de VMID en plaats in de bewonerslijsten uit het
   kooi-geheugen; de ringen aan de switch), grants, de servicer registreren
   (de kooi spawnt er één voor deze levensduur, `Cage::serve`), dispatch, en
   wachten tot de boot niet meer pending is. Een
   fout vóór de dispatch is een rollback; een onzekere dispatch is
   `HOPOS_FATAL` (de watchdog reset, docs/sequence-hopos.md §13.5).
4. **Draaien.** De servicer leest de outbox (logregels naar de console en
   `NEXT_LOG`, SMP-verzoeken naar de lifecycle).
5. **Stop.** `STOP_SLOT` met een termijn (Hop: 3 s): de kill-vlag met die
   termijn in ms erin (`CTRL_KILL`; 1 was de oude vorm zonder termijn),
   poorten weg, wachten op stil, anders revoke met 1 s gratie. Tijdens het
   wachten houdt de app zijn net, zijn generatie (dus de system-API) en zijn
   outbox-lezer: dat is zijn gratie, SIGTERM vóór SIGKILL. Dan pas evict
   uit de tabel en de ringen van de switch. Stil: partitie, kooi-blok, VMID
   en cores vrij. Niet stil: `HOPOS_FATAL`. De servicer-tabel meldt de stop
   (`Servicers::take_stopped`) en de committer legt hopfs vast.

**Hop zelf.** Hop is de bewoner van slot 1, en de kern is zijn supervisor
(`Supervisor`, `hopos/src/slots.rs`). Eén taak plaatst hem, bewaakt hem
(elke wissel één regel) en herstart hem alleen als hij daar zelf om vraagt,
terwijl elke andere bewoner doordraait:

- **Een vraag van Hop** (`HOP_RESTART`, met hetzelfde image of een nieuw
  dat hij rauw stroomde met de jobnaam `hopos.hop-image`): de system-API
  toetst velden en stroom, de haak toetst som en ELF en kopieert het image
  in de kern-heap, het slot gaat terug in de pool. Dan vertrekt Hop zelf,
  zoals een systemd-dienst die om zijn eigen herstart vraagt: hij schrijft
  het antwoord en zijn staat weg en keert terug uit `main`. De kern wacht
  daar hoogstens zijn termijn op (standaard 3 s,
  `HOPOS_HOP_RESTART reason=request image=new|same n=K`) en belt pas daarna
  de stop-bel met een korte termijn als vangnet
  (`HOPOS_HOP_STOPPED reason=request left=self|bell`, en geen
  `HOPOS_HOP_EXIT`: deze exit is gevraagd); dan de nieuwe Hop. Tot 22-10
  stopte de kern hem na een vaste halve seconde, en een Hop die nog bezig
  was, schreef zijn antwoord nooit. Het image van de Hop die draait blijft
  in de heap voor een volgende herstart met hetzelfde image; een vorig
  image wordt niet bewaard.
- **Een Hop die faalt is een node die faalt** (Derek, 23-10): een fault,
  een exit die niemand vroeg, of een nieuw image dat niet plaatst. De kern
  zet de diagnoseregel neer (`HOPOS_HOP_FAULT`, `HOPOS_HOP_EXIT`,
  `HOPOS_HOP_FAIL`, dan `HOPOS_HOP_DOWN`) en laat hem liggen zoals hij viel.
  Geen herstart met back-off, geen terugval op een vorig image: Hop's
  hartslag en de canary stoppen, de node-watchdog houdt na `STALE_NS` zijn
  pets in, de hardware reset, en de koude boot start de Hop van het
  bootmedium (§4.10). Falen gaat naar het oude; is het ijzer de oorzaak,
  dan helpt een verse start in dezelfde boot niet, en een koude boot
  ontdoet de node juist van zijn toestand. Na een stroomonderbreking is
  het net zo de Hop van het bootmedium.

Elke nieuwe Hop krijgt hetzelfde slot (en dus de bevoegdheid: het token
hoort bij slot 1), dezelfde env, poorten, volume en `Placement`
(`HOPOS_HOP_START`). De start zelf is het seintje: de kern plaatst hem pas
als de oude bevestigd weg is, en laat zijn system-verbindingen toe vanaf
zijn eerste instructie (de generatie van het slot staat vóór het
startschot, en de verbindingsteller hoort bij een levensduur, zodat de
plaatsen van de oude Hop niet meetellen). Hop wacht dus niet en probeert
niet opnieuw: een weigering of een mislukte call naar de kern bij zijn
start is een fout, en dan stopt hij hard; de kern laat hem liggen
(`HOPOS_HOP_DOWN`) en de watchdog reset de node. Een toets in kern houdt
die volgorde vast (`a_resident_is_admitted_from_its_first_instruction`). Wat de oude half deed, een open image-stroom, gaat
terug in de pool (`HOPOS_HOP_STREAMS_DROPPED`). Zijn agent-staat vindt hij
in de kern terug: Hop legt hem bij elke wijziging neer (`STATE_PUT`, één
blob tot 1 MiB, vervangend en atomair) en leest hem na de start
(`STATE_GET`); `HOPOS_HOP_STATE kept=B` zegt bij elke herstart hoeveel er
klaarligt. Het blob leeft in de heap van de system-API en sterft met de
boot, zoals de fdstore van systemd: een koude boot (ook die van de
watchdog na een falende Hop) begint dus leeg, en een
staat van een vorige boot kan nooit een bewoner van deze voor de zijne
houden. Wat een kern-flip ermee doet: [flip.md](flip.md).

### 4.4 De app-kant: applib

applib (`applib/`) is de runtime waar elke Rust-app tegen linkt (op
0x50010000, het canonieke adres dat ook Go gebruikt). `_start` zet vectoren,
de stage-1-identity map en de MMU; `main!` → `rt::run` maakt de `App`
(staart, outbox, control-page, env), de heap (dezelfde crate als de kern),
meldt READY, en draait de executor met de hartslag (`watch`, elke 50 ms)
en `main`, plus `smp::bring_up` als de app meer dan één core heeft; opent
de app zijn net, dan komt de RX-pomp erbij (`appnet.rs`). Ziet de
hartslag de kill-vlag, dan luidt hij de
stopbel van de app (`App::stop`, een `sync::Stop`; `app.stopped().await`
voor de hoofdlus) met de termijn van de kern min 500 ms als gratie
(`HOPOS_APP_STOP grace_ms=`). Keert `main` terug, dan volgt het
net-afscheid (`App::shutdown`, `HOPOS_APP_SHUTDOWN`) en de exit; is `main`
na de gratie nog niet terug, dan doet de hartslag dat afscheid zelf
(`HOPOS_APP_STOP_FORCED`). De slaper (`applib/src/sleep.rs`) wapent eerst
de RX-deurbel en yieldt dan (HVC #1) of wacht op WFE.

Wat een app verder krijgt: een leannet-stack per app (`appnet.rs`: IP en MAC
uit het slotnummer, de kern als gateway, DNS-stub, TCP/UDP/UDP6-handvatten),
de system-API-client (`sys.rs`), de store (`store.rs`), SMP met één executor
per core (`smp.rs`), `log!` naar de outbox, een ChaCha20-DRBG (`rand.rs`),
het glas en de invoer (`fb.rs`), de codec-client (`codec.rs`) en `stacktask`
voor synchrone C-code. Alle getallen van het contract komen uit `abi` via
`applib/src/contract.rs`.

### 4.5 Het contract: abi

`abi/` is het enige dat kern en app delen, ABI-versie 1, byte voor byte
gelijk aan Go-ABI 10.

- **De staart**: de bovenste 2 MiB van elke partitie, met de control-page
  (0x0), de outbox-ring (0x1000), de stub, de RISC-V-map en vanaf 0x20000 de
  net-ringen (`abi/src/layout/`). `Tail` rekent elk adres uit.
- **De ring** (`abi/src/ring.rs`): SPSC over gedeeld geheugen, monotone
  indexen, records die nooit wrappen, een `Coherence`-belofte per kant, en
  een lezer die de schrijver wantrouwt (een onmogelijke kop is `Corrupt`).
  `write` meldt de overgang van leeg naar niet-leeg: het deurbelcontract.
- **De system-API** (`abi/src/systemapi.rs`, `hopabi.rs`): een frame
  "HOPS" van 12 bytes over TCP naar 10.100.0.1:10100, met daarin een
  verzoek of antwoord met een kop van 24 bytes. Bestands-, store-, codec-,
  device- en bevoegde ops (`PrivOp`, alleen Hop).
- **De plaatsing** (`place.rs`), het PA-plan (`layout/plan.rs`),
  `sha256`, `checksum` (FNV), `NatState` en `FlowState` (de NAT-staat voor
  de flip, sinds c6e0bf8 één definitie), en `glass` (het glas en het
  invoercontract: de `FB_*`-sleutels, poort 7879, de JSON-invoerregel).

### 4.6 De system-API, van app naar kern en terug

De app opent met zijn eigen stack een TCP-verbinding naar 10.100.0.1:10100.
De switch toetst bron-MAC en -IP en geeft het frame aan poort 0, de
node-stack van de kern (leannet zonder ipv6, `hopos/src/net.rs`). De
listener laat de verbinding toe als het bron-IP bij een levende generatie
hoort (hoogstens 2 verbindingen per slot, Hop 3). Een werker draait
`System::serve`: kop lezen, dispatchen naar log, `FsActor`, store, codec,
device of `PrivOp`, antwoord terug door dezelfde stack. Een bewaker sluit de
verbinding als de generatie verdwijnt of na 300 s stilte.

### 4.7 Het netwerk: switch, NAT, pomp

net/ is geen TCP/IP-stack maar een L2-switch met NAT. Er is in Rust één
TCP/IP-implementatie, leannet, twee keer geïnstantieerd: als node-stack in
de kern en per app in applib. DHCP draait alleen in de kern, DNS alleen in
applib.

- **In:** NIC → `Pump::rx_pass` → ingress-rij → `Switch::switch_pass` →
  `uplink_in`: multicast naar alle slots, IPv6 op een slot-MAC naar dat
  slot, `Nat::inbound` voor het node-IP (antwoord op een masquerade-flow, of
  DNAT naar een gepubliceerde poort), de rest naar poort 0 (DHCP,
  listeners).
- **Uit:** de app schrijft zijn TX-ring en kickt; de switch kiest relay
  (naar een ander slot), forward (broadcast, ARP voor de gateway beantwoordt
  hij zelf), of de gateway: naar poort 0 voor 10.100.0.1, SNAT voor een
  antwoord van een gepubliceerde poort, hairpin voor het eigen node-IP, of
  masquerade naar de uplink.
- **Poorten:** de poorten van een start, die van een jobspec en Hop's
  8080 en 9080 (in zijn `StartSpec`, sinds 110b945), via `Cage::publish`
  naar `Nat::publish` (tcp en udp), hoogstens 512.

### 4.8 hopfs

`kern/src/hopfs.rs`: de boom in RAM, de data in blokken van 4 KiB op de
schijf, twee metaplekken met SHA-256. Een commit schrijft de boom naar de
andere plek en dan de kop; vrijgaven wachten tot de commit erna, zodat de
oude boom nooit naar hergebruikte blokken wijst. De actor
(`kern/src/rpc.rs`) plant synchroon en doet de I/O als futures in een pool
van 16, met de regels: per slot één call tegelijk (lezingen mogen naast
elkaar), één schrijver per bestand, en destructieve ops alleen als niets
anders loopt. `OP_READ_MANY` (21) is een bundel van tot 16 lezingen uit één
bestand in één call: alles in één batch op de wachtrij van het device, één
antwoord met per lees de uitkomst ([apps.md](apps.md), "Veel kleine
lezingen").

Paden: een volume uit de jobspec (`/data` → `/volumes/demo`) wint op
langste prefix, anders `/.tasks/slot<N>`. Die root wist de actor lui, bij
de eerste bestandscall van een nieuwe levensduur (`prepare` in
`kern/src/rpc.rs`): wat een bewoner achterliet, staat er tot zijn
opvolger een bestandscall doet. Een levensduur die bij een warme flip
geadopteerd werd, houdt zijn root.
`OP_SYNC` is een bevestigde barrière (`docs/storage-sync.md`). De committer
legt elke 10 s vast, of zodra een slot stopt.

Onder hopfs: `blkdev::Queue` (blk-mq in het klein, één pacer kijkt op de
bel van de lijn naar het device en wekt de anderen) over nvme of
virtioblk.

### 4.9 De kern-flip

Het beleid staat in `hopos/src/flip.rs`, het mechanisme in
`kern/src/kernflip.rs` en `cpu/src/el2/chain.rs` (`docs/flip.md` voor de
procedure).

1. Hop stroomt een bundel in een rauw slot en stuurt `PrivOp::Flip` met de
   sha. De kern toetst de som en de switch-code, maakt de bundel plat achter
   Hop en relokeert (HOPRELO1).
2. Warm: hopfs bevriezen en committen, de NAT-flows vangen
   (`SnapshotNat`), USB stil, `Lifecycle::snapshot`, het handoff-blob op
   `FLIP_HANDOFF_PA`, en de sprong. Koud: bewoners stoppen, app-cores uit,
   springen zonder adoptie.
3. De nieuwe kern leest het blob (`kernflip::adopted`), houdt de poorten
   vast vóór de eerste switch-ronde, adopteert de bewoners, publiceert de
   poorten opnieuw, zet de NAT terug; de guard van de watchdog (120 s,
   voor elke boot) bewaakt de landing.

### 4.10 Rondom: watchdog, telemetrie, console, config

- **Watchdog** (`hopos/src/watchdog.rs`, beleid in `kern/src/watchdog.rs`):
  gewapend als eerste stap van `setup`, met één guard van 120 s voor elke
  boot: zonder levensteken binnen die termijn geen pets meer. Daarna elke
  2 s een verse TCP-verbinding naar Hop; beweegt de hartslag niet, dan
  stopt het aaien en reset de hardware-watchdog de node. De hardware zit per
  board; op QEMU virt is het de i6300esb op PCIe, in elke ring
  (`driver/i6300esb`, `board/qemuvirt/src/wdt.rs`). Hard falen loopt
  erover: `cpu::fatal!` geeft één regel `HOPOS_FATAL <reden>` en parkeert,
  de pets stoppen, de watchdog reset. Zo ook een falende Hop (§4.3):
  `HOPOS_HOP_DOWN`, `HOPOS_FATAL`, en de koude boot start de Hop van het
  bootmedium (`tools/qemu-test-hop-fail.sh`).
- **Telemetrie** (`hopos/src/telemetry.rs`): thermiek en klokbeleid per
  board, met `driver/dvfs` als governor.
- **Console**: één ring van 32 KiB op de plek van de zwarte doos
  (`kern/src/conport.rs`, `hopos/src/conport.rs`), die een flip en een
  reset overleeft, met drie lezers: de UART (een pomp, de schrijver wacht
  niet op de baudrate), `nc NODE 5555` (eerst de ring, dan live; hoogstens
  4 lezers, gewekt door de schrijver) en Hop met `NEXT_LOG` 0, regel voor
  regel. De tee schrijft verder alleen het glas. Na een reset staat de
  console van de dode kern vóór de nieuwe boot (`HOPOS_CONSOLE_CARRIED`).
- **Config**: een venster van 16 KiB (`#HOPCFG1`) in elke kern
  (`board/src/cfgwin.rs`), gevuld door `image/hopcfg.py` of `hop image`.
  Een gevuld venster wint van de oude bronnen (ESP-`hopos.cfg`, initrd,
  0xF000 op de M4, bootargs). `kern/src/nodecfg.rs` maakt er de env van Hop
  van.

### 4.11 Wat pollt en waarom

De kern wacht op een bel (R1 van docs/sequence-hopos.md §13.1): de NIC, de
kick van een app-core (het interne LAN: de TX-ringen en de outboxen), de
schijf, en de timer van een deadline. Elke NIC en elke schijf heeft een
lijn (§14.1 O3); een board dat er geen bedraadt, start niet. Op de lijn
van de NIC zit ook de TX-completion (elke driver, sinds 01-11): een volle
TX-ring die leegloopt, wekt de pomp (`netif_wake_queue`). Daarnaast
wacht elke wacht hoogstens zijn termijn (de 5 s van een blok-opdracht,
`blkdev::TERM`), en dat is geen poll. Wat wél op een klok kijkt in plaats
van op een bel, staat hier, met de reden. De code verwijst bij elke poll
naar deze tabel. Verder pollt niets (O4, 01-11).

| Wat | Waar | Ritme | Waarom geen bel |
| --- | --- | --- | --- |
| De USB-invoer | `gui/usbin` (`POLL_INTERVAL_NS`), alleen met `gui` | 4 ms | Besluit O4: een optioneel vlak, en een poll houdt de flip eenvoudiger dan een extra lijn (xHCI) |
| De codec | `kern/src/codecabi.rs` (`POLL_PUMP`), alleen met `media` | 1 ms, alleen tijdens een wachtende `OP_CODEC_POLL` | Besluit O4: de engine heeft hier geen interrupt (`media/mve`); de VPU-lijn (intid 358 op de O6N) vraagt ijzer |
| De link, vóór de pomp start | `hopos/src/net.rs` (`LINK_POLL`), via `netdev::Device::link` | 50 ms, alleen tot de eerste link | De PHY heeft hier geen bel: geen board bedraadt een PHY-interrupt, en een link-wissel van de MAC zelf (de LinkChg van de Realtek) heeft niet elke NIC; één weg voor alle boards. Linux pollt zo'n PHY elke seconde (phylib `PHY_POLL`, `phy_state_machine`) en zet de MAC bij de wissel (`adjust_link`); hier zet de driver hem bij de eerste link, en daarna kijkt niemand meer. De probe wacht niet (01-11, elk board); begrensd door de guard van 120 s |
| De slaap van de kern op de LicheeRV | `cpu::riscv::idle::WFI_CAP_NS` (`board/licheerv`) | hoogstens 2 ms | IJzer: op de SG2002 wekt een `wfi` op `mtimecmp` niet betrouwbaar (01-08), en de C906L heeft geen bel naar de C906B. Na elke slaap kijkt de deur in de ringen en de outboxen |
| De slaap van de kern op WFE | `cpu::idle::Mode::Wfe`: de Pi's, de M4 als de timer-FIQ zijn core niet bereikt | de event-stream, ~1 ms | IJzer: WFI op de timer is op de Pi's niet gemeten, en op de M4 bereikt de timer-FIQ een door de kern opgebrachte core niet (29-08). Een slaap die niet wekt is een hang |
| De wekker van de app-cores op de M4 | `hopos/src/cage.rs` (`WAKE_EVERY`) | 1 ms | IJzer: een app-core die in de WFI van de switcher slaapt, wekt geen eigen timer-FIQ (02-09); de kern is de enige die hem wekt |
| De p-states op de M4 | `board/apple/src/lib.rs` (`PSTATE_WATCH_EVERY`) | 2 s | Een meting, alleen-lezen: de APSC heeft geen melding naar ons |

Geen poll maar een klok, omdat de klok het werk is: het aaien van de
watchdog (2 s), `HOPOS_TICK` (1 s), de telemetrie (1 s), de flow-expiry van
de NAT (30 s), de commit van hopfs (`COMMIT_EVERY`), de meetregel van de
schijf (10 s), de timers van TCP en de vernieuwing van de DHCP-lease.

Nog een poll, bij een andere agent van 01-11 (§14.8); weg of naar een bel,
of anders deze tabel in:

| Wat | Waar | Ritme | Eigenaar |
| --- | --- | --- | --- |
| De stop wacht op een stille context | `kern/src/slots.rs` (`STOP_POLL`) | 10 ms | D (de exit-kick bestaat nu: de stop kan op de kick wachten) |
| Hop bewaken, en wachten tot hij weg is | `hopos/src/slots.rs` (`HOP_WATCH`, `LEAVE_POLL`) | 1 s, 50 ms | A/D (§13.4) |
| De TCP-lezers van de console | `kern/src/conport.rs` (`POLL`) | 100 ms | B (§14.5) |
| De UART-pomp zolang de UART achterloopt | `hopos/src/conport.rs` (`UART_STEP`) | 1 ms | B (§14.5) |
| De input-listener op de stack | `hopos/src/net.rs` (`STACK_RETRY`) | 100 ms | A/C (§13.3) |
| De lease-lus | `hopos/src/net.rs` | 5 s | A (§13.3); `nic_retry` is weg (01-11): geen link is geen net, de guard reset |
| De slaap van een app-core in de yield | de switcher (`SCHED_SLEEP_CAP`, 10 ms op QEMU riscv), §8.12 | 10 ms | open: de kern zet bij een stop de wek-latch van de keten |
| De beurt van een bewoner van de OS-core | `cpu::el2::TURN_CAP_NS` | 10 ms | open: het kwantum van de gedeelde OS-core, zie het rapport van E |

## 5. De lagen één voor één

### 5.1 Het fundament

| Crate | Wat | Gebruikt door |
| --- | --- | --- |
| `dev` (0,9k) | MMIO-lezen en -schrijven, `Reg<T>`, kopieën, barrières, `push`/`pull` (cache), `poll_until`, `delay`, de memcpy-lussen. Op de host no-ops. | ± 50 crates |
| `bounded` (0,3k) | `BoundedVec<T, N>`, `Full<T>` en `Text<N>` (`fmt::Write` op een vaste buffer). Geen map, set of ringbuffer. | 35 bestanden |
| `sync` (1,5k) | `Signal`, `Stop`, `AtomicWaker`, `WakerSet`, `spsc::Channel`, `mpsc::Mailbox`, `Local`/`LocalCell`, `select`, `Futures` (futures zonder heap; tot 04-10 `Pool`), `Doors` (werkers met een acceptor), `Timer`, `yield_now`, `Oneshot` met `oneshot::call` (verzoek en antwoord). Geen mutex. | overal |
| `heap` (1,2k) | De allocator van kern en apps: vrije lijsten met grenslabels, klassen per 16 bytes tot 1 KiB, `HeapLock` (spinslot met core-id). | `board/src/heap.rs`, `applib/src/heap.rs` |
| `executor` (0,8k) | Eén per core: taken in een box, timerwiel, ready-bits, `Sleeper`-trait, uitstelbare timers, `ExecTimer`. | hopos, applib, net |
| `abi` (7k) | Zie §4.5. `forbid(unsafe)`. | kern, net, cpu, boards, applib, hopos, gui |
| `netdev` (0,1k) | Het NIC-contract: `transmit`, `receive`, `flush`, `mac`, `irq`, `stats`; `IrqAck` en `AckSlot` voor de ack van de lijn. | 7 NIC-drivers, net, applib |
| `blkdev` (1,3k) | Het blokcontract: `AsyncBlockDevice` (tickets; zonder tags één, ticket 0), `Disk`, `Pace`, `Queue` als enige voorkant, `BlockIo` voor hopfs. | nvme, virtioblk, hopos |

### 5.2 cpu en fw

`cpu/` (16k) is de architectuur, in twee sporen: boot (`boot`, `vectors`,
`console`) en kooi (`el2/`, `psci`, `smp`, `irq`, `gicv3`, `idle`, `trng`,
`drbg`, `memattr`). `el2/` bevat stage-2, de switcher, dispatch, de
bewonerslijst, de OS-core en de flip-sprong; `riscv/` is de machine-mode-helft
(csr, boot, trap, clint, plic, idle, pmp, sv39, switch, oscore). Op de host
bouwt elk arm64-module als stub, omdat hopos ze bij naam noemt.

`fw/` (5k, `forbid(unsafe)`) leest firmware op `&[u8]`: FDT, ACPI
(MADT, MCFG, SPCR, GTDT, IORT, PCCT, FADT), AML (alleen `_PRT`), de
Apple-ADT, xnuboot, GPT en `bootcfg`. Onvertrouwde input geeft `None`,
nooit een panic.

### 5.3 driver

| Groep | Crates |
| --- | --- |
| Interrupts | `gicv2` (Pi), `gicv3` met ITS (QEMU, Radxa, Altra, O6N), `aic` (M4); de PLIC staat in cpu |
| Console | `pl011`, `ns16550`, `fb` (tekst op een framebuffer) |
| PCIe | `pcie` (ECAM, BAR's, capabilities, MSI-X), `brcmpcie` (Broadcom-root-complex van de Pi's) |
| NIC | `nic/mdio` (gedeelde clause 22), `genet` (Pi 4), `gem` (Pi 5, RP1), `igb` (Altra, QEMU), `rtl8126` (O6N), `stmmac` (Radxa, LicheeRV), `tg3` (M4), `virtionet` |
| Opslag | `virtiopci` (transport mmio en pci), `virtioblk`, `nvme` (pci en Apple-ANS) |
| Firmware | `rtkit` (Apple-coprocessoren), `smc` (Apple), `scmi` (O6N, Radxa), `smpro` (Altra), `vcmail` (Pi) |
| RNG | `rkrng` (Radxa), `rng200` (Pi) |
| Watchdog | `dwwdt` (Radxa, LicheeRV), `i6300esb` (QEMU virt, in elke ring); de andere zitten in hun board |
| USB | `usb/xhci`, `usb/dwc3`, `usb/hid` |
| Geen driver | `dvfs` (klokbeleid), `codec` (het videocodec-contract) |

Een NIC is van de RX-pomp, een blokdriver van de hopfs-actor via
`blkdev::Queue`, xhci van de USB-taak, de firmwarekanalen van de
thermiek-taak.

### 5.4 board

`board/src/lib.rs` definieert `trait Board: Sync + Watchdog + Thermal +
ClockKnob` (sinds a0eaeb0): `Nic`, `Sleeper` en `Disk` als types,
console, privilege, firmware, heap, `discover`, klok, geheugen, cores en
hun klasse (`this_core`, `os_core`, `os_bell`, `kick_self`), de config
(`config`, `bootargs`, `boot_param`), het PA-plan,
`start_interrupts`/`dispatch_interrupts`, `probe_nic`, `probe_disk`,
constanten als `FLAVOR` en `HOP_MEM`, en met `gui` de framebuffer en
USB-hosts. Daarnaast `cfgwin`, `stage` en `heap`.

hopos kiest precies één board met een feature (`board-qemuvirt`, `-rpi4`,
`-rpi5`, `-rk3566`, `-uefi`, `-o6n`, `-altra`, `-qemuvirt-riscv`,
`-licheerv`, `-apple`), linkt het als `vboard` en kiest het linkscript in
`hopos/build.rs`.

| Board | Ijzer en boot | Eigen |
| --- | --- | --- |
| qemuvirt | QEMU virt, `-kernel`, DTB | virtio-mmio, ramfb |
| qemuvirt-riscv | QEMU virt riscv64, `-bios none` | proefbank van de LicheeRV, kooi-zelftest |
| uefi | PE-stub op EDK2 of vendor-UEFI, ACPI | GICv3+ITS, SBSA-watchdog, GOP, vaste kernvensters |
| o6n | Orion O6N (Cix P1), bovenop uefi | rtl8126, NVMe, SCMI-thermiek, `_CPC`-klok, tien xHCI's, VPU, VHE |
| altra | Ampere Altra, bovenop uefi | igb met MSI-X, NVMe, SMpro-thermiek |
| raspi | de gedeelde Pi-laag, `Raspi<S: Soc>` | VideoCore-mailbox, GIC-400, PM-watchdog, RNG200 |
| rpi4 / rpi5 | `Soc` voor BCM2711 / BCM2712 | GENET en VL805 / de RP1-keten met GEM |
| rk3566 | Radxa Zero 3E, U-Boot `booti` | DWMAC4, SCMI-klok, TSADC, DW-WDT, VOP2 |
| licheerv | SG2002, FSBL, M-mode zonder SBI | dwmac1000 met ePHY, C906L als app-hart, Hop ingebakken |
| apple | Mac mini M4, iBoot of m1n1, ADT | AIC, DART, tg3, ANS-NVMe via RTKit, eigen MMU |

### 5.5 kern en net

Zie §4.3 tot §4.9 voor de stromen. Overige modules in `kern/`: `store`
(object-store-ops in de rij, Hop doet de S3-kant), `deviceabi` (de optische
drive via `/devices/discN`), `codecabi` (codec-sessies, feature `media`),
`grants` (het framebuffer-venster in één kooi), `nodecfg`, `watchdog` en
`load` (alleen rekenwerk), `conport` (de bouwstenen van poort 5555).

In `net/` verder: `flows` (conntrack, 4096 flows, 512 per slot), `host`
(poort 0 en de node-stack achter een trait), `gw` (de 1:1-herschrijving
10.100.0.1 ↔ node-IP), `plan` (adresplan), `ring`, `wire`, `nodemac`.

### 5.6 hopos

De binary (11k): `main.rs` (kmain, setup, tick), en de lijm die de traits
van kern invult en de taken spawnt: `slots`, `kooi`, `cage`/`cage_riscv`,
`glue`, `config`, `net`, `storage`, `flip`, `codec`, `optical`, `gui`,
`usb`, `off`, `conport`, `watchdog`, `telemetry`, `seed`, `clock`, `mem`.
Features buiten het board: `gui`, `media` en `vhe`. Headless is zonder
flag; `gui` en `media` staan los en trekken elk de interne bouwsteen `usb`
mee; wat een build niet heeft, staat als stub in `off.rs`. Meten gaat op
één manier (sequence-hopos.md §14.6): tellers op de control-page,
`SLOT_STATUS`, Hop rekent.

### 5.7 gui, media en usb

- `usb`: de bouwsteen van beide: bezit de xHCI-hosts, volgt in- en
  uitpluggen en bedient bulk-opslag voor optical; met `usb/hid` (alleen
  gui) de rapporten van toetsenbord en muis.
- `gui/fbgrant`: wie het glas krijgt (`GUI=display` of `FB=1`), de
  `FB_*`-env, en de kernconsole van het scherm af en terug.
- `gui/usbin`: zet HID om naar JSON-regels voor de houder van het glas
  (poort 7879).
- `gui/rkscan`: VOP2 en DW-HDMI van de RK3566 (nog niet op ijzer).
- `media/mve`: de Linlon V8-VPU van de O6N als `driver_codec::Engine`.
- `media/optical`: BOT en MMC READ(10) over `usb`.

### 5.8 Apps

| App | Wat |
| --- | --- |
| `appspike` | De ABI-toets met rollen (SMP, SHARE, FLIPCONN, STORE, VOLUME, FAULT); markers voor de QEMU-tests |
| `welcome` | De nodepagina en `/health`; het referentievoorbeeld |
| `bench` | De serverkant van `tools/netmeter`, plus lastrollen (BURN, MCAST, …) |
| `vitals` | De board-dokter: HTTP-pagina met cpu-, mem-, net- en disktests |
| `syncprobe` | De duurzaamheidsprobe voor `OP_SYNC` |
| `display` | Houdt de fb-grant, tekent klok, cursor en invoer |
| `decode` | Een stream door de VPU (feature `media`) |
| `cloudflared-lean` | De Cloudflare-tunnel in Rust (TLS 1.3, HTTP/2, Cap'n Proto); sinds 19-10 in de Hop-repo (`apps/cloudflared-lean`), met Hop's release |

Go-apps (`go/`, `docs/go-apps.md`): gebouwd met de gepatchte
tamago-toolchain tegen de Go-SDK `metal/v2` (buiten deze repo), met een
eigen netstack. De kern plaatst ze via dezelfde weg (ABI-stempel 10).

### 5.9 Bouwen, testen, release

- **Images**: per board een script in `image/` (build, objcopy, het
  config-venster vullen, Hop als staging, firmware, `tools/mkcard`). Hop
  komt uit de hop-repo via `tools/hop-build.sh`, gepatcht tegen applib, abi
  en sync van deze werkboom. `image/flip-bundle.sh` maakt de flipbundels.
- **Gate** (`tools/gate.sh`): host-tests, clippy, fmt, target-builds van
  alle boards en smaken. Geen QEMU.
- **QEMU-tests**: 21 shellscripts plus `qemu-test-sync.py` op de gedeelde
  laag `tools/lib.sh` (± 520 regels, sinds d4d8700). Elk wacht op markers
  in de console. `tools/qemu-all.sh` draait ze als 33 ringen na elkaar,
  met één eindregel. `tools/soak.sh` (ijzer) en `tools/qemu-soak-hop.sh`
  (QEMU) voor uren.
- **Release** (`tools/release.sh`): elk board headless en headfull, de
  bundels, de apps, `SHA256SUMS`; publiceert zelf niets.
- **Host-tools**: `mkcard` (MBR + FAT16 + raw blobs), `netmeter`, `loc`.

## 6. Waar de documentatie staat

| Doc | Over |
| --- | --- |
| `README.md` | Pitch, mapindeling, de belangrijkste commando's |
| `docs/README.md` | Wat bewezen is op QEMU, de vlakken, images per board |
| `docs/boards*.md` | Per board de checklist; `boards.md` is UEFI én de config voor alle boards |
| `docs/flip.md`, `gui.md`, `media.md`, `apps.md`, `go-apps.md` | De vlakken |
| `docs/heap.md`, `stacktask.md`, `storage-sync.md`, `ipv6.md` | Losse ontwerpen |
| `docs/measurements.md` | Metingen per board naast v2 |
| `docs/review-*.md`, `sequence-hopos.md` | Momentopnames van reviews; de volgorde in de tijd met per stap de afweging (24-10) |
| `ALLES.md` | De lijst van wat er nog moet |
| `jobs/README.md` | Jobspecs en het releaseschema |

## 7. Wat nergens in deze repo staat

Hop zelf (`agentd-hopos` in de hop-repo), de `hop`-CLI met `hop image`,
leannet, leanhttp en de andere lean-crates, de Go-SDK `metal/v2`, en de
kopie van applib in Stulp (`stulp/vendor/applib`).

## 8. Opgevallen bij het schrijven

Wat hieronder staat kwam boven toen de lagen naast elkaar lagen. Het is per
thema gegroepeerd, niet per crate, omdat de meeste dubbelingen over een
laaggrens lopen. **H** = beide kanten in de code gelezen, **M** =
waarschijnlijk, **V** = vermoeden. Regelaantallen zijn schattingen.

Nagelopen op 24-10: op 8.7 en 8.8 na is elk thema op 04-10 aangepakt, met
de commit bij de kop. Wat opgelost is, staat in de vorm van nu; wat bleef,
staat erbij als open of bewust.

### 8.1 Mogelijke fouten (eerst kijken)

Nagelopen op 04-10; wat een fout was, is gerepareerd.

- **O6N: `hopos.oscore=small` viel terug.** `BOARD.os_core()` kwam via
  `Deref` bij `Uefi::os_core` met de MADT-klassen (op de Cix allemaal 0,
  dus alles big). Nu kiest `board_uefi::On::os_core` (de O6N en de Altra
  zijn sinds a0eaeb0 `On<Cix>` en `On<Ampere>`) via `Uefi::os_core_by` met
  de klassen van het board; `small` is core 2 (MPIDR
  0x000), `big` core 0. Hosttoets in `board/o6n/src/class.rs`; op ijzer nog
  niet gezien.
- **De kern na `move_to_os_core` draaide op 64 KiB zonder wachtpagina.**
  Gemeten: 122 KB op virt, 139 KB onder EDK2. `cpu::smp::NODE_STACK` is nu
  256 KiB zoals de boot-stack, en de stackmeter meet de stack waarop de kern
  draait (hij wiste tot dan van de boot-stack tot aan de nieuwe sp). Een
  wachtpagina heeft die stack nog niet.
- **Hop op twee manieren gebouwd.** Opgelost: alle image-scripts gebruiken
  `tools/hop-build.sh`.
- **`gem::transmit` spint niet meer**: sinds bca6c70 zet hij de descriptor,
  schrijft TSTART en keert terug (`tx_restart`).
- **stmmac had een eigen `poll_until`**: nu `dev::poll_until`.
- **`claim` en `filter_map(Core::new)`** (`kern/src/slots.rs`): kan niets
  raken. Zonder groep ligt de run helemaal in de app-cores, de OS-core komt
  alleen met een groep.
- **De grenzen van de flip-handoff**: één set in `kern::kernflip`
  (`MAX_FLIP_*`, de volumes uit `abi`), voor de export en voor `decode`.

### 8.2 Hetzelfde begrip, meer definities

Opgelost op 04-10 (c6e0bf8, "abi als enige waarheid"), op wat hieronder
als open of bewust staat na.

- `Region`: `abi::Region` in kern en `fw::fdt`; `board::Region` blijft
  (bewust: zijn basis is een getypte `Pa`). `Slot` één keer
  (`abi::layout::Slot`, zestien bits, ook in de kern). `Core` nog twee
  keer met twee betekenissen (fysiek in abi, logisch in kern); de trait in
  heap heet `CoreId`. H.
- `Full<T>` één keer: `bounded::Full`. H.
- `Pool`: `sync::Pool` heet `Futures`, `kern::pool::CorePool` heet
  `Places`; `abi::layout::Pool` is de enige pool. H.
- **Het adresplan** één keer, in `abi::layout` (`slot_ip4`, `slot_mac`,
  `port_ip4`, `port_mac`, `ip4_port`); `net::plan` en `applib::net` geven
  het door, en de inverse in `kern::system::slot_from_remote` en
  `net::gw::from_host` is `ip4_port`. H.
- **De NAT-staat voor de flip** één keer: `abi::NatState<F>` met
  `abi::MAX_FLOWS`, naast `abi::FlowState`. `NatSnapshot` en de omzetting
  in hopos zijn weg. H.
- **`Timer`**: één `sync::Timer` voor kern, xhci en dwc3, één
  `executor::ExecTimer` voor hopos en applib. Bewust gebleven:
  `applib::sys::Timer` (een slaaptrait die Hop zelf implementeert) en
  `Conn` twee keer (server en client). Open: `blkdev::Pace` en `dvfs::Host`
  als klok-en-slaap, en de klokparameter heet nog `clock` of `now` per
  driver. H.
- **Codec-types**: applib gebruikt die van `abi::hopabi::codec` (de
  eventsoort is `abi::hopabi::codec::Kind`, de `EVENT_`-constanten zijn
  weg); `driver/codec` houdt zijn eigen contract. H.
- **Het glas** en het invoercontract (JSON-regels, poort 7879,
  `FB_*`-sleutels, de pixelregel) staan één keer, in `abi::glass`; fbgrant,
  usbin, `driver_fb` en `applib::fb` gebruiken hem. H.
- De kleine botsingen zijn weg: `MAX_CHUNK` staat niet meer in abi
  (applib's is `MAX_IO_CHUNK`, 1 MiB), de ringsoort heet `RING_LOG`, de
  traits heten `CacheMaint` en `GlassConsole`, en de temperatuur van een
  board heet overal `temp_milli_c`. H.

### 8.3 Het contract buiten abi

Opgelost op 04-10 (c6e0bf8).

- **De draadvorm van de system-API staat één keer**, in abi: de kern leest
  en schrijft de kop met de abi-encoder en decodeert calls met
  `abi::hopabi::decode_req` (`Call` en `put_resp_head` zijn weg), en
  `applib/src/sys.rs` codeert niets meer zelf (`encode_req_head`). H.
- **`abi` draagt ook kern-interne dingen**: `sha256`, `checksum` en
  `FlowState` hebben geen app-gebruiker. Bewust, en zo in de moduledoc. H.

### 8.4 Het Board-contract is grotendeels impliciet

Opgelost op 04-10 (a0eaeb0, "Het Board-contract expliciet").

- Het contract is expliciet: `Board: Sync + Watchdog + Thermal +
  ClockKnob`, met `probe_disk` en type `Disk`, `this_core`, `os_core`,
  `os_bell`, `kick_self`, `config`, `bootargs`, `boot_param`, de
  flip-haken en constanten als `FLAVOR`, `PSCI` en `HOP_MEM` (§5.4).
  Alle tien borden implementeren de vier traits. H.
- Het **`cfg`-bos in hopos** is weg uit `watchdog.rs`, `telemetry.rs`,
  `bench.rs`, `net.rs`, `cage.rs`, `kooi.rs` en `slots.rs`; in `flip.rs`
  staan er nog twee voor de UEFI-borden, de rest per architectuur, en
  elk board levert zijn `BLACK_BOX`. Wat blijft is `cfg` op de
  architectuur (riscv64) en op features (`vhe`, `gui`). H.
- **O6N en Altra** zijn `On<Cix>` en `On<Ampere>` op
  `board_uefi::On<P: Platform>`: de delegatie, de `Deref` naar `Uefi` en de
  dubbele `probe_disk` zijn weg. H.
- **De DW-WDT** is één driver (`driver/dwwdt`); de Radxa houdt zijn
  klokpoorten, de LicheeRV zijn RTC-routing. Open: de andere
  watchdog-drivers staan nog in board/ (`board/*/src/watchdog.rs`,
  `wdt.rs`). H.
- De O6N-governor is gelijk aan de rest: de boot-flank synchroon vóór het
  net (`HOPOS_CLOCK_EDGE`), de klok vol vóór een flip-sprong. H.

### 8.5 Interrupts

Opgelost op 04-10 (8e8cea4, "Interrupts via één Controller").

- **gicv3 implementeert `cpu::irq::Controller`**, net als gicv2, aic en de
  PLIC, en elk board gebruikt de `Dispatcher`: de eigen claim/eoi-lussen
  van qemuvirt, rk3566, uefi, licheerv en qemuvirt-riscv zijn weg, en
  `STRAY_LIMIT` geldt overal. De lijntabel van uefi is weg (LPI's via de
  ITS, de rest via de GIC, in een kleine `Irqchip`-wrapper). H.
- De distributor staat één keer, als `driver_gicv3::dist`; gicv2 gebruikt
  hem. H.
- Dood en weg: `riscv::trap::IRQ_FLAG`, `take_irq`, `irq::run`, `report`
  en `wait`. H.

### 8.6 Drivers: hetzelfde skelet met de hand

Grotendeels opgelost op 04-10 (67e5a3a, "De drivers").

- **Het NIC-skelet**: `netdev::IrqAck` en `AckSlot` (igb, rtl8126, tg3 en
  stmmac implementeren de trait; de `static NIC_ACK` van altra, o6n,
  apple, rk3566 en licheerv is een `AckSlot`). Bewust niet samengevoegd:
  de DMA-indeling en de RX-kopie per chip. H.
- **Link en PHY**: open; `mdio::wait_link` is alleen een voorstel (quirks
  per chip). M.
- **virtionet en virtioblk** delen via virtiopci `negotiate`,
  `driver_ok`, `publish`, `IrqAck` en de opzetfouten. H.
- **brcmpcie implementeert `driver_pcie::Config`**; rpi5 gebruikt `probe`,
  `msix` en `msix_table_addr`. H.
- **rkrng en rng200** gebruiken `dev::Mmio` in plaats van hun eigen
  `Regs`/`Mmio`. H.
- **Wachten op de klok** in brcmpcie, de ITS, de virtio-reset en de MDIO
  van genet. Open: de MDIO van gem (`MDIO_POLLS`) en de redistributor-wek
  van gicv3 (`WAKER_POLLS`) tellen nog. H.
- **De tellers** gaan via `netdev::Stats` en `Device::stats` naar de tik
  (`nic(bad= full= bells=)`); virtioblk meldt `commands=` en
  `slowest_us=` in `HOPOS_DISK_STATS`. H.
- **Eén bump-allocator**, `dev::Bump`, voor virtionet, xhci, rtkit en
  `pcie::MmioWindow`. H.
- **Eén consolebeleid**, `dev::Stall` (herstellen), voor pl011, ns16550 en
  de M4. H.
- **Het rearm-moment** ligt vast: de lijn gaat weer open in `receive` als
  die `None` geeft. H.
- De PCCT staat in `fw::acpi::pcct_subspace`, de AML van de O6N-klok in
  `fw::aml::cpc`. H.
- Niet alles onder `driver/` is een driver: `codec` is een contract (zoals
  `netdev`, `blkdev` in de root), `dvfs` beleid. Andersom zijn `gui/rkscan`
  en `media/mve` wel drivers. Smaak.

### 8.7 arm64 en riscv naast elkaar

- `ArmSleeper::resident` en `RvSleeper::resident` zijn bijna kopieën, het
  `sleep`-skelet ook. Het fault-rapport van de OS-core (`settle`) ook, met
  kleine semantische verschillen (kick in `settle` alleen op arm). H.
- De uit-stub voor de koude flip twee keer met een andere signatuur. H.
- riscv heeft een eigen FNV (`hopos/src/cage_riscv.rs:443`) naast
  `abi::checksum::Fnv64`. H.
- De kooi bouwen: op arm in `cpu::el2::stage2`, op riscv in
  `hopos/src/cage_riscv.rs` en nog een keer in de zelftest van
  qemuvirt-riscv. H.
- `cpu::el2` bevat arch-neutrale delen die riscv gebruikt (roster, de
  rotatieregel, `OS_STATS`, `chain`). "el2" is daar een misnomer. H.

### 8.8 arm64 assembly

- Het EL2-regime voor bewoners staat in de switcher én in
  `oscore::arch::prepare`, met dezelfde letterlijke waarden; de
  EL1-restore (19 woorden) ook. H.
- "MMU en caches uit" op vijf plekken; drie boot-stubs (cpu, uefi, apple)
  met elk eigen MAIR/TCR/SCTLR, en de nVHE-tak van uefi met letterlijke
  waarden die ook in `el2.rs` en `boot.rs` staan. H. Deels moet het blob
  blijven.
- Eén flip-ingang sinds 01-11 (O6): x0 = de feitenpagina op elk
  arm64-board (`docs/flip.md`). Gedaan.

### 8.9 Config en firmware-feiten

Nagelopen op 04-10; wat dubbel was, is één keer.

- **"Eerst de bootargs, dan het bestand, de laatste wint"** staat één keer:
  `fw::bootcfg::layered` en `param`. `Board::boot_param` gebruikt hem,
  `kern::nodecfg::NodeCfg` ook (drie lagen: de bank van QEMU, de bootargs,
  het bestand); `cfgwin::param` en `nodecfg::text` zijn weg.
- **Eén weg naar een sleutel**: `Board::config`, `Board::bootargs` en
  `Board::boot_param`, voor de kern én het board zelf. Weg: de vrije
  `boot_param` van de Pi's, de Radxa en de LicheeRV, `cfg_text` en
  `bootargs` van de Radxa, de eigen lezers in de UEFI-stub en op Apple,
  en `bench::bootparam`/`cfg_text` (die bij elke aanroep een `String`
  bouwde). Apple las `hopos.cages` uit 0xF000 en niet uit het venster; nu
  via het contract. `NodeCfg` wordt één keer gemaakt (main.rs) en gaat als
  waarde naar de slots.
- **Eén bewaarde DTB per boot**: `board::dtb::Dtb` (vinden, wegen, bewaren)
  voor virt, riscv-virt en de Pi's; de Radxa bewaart zijn heap-kopie erin,
  en `board::dtb::Kept` vervangt zijn `copied`/`publish`.
- **Het config-venster** blijft op drie plekken, met elk een eigen taak:
  `board/src/cfgwin.rs` leest het in de kern, `image/hopcfg.py` vult het bij
  het bouwen (ook `image/apple/load.py` en `tools/qemu-test.sh` gebruiken
  hem), `hop image` schrijft een kaart of stick van een operator.
  `hopcfg.py` vervangen door `hop image` vraagt een gebouwde hop-CLI op de
  bouwmachine bij elk image-script en elke QEMU-toets, een afhankelijkheid
  tussen de repo's buiten de tags om, en `hop image` kent geen lagen.
- **Geen gedeeld feitentype, bewust**: het gedeelde type van een
  framebuffer is `driver_fb::Desc` (elk board zet zijn bron erin om);
  `fdt::Fb` en `xnuboot::Fb` zijn de lezingen van twee formaten, en die van
  xnuboot komt alleen in een bootregel. De GIC uit de MADT (versie, alle
  GICR-bereiken) is een ander feit dan `fdt::GicV3`.

### 8.10 Verzoek en antwoord zonder bouwsteen

Opgelost op 04-10 (110b945): `sync::Oneshot<T>` (het nieuwste antwoord
wint) met `oneshot::call` (legen, zenden, wachten). `slots::Reply` draagt
er twee, `switch::Ack` en `NatReply` zijn aliassen, en de helpers
`slots::call`, `rpc::call`, `rpc::freeze`, `rpc::kern_read` en
`deviceabi::call` gaan erdoorheen; `sync::Doors` leunt erop (zie 8.12).
Open: de `done[i]` van store is nog een `Signal` per plek. H.

### 8.11 Twee paden waar één bedoeld is

Opgelost op 04-10 (110b945, "Oneshot en één pad"), op de DRBG's na.

- **blkdev** kent alleen tickets (`start_tag`, `poll_tag`, `reap`), en
  `Queue` is de enige voorkant: `Paced`, `InFlight`, `start`/`poll_done`
  en het single-pad van nvme zijn weg, en de meetbank meet door de
  `Queue`. Het GPT-leespad van nvme blijft (de probe heeft het
  NVMe-foutdetail nodig). H.
- **hopfs** heeft één I/O-pad: `Fs::read_at` en verwanten zijn weg, de
  tests en de meetbank gebruiken `hopfs::path`. H.
- **Logregels van een app** gaan via de outbox; `LOGNET` is weg, en
  `Kind::Log` over de system-API blijft voor de Go-SDK. H.
- **Hop's poorten** 8080 en 9080 staan in zijn `StartSpec` en gaan zoals
  die van een jobspec via `Cage::publish` (tcp en udp), `Resident.ports`,
  de flip en `republish`; de twee lussen rond `net::publish` zijn weg. H.
- **Switch-beheer**: de lifecycle roept bij elke stop `Cage::detach`; de
  FIXME is weg. H.
- **Twee DRBG's** (Hash-DRBG in de kern, ChaCha20 in applib) en twee
  jitter-oogsters. Bewust gescheiden; wel twee keer review. Laag.
- De `memcpy`/`memcmp`/`bcmp`-shims zijn één macro, `dev::mem_symbols!`,
  voor applib en hopos. H.

### 8.12 Pollen waar een gebeurtenis hoort

Nagelopen op 04-10; wat hieronder als gedaan staat, wacht nu op een bel.

- **De acceptor van welcome, bench en vitals** en de pool van de
  system-API in de kern zijn één bouwsteen: `sync::Doors` (per werker een
  deur met een bel, en een bel voor de acceptor die een werker luidt als
  hij vrijkomt). Met alle werkers bezet wacht `Doors::place` op die bel,
  niet op de klok; de kern weigert nog steeds meteen (`Doors::hand`).
  Welcome en vitals serveren sinds 06-11 via `applibhttp`, dat dezelfde
  `Doors` gebruikt plus een park voor stille keep-alive-verbindingen.
- **decode** vraagt de codec met een wachtende poll: `OP_CODEC_POLL` met
  `n` = de wacht in ms (de kern klemt op een seconde). De kern houdt het
  antwoord tot het eerste event en pompt de engine zelf elke milliseconde;
  de app slaapt op het antwoord. Open: de VPU-interrupt (intid 358 op de
  O6N) als bel in plaats van die pomp, en dat vraagt ijzer. V.
- De committer wacht op de bel van de servicer-tabel
  (`Servicers::changed`) en op `COMMIT_EVERY`; `flip::restore_nat` wacht
  op de adoptie (`ADOPTED` is een `Stop`); het slot van een start kiest de
  lifecycle in de claim zelf.
- **Een stille secundaire core** slaapt in WFE tot zijn eigen deadline: de
  vlaggen (stop, delen, yield) ziet hij na elke WFE. In de yield (QEMU,
  Apple, een gedeelde core) blijft de vangrail van 10 ms, want daar maakt
  alleen zijn wektijd of een HVC #4 van een sibling een context due, niet
  de kick van de kern bij een stop of de SEV van een waker. Open: de kern
  zet bij een stop de wek-latch van de hele keten; dan kan de vangrail ook
  in de yield weg. `CTRL_WAKES` telt de rondes van alle cores.

### 8.13 Apps

Opgelost op 04-10 (249963d, "De apps"), op de keuzes na.

- `bounded::Text<N>` is de `fmt::Write` op een vaste buffer (afgekapt op
  een hele UTF-8-letter) voor bench, vitals, display en usbin; welcome en
  vitals delen `applib::text::Room`. H.
- `applib::app::port_of` één keer, `TcpStream::read_exact` naast
  `write_all`, `applib::park()` voor het blijven staan,
  `applib::text::json_str`. H.
- De burn-werklast en het idle-percentage één keer (`applib::burn`,
  `applib::clock::idle_pct`). H.
- syncprobe (57 regels) kan een rol in appspike worden; replica gebruikt
  hem, dus dat kost afstemming. Keuze, een voorstel voor Derek.
- cloudflared-lean (Rust, 5,4k, sinds 19-10 in de Hop-repo) naast
  go/cloudflared (upstream-CLI, de Go-SDK, en ABI 1 die vast moet
  blijven). Keuze.
- `sys::Client::device_command` en `Net::resolve_via` hadden geen
  gebruiker en zijn weg. H.
- Stulp heeft een eigen kopie van applib die 1693 diff-regels afwijkt (de
  stand van 04-10). Voorstel: de overlay laten vallen en een tag vanaf
  3.0.9 pinnen. H.

### 8.14 Scripts

Grotendeels opgelost op 04-10 (d4d8700, "De scripts").

- **De QEMU-tests** staan op `tools/lib.sh` (opruimen, poorten, strip,
  artifact-server, hop-build, wachtlus, POST, rapport, en `qemu_virt`,
  `qemu_edk2` en `qemu_rv` één keer); `qemu-riscv-test-hop.sh` roept
  `qemu-test-hop.sh` met `HOP_ARCH=riscv64`. De ringen gingen van 5122 naar
  3734 regels. H.
- **De image-scripts**: `image/raspi.sh` voor de Pi 4 en 5,
  `image/elf.py` als de ene ELF-lezer met de RELATIVE-toets, `pick_app`
  en `need_objcopy` één keer, en `mkcard -verify` leest elke kaart en
  stick terug (de terugleesproef van de Radxa is weg). H.
- Inbakken heet overal `HOPOS_EMBED`. H.
- **Lijsten**: de apps één keer als `APPS` en `APPS_RISCV64` in
  `tools/lib.sh` (gate, release, de notes en jobs/README lezen die),
  `STAGE_MAX` één keer. Open: de koude linkadressen uit een
  linkersymbool (raakt het flip-pad), en de drie node-configs. H.
- De runner is `tools/qemu-all.sh`: alle ringen na elkaar, per ring groen
  of ROOD met duur en laatste regel, en één eindregel. H.

### 8.15 Docs

Opgelost op 04-10 (f1917f0), op wat als open staat na.

- Bouwen per board staat op één plek, de tests in één tabel in
  `docs/README.md` (met de twee riscv-toetsen), en de root-README noemt
  alle mappen en boards. H.
- `docs/boards.md` heet ook config ("de config van elke node, en de
  UEFI-machines"). H.
- Bijgewerkt: de media-config in v3-vorm, cloudflared-lean naar de
  Rust-ELF, `go/cloudflared/README.md` (gVisor alleen als toets dat het
  er niet in zit), `image/apple/console.py`, en het commentaar in
  `hopos/src/main.rs` over de heap. Open: `docs/measurements.md` zegt nog
  "Zero 3W" (de kop van de meting van 04-10). H.
- `heap.md`, `stacktask.md`, `storage-sync.md`, deze omschrijving en de
  reviews zijn gelinkt vanuit `docs/README.md`; de reviews zonder
  regelnummers. H.
- De executor zet nog elke taak in een box, maar een taak die geen plaats
  krijgt, is luid: `Stats::dropped` en `HOPOS_TASK_DROPPED` op de tik.
  Open: de handboekregel "statische taken" zelf (in `rustdoc/README.md`,
  buiten deze repo). H.

### 8.16 Dood of zonder gebruiker

Opgelost op 04-10 (f1917f0, en `irq::run`/`report`/`wait` in 8e8cea4):
het veld `Handoff.agent` is weg (het woord blijft in het blob, lengte 0,
`HAND_VERSION` 6), net als `CommitWhy::Flip`, `FsActor::peak` en
`::generation`, de vrije functie `codecabi::serve`, de riscv-`jitter`,
`board/licheerv/src/lottery.rs` en de feature `window-b000`.
`FsActor::handle` en de andere test-ingangen staan alleen nog onder
`cfg(test)`. De `sync::Stop` die nooit afging (pomp, switch, host-taak,
flow-expiry) is weg; `Stop` zelf is de stopbel van een app geworden
(`App::stop`, §4.4) en de bel van de adoptie (`flip::ADOPTED`). H.
