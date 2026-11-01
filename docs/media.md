# Het media-vlak: de videocodec van de O6N

De hardwaredecoder van de Orion O6N (Arm China Linlon V8, "mve") en de
codec-dienst waarmee een app een stream door de decoder haalt, in v3.

Alles zit achter de feature `media` (op `hopos`, `kern`, `applib`, `abi`
en `board-o6n`). Kaal kent de kern de codec-ops niet en antwoordt hij
"onbekende op". `media` trekt `gui` niet mee: de optische drive loopt over
de bouwsteen `usb` (zonder HID en zonder de ronde van 4 ms), en een
media-node is headless zoals lumen (docs/sequence-hopos.md §14.3).

## De vorm

| Crate / bestand | Wat het bezit |
| --- | --- |
| `driver/codec` (`driver-codec`) | Het contract: `Engine::open(&Config) -> Session`, `feed`, `offer`, `next_event`, `close`, `reap`; `Buffer { pa, size }`; `Event` met `Kind::{Consumed, Produced, Format, Done, Fault}`; de `Firmware`-bron. Een `Session` is een handvat zonder `Clone`; wie hem laat vallen, legt zijn nummer in het `Graveyard` van de engine en de engine sluit de sessie bij zijn volgende beurt. |
| `media/mve` (`media-mve`) | De Linlon V8: registers (getypeerd, `const`-asserties op elke offset), de arena (bitmap, first-fit, uitlijning), de page tables van de codec-MMU, het laden van de firmware, de ringen met de lopende checksum, de pomp (RPC-geheugen, berichten, buffers, de output-flush-handdruk, EOS op het laatste beeld), de vaste plekken per buffer. Eén `Device` per VPU, van één taak; geen slot. |
| `kern/src/codecabi.rs` | De system-functies (open, feed, offer, poll, close), de sessietabel per levensduur van een slot, `codec_grant` met de drie toetsen, en het cache-onderhoud (de VPU is niet coherent). |
| `abi/src/hopabi.rs`, `hopabi::codec` | De payloads op de draad: `OpenArgs` (8), `FeedArgs` (24), `BufArgs` (4), `Event` (64 bytes), byte voor byte die van v2. |
| `applib/src/codec.rs` | De client: `Session::{open, feed, offer, poll, close}` over `sys::Client::call_once` (nooit herhaald: twee keer dezelfde feed is twee happen bitstream). `poll` wacht: ligt er niets, dan houdt de kern het antwoord vast tot het eerste event (hoogstens `wait` en een seconde) en pompt hij de engine elke milliseconde; de app slaapt op het antwoord. |
| `board/o6n/src/codec.rs` | De VPU aanzetten: vensters, interrupt en `_CCA` uit de DSDT (`scan_dsdt`), arena ongecached, stroomdomeinen via SCMI naar de TF-A, klokken en perf-domein via de SCP, reset, en de stroomcyclus als het blok vastzit (`needs_recovery`, `cycle_domains`). |
| `media/optical` (`media-optical`) | De bulk-only-transportlaag van een optische drive over een `Transport`-trait (zie hieronder). |
| `hopos/src/codec.rs` | `codec::up` ná de opslag: de dienst aanmelden bij de system-API en de bring-up als taak: de arena van de lifecycle-actor, de firmware van de hopfs-actor, op de O6N de VPU aan en de driver erin en de opruimtaak die eens per seconde de sessies van gestopte bewoners sluit. |
| `kern/src/slots.rs`, `kern/src/partmem.rs` | De arena buiten de partitie-pool: `Request::ReserveDevice` en `ReleaseDevice` aan de lifecycle-actor (de eigenaar van de pool), `PartitionPool::reserve_device` eronder. Eén regel `HOPOS_POOL_DEVICE`. |
| `kern/src/rpc.rs` | De kern als lezer van hopfs: `FsMsg::KernRead`, `kern_read` (in stukken, de buffers heen en terug) en `read_file` (een heel bestand, begrensd). Zonder slot en generatie; de roots van de taken zijn ook voor de kern dicht. |
| `apps/decode` | De kleinste app op de codec-dienst: een stream van het volume of een URL door `applib::codec`, en de fps (`HOPOS_DECODE`). |

### Grote data loopt niet door de kern

Een 4K-beeld in P010 is 3840 x 2160 x 1,5 x 2 = 24.883.200 bytes; bij 24 fps
is dat 597 MB/s. Het slot-LAN piekt op 550 MB/s. De beelden kunnen dus niet
over de system-calls, en ze horen er ook niet: een buffer is een stuk van de
eigen partitie van de app (`off`, `n` vanaf `RamStart`, hele pagina's). De
kern toetst het (`codec_grant`: binnen de partitie, geen omloop,
pagina-precies), de driver hangt het in de page tables van de codec, en dat
is de grant én de isolatie: de VPU kan per constructie niets aanraken wat
niet van deze taak is.

Het cache-onderhoud doet de kern: `feed` schrijft uit (`dc cvac`), `offer`
schrijft uit en gooit weg (`dc civac`), een gevuld resultaat wordt weggegooid
vóór de app het leest, een teruggegeven invoer kost niets.

### Levensduur en evict

De sessietabel hoort bij één levensduur van een slot (de generatie van de
servicer). Elke call draagt de generatie waarvoor zijn verbinding werd
toegelaten; een tabel van een andere generatie geeft `None` (Revocable). Een
verzoek van de vorige huurder landt zo nooit in de tabel van de opvolger. Bij
evict valt de tabel, vallen de handvatten, en sluit de engine de sessies:
`Drop` is de vrijgave, er is geen `ReleaseCodecs` meer.

## Tests (v2 naar v3)

| v2 | v3 | Toetsen |
| --- | --- | --- |
| `mve/session_test.go` (975 regels, 15 toetsen) | `media/mve/src/tests.rs` | alle 15 (37 toetsen in het crate), met dezelfde nep-VPU (een echte page walk door het geheugen, een nep-firmware die de checksum narekent), plus: een gevallen handvat sluit de sessie, een vreemd handvat is dicht, een vast slot is een fout zonder lek, describe/state dragen de getallen |
| `mve/mve_test.go` (11) | idem | alle 11 (arena, MMU, firmwarekop) |
| `mve/queue_test.go` (7) | idem | alle 7 (het ringprotocol) |
| `kern/slots/codecabi_test.go` (5) | `kern/src/codecabi/tests.rs` | alle 5 (de race van de onderbroken Open is in Rust ondeelbaar; de toets bewijst nu de weigering vóór het ijzer), plus het cache-onderhoud per richting, de weigering buiten de partitie en de weigering zonder ijzer |
| `board/o6n/hop/vpu_recovery_test.go` (3) | `board/o6n/src/codec.rs` | alle 3, plus de DSDT-lezer |
| `abi/hopabi/codec.go` | `abi/src/hopabi.rs` | de offsets van `EncodeEvent` byte voor byte, de roundtrips |
| `optical/optical_test.go` (19) | `media/optical/src/tests.rs` | de BOT-toetsen: de CBW van de spec, grenzen en richtingen, het residu, een gefaalde datafase die niet verstopt wordt, plus een verloren spoor (tag, phase error, CBW) dat reset, de herkansing op de status en beide sense-formaten; de MMC-toetsen volgen met de MMC-laag |
| (nieuw) | `driver/codec`, `applib/src/codec.rs` | nummering gelijk aan v2, het graveyard; de client spreekt de draad van de kern en herhaalt nooit; de codec uit de extensie |
| `slots/partmem_test.go` (`ReserveDevice`) | `kern/src/slots.rs` `a_device_block_leaves_the_pool_and_comes_back` | het blok via de actor: op de korrel, van de capaciteit af, nooit in een partitie, een weigering met de getallen, en terug |
| (nieuw) | `kern/src/rpc/tests.rs` `the_kern_reads_a_firmware_blob_without_a_slot` | de kern-lezing op de nep-hopfs: een blob van 300 KB heel door brievenbus en actor, te groot zonder allocatie, de naam die mist als `NoEnt`, in stukken met dezelfde buffers, de roots van de taken dicht, bevroren is `Busy` |
| (nieuw) | `apps/decode` | buffers op hele pagina's en terug te vinden op hun afstand, de fps en MB/s van de meting, de codec uit de naam van de stream |

## De drie haken

Alle drie zijn er (29-09). De bring-up in `hopos/src/codec.rs` loopt ze na
elkaar, elk als bericht aan zijn eigenaar:

1. **De partitie van een levende bewoner** (`kern/src/slots.rs`):
   `Servicers::partition(slot) -> Option<Region>` geeft de basis en maat uit
   `ServicerCtl` zolang de servicer leeft, en `hopos::codec::NodeLives`
   gebruikt hem; na de stop weigert elke grant met `STATUS_DENIED`.
2. **Een arena buiten de partitie-pool**:
   `Request::ReserveDevice` aan de lifecycle-actor, één keer bij boot, van
   `hopos.codec` MB (standaard 768 op de O6N; 0 is uit, `HOPOS_CODEC_OFF`).
   De actor zegt waar het blok ligt en wat er voor de slots overblijft
   (`HOPOS_POOL_DEVICE base=… mb=…`, of `_FAIL` met de getallen). De actor
   handelt het af ná een eventuele adoptie (kern-flip) en vóór de plaatsing
   van Hop. Komt het ijzer niet op, dan gaat het blok terug
   (`HOPOS_POOL_DEVICE_RELEASE`). Op een board zonder VPU is de standaard
   64 MB, en gaat hij na `HOPOS_CODEC_NONE` meteen terug: zo bewijst QEMU
   de weg.
3. **Een kern-lezing van hopfs** (`kern/src/rpc.rs`): de hopfs-actor leest
   voor de kern zelf, ná de mount en vóór de VPU aangaat, de zestien blobs
   (`media_mve::FIRMWARE`, elf decoders en vijf encoders van zo'n 300 KB,
   door Lumen van Sky1-Linux/sky1-firmware gehaald) als
   `/firmware/<naam>.fwb` of
   `/codec-firmware/<naam>.fwb` (waar Lumen ze neerzet: zijn jobspec mount
   `/firmware` op dat volume, `jobs/hopos-media-o6n.cfg`). Eén regel
   met het aantal; wat mist staat bij naam in `HOPOS_CODEC_NOFW
   missing=hevcdec,…`, en een open van die codec weigert met "no firmware
   for this codec". Zonder volume: `HOPOS_CODEC_NOFW missing=all`.

### De proef op QEMU

Een media-kern op QEMU virt (`--features board-qemuvirt,media`) met Hop in
slot 1 en apps/decode via Hop geplaatst, gemeten 29-09:

```
pool: 64 MB for the codec arena at 0xbc000000 outside the partition pool, 1712 MB left for slots (largest 1536 MB) HOPOS_POOL_DEVICE base=0xbc000000 mb=64
codec: no video codec on this board, codec calls refused HOPOS_CODEC_NONE
pool: 64 MB of the codec arena at 0xbc000000 back in the partition pool, 1776 MB for slots HOPOS_POOL_DEVICE_RELEASE
HOPOS_HOP_START slot=1 core=0 cpu=0 entry=0x50010000 part=0xbc000000+0x4000000 ...
slot 2: decode: the node has no hevc decoder for this app: system call op 14: status 1: this node has no codec hardware; staying up without one HOPOS_DECODE_NOCODEC
```

De reservering komt vóór de plaatsing van Hop, en Hop krijgt daarna precies
het blok dat terugkwam. Decode blijft staan (geen exit, geen herstart).

## De optische drive

De optische drive (`media/optical`) is
in code aangesloten: de bulk-only-transportlaag (CBW, datafase, CSW met de
toetsen op signature, tag, residu en status, reset-recovery, de ene
herkansing op een gestalde status, REQUEST SENSE) tegen een
async `Transport`-trait (`media/optical/src/asynchronous.rs`) met zeven
toetsen tegen een nep-drive, de MMC-laag erboven (`mmc.rs`: openen, de
maat, READ(10) over sectoren van 2048 bytes) en de device-command-ABI
(`OP_DEVICE_COMMAND`, `abi/src/hopabi/device.rs`). Een app bereikt een drive alleen via een
expliciete mount op `/devices/discN` (`kern/src/deviceabi.rs`); de
optical-owner (`hopos/src/optical.rs`, feature `media`) is een eigen actor
naast hopfs en leent zijn transfers één tegelijk aan de USB-eigenaar, die
dus nooit in de hopfs-actor wacht. De MMC-laag heeft nog geen eigen
hosttest en is niet op QEMU of ijzer gedraaid. De owner start ook wanneer
geen USB-controller beschikbaar is, zodat een discaanvraag dan `NoEnt` geeft
in plaats van te blijven wachten. BOT-reset heeft een eigen begrensde deadline;
een verlopen leesdeadline verhindert niet meteen de reset. De encoder blijft
in de aparte Lumen-app; de SDK biedt maximaal twaalf app-cores en 512 KiB
stack per daadwerkelijk gestarte secundaire core.

## Checklist: de O6N-mediatest

Bouwen: `MEDIA=1 BOARD=o6n CFG=jobs/hopos-media-o6n.cfg sh
image/uefi-run.sh` (de gate bouwt hetzelfde). De ESP staat in
`target/uefi-esp-o6n-media/`: `EFI/BOOT/BOOTAA64.EFI` en `hopos.cfg` op een
FAT32-stick. De config geldt (`hopos.codec=768`,
`hopos.storage=stateful`); voor de meting zonder Lumen erbij apps/decode
met een stream op `/data/clip.hevc` van het volume: `cargo build --release
--target aarch64-unknown-none-softfloat -p decode`, de jobspec in
`apps/decode/README.md`.

| Stap | Marker of regel | Wat het bewijst | Afwijking |
| --- | --- | --- | --- |
| 0a | `pool: 768 MB for the codec arena at 0x… outside the partition pool, N MB left for slots … HOPOS_POOL_DEVICE base=0x… mb=768` | de arena komt uit de pool, vóór de eerste plaatsing | `HOPOS_POOL_DEVICE_FAIL` met vrij en grootste gat: `hopos.codec` omlaag, of de pool is kleiner dan gedacht; "did not answer": de slots kwamen niet op |
| 0b | `codec: 16 of 16 firmware blobs read from the volume (… KB)` | de kern-lezing van hopfs: `/firmware` of `/codec-firmware` | `HOPOS_CODEC_NOFW missing=…`: die blobs staan niet op het volume (Lumen haalt ze vóór zijn eerste back-up; de eerstvolgende open laadt ze bij); `missing=all` zonder volume |
| 1 | `vpu: TF-A SCMI channel alive, power protocol vX.Y` | het SMC-kanaal naar de TF-A antwoordt | geen regel: het kanaal op 0x84380000 is niet gemapt of de SMC-functie klopt niet |
| 2 | `vpu: power domains 4 5 11-15 on (confirmed by the firmware)` | hub, top en vier cores aan, teruggevraagd | `power domain N not on`: de TF-A weigert; niet verder, de eerste registerlees zou een SError zijn |
| 3 | `vpu: mm ni700 clock on ...`, `vpu: vpu apb clock on ...` | de klokken van interconnect en blok | "no SCMI channel offers the clock protocol": registers lezen dan nul |
| 4 | `HOPOS_VPU_RECOVER` (alleen na een vastgelopen blok) | de stroomcyclus 15..11 uit, 11..15 aan | na de cyclus moet PGCTRL `0x07cefffc` zijn en TERMINATE nul (gemeten 27-09) |
| 5 | `vpu: id 0x56648002 rcsu ... (windows 0x14240000/0x14230000, intid 358, cca Some(false))` | het blok leeft; vensters, interrupt en `_CCA = 0` uit de DSDT | id 0: geen klok of hub; 0xffffffff: geen bus; `cca Some(true)`: de firmware noemt het blok coherent, de arena mag dan gecached |
| 6 | `codec: Linlon V8 (id 0x56648002 rev ...): 4 cores, N sessions, fuse ..., arena 768 MB (768 MB free) HOPOS_CODEC_UP` | de driver draait, de arena is heel | `HOPOS_CODEC_OFF` met de reden (stroom, probe), en dan `HOPOS_POOL_DEVICE_RELEASE`: de arena is terug in de pool |
| 6a | apps/decode via Hop (`apps/decode/README.md`): `slot N: decode: hevc session 1 open … HOPOS_DECODE_OPEN`, dan `… HOPOS_DECODE fps=… MBps=…` | dezelfde meting door de hele ABI: open, feed, offer, poll over de draad, de grant in de eigen partitie | `HOPOS_DECODE_NOCODEC` met de tekst van de kern ("no firmware for this codec", "all hardware sessions in use"); `HOPOS_DECODE_FAIL` met de reden |
| 7 | Lumen start een back-up | open, feed, offer, poll over de draad; de firmware vraagt zijn referentieframes (RPC) | een Fault met `firmware asked for N MB and arena ran out`: `hopos.codec` omhoog |
| 8 | fps in `/api/state` van Lumen | de meting: **24 fps 4K P010 = 24.883.200 bytes per beeld = 597 MB/s door de grant**, geen byte over de verbinding. v2 haalde 27,25 fps met Lumen op GAMEOFTHRONES_S1_D1 (27-09, met de software-encoder erachter) | lager dan 24 op 4K: eerst de cache-ops (`dc civac` over 24 MB per beeld), dan de pomp (de opruimtaak en de wachtende poll, die de kern elke milliseconde pompt) |
| 9 | stop Lumen midden in een film; start hem opnieuw | evict sluit de sessies binnen één seconde (de opruimtaak); de nieuwe levensduur opent op hetzelfde LSID | "all hardware sessions in use": een levensduur die niet viel |
| 10 | twee films achter elkaar | na de eerste is de arena weer heel (`describe` na close) | minder vrij dan totaal: een lek in RPC-geheugen of tabellen |

Een stille sessie (geen events, geen Fault) is de eerste vraag voor `state`:
`enable`, `jobqueue`, per LSID `sched`, `irqhost`, `mmu` en de tellers
(`flushes`, `flushback`, `eos`, `rpc[allocs]`, `bufs[offered back held]`).

## Firmware installeren na boot

De media-kandidaat leest bij `OP_CODEC_OPEN` een ontbrekende blob alsnog via
het `FsInbox` van dezelfde system-verbinding. Eerst `/firmware/<name>.fwb`,
dan `/codec-firmware/<name>.fwb` wanneer de eerste niet bestaat. De driver
levert de vaste naam; verzoeken mogen geen vrij bestandspad kiezen. De read
is begrensd op 4 MiB. Ontbrekende, lege, te grote of ongeldige blobs geven
meteen een fout aan de app, zonder hardware-sessie.

Het lezen gebeurt buiten de synchrone codeccel. De antwoordplek blijft van
de verbinding tot de filesystemactor antwoordt; er wordt geen pending read
opgegeven en daarna met dezelfde reply hergebruikt. Na de read valideert
MVE de firmware, neemt de cache hem in eigendom en toetst de gewone open de
slotgeneratie opnieuw. Een gestopte app krijgt zo geen nagekomen sessie.
Bestaande sessies hebben hun eigen firmwarekopie in de arena. Cachehits doen
geen filesystem-I/O.

De app blijft verantwoordelijk voor downloaden en inhoudspins vóór openen;
de kernel krijgt hier geen HTTP-client bij. Lumen doet dit voor alle zestien
firmwares via zijn bestaande installer. De system-wire-ABI verandert niet.

Hosttests gebruiken een echte HopFS-actor en toetsen beide mappen, een tweede
open zonder opslagread, missende/te grote bestanden en lifecycle-wisseling
terwijl de read loopt. De nagebootste MVE valideert een late firmware en
opent daarna zonder opnieuw proben. De O6N-kernel (`board-o6n,media`, PIE)
bouwt met strikte clippy. Dit is nog geen decoder-run op fysieke O6N-hardware.
