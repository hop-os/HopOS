# HopOS v3: de docs

Wat er is, hoe je het bouwt, en wat er per board te bewijzen valt. De
checklists per board zijn de lat voor een dag op het ijzer; elke stap noemt
de consoleregel die erbij hoort en wat een afwijking betekent.

De IPv6-slotbaan voor Matter/Thread staat in [ipv6.md](ipv6.md), inclusief
de SDK-API, de bronkopie en de resterende hardwareproef.

## De QEMU-toetsen (04-10-2026)

De enige lijst van alle toetsen; de kop van elk script zegt welke markers
hij eist en welke knoppen hij heeft. `sh image/qemu-run.sh` boot de kern op
virt zonder toets.

`sh tools/qemu-all.sh` draait alle ringen hieronder na elkaar (en de
riscv-share- en riscv-flip-ring), per ring groen of ROOD met zijn laatste
regel, en één eindregel; `sh tools/qemu-all.sh hop flip-cold` alleen die.
Elke ring kiest vrije host-poorten, dus twee runners botsen niet.

| Test | Wat hij bewijst |
| --- | --- |
| `sh tools/qemu-test.sh` | boot op virt, netwerk, opslag, appspike in slot 1 en 2 in een stage-2-kooi, tien toetsen; het zaad van de kern op de control-page (`HOPOS_RNG_SLOTS source=jitter`, en in de app `HOPOS_APP_RNG source=jitter`: virt heeft geen TRNG, [boards.md](boards.md)) |
| `GUI=1 sh tools/qemu-test.sh` | hetzelfde met de console op een `ramfb`: de bunny, mem, datum en tijd op het glas, geschoten via de QEMU-monitor |
| `GUI=display sh tools/qemu-test.sh` | de hele gui-keten: qemu-xhci met toetsenbord en muis (`HOPOS_USB_UP`), de display-app krijgt het glas in zijn kooi (`HOPOS_FB_GRANT`, `HOPOS_DISPLAY_UP`), verbindt met de input-listener (`HOPOS_INPUT_CONN`), een `sendkey` en een `mouse_move` komen aan (`HOPOS_DISPLAY_INPUT keys=1 moves=1`), de screendump toont de app, en na de stop komt de console terug (`HOPOS_FB_RELEASE`) |
| `sh tools/qemu-test-fault.sh` | de vectortabel van applib op EL1: appspike met `FAULT=1` leest net onder zijn RAM, en de kern drukt de echte vertaalfout (`HOPOS_APP_MMU`, dan `HOPOS_SLOT_FAULT` met vector 4 en `esr=0x96000006`); de alignment-val zelf bewijst alleen de Pi 5 |
| `sh tools/qemu-test-hop.sh` | de kern start Hop met het token op de OS-core; een `POST /v1/jobs` van de host laat Hop appspike plaatsen; na een koude herstart vindt hopfs zijn boom terug en begint Hop schoon |
| `SMP=2 sh tools/qemu-test-hop.sh` | hetzelfde met twee cores: kern plus Hop op één core, de app op de andere |
| `SMP=2 OSCORE=1 sh tools/qemu-test-hop.sh` | de kern verhuist bij boot naar core 1 |
| `sh tools/qemu-test-welcome.sh` | Hop plaatst welcome met `"ports":{"http":80}`; de kern zet poort 80 door naar het slot, `curl` van de host krijgt de pagina met de bunny en `/health`, en na `DELETE /v1/jobs/welcome` is de poort weer dicht |
| `GO_ELF=pad sh tools/qemu-test-welcome.sh` | hetzelfde met een Go-welcome (tamago) als artifact: een Go-image met slot-ABI 10 draait ongewijzigd op ABI 1 ([go-apps.md](go-apps.md)) |
| `sh tools/qemu-test-hop-restart.sh` | Hop wisselt onder welcome vandaan: `POST /restart` met hetzelfde en met een nieuw image, de staat via de kern (`HOPOS_HOP_STATE kept=N`), `HOP_ADOPTED n=1`, welcome blijft in slot 2 met zijn pagina en één plaatsing; een verkeerde som geeft 502 en Hop blijft |
| `sh tools/qemu-test-hop-fail.sh` | een falende Hop is een falende node: met de watchdog van virt (de i6300esb, die elke ring heeft) geeft `POST /restart` Hop een geldig maar kapot image (appspike); de kern laat het liggen (`HOPOS_HOP_EXIT`, `HOPOS_HOP_DOWN`) en valt (`HOPOS_FATAL`), de pets stoppen, QEMU reset, en de koude boot start de gestagede Hop schoon (geen `HOP_ADOPTED`); de tijd van de val tot `HOP_UP` |
| `sh tools/qemu-test-smp.sh` | een SMP-app: appspike met `cores: 2` krijgt zijn tweede core in de eigen kooi, telt erop, en houdt beide cores na een herstart door Hop |
| `sh tools/qemu-test-share.sh` | een sharegroep: twee appspikes delen één app-core in coöperatieve rotatie, allebei groen, en een lid dat stopt komt terug naast het levende lid |
| `sh tools/qemu-test-reclaim.sh` | core-reclaim: een bewoner die nooit yieldt (bench, BURN=1) houdt een gedeelde app-core vast; het nieuwe lid van zijn groep wacht 2 s, de kern offert de vasthouder (`HOPOS_CORE_RECLAIM`) en het nieuwe lid komt op |
| `sh tools/qemu-test-bench.sh` | de meetketen: Hop plaatst `apps/bench` met poort 80, `tools/netmeter` meet van de host (rtt, storm, in, out), twee apps meten elkaar door de switch, BURN draait, multicast in de node (een bench joint 224.0.0.251, een tweede zendt, de switch floodt, `HOPOS_BENCH_MCAST recv=3`), en de schijf met `BENCH=disk` (hopfs door de system-API, `HOPOS_BENCH_DISK`); de getallen staan in [measurements.md](measurements.md) |
| `sh tools/qemu-soak-hop.sh 30` | de soak van de hoplb-kring (Hop, welcome, hoplb met de hairpin door de switch, plus een poller op Hop): N runs achter elkaar, een waakhond op `HOPOS_TICK` die bij een tik die meer dan 3 s uitblijft `info registers -a` en de meetlat van de OS-core uit de monitor dumpt; ook met `SMP=2`, `SMP=8` en `OSCORE=1` |
| `sh tools/qemu-test-slowdisk.sh` | een trage schijf (`DISKLAT`, standaard 1,5 s per verzoek) zet de OS-core niet stil: tijdens een hopfs-commit antwoordt welcome binnen 100 ms en slaat de tik niets over |
| `sh tools/qemu-test-volumes.sh` | de volumes van een jobspec: een job met `"volumes":{"/data":"/volumes/demo"}` schrijft, hopfs commit, Hop herstart hem, en hij vindt zijn bestand terug (`HOPOS_SLOT_MOUNTS`) |
| `sh tools/qemu-test-store.sh` | de store-ops: appspike doet push, list, pull en drop op zijn map `apps/<cluster>/<job>/` in een S3-nep op de host, van app via kern naar Hop en terug (`HOPOS_APPSPIKE_STORE ok`) |
| `python3 tools/qemu-test-sync.py` | de bevestigde opslagbarrière (`OP_SYNC`, [storage-sync.md](storage-sync.md)): Hop plaatst `apps/syncprobe` met een eigen volume, die schrijft en synct een journal en 8192 databasebytes, haalt het journal weg met een barrière op de map, en QEMU krijgt SIGKILL; de koude boot van hetzelfde volume leest de bytes terug en mist het journal (`HOPOS_SYNC_WRITE`, `HOPOS_SYNC_READ`) |
| `sh tools/qemu-test-cluster.sh` (in de hop-repo) | een cluster van een QEMU-node en een host-agent met een hoplockserver als lock: beide nodes in `hop agents`, en na het hard doden van de host-leider wint de HopOS-node de lease en plaatst hij een job op de host |
| `sh tools/qemu-test-flip.sh` | de kern-flip: hopfs bevroren en gecommit, de NAT-flows gevangen, de sprong, Hop overleeft zonder herstart, en een uitgaande TCP-verbinding van een app (rol FLIPCONN) loopt door: drie antwoorden via kern A, drie via kern B, over één verbinding |
| `MISMATCH=1 sh tools/qemu-test-flip.sh` | een bundel met een andere switch-code wordt vóór de sprong geweigerd (Hop geeft 502) |
| `COLD=1 sh tools/qemu-test-flip.sh` | de koude flip (`"cold":true` op `POST /flip`): Hop stopt zijn taken, de kern zet de app-cores uit en springt zonder adoptie, Hop start koud en plaatst de job opnieuw |
| `OSCORE=1 sh tools/qemu-test-flip.sh` | de flip vanaf een verhuisde kern (ook met `COLD=1`, en met `BOARD=uefi OSCORE=2`) |
| `BOARD=rpi4 sh tools/qemu-test-flip.sh` | de flip-ingang van de Pi op raspi4b: een core met het merkteken komt tot de kern en leest de DTB opnieuw |
| `sh tools/qemu-uefi-test.sh` | de EFI-stub op EDK2, ACPI, PCIe, virtio over PCI met MSI-X via de ITS (`nic=` loopt op in de tik), de hele appspike-keten |
| `GUI=1 sh tools/qemu-uefi-test.sh` | hetzelfde met de GOP van EDK2 als console |
| `sh tools/qemu-test-nvme.sh` | de NVMe-kern van de O6N en de Altra: het Altra-image op EDK2 met `-device nvme` en igb, appspike in slot 1 en 2 schrijft, leest terug en leest een bundel (`OP_READ_MANY`) door driver/nvme, met de wachtrij op MSI-X via de ITS (`HOPOS_NVME_UP`, `HOPOS_NVME_IRQ`, `HOPOS_APPSPIKE_FS ok`) |
| `FEATURES=vhe CPU=neoverse-n1 sh tools/qemu-uefi-test.sh` | de VHE-switcher (E2H=1), de smaak die de O6N eist, op een VHE-model |
| `BOARD=uefi sh tools/qemu-test-flip.sh` | de kern-flip op EDK2: kern B landt op de PIE-basis van kern A (ook `COLD=1`, en onder VHE met `FEATURES=vhe CPU=neoverse-n1`) |
| `sh tools/qemu-rpi4-test.sh` | het Pi 4-board op QEMU's raspi4b tot de executor-tik |
| `sh tools/qemu-riscv-test.sh` | QEMU virt riscv64 in machine mode: boot, net, opslag, de zelftest van de PMP-kooi (met de kill-tick), en appspike twee keer door de lifecycle van de kern in een PMP-plus-Sv39-kooi |
| `sh tools/qemu-riscv-test-hop.sh` | Hop als bewoner op het hart van de kern op riscv64, een `POST /v1/jobs` plaatst appspike, en Hop komt na een herstart terug |
| `sh tools/qemu-riscv-test-share.sh` | twee bewoners op het ene app-hart (de vorm van de LicheeRV), één yieldt nooit (bench, BURN=1): de tijdschijf van de switcher houdt welcome bereikbaar, twintig GETs elk binnen `MAX_MS` |
| `sh tools/qemu-riscv-test-flip.sh` | de koude kern-flip op riscv64 met `hop flip --cold`: een warme flip wordt geweigerd, de koude landt (`HOPOS_FLIP_BOOT gen=2`), hart 1 komt uit de uit-stub terug, Hop start koud en plaatst de init-job opnieuw |

`sh tools/gate.sh` is de poort vóór elke commit: host-tests, clippy met
`-D warnings`, rustfmt, en de target-builds van alle boards (ook met `gui`
en `media`, en de riscv64-boards).

`cargo run -p loc` telt de regels zoals de compiler ze linkt: per board de
release-build van het image, uit de dep-info de bronbestanden, en dan de
vaste emmers (portable per laag, per ISA, per board, lean, gui)
met de toetsen onder `#[cfg(test)]` apart; `-v` noemt de files per emmer.

`python3 tools/meet.py` draait de meetronde van [measurements.md](measurements.md)
op het ijzer: vitals op alle borden tegelijk, bench in de node, de draad
serieel tegen de O6N (de O6N zelf tegen de M4), en daarna weer opgeruimd. Hij
wacht op de markers op de console in plaats van te slapen, schrijft per bord
een fragment in `target/meet/`, en zet met `--write` de cellen vooraan in
measurements.md. Zeven borden kosten ruim drie minuten.

## De vlakken

| Vlak | Doc | Wat erin staat |
| --- | --- | --- |
| Boards | [boards.md](boards.md), [boards-pi.md](boards-pi.md), [boards-radxa.md](boards-radxa.md), [boards-riscv.md](boards-riscv.md), [boards-apple.md](boards-apple.md) | per board het bouwen, de checklist met de verwachte regels, de interrupts, de watchdog, de klok en de thermiek; in boards.md ook de config van elke node en het config-venster |
| Kern-flip | [flip.md](flip.md) | de procedure per board, de markers van kern A en kern B, de faalmodi, de boot-guard, de koude weg |
| Gui | [gui.md](gui.md) | de console op het glas, de framebuffer-grant aan een display-app, de USB-invoer, de beeldketen van de Radxa |
| Media | [media.md](media.md) | de videocodec van de O6N (Linlon V8), de codec-dienst, de optische drive, de checklist en de meting (24 fps 4K P010) |
| Apps | [apps.md](apps.md) | hoe een app slaapt op gebeurtenissen en niet op de klok, de bouwstenen (`readable()`, `select`, `after` als deadline), de meetlat `HOPOS_SLOT_LOAD`, het Go-equivalent, de lessen uit de Stulp-port (hartslag, budget, meten per taak en per schakel, geen staatkopieën, allocator, één eigenaar per peer, niet op ACKs wachten, faalpaden, wachtrijen, timers) en de reviewchecklist |
| Meten | [measurements.md](measurements.md) | per board de laatste v3-meting naast de lat van v2, het gereedschap, en de handleiding voor het bijwerken; `tools/soak.sh` voor uren |
| Geheugen | [heap.md](heap.md), [stacktask.md](stacktask.md) | de gedeelde allocator van kern en apps (vrije lijsten, plafond, tellers) en waarom; synchrone C-code als taak met een eigen stack (`applib::stacktask`) |
| Opslag | [storage-sync.md](storage-sync.md) | de bevestigde opslagbarrière `OP_SYNC` voor een database in een app |
| Reviews | [description.md](description.md), [sequence-hopos.md](sequence-hopos.md), [review-2026-10-01.md](review-2026-10-01.md), [review-port-2026-10-02.md](review-port-2026-10-02.md) | de omschrijving laag voor laag met wat dubbel of scheef is; de volgorde in de tijd met per stap de afweging, en wat te schaven valt (24-10); de leesreview van 1 oktober (wat weg kan); de port-review Go v2.2.7 naast Rust v3 |

## Images per board

Hoe je een board bouwt en flasht staat op één plek: in zijn eigen doc,
onder "Bouwen". `CFG=` geeft elk image een eigen config, `GUI=1` de
gui-smaak op de boards met een framebuffer ([gui.md](gui.md)).

| Board | Script | Doc |
| --- | --- | --- |
| UEFI generiek (EDK2, QEMU), Orion O6N, Ampere Altra | `image/uefi-run.sh` | [boards.md](boards.md) |
| Raspberry Pi 4 en 5 | `image/rpi4.sh`, `image/rpi5.sh` | [boards-pi.md](boards-pi.md) |
| Radxa Zero 3E | `image/radxa-zero3.sh` | [boards-radxa.md](boards-radxa.md) |
| QEMU virt riscv64, LicheeRV Nano (SG2002) | `tools/qemu-riscv-test.sh`, `image/licheerv-agent.sh` | [boards-riscv.md](boards-riscv.md) |
| Mac mini M4 (t8132) | `image/apple-m4.sh` | [boards-apple.md](boards-apple.md) |
| Kern-flip-bundel (elk board) | `image/flip-bundle.sh` | [flip.md](flip.md) |
| Een release (elk board headless en headfull, de bundels, de apps) | `tools/release.sh <versie>` (`--dry-run`: alleen bouwen) | [jobs/README.md](../jobs/README.md) |

De firmware van de boards staat in `image/firmware/<board>/` (de Pi's, de
boot-keten van de Radxa, de donor van de LicheeRV; herkomst en sha256 in de
`LEESMIJ.txt` ernaast). Elk image-script neemt standaard de config in drie
lagen uit `image/cfg` (`default.cfg`, `<board>.cfg`, `headless.cfg` of met
`GUI=1` `headfull.cfg`; de laatste waarde wint); `CFG=` met een lijst
bestanden vervangt ze. Hij komt op elk
board in het venster van de kern (`image/hopcfg.py`; [boards.md](boards.md),
"Het config-venster"), en `hop image` van de hop-repo zet er een andere in,
in een image, een bundel of op een kaart. De kaarten en sticks bouwt
`tools/mkcard`. Go-apps zoals cloudflared staan in `go/`
([go-apps.md](go-apps.md)).

Hop zelf komt uit de hop-repo (`agentd-hopos`); de image-scripts bouwen
hem uit `HOP_DIR` met `tools/hop-build.sh` (tegen de applib van deze
werkboom, zoals de release) en bakken hem in als bewoner. Secure Boot
moet uit op de UEFI-boards. De host-kant van Hop (de `hop`-CLI, `agentd`, de runners en de
stores) staat in dezelfde hop-repo en bouwt met `cargo build --release -p
agentd -p cli`.

Een devicedag eindigt met een pagina in de browser: bouw
[`apps/welcome`](../apps/welcome/README.md), zet `welcome.elf` op een
HTTP-server op de laptop, en `curl -X POST -d '{"name":"welcome","driver":"hop","artifacts":[{"url":"http://LAPTOP:8000/welcome.elf"}],"memory_limit":33554432,"ports":{"http":80}}' http://NODE:9080/v1/jobs`
(met `hopos.insecure=1`); daarna toont `http://NODE/` de bunny, de node, het
slot, de uptime, de verzoeken en de heap. Daarna de meetreeks uit
[measurements.md](measurements.md) en een flip uit [flip.md](flip.md).

De console van een node zonder UART aan de laptop: `nc NODE 5555` geeft
eerst de bewaarde console (een ring van 64 KiB, de hele boot) en leest dan
live mee; hoogstens vier lezers tegelijk, en de vijfde hoort
`console: full, 4 readers`. Een lezer die weg is (een FIN, een RST, een
mislukte schrijf, of 10 s data uit zonder ack) geeft zijn plaats meteen
terug; elke honderdste is een regel `HOPOS_CONPORT_FREED` met de redenen en
het budget van de stack. Aan met `hopos.console=1` in de
config of cmdline, uit met `hopos.console=0`; zonder die sleutel volgt hij
`hopos.insecure=1` (`HOPOS_CONPORT_UP` in de boot).

## Wat er nog moet

Eén lijst, per node en voor alles tegelijk: [ALLES.md](../ALLES.md) in de
root van de repo. Wat af is gaat eruit. De eerste groene node was de Pi 5 op
30-09-2026, inclusief de eerste kern-flip op ijzer.

Hoe de code geschreven is: het Rust-handboek van haas.software
(`rustdoc/README.md`) en de port-notities (`rustdoc/PORT.md`).
