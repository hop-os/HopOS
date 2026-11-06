# vitals: de board-dokter

Eén HopOS-app die de vitale functies van een node meet en benchmarkt. Voor
bring-up en diagnose van nieuwe boards: plaats vitals, open de pagina, druk
op **Run all**, en vergelijk het rapport (knop **Copy report**) met een
gezond board.

De tests meten hetzelfde als vitals van v2, met dezelfde werklast en
dezelfde rekensom, zodat de v3-getallen naast die van v2 liggen
(`docs/measurements.md`, per board de tabel Vitals).

| test | meet | zegt iets over | marker |
|---|---|---|---|
| idle *(passief, loopt altijd)* | idle-%, wekken/s, kosten per wek uit `CTRL_IDLE` (0x48) en `CTRL_WAKES` (0x108) over de laatste 60 s | de slaap van de app-core: WFE met event-stream, de yield op een gedeelde core | `HOPOS_VITALS_IDLE` |
| cpu | LCG-stappen/s op één core (met een yield per burst) | de klok van het hart | `HOPOS_VITALS_CPU` |
| smp | eerst de gedeelde heap (elke core schrijft een blok, core 0 leest het), dan 64M stappen serieel tegen verdeeld | komen alle harten echt op, in dezelfde kooi | `HOPOS_VITALS_SMP` |
| burn | volgehouden last op alle cores, per seconde het tempo en `CTRL_TEMP` | thermal throttling, dvfs-terugklok, de heatsink | `HOPOS_VITALS_BURN`, elke 10 s `HOPOS_VITALS_BURN_TICK` |
| membw | STREAM-achtig copy en triad | de DRAM-controller | `HOPOS_VITALS_MEMBW` |
| memlat | pointer-chase over 32 KB tot 8 MB | de cache-hiërarchie, DRAM-latentie | `HOPOS_VITALS_MEMLAT` |
| alloc | allocatietempo met ~2 MB levend, geweigerde allocaties, grootste vrije brok | de heap van de app onder churn | `HOPOS_VITALS_ALLOC` |
| rx | download van een externe bron (plain http, met Content-Length) | doorvoer app-stack, switch, NAT, NIC, internet | `HOPOS_VITALS_RX` |
| tx | client-gedreven: `curl -o /dev/null http://node:8090/blob?mb=64` | de zendkant van hetzelfde pad | `HOPOS_VITALS_TX` |
| up | client-gedreven: een reeks PUTs van 1 MiB naar `/sink` (`perf.sh`) | de ontvangkant van hetzelfde pad | `HOPOS_VITALS_UPLOAD` (de eerste en elke zestiende) |
| disk | schrijven en lezen door de system-API (1 MiB per call), 4 KiB-writes, kale `stat`-calls | LAN-pad tegen hopfs en schijf: alles boven de stat-vloer is schijf | `HOPOS_VITALS_DISK` |
| storm | veel korte verbindingen (dial, `GET /ping`, sluiten) naar de eigen poort | verbindingsopbouw onder druk | `HOPOS_VITALS_STORM` |
| rtt | kale TCP-handshakes naar de gateway (10.100.0.1, de system-poort 10100) | de vloer van het interne pad | `HOPOS_VITALS_RTT` |
| timer | overslaap bij 1, 5 en 20 ms, veertig keer elk | timerbron, event-stream, executor | `HOPOS_VITALS_TIMER` |

Elke marker staat achter de regel `vitals: <test> done in <ms> ms` en draagt
de meetwaarden als `key=value` (dezelfde namen als in de JSON). Een fout is
`HOPOS_VITALS_FAIL test=<naam>`, overslaan `HOPOS_VITALS_SKIP test=<naam>`,
en `test=all` sluit af met `HOPOS_VITALS_ALL failed=N skipped=N`. De start:
`HOPOS_VITALS_UP port=<n>`.

## Wat wegviel, en waarom

- **sqlite**: er is geen SQLite in het Rust-slot (geen C, geen VFS over de
  system-API). Het schrijfpatroon van een database meet disk nog wel: de
  256 writes van 4 KiB.
- **gc**: Rust heeft geen collector, dus geen cycli en geen pauzes om te
  meten. In de plaats kwam **alloc**: hetzelfde patroon (blokken van 32 KiB,
  een levende set van ~2 MB) tegen de allocator van applib, met wat daar wel
  telt: het tempo, de weigeringen en de versnippering (de grootste vrije
  brok na afloop).
- **ramp** (hoe snel de klok opschaalt onder plotselinge last): niet in deze
  ronde. Het tempo per seconde van burn en de cpu-test dekken de klok; de
  ramp in vensters van 5 ms komt terug als een board erom vraagt.
- **syscall** (de stat-vloer naast een keep-alive GET naar de agent): de
  stat-vloer zit in disk (`floor_p50`, `floor_p99`); de agent is in v3 geen
  gateway meer maar een bewoner in slot 1, en app naar app meet
  `apps/bench` (`BENCH=ping`).
- **push** (upload naar een andere app op de node): dat is `apps/bench`
  met `BENCH=push`.
- **HOP_ADDR en HOP_KEY**: de temperatuur kwam in Go van de agent-API van
  Hop. In v3 zet de kern hem elke seconde op de control-page (`CTRL_TEMP`),
  dus vitals praat niet meer met Hop en heeft geen sleutel nodig. Een board
  zonder sensor geeft 0, en dan zegt burn dat er geen temperatuur is.
- **VITALS_NOCTRL**: de Go-app las de control-page zelf, en op de M4 faultte
  die lees (31-08). In v3 leest applib de control-page voor zijn eigen
  heartbeat al (`Ctrl::get`, met de pull ervoor); een app die opkomt, kan
  hem lezen.

## De vorm van v3

- **Eén test tegelijk.** `/api/run` claimt een veld in de tabel en start de
  test als eigen taak; een tweede start krijgt 409. Alles wat de tabel
  raakt, draait op de executor van core 0 (handboek §1.1, een leesbare
  tabel zonder lening over een `.await`).
- **Meer cores.** Met `"cpu_shares":2048` (twee cores) of meer draaien smp en
  burn over alle cores van de app: het werk gaat met `smp::spawn_on` naar de
  executor van de andere core, de uitkomst komt terug als atomics. Met één
  core slaat smp zichzelf over (`skipped`), zoals Go.
- **De triad rekent op `u64`.** Het target van de apps is
  `aarch64-unknown-none-softfloat`: een `f64`-optelling is daar een
  bibliotheekaanroep, en dan meet de triad de softfloat in plaats van het
  geheugen. De bytes per element blijven 24, dus het getal staat naast dat
  van Go.
- **memlat houdt de rekensom van Go**: een set met het label `size` heeft
  `size / 8` woorden van 4 bytes. Zo liggen de getallen naast de Go-kolom.
- **storm** gaat standaard naar het eigen adres en de eigen poort; de stack
  van de app keert dat intern om (loopback), dus hij meet de app-stack en
  zijn server, client en server in één slot. Voor de hairpin door switch en
  NAT: `?addr=NODE_IP:8090` of `VITALS_STORM_ADDR`. Zuiver meten is van
  buitenaf naar `/ping` stormen (`tools/netmeter`).
- **Geheugen**: elke buffer komt met `try_reserve` uit de heap; een
  partitie die te klein is, geeft een fout in het rapport, geen paniek.
  128 MB (`"memory_limit":134217728`) geeft de geheugentests ruimte om
  boven de caches uit te meten; de bandbreedtebuffer is een achtste van de
  partitie, hooguit 16 MB.

## Endpoints

```
GET  /                  de pagina: Run all, een knop per test, Copy report
GET  /api/state         alles als JSON: node, idle, temperatuur, resultaten
GET  /api/run?test=X    een test starten (of test=all); 409 als er een loopt
GET  /ping              "pong": het doelwit van storm
GET  /blob?mb=N         N MB naar de client (standaard 32), het resultaat als "tx"
PUT  /sink              een body tot 1 MiB (leanhttp's grens), de reeks als "up"
GET  /health            "ok"
```

De parameters van `/api/run` (ook bij `test=all`, dan voor elke test die
ze kent):

| param | test | standaard | grenzen |
|---|---|---|---|
| `secs` | cpu, burn, alloc | 5, 120, 3 | 1..60, 10..600, 1..30 |
| `mb` | rx, disk | 32, 64 | 1..1024 |
| `n` | rx (verbindingen), storm (werkers), rtt (handshakes) | 1, 8, 100 | 1..8, 1..16, 10..2000 |
| `reqs` | storm | 200 | 10..10000 |
| `kb` | disk (chunk) | 1024 | 4..1024 |
| `url` | rx | `VITALS_RX_URL` | plain http |
| `addr` | storm, rtt | eigen poort, 10.100.0.1:10100 | `ip:poort` |
| `path` | disk | `/vitals-disk.bin` | een pad in het eigen zicht, of een mount |
| `hole=1` | disk | uit | leest gaten: het transport kern naar app zonder schijf |
| `depth` | disk met `rand` | 1 | 1..16: zoveel lezingen per call (een bundel, `read_many`); 1 is de gewone lees |
| `bundles` | disk met `rand` | 1 | 1..2: zoveel bundels tegelijk, elk over een eigen verbinding |

De drie doorvoermetingen in één keer, vanaf een client met curl en python3:

```sh
apps/vitals/perf.sh <node[:8090]> [mb]     # download (/blob), upload (/sink), disk
```

## Config (jobspec-env, alles heeft een default)

| env | default | betekenis |
|---|---|---|
| `ER_PORT_HTTP` | `8080` | de gepubliceerde poort (Hop zet hem via `ports:{http:...}`) |
| `VITALS_RX_URL` | `http://cachefly.cachefly.net/100mb.test` | bron van rx (plain http, met Content-Length; de node heeft DNS nodig, `DNS` in de env) |
| `VITALS_STORM_ADDR` | *(leeg)* | doel van storm als `ip:poort`; leeg is de eigen poort |

## Plaatsen

Er is nog geen `hop`-CLI: de jobspec gaat met `curl` naar de leader van Hop
(poort 9080 op het adres van de node), zoals bij welcome en bench. De ELF
staat op een HTTP-server die de node kan bereiken:

```sh
curl -X POST -H 'Content-Type: application/json' \
  -d '{"name":"vitals","driver":"hop",
       "artifacts":[{"url":"http://192.168.1.208:8000/vitals.elf"}],
       "memory_limit":134217728,"cpu_shares":2048,"ports":{"http":8090}}' \
  http://NODE:9080/v1/jobs
```

Op de console: `HOP_JOB_PLACED`, de publicatie van poort 8090
(`HOPOS_SLOT_PUBLISH`) en `vitals ...: serving http on ... HOPOS_VITALS_UP
port=8090`. Dan `http://NODE:8090/` in de browser, of van de laptop:

```sh
curl 'http://NODE:8090/api/run?test=all'
curl -s http://NODE:8090/api/state | python3 -m json.tool
```

Weg: `curl -X DELETE http://NODE:9080/v1/jobs/vitals`. Blijvend: dezelfde
JSON op één regel achter `hopos.init[]=` in `hopos.cfg`.

Geen sharegroup: benchmarkcijfers horen van een eigen core te komen (de
timer- en idle-cijfers op een gedeelde core zijn juist wel interessant; dat
is een aparte plaatsing met `"tags":{"sharegroup":"..."}`).

## Bouwen

```sh
cargo build --release --target aarch64-unknown-none-softfloat -p vitals
rust-objcopy --strip-debug target/aarch64-unknown-none-softfloat/release/vitals vitals.elf
cargo test -p vitals                                          # de rekenkant op de host
cargo clippy -p vitals --all-targets -- -D warnings
```

Zonder debug-info (de symbolen blijven: de plaatsing leest ze). Eén ELF voor
elk ARM64-board.

## Kanttekeningen

- Onder QEMU-TCG is WFE een no-op: idle-cijfers zijn ijzer-cijfers.
- Bij meer dan één core is het idle-percentage `n/a`: `CTRL_IDLE` is de
  som van de slaap van elke core, geen percentage van één wandklok. Het
  wektempo blijft de gemeten som over de cores.
- storm stormt door zijn eigen slot heen (client én server), dus die
  cijfers zijn een ondergrens.
- rtt klopt aan bij de system-poort van de kern; elke handshake is voor de
  kern een system-verbinding die meteen sluit, en de kern zegt dat met een
  regel `system: slot N done` per stuk. Honderd handshakes zijn honderd
  regels op de console; `?addr=` kiest een stiller doel.

Op QEMU virt (30-09, TCG, `cpu_shares` 2048, 64 MB, Hop in slot 1) liep
`test=all` groen: twaalf tests in 34 s, smp met een speedup van 1,93 op twee
cores, disk 44,0 / 38,7 MB/s met een stat-vloer van 1191 µs. QEMU bewijst de
keten, geen plafond.

## Vorm

- `src/main.rs`: de start, de server (`applibhttp`: een acceptor met een
  park voor stille keep-alive-verbindingen en een vaste pool van acht
  werkers) en de routes (dezelfde vorm als welcome).
- `src/run.rs`: de tests, de claim van één tegelijk, de tabel en de
  state-JSON.
- `src/report.rs`: een rapport, puur: meetwaarden, regels in een vooraf
  gereserveerde buffer, JSON en de marker-staart.
- `src/idle.rs`: de sampler en het venster.
- `src/cpu.rs`, `src/mem.rs`, `src/net.rs`, `src/disk.rs`: de tests.
- `src/page.rs`: de pagina, één statisch document.
- `perf.sh`: de clientkant van download, upload en disk.
