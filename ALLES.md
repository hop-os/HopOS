# ALLES

Wat er nog moet, sinds de eerste boot op ijzer (30-09-2026). Alleen de
to-do's; wat af is gaat eruit. Stand van de borden in de tabel; de getallen
in docs/measurements.md, de details per board in docs/boards-*.md.

## De tabel

De afvinkmatrix van de Go-tijd (OLD/docs/support.md: boot, idle en klokken,
devices en diensten per board), nu voor v3 en bijgehouden op ijzer. Legenda:
✓ gezien op het board, ○ gebouwd maar op dit board nog niet gezien, ✗ ontbreekt
of faalt, en een streep waar het bewust niet komt. Stand 04-10-2026, ochtend (main 182480b; Pi 4 P40, Pi 5 Q40, Radxa X40, O6N O40, M4 M40 met Hop 3.0.0, LicheeRV R40, Altra A56g met de boot-stack-fix; release v3.0.13 (abi::sha256 uit leancrypto, lean 3.1.12, Dialer::last_error; de VPU-stroomcyclus van 3.0.12; config als 3.0.11, stateless als default), Hop in de boom op v3.0.13 (9ea8e47, nog geen Hop-release); de media op 3.0.11, de O6N te flippen naar 3.0.12).
Een cel zegt alleen of het slaagt, met hooguit de stempel of een paar
woorden waarom niet; een gepolde NIC is geen ✓. De getallen staan in
docs/measurements.md, de details per board in docs/boards-*.md.

| | QEMU virt | Pi 5 | Pi 4 | Radxa | Altra | O6N | M4 | LicheeRV |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Boot, EL2, kooi, zelftest | ✓ | ✓ P1, 03-10 | ✓ P1, 03-10; koud ✓ de VL805-firmware laadt (P1g) | ✓ X1, 03-10 | ✓ A3, 03-10 (eerste v3-boot; venster A000, 128 cores) | ✓ O2, 03-10 | ✓ M7 | ✓ R13, 03-10 |
| Hop als bewoner, welcome door de DNAT | ✓ | ✓ P1, in `system` | ✓ P1, in `system` | ✓ X1, in `system` | ✓ A3, in `system` | ✓ O2, in `system` | ✓ M1, HOPOS_HOP_RESUMED met HOPOS_PRIVILEGE | ✓ R10, 03-10 |
| Kern-flip, warm | ✓ | ✓ P2g tot P4g, 03-10; koud weigert vroeg en Hop zet de taken terug (P4, op ijzer) | ✓ I | ✓ I | ✓ A4g, 03-10 (gen 2, bewoners en NAT mee) | ✓ H; I ○ koude boot, fix de61b4b | ✓ M1, 03-10 (gen 2, Hop met bevoegdheid mee, spin door); koud – geen PSCI | – niet op riscv64; koud ✓ R10, 03-10 |
| NIC met interrupt | ✓ | ✓ | – gepold, zoals Go | ✓ | ✓ A8g, 03-10 (MSI-X via ITS 7 van zijn root-complex) | ✓ | ✓ M1, 03-10 (AIC 1253) | ✓ R13, 03-10 |
| Hardware-IRQ (NIC, kick, timer) | ✓ | ✓ | ✓, NIC gepold | ✓ | ✓ A8g (ITS, timer, NIC) | ✓ | ✓ M1 (timer-FIQ, fast IPI, NIC op de AIC) | ✓ R13, 03-10 |
| Gebruik per taak, van de kern en van Hop (cpu, geheugen; slot 0, systeemtaken) | ✓ arm64, 03-10 | ✓ P1, 03-10 | ✓ P1, 03-10 | ✓ X1, 03-10 | ✓ A3, 03-10 | ✓ O2, 03-10 | ✓ M1, 03-10 | ✓ R13, 03-10 |
| Off-link door de NAT, SNTP | ✓ | ✓ | ✓ | ○ niet nagelopen | ✓ A3 (welcome van GitHub, klok van Hop) | ○ niet nagelopen | ○ niet nagelopen | ○ niet nagelopen |
| Watchdog gewapend en geaaid | – geen watchdog in virt | ✓ P1, canary | ✓ P1, canary | ✓ X1, canary | ✓ A3, SBSA, canary | ✓ O2, canary | ✓ | ✓ R3, 03-10 |
| Hardware-RNG voor de kern | ✗ alleen jitter | ✓ | ✓ | ✓ | ✗ jitter: geen SMCCC-TRNG in de firmware, efi-rng hing (juli) | ○ wacht op de stick | ✗ alleen jitter | ✗ geen bron |
| Hardware-RNG voor de slots (CTRL_RNG_SEED, fc5348f) | ○ alleen jitter | ✓ P1, 03-10 | ✓ F | ✓ F | ○ jitter | ○ wacht op de stick | ○ alleen jitter | ○ niet nagelopen |
| Temperatuur in de tik | – geen sensor in virt | ✓ | ✓ | ✗ sensor converteert niet | ✓ A3, SMpro 47 C | ✓ | ✗ niet gebouwd | ✓ R5, 03-10 |
| Klokbeleid (dvfs) | – geen klok in virt | ✓ | ✓ | – klok van de firmware, bewust | – firmware-domein, bewust | ✓ | ✓ M7 | – vaste klok |
| Console op het glas | ✓ | ✗ firmware weigert sinds de herflash | ✓ | ✓ | ✓ A4, 03-10 (GOP 1024x768) | ✓ | – bewust uit | – geen scherm |
| USB xHCI (HID, display-app) | ✓ | ○ niets ingeplugd | ✓ P2g, 03-10 (enable slot met completion, d107b31) | ○ niets ingeplugd | ○ niets ingeplugd | ✓ | – niet gepland | – niet gepland |
| Opslag (hopfs, volumes, OP_SYNC) | ✓ | – bewust geen schijf | – geen schijf | – stateless, bewust | ✓ A4, 03-10 (NVMe SN770 500 GB, hopfs vers, commits) | ✓ | ✓ M24 | – geen SD-driver |
| Console op 5555 | ✓ | ✓ | ✓ | ✓ | ✓ A3 | ✓ | ✓ | ○ niet nagelopen |
| Hardwaredecoder (media-smaak) | – geen media-smaak | – geen media-smaak | – geen media-smaak | – geen media-smaak | – geen media-smaak | ✓ 30-09, weg tot de koude boot | – geen media-smaak | – geen media-smaak |
| Kaart of stick klaar in `target/` | – niets te flashen | ✓ P1 geschreven | ✓ P1 | ✓ X1 | ✓ A4 (gui) op de stick | ✓ O2 op de stick | ✓ M24 | ✓ R13 |

## Te doen

### Nu

- [ ] Hoofdstuk 8 van docs/description.md: alles geland behalve 8.7 en 8.8
      (arm64 en riscv naast elkaar, de assembly: d8-isa2.patch ligt klaar
      maar past niet meer op slots.rs na 8.10/8.11; na het weekend
      herbasseren, puur opruimen zonder gedrag). 3.0.10 draagt de rest; op
      ijzer te zien per bord bij de flip van 3.0.10 (bootregels gelijk,
      `HOPOS_SLOT_PUBLISH` voor Hop's poorten, `HOPOS_NVME_IRQ`).
- [ ] De media met `hop image --config` op v3.0.11 (Derek; Hop blijft 3.0.8,
      de ABI is gelijk). 3.0.11 vergeet standaard: `hopos.storage=stateless`
      in de config (sinds 07-11 image/cfg/headless.cfg en headfull.cfg)
      negeert bij een koude boot het indexbestand
      (`HOPOS_FS_STATELESS`); alleen de O6N-stick krijgt in zijn eigen laag
      `hopos.storage=stateful`, en een flip-landing blijft altijd stateful.
      Op ijzer te zien: de eerste koude boot van elk medium drukt
      `HOPOS_FS_STATELESS`, de O6N niet. Radxa-kaart
      (v3.0.5 stormt na 2 GiB door de MMC-maskers), Altra-stick (v3.0.7
      boot daar niet), M4 via Recovery (Hop is daar nog 3.0.0 en de leader
      kent geen jobs: spin opnieuw POSTen, daarna pas de watchdogtoets
      daar), de LicheeRV-kaart met `CFG=`. Daarna de twee soaks: spin op de
      M4, de Blu-ray-backup op de O6N.
- [ ] SQLite op HopOS (replica, lokaal 2f3fdb5, geen remote): stromen zit op
      230 tot 490 MiB/s op de M4, losse opzoekingen op één ronde per pagina,
      een commit op drie flushes. Volgende: batch-atomic in hopfs
      (`SQLITE_IOCAP_BATCH_ATOMIC`, hermappen via `pending`, één vastlegging
      bij OP_SYNC) haalt het journal en twee van de drie syncs weg; na het
      weekend. Replica docs/bench.md heeft de tabel.
- [ ] De nette stop (b1ae2a8, na 3.0.13): op ijzer te zien met een
      `DELETE /v1/jobs/welcome`: `HOPOS_SLOT_UNPUBLISH`, binnen 50 ms
      `HOPOS_APP_STOP grace_ms=2500`, `HOPOS_APP_SHUTDOWN`, `HOPOS_SLOT_STOPPED`,
      zonder FORCED of QUARANTINE (een app uit de apps-release van 3.0.13 kent
      de bel nog niet en gaat na 2,5 s via FORCED). Op de O6N met decode: geen
      "session slot 0 will not terminate" meer. Hop zelf: `select(app.stopped(),
      serve)` bij de volgende applib-bump; een jobspec-veld `stop_timeout`
      (TimeoutStopSec) is de stap erna. Stateless geldt nu ook op riscv
      (05aee54): de LicheeRV begint koud leeg, een flip blijft stateful.
### Fixen

- [ ] `HOPOS_IRQ_STUCK` wordt nergens gedrukt (hing aan het dode irq::run;
      docs/boards-pi.md belooft hem): één regel bij een storm die de ronde
      beëindigt en bij de eerste verdwaalde lijn.
- [ ] Een warme flip verplaatst Hop niet; na een flip met een andere groep
      zegt de bootregel `hop` terwijl Hop nog in `system` zit, en jobs met
      `sharegroup: hop` krijgen tot de volgende koude start een eigen core.
      De bootregel moet zeggen waar Hop echt zit.
- [ ] Hop: chunked transfer weigert; een plaatsing zonder capaciteit blijft
      proberen (ruis); na elke flip `HOP_STORE_KERNEL NEXT_STORE failed
      (timed out)` (04-10 op Pi 4, Pi 5 en M4).
- [ ] vitals: de standaard-rx-URL werkt niet (`CONNECT is not supported`).
- [ ] De O6N-stick staat nog op v3.0.16 met Hop 3.0.14: de node draait
      3.0.20 met Hop 3.0.18 via warme flips en `POST /restart` (24-10; de
      laatste flip haalde zijn bundel zelf van GitHub). Een koude boot valt
      terug op de stick; de stick naar de nieuwste release zodra hij er toch
      uit is.
- [ ] De M4 draait kern 3.0.20 via warme flips (24-10, de spin-jobs en
      haasd-test doorgedraaid), maar Hop is daar nog 3.0.9: die kent
      `POST /restart` niet, en een koude flip weigert de M4 (geen PSCI). Eén
      keer opnieuw installeren via Recovery met de stick van de nieuwste
      release (de jobspecs vooraf bewaren, de leader kent ze daarna niet);
      daarna gaat elke Hop-update op afstand onder de apps door.
- [ ] Het config-venster: de bootregel `HOPOS_CFG_WINDOW` per bord, een
      warme flip met `HOPOS_FLIP_CFG`, en `hop image --write` op een echte
      kaart (de LicheeRV eerst met `CFG=`, anders boot hij zonder config).
- [ ] De handoff-fix: het STALE-pad (een gepatchte stickkern die een oud
      paar vindt, dus pas met een 3.0.8-stick) en de nieuwe flip-ingang op
      de M4 en de Pi's (op de O6N gezien: O2 koud op met de doos van de
      toetskern, O2 warm naar main landt).
- [ ] O6N met `hopos.oscore=small`: de verhuizing van de kern naar een
      kleine core is nog nooit op ijzer gelopen (viel tot 0f03544 terug op
      een big core); verwacht `HOPOS_OSCORE_MOVE` naar core 2 en in de tik
      `stack_kb` rond 156. De M4 met `small` net zo.
- [ ] Altra: de koude flip (met de boot-stack-fix moet hij nu lukken).
- [ ] Core-reclaim in een sharegroup met een rekenaar
      (`HOPOS_CORE_RECLAIM` na 2 s).
- [ ] Radxa: de scrub van een Device-gemapte pool (twee keer dezelfde
      tamago-app); op de M4 op 03-10 gezien.
- [ ] Pi 4: HID en de display-app op de VL805.
- [ ] O6N: de mediaketen met Lumen (MMC, HEVC, WebDAV) na een koude boot,
      en daarna `DELETE`: een gewone job en een flipbundel moeten plaatsen
      (`HOPOS_HEAP_REFUSED` meldt het als de heap op is).

### Meten

- [ ] De meetronde (tools/meet) op 3.0.17 over alle borden: de zendring
      dropt niet meer (d217db7). Op de O6N gezien (24-10, A/B met twee warme
      flips, één stroom, 64 TX-plekken): naar de Mac over wifi 11,1 tot 11,8
      MB/s op 3.0.16 tegen 69,6 tot 72,9 op 3.0.17; bench push naar de M4
      over de draad 69,4 tot 69,7 tegen 117,6 tot 117,9 (de lijn), met
      txdrop=0 err=0 en full=589131 held=180796 xoff=97883. De ring van 256
      is dus niet nodig. De andere borden hebben ook 64 TX-plekken (igb, gem):
      daar dezelfde winst verwacht, nog niet gemeten.
- [ ] De hairpin-storm ligt sinds de middag lager dan vanochtend, ook met een
      stille node op 3.0.10: O6N 906 tegen 1983, Pi 4 1091 tegen 1942, Altra
      707 tegen 1000, Radxa 468 tegen 620 conn/s (tools/meet, 04-10 16:xx);
      de M4 doet 1954 zoals vanochtend. Welke middagcommit (hop-cost5 of de
      lees-bundel zaten al in de ochtendmeting; sindsdien de Dispatcher, de
      conport-pomp, Hop in een eigen groep, de drivers, oneshot, Doors)
      verlaagt de hairpin: bisect met `tools/meet.py --only o6n --skip-wire`.
- [ ] Pi 4 ontvangen 65 MB/s tegen zenden 113: de genet leest sinds bca6c70
      uit WB met een veeg en dat gaf niets (P70 64,8, daarvoor 52 tot 72),
      dus de rem zit in de gepolde RX-weg zelf (geen IRQ-lijn: de pomp pollt
      de ring), niet in de kopie. Meten waar de tijd per frame zit.
- [ ] De pull van de O6N naar Pi 5 en Radxa zakte van 111 MB/s (gisteravond
      en 08:45) naar 85 (vanaf 10:00) en bleef daar, ook na een koude boot
      van de O6N en met de oude bench-ELF aan beide kanten (A/B: nieuw 85,
      oud 87, gemengd 62 tot 76); de push de andere kant op blijft 110. Wat
      verandert er in de ontvangers of op de draad in die uren.
- [ ] De OS-core als grens voor veel kleine calls: meten met een echte
      database van 100 GB of meer.
- [ ] Radxa: de hairpin-storm haalt 620 tot 757 conn/s met p99 16 ms,
      tegen 1900 op de Pi 4: waarom.
- [ ] M4: app naar app op een P-core 4350 MB/s tegen 6379 op M33, en niet
      sneller dan op een E-core: waarom.

### Later

- [ ] Lezen door apps verder dan de bundel: een bundel antwoordt pas als
      zijn traagste lees terug is (5,3 keer de enkele per app met één
      bundel, 8,5 met twee); tien keer vraagt voltooiingen los van de call,
      zoals io_uring, met een ring. Pas als een werklast het vraagt.
- [ ] De SN770 van de Altra wordt na minuten willekeurig lezen over 1 tot 2 GB
      op het device trager (38 naar 70 us per lees), blijft zo na 150 s rust
      en herstelt bij een controller-reset (flip); de kern bleef 14 us.
- [ ] LicheeRV: de 16550-console wacht nog per teken (de PL011-borden
      hebben sinds de conport-pomp een niet-wachtende UART); een
      `write_nowait` dat tot de FIFO-diepte schrijft op THRE (Linux
      tx_loadsz). En de timer-overslaap p99 van 2,7 ms op de LicheeRV en 3
      tot 10 ms op de M4 is daarmee nog niet verklaard.
- [ ] OS-core WFE-pad: met een vitals tegen de applib van HEAD had 4 van 25
      runs één dial van 0,75 tot 1,3 ms: de kick van de app ging op +33 tot
      +82 us uit, de kern pakte de SYN pas een event-stream-periode later op
      (Pi 4, 04-10).
- [ ] Eerlijkheid op de OS-core: 4 KiB-calls van een buurman zakken van 82
      naar 15 tot 19 MB/s naast een bulk-app (M4, 01-10); er blijft
      voortgang, dus pas knippen (kleinere brokken met een yield) als het
      ergens knelt; eerst opnieuw meten op main met TURN_CAP.
- [ ] Pi 5: het glas (de firmware weigert elke framebuffer sinds de
      herflash, 0x80000001).
- [ ] Wachtpagina onder de boot-stack op UEFI en RISC-V (de overloop van
      302d257 schreef stil over .bss; na a6ceae5 is de diepste boot 156 KB
      van 256) en een wachtpost in qemu-test.sh; de device-op (19) zonder
      hosttests; de schijf-interruptlijn buiten QEMU virt.
- [ ] Radxa: TSADC geeft geen code (daardoor geen thermische rem op 1800 MHz;
      `hopos.mhz=1416` klemt); geen serienummer-terugval voor de MAC.
- [ ] LicheeRV: apps in groep `hop` op de C906L houden een device-staart
      (4,5 MB/s over de draad tegen 11,4 in `system`); gecachet kan pas als
      de app zelf cache-onderhoud doet.
- [ ] Altra RNG: de firmware heeft geen SMCCC-TRNG (TRNG_VERSION
      NOT_SUPPORTED, 03-10) en het EFI_RNG_PROTOCOL hing er in juli; blijft
      jitter tot iemand efi-rng daar met een tijdslimiet durft te proberen.
- [ ] Hop op de host: SIGTERM; de S3-lock met twee HopOS-nodes op ijzer.
- [ ] De plugins als node (hoplb, hopdns, hopprom,
      cloudflared-lean met een echt token) en Replica (`sqlite-persist`).

## Het plafond: Linux of macOS op dezelfde M4 tegen HopOS (01-10 avond, M33)

Uit reviews en Asahi-metingen, niet door ons gemeten; HopOS wel.

| pad | Linux/macOS op de M4 | HopOS M33 |
| --- | --- | --- |
| sequentieel lezen, rauw | 3000 tot 3500 MB/s | 1925 rauw, 1690 door de app |
| sequentieel schrijven, rauw | 2500 tot 3000 | 4950 rauw, 1270 door de app |
| willekeurig 4 KiB lezen, één tegelijk | 15.000 tot 20.000 per seconde | 11.900 |
| willekeurig 4 KiB lezen, met wachtrij, één proces | 100.000 tot 200.000 | 175.000 rauw in de kern |
| willekeurig 4 KiB lezen, door apps | 100.000 tot 200.000 (met page cache veel meer) | 25.000 met vier apps, 36.000 met acht |
| app naar app, één stroom (loopback) | 5000 tot 10.000 MB/s | 6380 |
| hete data uit RAM | page cache, tientallen GB/s | geen cache |

App naar app zit op het Linux-niveau, de schijf zelf halen we (175.000 is
het ijzer). Wat ertussen zit, de OS-core met ~27 µs per call, is de
resterende factor vier tot acht voor veel kleine leesopdrachten door apps,
naast de page cache voor hete data. Derek (01-10): "ik vind dit al
behoorlijk". Geen haast.
