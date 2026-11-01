# Metingen per board

PORT.md §1: deze pagina is de lat. Een Rust-HOP mag op geen gemeten getal
langzamer zijn dan v2. Per board staat de laatste v3-meting (*Nu*), het
beste getal dat v3 ooit haalde (*Hoogste v3*) en het v2-getal.

Stand: 04-10-2026 16:00, een schone ronde met `tools/meet` op 3.0.10 over
alle zeven borden. De geschiedenis van een cel staat in git.

## Zo schrijf je deze pagina

1. **Eén sectie per board**, altijd in deze volgorde: O6N, Altra, Pi 5,
   Pi 4, Radxa, LicheeRV, M4, QEMU. Een nieuw board komt erbij als nieuwe
   sectie met dezelfde opbouw.
2. **Elke sectie heeft dezelfde opbouw** (met na *In rust* de tabel *Watchdog*): één regel *Opzet* (de stempel,
   de datum, de commit als je die weet, en hoe vitals geplaatst is), dan de
   tabellen *Vitals*, *Netwerk*, *In de node*, *In rust*, en waar ze er
   zijn *Opslag* en *Media*. Daarna hoogstens drie zinnen *Lezing* en een
   lijst *Bord*.
3. **Elke tabel heeft vier kolommen:** `Meting | Nu | Hoogste v3 | v2`. In
   *Nu* staat het getal van de laatste ronde (de stempel en de datum staan
   één keer in *Opzet*; leeg als de ronde de rij niet meet), in *Hoogste
   v3* het beste getal dat v3 ooit haalde met `(stempel, dd-mm)`, in *v2*
   het v2-getal van dit board (de lat, leeg als v2 er geen had). De tabel
   *Watchdog* is geen meetreeks en houdt `Meting | v3 | v2 | Run`.
4. **Hoogste is per rij de beste richting**: meer is beter, behalve bij
   rtt, memlat, overslaap, wekken/s, verbindingscyclus, kern-cpu,
   kern-geheugen, Hop en temperatuur (`LAAG` in `tools/meet.py`). Het
   eerste getal van de cel beslist, de volgende bij gelijkspel; een bereik
   telt met zijn eerste getal. `tools/meet.py --write` overschrijft Nu en
   zet Hoogste alleen bij, met de nieuwe stempel, als Nu beter is.
5. **Eén run per punt**, vitals met `secs=5`, bench met één pull per
   richting. Doe een tweede run alleen bij een vreemd getal, en zet de
   reden erbij. Heb je toch meer runs, schrijf dan een bereik (`783–810`),
   nooit alleen de beste.
6. **Getallen kaal**, met de eenheid in de naam van de rij. Gebruik een
   decimale komma en een punt voor duizendtallen (`302.439`). MB/s is
   decimaal (1.000.000 bytes per seconde). De netmeter van v2 rekende
   MiB/s; `tools/netmeter` rekent decimaal.
7. **Geen tekst in een cel.** Peer, cores en grootte staan in *Opzet*.
   Een opmerking die een getal verklaart (wat de grens is, een ongelijke
   opzet achter een Hoogste) komt in één regel onder de tabel. Verhalen
   horen in ALLES.md of in de boards-*.md.
8. **Wat niet gemeten is, laat je weg.** Er is één uitzondering: een rij
   met een v2-getal blijft staan, met `—` bij Hoogste v3, zodat de open lat
   zichtbaar is. Wat nog moet komt in de regel *Nog te meten*.
9. **Geen Wi-Fi-getallen.** Een meting van of naar de laptop over Wi-Fi
   meet de Wi-Fi, niet het board. Meet over de draad, node naar node of
   vanaf een bedrade host.
10. **Een meting die op elk board hetzelfde is** (een faalreden, een
    eigenschap van de tool) komt één keer onder *Gereedschap*, niet bij
    elk board.

## Gereedschap

| Wat | Commando | Marker |
| --- | --- | --- |
| De hele ronde: inrichting, vitals, bench in de node en over de draad, opruimen | `python3 tools/meet.py` (`--only pi4,pi5`, `--skip-wire`, `--peer IP`, `--stamp m4=M40` als de console de bootregel kwijt is, `--write` zet Nu en Hoogste v3, `--write --no-run` schrijft de fragmenten van `--out` zonder te meten, `--selftest` toetst `--write`) | per bord `target/meet/<bord>.md`, de tijd per fase |
| Vitals plaatsen | `curl -X POST -d '{"name":"vitals","driver":"hop","artifacts":[{"url":"http://LAPTOP:8000/vitals.elf"}],"memory_limit":134217728,"cpu_shares":2048,"ports":{"http":8090}}' http://NODE:9080/v1/jobs` | `HOPOS_VITALS_UP port=8090` |
| Vitals, alles | `curl 'http://NODE:8090/api/run?test=all&secs=5'` | `HOPOS_VITALS_ALL failed= skipped=` |
| Vitals, één test | `curl 'http://NODE:8090/api/run?test=<naam>'` (cpu, smp, burn, membw, memlat, alloc, storm, rtt, timer, idle, disk, rx) | `HOPOS_VITALS_<TEST> key=value ...`, `HOPOS_VITALS_FAIL`, `HOPOS_VITALS_SKIP` |
| De bench in een slot (server) | `curl -X POST -d '{"name":"bench","driver":"hop","artifacts":[{"url":"http://LAPTOP:8000/bench.elf"}],"memory_limit":67108864,"ports":{"http":80}}' http://NODE:9080/v1/jobs` | `HOPOS_BENCH_UP role=serve port=80` |
| Van de host naar een app | `cargo run --release -p netmeter -- NODE:80 --phases in,out,rtt,storm,udp --json > run.json` | `NETMETER phase=...` |
| App naar app (in de node of over de draad) | job met `"env":{"BENCH":"pull","BENCH_PEER":"IP:80","BENCH_BYTES":"419430400"}` (of `ping`, `push`) | `HOPOS_BENCH_PULL`, `HOPOS_BENCH_RTT`, `HOPOS_BENCH_COLD` |
| Last op een app-core | job met `"env":{"BURN":"1"}` (`BURN_WORK`, `BURN_REST` in s) | `HOPOS_BENCH_BURN` |
| Geheugen onder druk | job met `"env":{"THRASH":"1"}` | `HOPOS_BENCH_THRASH` |
| Multicast tussen twee apps | een job met `"env":{"MCAST":"listen"}`, dan een met `"env":{"MCAST":"send"}` | `HOPOS_BENCH_MCAST recv=N` |
| De OS-core in rust | `hopos.tick=1` in `hopos.cfg` of op de cmdline | `HOPOS_TICK` |
| hopfs vanuit een app | job met `"env":{"BENCH":"disk"}` | `HOPOS_BENCH_DISK` |
| De watchdog | `POST :9080/restart` met een image dat geen Hop is (appspike), zoals `tools/qemu-test-hop-fail.sh`: Hop valt, de pets stoppen, de reset volgt | `HOPOS_HOP_DOWN`, dan `HOPOS_BOOT` |
| Kern en Hop in rust | `GET :9080/v1/agents` (`kern_cpu_percent`, `kern_mem_bytes`, `hop_cpu_percent`, `hop_mem_bytes`, `temp_milli_c`), drie peilingen 10 s uit elkaar, alleen welcome geplaatst | |
| De keten op QEMU | `sh tools/qemu-test-bench.sh` | `bench-keten groen` |
| Uren lang plaatsen en stoppen | `sh tools/soak.sh NODE` (`FLIP=1` voor de flip erbij) | `SOAK` |

Code: `apps/vitals` (README daar), `apps/bench` (protocol in
`apps/bench/src/proto.rs`), `tools/netmeter`, `tools/meet.py`,
`hopos/src/bench.rs`.

Overal hetzelfde:
- `test=rx` faalt zonder `url`. De standaardbron (cachefly) vraagt een
  tunnel: `http: CONNECT is not supported`. Daarom geeft `test=all` met
  één core `failed=1` (rx) en `skipped=2` (smp met één core, disk zonder
  schijf).
- De vitals-*storm* gaat naar de eigen poort, de vitals-*rtt* naar de kern
  (10.100.0.1:10100).
- Op QEMU is WFE een no-op. Rijen over idle tellen alleen op ijzer.

## Orion O6N

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 4 cores (`core-class: big`),
128 MiB. Over de draad tegen de M4 (meet-serve :9100, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 752 | 754 (O3, 03-10) | |
| smp, speedup | 3,84 | 3,99 (O2, 03-10) | |
| burn begin → eind, Msteps/s / max °C | 2990 → 2990 / 47,0 | 2991 → 2991 / 48,0 (O95, 04-10) | |
| membw copy / triad, GB/s | 40,9 / 27,7 | 42,0 / 28,2 (O95, 04-10) | |
| memlat 32 KB / 2 MB / 8 MB, ns | 1,74 / 20,5 / 32,2 | 1,74 / 20,5 / 32,2 (O95, 04-10) | |
| alloc, allocs/s | 520.216 | 520.336 (3.0.10, 04-10) | |
| storm, conn/s (p99 ms) | 9971 (0,86) | 9971 (0,86) (3.0.10, 04-10) | |
| rtt naar de kern p50 / p99, µs | 139 / 992 | 99 / 7761 (O40, 04-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 48 / 92 | 48 / 71 (O40, 04-10) | |
| idle, wekken/s | 44,4 | 43,0 (O95, 04-10) | |

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 87,2 | 117,6 (O95, 04-10) | 111,3–116,5 |
| De node uit, MB/s | 95,9 | 113,0 (O40, 04-10) | 114,3–117,6 |
| rtt over de draad p50 / p99 / koud, µs | 203 / 271 / 293 | 73 / 114–119 / 75 (O40, 04-10) | 156–201 |
| Verbindingscyclus p50, ms | 0,78 | 0,30 (O40, 04-10) | 3,6 |
| Storm over de draad, conn/s (p99 ms) | 4905 (2,85) | 9584 (0,98) (O95, 04-10) | 844 (19,6) |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 985 (12,1) | 1983 (10,0) (O40, 04-10) | |

Hoogste uit O40 (uit, rtt en cyclus): naar de Pi 5 en de O6N druk.

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 5156 | 5156 (3.0.10, 04-10) | |
| rtt warm p50 / p99, µs | 13 / 37 | 13 / 37 (3.0.10, 04-10) | |
| rtt koud p50, µs | 13 | 13 (3.0.10, 04-10) | |

**In rust**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Kern-cpu, % | 1 | 1 (O40, 04-10) | |
| Kern-geheugen gebruikt / totaal, MiB | 37,4–37,5 / 224 | 31,4 / 224 (O95, 04-10) | |
| Hop geheugen, MiB / cpu, % | 0,53 / 1 | 0,41 / 1 (3.0.10, 04-10) | |
| Temperatuur, °C | 42,0 | 40,0 (O40, 04-10) | |

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | **47 s, maar de stickkern O2 landde eerst op de oude flip-overdracht in het DRAM en gaf pas na de boot-guard een koude boot (2,5 min); post-mortem van de tussenboot, niet van O41w (O41w, 04-10)** | | O41w 04-10 |

**Opslag**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Rauw 1 MiB, schrijven / lezen, MB/s | | — | 3056,8 / 2705,6 |
| hopfs 1 MiB, schrijven / lezen, MB/s | | 3584 / 2992 (nvmebench, 04-10) | 2970,1 / 2716,7 |
| App-opslag via vitals disk, schrijven / lezen, MB/s | 1580 / 1934 | 1580 / 1934 (3.0.10, 04-10) | 556,8–625,4 / 727,5–796,8 |
| 4 KiB schrijven via de app, MB/s | 81,7 | 84,2 (O40, 04-10) | 15,15–17,08 |
| Willekeurig 4 KiB lezen door 1 / 2 / 4 / 8 / 9 apps, opdrachten/s | | 7.983 / 16.561 / 29.052 / 46.868 / 50.150 (O43, 04-10) | |
| Idem zonder schijf (`hole=1`), opdrachten/s | | 22.694 / 46.643 / 56.672 / 78.017 / 78.226 (O43, 04-10) | |

v2 rauw is gecachet, v2 app-opslag 256 MiB. Door apps: de drive haalt rauw
15,6k met één lees in de lucht en 71k met zestien; zonder schijf zit de
OS-core vol (~12,8 µs per call).

**Media**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| 4K P010 door de grant (apps/decode), fps | | 85,7 (30-09) | 27,25 |

v2-lat 24 fps; 85,7 fps is 2134 MB/s.

Lezing: de OS-core-rondreis is op O40 77 tot 82 µs met welcome en
bench-serve erbij; de 55 van O16h was in een lege node. De 298 tegen 855
op 30-09 is de kleine core, geen regressie. Sinds 68a86c6 klokt de O6N
binnen één ronde naar vol als er een rekenaar in een slot staat.

Nog te meten: de rest van de `nvmebench`-regels in de tabel (de kern van
04-10 haalde SEQ lezen 3263 MB/s, RANDQ 16 tegelijk 71k IOPS); media na
een koude boot.

Bord:
- Boot: VHE, kick via SGI 1. Timer: CNTHP (PPI 26).
- NIC: RTL8125B, MSI-X via de ITS (LPI 8192).
- Watchdog: SBSA, 8,5 s.
- RNG: geen FEAT_RNG of SMCCC-TRNG; de kern gebruikt EFI_RNG_PROTOCOL
  (`hopos.efirng=1`), de slots jitter.
- Temperatuur: SCMI. Klokbeleid: vijf `_CPC`-domeinen.
- Console op het glas: GOP 1920x1080. USB: 10 xHCI's, Blu-ray over
  USB-BOT.
- Opslag: NVMe Lexar 4 TB. Hardwaredecoder: Linlon V8.

## Ampere Altra

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 4 cores, 128 MiB. Over de draad
tegen de O6N (bench-serve :9000, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 495 | 495 (A95, 04-10) | |
| smp, speedup | 3,97 | 4,01 (A4, 03-10) | |
| burn begin → eind, Msteps/s / max °C | 1979 → 1979 / 40,0 | 1979 → 1979 / 40,0 (A95, 04-10) | |
| membw copy / triad, GB/s | 24,6 / 23,4 | 25,9 / 24,0 (A4, 03-10) | |
| memlat 32 KB / 2 MB / 8 MB, ns | 1,54 / 5,33 / 27,5 | 1,54 / 5,33 / 27,5 (A95, 04-10) | |
| alloc, allocs/s | 550.511 | 557.016 (A4, 03-10) | |
| storm, conn/s (p99 ms) | 9573 (0,92) | 9958 (0,92) (A4, 03-10) | |
| rtt naar de kern p50 / p99, µs | 376 / 3340 | 84 / 158 (A56g, 04-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 309 / 1040 | 236 / 1022 (A4, 03-10) | |
| idle, wekken/s | 21,8 | 21,2 (A95, 04-10) | |
| disk schrijven / lezen / 4 KiB schrijven, MB/s | 1337 / 2196 / 77,6 | 1364 / 2213 / 79,1 (3.0.10, 04-10) | |

Hoogste rtt naar de kern uit een tweede run (in `test=all` 280 / 9218).

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 80,4 | 88,8 (3.0.10, 04-10) | 107,7–110,8 |
| De node uit, MB/s | 109,4 | 110,6 (3.0.10, 04-10) | 108,0–110,6 |
| rtt over de draad p50 / p99 / koud, µs | 172 / 226 / 249 | 172 / 226 / 249 (3.0.10, 04-10) | |
| Verbindingscyclus p50, ms | 0,93 | 0,90 (A95, 04-10) | 5,1 |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 983 (12,7) | 990–1074 (12,4–14,4) (A56g, 04-10) | |
| Storm over de draad, conn/s (p99 ms) | 4956 (3,54) | 4956 (3,54) (3.0.10, 04-10) | |

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 5738 | 5762 (3.0.10, 04-10) | |
| rtt warm p50 / p99, µs | 14 / 26 | 14 / 25 (3.0.10, 04-10) | |
| rtt koud p50, µs | 21 | 15 (3.0.10, 04-10) | |

**In rust**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Kern-cpu, % | 1 | 1 (A56g, 04-10) | |
| Kern-geheugen gebruikt / totaal, MiB | 31,8–31,9 / 224 | 31,3 / 224 (3.0.10, 04-10) | |
| Hop geheugen, MiB / cpu, % | 0,55 / 1 | 0,42 / 1 (3.0.10, 04-10) | |
| Temperatuur, °C | 39,0–40,0 | 38,0 (A56g, 04-10) | |

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | **Hop gestopt 09:36:35, reset binnen 22 s; terug op de stickkern A4 om 09:40:14 (de firmware-boot duurt ruim 3 min); geen post-mortem, de zwarte doos overleeft op de Altra geen reset; daarna main plus fix warm vanaf A4 op (A58w, 04-10)** | | A58w 04-10 |

**Opslag**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Rauw 1 MiB, schrijven / lezen, MB/s | | 2466 / 2109 (A57b, 04-10) | |
| hopfs 1 MiB, schrijven / lezen, MB/s | | 2493 / 2251 (A57b, 04-10) | |
| Sequentieel 1 GiB, schrijven / lezen, MB/s | | 2549 / 2459 (A57b, 04-10) | |
| Willekeurig 4 KiB, schrijven / lezen, IOPS | | 141.000 / 24.500 (A57b, 04-10) | |
| Willekeurig 4 KiB lezen, 16 tegelijk, IOPS | | 186.000 (A57b, 04-10) | |
| Willekeurig 4 KiB lezen door 1 app, per call / bundel 4 / bundel 16, opdrachten/s | | 18.827 / 38.787 / 100.418 (A74f, 04-10) | |
| Idem 4 apps, opdrachten/s | | 54.293 / 99.353 / 249.103 (A74f, 04-10) | |
| Idem 8 apps, opdrachten/s | | 84.995 / 123.567 / 305.060 (A74f, 04-10) | |

Door apps met de bundel (`read_many`, fd11581); twee bundels met 1 app
159.369; vóór de bundel (A56g) 18.918 / 55.645 / 78.944 per call.

Bord:
- NIC: **igb op MSI-X via de ITS van zijn root-complex (ITS 7, LPI 8224),
  eerste interrupt na 4 µs (A56g, 04-10)**; eerder bewust gepold (de
  `_PRT`-INTx doodt de SoC). Kick via SGI 1.
- Watchdog: SBSA, 12,0 s. RNG: **geen SMCCC-TRNG (`TRNG_VERSION
  NOT_SUPPORTED`), jitter (04-10)**; eerder SMCCC-TRNG of `rndr`.
  Temperatuur: SMpro.
- Console op het glas: GOP, alleen na een koude boot (na een warme flip
  `HOPOS_FB_NONE`). Opslag: NVMe SN770 500 GB.
- Zwarte doos: overleeft op de Altra geen reset (`HOPOS_FLIP_BLACKBOX_EMPTY`).

## Raspberry Pi 5

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 2 cores, 128 MiB. Over de draad
tegen de O6N (bench-serve :9000, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 285 | 285 (Q40, 04-10) | |
| smp, speedup | 2,00 | 2,00 (30-09) | |
| burn begin → eind, Msteps/s / max °C | 571 → 571 / 61,5 | 571 → 571 / 62,0 (3.0.10, 04-10) | |
| membw copy / triad, GB/s | 8,31 / 6,89 | 8,32 / 6,89 (3.0.10, 04-10) | |
| memlat 32 KB / 2 MB / 8 MB, ns | 2,67 / 15,4 / 58,4 | 2,67 / 15,4 / 58,4 (Q90, 04-10) | |
| alloc, allocs/s | 291.390 | 302.439 (P4, 03-10) | |
| storm, conn/s (p99 ms) | 4899 (1,56) | 4911 (1,56) (Q90, 04-10) | |
| rtt naar de kern p50 / p99, µs | 237 / 997 | 165 / 9141 (Q40, 04-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 213 / 1000 | 213 / 215 (Q40, 04-10) | |
| idle, wekken/s | 20,7 | 20,6 (Q40, 04-10) | |

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 83,0 | 111,3 (Q40, 04-10) | 57,0–69,6 |
| De node uit, MB/s | 108,7 | 109,5 (Q90, 04-10) | 41,6–50,6 |
| rtt over de draad p50 / p99 / koud, µs | 164 / 205 / 257 | 73 / 118 / 88 (Q40, 04-10) | |
| Verbindingscyclus p50, ms | 0,84 | 0,62 (Q40, 04-10) | 1,1–3,6 |
| Storm over de draad, conn/s (p99 ms) | 4917 (2,90) | 4965 (2,46) (Q90, 04-10) | |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 1983 (5,16) | 1983 (5,16) (3.0.10, 04-10) | |

Hoogste uit Q40 (in, rtt en cyclus): de O6N druk.

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 782 | 783–810 (P4, 03-10) | |
| rtt warm p50 / p99, µs | 9 / 24 | 9 / 24 (3.0.10, 04-10) | |
| rtt koud p50, µs | 9 | 9 (Q40, 04-10) | |
| OS-core stil, slaapjes/s / NIC-irq/s | | ~900 / ~70 (idlestat) | |

v2 OS-core stil: 113 RX-rondes/s (irq).

**In rust**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Kern-cpu, % | 1 | 1 (Q40, 04-10) | |
| Kern-geheugen gebruikt / totaal, MiB | 30,4–30,5 / 127,5 | 29,9 / 127,5 (3.0.10, 04-10) | |
| Hop geheugen, MiB / cpu, % | 0,52 / 1 | 0,39 / 1 (3.0.10, 04-10) | |
| Temperatuur, °C | 55,4–56,0 | 50,5–51,0 (P4, 03-10) | |

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | **30 s naar de kaartkern P4; post-mortem gezien (Q41w, 04-10)** | | Q41w 04-10 |

Lezing: uit is de helft van in. De GEM zendt met losse woorden uit NC,
dezelfde vorm als de dwmac4 van de Radxa vóór X2.

Bord:
- Warme flip: werkt, drie keer geland binnen 10 s (04-10).
- Watchdog: BCM PM, 12 s.
- Console op het glas: gezien via een flip op de eerste kaart. Sinds de
  herflash weigert de firmware.

## Raspberry Pi 4

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 2 cores, 128 MiB. Over de draad
tegen de O6N (bench-serve :9000, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 250 | 250 (P40, 04-10) | |
| smp, speedup | 2,00 | 2,00 (30-09) | |
| burn begin → eind, Msteps/s / max °C | 499 → 499 / 57,9 | 499 → 499 / 57,9 (3.0.10, 04-10) | |
| membw copy / triad, GB/s | 3,39 / 2,84 | 3,44 / 2,84 (3.0.10, 04-10) | |
| memlat 32 KB / 2 MB / 8 MB, ns | 3,33 / 22,6 / 110 | 3,33 / 21,7 / 110 (P40, 04-10) | |
| alloc, allocs/s | 93.845 | 100.921 (P2g, 03-10) | |
| storm, conn/s (p99 ms) | 3300 (2,92) | 3300 (2,92) (3.0.10, 04-10) | |
| rtt naar de kern p50 / p99, µs | 448 / 2678 | 322 / 5312 (P40, 04-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 213 / 1027 | 213 / 213 (P40, 04-10) | |
| idle, wekken/s | 20,6 | 20,3 (P40, 04-10) | |

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 63,8 | 71,9 (P40, 04-10) | 6,6 |
| De node uit, MB/s | 112,1 | 112,9 (P70, 04-10) | 42,3 |
| rtt over de draad p50 / p99 / koud, µs | 1213 / 1221 / 1225 | 1212–1213 / — / 1290–1300 (H, 30-09) | |
| Verbindingscyclus p50, ms | 2,43 | 2,43 (P40, 04-10) | |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 1086 (9,36) | 1942–1989 (5,6–6,8) (P40, 04-10) | |
| Storm over de draad, conn/s (p99 ms) | 1966 (4,95) | 2465 (4,88) (P90, 04-10) | |

Hoogste in uit P40: de O6N druk. v2 in: 2.2.6, gepold. Hairpin P40: vijf
runs.

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 399 | 399 (3.0.10, 04-10) | |
| rtt warm p50 / p99, µs | 15 / 27 | 15 / 27 (3.0.10, 04-10) | |
| rtt koud p50, µs | 19 | 19 (3.0.10, 04-10) | |

**In rust**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Kern-cpu, % | 1 | 1 (3.0.10, 04-10) | |
| Kern-geheugen gebruikt / totaal, MiB | 30,4 / 127,5 | 29,9 / 127,5 (3.0.10, 04-10) | |
| Hop geheugen, MiB / cpu, % | 0,52 / 1 | 0,38–0,39 / 1 (3.0.10, 04-10) | |
| Temperatuur, °C | 51,6–52,5 | 51,1–51,6 (P40, 04-10) | |

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | **27,5 s naar de kaartkern P1; post-mortem gezien (P41, 04-10)** | | P41 04-10 |

Lezing: app naar app in de node wordt begrensd doordat de OS-core vol zit
(A72 op 1500 MHz).

Bord:
- Warme flip: zes keer, gen 4 op stempel I.
- Console op het glas: 32 bpp.

## Radxa Zero 3W

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 2 cores, 128 MiB. Over de draad
tegen de O6N (bench-serve :9000, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 326 | 326 (3.0.10, 04-10) | |
| smp, speedup | 1,99 | 2,00 (30-09) | |
| burn begin → eind, Msteps/s | 648 → 648 | 648 → 648 (3.0.10, 04-10) | |
| membw copy / triad, GB/s | 4,67 / 3,89 | 4,85 / 3,89 (X40, 04-10) | |
| memlat 32 KB / 2 MB / 8 MB, ns | 1,72 / 92,0 / 145 | 1,54 / 92,4 / 146 (3.0.10, 04-10) | |
| alloc, allocs/s | 99.757 | 99.790 (X90, 04-10) | |
| storm, conn/s (p99 ms) | 2421 (5,21) | 2421 (5,23) (X90, 04-10) | |
| rtt naar de kern p50 / p99, µs | 825 / 5731 | 609 / 681 (X40, 04-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 365 / 854 | 365 / 854 (3.0.10, 04-10) | |
| idle, wekken/s | 21,0 | 20,8 (X40, 04-10) | |

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 79,2 | 111,2 (X20, 04-10) | 55,8–56,6 |
| De node uit, MB/s | 108,2 | 112,4 (X20, 04-10) | 98,8–99,6 |
| rtt over de draad p50 / p99 / koud, µs | 182 / 713 / 266 | 178 / 221 / 282 (X90, 04-10) | |
| Verbindingscyclus p50, ms | 1,72 | 0,96 (X40, 04-10) | 4,1 |
| Storm over de draad, conn/s (p99 ms) | 1650 (8,12) | 1650 (8,12) (3.0.10, 04-10) | |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 479 (23,8) | 620–757 (16,4–17,3) (X40, 04-10) | |

Hoogste in en uit uit X20: de serve op de O6N op een eigen core. Cyclus en
hairpin X40: de O6N druk.

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 390 | 391 (X90, 04-10) | |
| rtt warm p50 / p99, µs | 25 / 62 | 25 / 62 (3.0.10, 04-10) | |
| rtt koud p50, µs | 42 | 40 (X40, 04-10) | |
| OS-core stil, slaapjes/s / NIC-irq/s | | ~1700–1900 / ~75 (`HOPOS_TICK`) | |

OS-core stil: bewoners 0,14 %.

**In rust**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Kern-cpu, % | 1 | 1 (3.0.10, 04-10) | |
| Kern-geheugen gebruikt / totaal, MiB | 31,9–32,0 / 64 | 31,9–32,0 / 64 (3.0.10, 04-10) | |
| Hop geheugen, MiB / cpu, % | 0,45–0,46 / 1 | 0,39 / 1 (3.0.10, 04-10) | |

Temperatuur: geen sensor.

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | **~100 s (DW-watchdog TOP 89 s) naar de kaartkern X1; post-mortem gezien (X42, 04-10)** | | X42 04-10 |

Lezing: als de serve van de O6N in `system` staat (op de OS-core van de
O6N), haalt de Radxa maar 22–23 MB/s in. Dan is de zender de grens, niet
de Radxa.

Bord:
- NIC-interrupt op SPI 64.
- Warme flip: vijf keer, gen 3 op stempel I.
- Console op het glas: HDMI, zonder EDID. USB: twee DWC3's up.

## LicheeRV Nano

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 1 core (`sharegroup: system`),
32 MiB. Over de draad tegen de O6N (bench-serve :9000, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 93,3 | 93,3 (3.0.10, 04-10) | |
| burn begin → eind, Msteps/s / max °C | 93,4 → 93,4 / 56,7 | 93,4 → 93,4 / 56,7 (3.0.10, 04-10) | |
| membw copy / triad, GB/s | 2,29 / 1,78 | 2,29 / 1,78 (R40, 04-10) | |
| memlat 32 KB / 2 MB, ns | 7,12 / 158 | 7,12 / 158 (3.0.10, 04-10) | |
| alloc, allocs/s | 58.237 | 58.357 (3.0.10, 04-10) | |
| storm, conn/s (p99 ms) | 431 (34,0) | 517 (30,4) (R40, 04-10) | |
| rtt naar de kern p50 / p99, µs | 2932 / 7104 | 471 / 1051 (R40, 04-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 552 / 2731 | 63 / 2717 (R40, 04-10) | |
| idle, wekken/s | 45,9 | 42,7 (R14, 03-10) | |

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 11,3 | 11,4 (3.0.10, 04-10) | |
| De node uit, MB/s | 9,3 | 9,3 (3.0.10, 04-10) | |
| Verbindingscyclus p50, ms | 6,24 | 6,24 (3.0.10, 04-10) | 16–19 |
| rtt over de draad p50 / p99 / koud, µs | 326 / 504 / 448 | 248 / 374 / 291 (3.0.10, 04-10) | |
| Storm over de draad, conn/s (p99 ms) | 347 (40,4) | 347 (40,4) (3.0.10, 04-10) | |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 124 (148) | 124 (148) (3.0.10, 04-10) | |

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 63,8 | 63,8 (3.0.10, 04-10) | |
| rtt warm p50 / p99, µs | 167 / 331 | 167 / 331 (3.0.10, 04-10) | |
| rtt koud p50, µs | 196 | 196 (3.0.10, 04-10) | |

**In rust**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Kern-cpu, % | 1 | 1 (3.0.10, 04-10) | |
| Kern-geheugen gebruikt / totaal, MiB | 30,0–30,1 / 40 | 30,0–30,1 / 40 (3.0.10, 04-10) | |
| Hop geheugen, MiB / cpu, % | 0,62 / 1 | 0,62 / 1 (3.0.10, 04-10) | |
| Temperatuur, °C | 56,0 | 53,5 (R40, 04-10) | |

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | **41 s naar de kaartkern R14; geen post-mortem (riscv heeft er nog geen) (R41, 04-10)** | | R41 04-10 |

Lezing: op R40 haalt een app in `system` (core 0, `rings=wb`) 60,3 MB/s app
naar app en 175 µs rtt; in `hop` op de C906L (`rings=maintained`) 23,4 MB/s
en 3,5 ms. Bij Hop op de C906L (tag `sharegroup: hop`) haalt dezelfde vitals
62,6 Msteps/s. Dat is 0,71 van wat hij in `system` haalt, precies de 700
van 1000 MHz. Op de C906L valt memlat bij 128 KB al uit de cache (133 ns).
Zonder loterij (R11) gebruikt de kern in rust 4–5 % cpu, tegen 46–48 % met
loterij (R10).

Bord:
- Loterij: gezien op R3 tot R10 (`HOPOS_LOTTERY_SWAPPED`, ac79e91). Na een
  koude flip komt hij opnieuw; `HOPOS_FLIP_SETTLED` na 70 s.
- Koude flip: gen 10 (R10). Warme flip: `flip::WARM` weigert vóór de
  sprong.
- IRQ: kick via MSIP van de CLINT; PLIC context 0 met 102 bronnen, de dwmac
  op bron 31. Met de loterij is de NIC gepold.
- Watchdog: canary `HOPOS_WD_CANARY_OK`. Temperatuur: TEMPSEN
  (`HOPOS_TEMPSEN_UP`).

## Mac mini M4

Opzet: 3.0.10, 04-10, tools/meet. Vitals met 2 cores, 128 MiB. Over de draad
tegen de O6N (bench-serve :9000, vitals :8090).

**Vitals**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| cpu, Msteps/s | 542 | 542 (3.0.10, 04-10) | |
| smp, speedup | 2,00 | 2,00 (M40, 04-10) | |
| burn begin → eind, Msteps/s | 1084 → 1084 | 1084 → 1084 (M40, 04-10) | |
| membw copy / triad, GB/s | 36,5 / 32,6 | 36,5 / 32,6 (3.0.10, 04-10) | |
| memlat 32 KB / 2 MB / 8 MB, ns | 1,84 / 7,04 / 16,0 | 1,84 / 7,04 / 16,0 (3.0.10, 04-10) | |
| alloc, allocs/s | 485.143 | 485.143 (3.0.10, 04-10) | |
| storm, conn/s (p99 ms) | 9593 (1,05) | 9882 (1,07) (M40, 04-10) | |
| rtt naar de kern p50 / p99, µs | 181 / 445 | 62–76 / 72–135 (M1, 03-10) | |
| timer 1 ms, overslaap p50 / p99, µs | 1023 / 1999 | 1023 / 1999 (3.0.10, 04-10) | |
| idle, wekken/s | 57,1 | 40,4 (3.0.10, 04-10) | |
| disk schrijven / lezen / 4 KiB schrijven, MB/s | 1206 / 1494 / 72,9 | 1206 / 1494 / 72,9 (3.0.10, 04-10) | |

**Netwerk**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 102,2 | 107,9 (M40, 04-10) | 43,3–46,7 |
| De node uit, MB/s | 80,1 | 117,4 (M40, 04-10) | 47,7–52,3 |
| rtt over de draad p50 / p99 / koud, µs | 200 / 384 / 302 | 116 / 146 / 129 (M40, 04-10) | 50–66 |
| Storm over de draad, conn/s (p99 ms) | 4882 (3,76) | 4951 (4,00) (3.0.10, 04-10) | 6379 (1,9) |
| Verbindingscyclus p50, ms | 0,81 | 0,54 (M40, 04-10) | |
| Storm, hairpin naar zichzelf, conn/s (p99 ms) | 1970 (5,06) | 2449 (5,09) (M40, 04-10) | |

v2 in en uit: HTTP PUT en GET.

**In de node**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| App naar app, MB/s | 3188 | 6379–6383 (M33, 01-10) | 764–769 |
| rtt warm p50 / p99, µs | 42 / 54 | 42 / 54 (3.0.10, 04-10) | |
| rtt koud p50, µs | 43 | 43 (3.0.10, 04-10) | |
| App naar app, 40 GiB, MB/s | | 2813 (M33, 01-10) | |

Hoogste app naar app (M33) op een P-core; Nu op twee E-cores.

**Watchdog**

| Meting | v3 | v2 | Run |
| --- | --- | --- | --- |
| Watchdogtoets (meetkern `wdtest`: Hop stopt na 60 s) | — (niet gedaan: Hop daar is nog 3.0.0 en de leader kent geen jobs) | | |

**Opslag**

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| Rauw 1 MiB, schrijven / lezen, MB/s | | 4952 / 1925 (M23, 01-10) | 5389–5756 / 1824–1971 |
| hopfs 1 MiB, schrijven / lezen, MB/s | | 5015 / 2026 (M23, 01-10) | 5480–5632 / 1781–1816 |
| Sequentieel 1 GiB, schrijven / lezen, MB/s | | 4758 / 1629 (M23, 01-10) | |
| Willekeurig 4 KiB, schrijven / lezen, IOPS | | 140.000 / 11.900 (M23, 01-10) | |
| Kern naar app zonder schijf (`hole=1`), MB/s | | 1879–1917 (M24, 01-10) | |
| App-opslag 256 MiB, schrijven / lezen / 4 KiB schrijven, MB/s | | 1288 / 1675 / 111 (M40, 04-10) | 1598–1657 / 1064–1072 |
| 4 KiB schrijven als RPC-lus, MB/s | | 82 (01-10) | |
| Willekeurig 4 KiB lezen door 1 app, per call / bundel 4 / bundel 16, opdrachten/s | | 8.399 / 21.758 / 55.834 (M70b, 04-10) | |
| Idem 4 apps, opdrachten/s | | 28.843 / 77.829 / 161.485 (M70b, 04-10) | |
| Idem 6 apps, opdrachten/s | | 39.744 / 104.493 / 171.345 (M70b, 04-10) | |
| Idem zonder schijf (`hole=1`), 1 / 2 / 4 / 6 apps per call, opdrachten/s | | 30.986 / 41.701 / 66.089 / 76.577 (M40, 04-10) | |
| Replica (SQLite op HopFS), persist-proef, ms | | 193 (M33, 01-10) | |

Zonder schijf laatst 1511 (M40, 256 MiB). RPC-lus: naast een bulk-app 15–19.
Door apps: met de bundel (`read_many`, fd11581), naast spin; twee bundels
met 1 app 80.521, rauw 175k met zestien. Vóór de bundel 7600 / 14.500 /
25.500 voor 1 / 2 / 4 apps. Replica: over een flip heen.

Lezing: op M40 haalt app naar app op een P-core 4350 MB/s waar M33 6379
deed, niet sneller dan op een E-core; Hop is daar nog 3.0.0. Rauw schrijven
en app-opslag schrijven liggen onder de v2-lat.

Nog te meten: *In rust* (na een Hop met telemetrie), de watchdogtoets.

Bord:
- Hop: ingebakken, gaat warm mee over flips (`HOPOS_HOP_RESUMED`).
- Warme flip: **gen 4 op M40, 04-10 (3 bewoners, 18 NAT-flows, Hop mee)**;
  gen 1 tot 8 op 01-10, adoptie 3 van 3. Koud niet (geen CPU_OFF).
- NIC: tg3 BCM57766 gepold (geen MSI). RNG: jitter.
- Opslag: ANS NVMe 414 GB, asynchroon met read-ahead sinds M22.
- Console op 5555: `hopos.replay=45`.
- Geïnstalleerd image: D4b. Er is een nieuw image met main nodig.

## QEMU virt

Opzet: 29-09, `sh tools/qemu-test-bench.sh`, TCG op een Mac M-serie, 4
cores, virtio-net via slirp, virtio-blk met een schijf van 64 MiB.

| Meting | Nu | Hoogste v3 | v2 |
| --- | --- | --- | --- |
| De node in, MB/s | 122,3 | 122,3 (29-09) | |
| De node uit, MB/s | 71,4 | 71,4 (29-09) | |
| rtt host naar app p50 / p99, µs | 1180 / 3472 | 1180 / 3472 (29-09) | |
| Verbindingscyclus p50, ms | 1,44 | 1,44 (29-09) | |
| Storm, conn/s (p99 ms) | 638 (2,7) | 638 (2,7) (29-09) | |
| App naar app in de node, MB/s | 62,9 | 62,9 (dbc522f) | |
| rtt in de node warm p50 / p99, µs | 115 / 340 | 115 / 340 (dbc522f) | |
| rtt in de node koud p50, µs | 375 | 375 (dbc522f) | |
| OS-core stil, wekken/s / lege RX-rondes/s | ~820 / ~94 | ~820 / ~94 (29-09) | |
| Rauw 1 MiB, schrijven / lezen, MB/s | 1260,9 / 1221,4 | 1260,9 / 1221,4 (29-09) | |
| hopfs 1 MiB, schrijven / lezen, MB/s | 1187,0 / 1207,2 | 1187,0 / 1207,2 (29-09) | |
| Sequentieel 32 MiB, schrijven / lezen, MB/s | 1293,0 / 1238,2 | 1293,0 / 1238,2 (29-09) | |
| Willekeurig 4 KiB, schrijven / lezen, IOPS | 32.051 / 32.123 | 32.051 / 32.123 (29-09) | |
| Multicast tussen twee apps, s tot groen | 4 | 4 (29-09) | |

App naar app met 16 MiB; OS-core stil: bewoners 0,3 %.

Lezing: QEMU bewijst de keten, geen plafond. Slirp termineert TCP op de
host, TCG rekent in software, en de vCPU's delen de host.

Bord: console via `ramfb`, USB via `qemu-xhci`.

## Wat nergens gemeten wordt

- De echo-rtt tijdens een kern-flip (v2, Pi 4: 4,3 tot 8,0 s).
  `tools/soak.sh FLIP=1` telt flips en overleving, niet de rtt.
- UDP over de draad. Vitals en bench hebben geen UDP-client.
- Energie: daarvoor is een meter nodig. Watchdog-herstel staat sinds 04-10
  per board in *Watchdog* (meetkern `wdtest`, de tijd van de console).
