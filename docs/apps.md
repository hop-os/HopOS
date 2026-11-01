# Apps schrijven voor HopOS

Een app is één ELF in één slot: een kooi met eigen partitie, een control-page
naar de kern, een netstack (Lean) op zijn eigen slot-IP, en een executor die
de taken van de app draait. `applib::main!` zet dat op; daarna is de app een
verzameling `async fn`-taken op die executor (het Rust-handboek, §2 en §4).
Dit document gaat over het ene ding dat een app goed moet doen om een goede
buur te zijn: **slapen op gebeurtenissen, niet op de klok.**

## Waarom dit het verschil maakt

Een slot krijgt zijn core van de kern. Idlet de executor (geen taak heeft een
gezet bit), dan geeft de slaper van applib de core terug met een wektijd: de
eerstvoorkomende echte timer. Op een eigen core is dat een `WFE`; op een
gedeelde core (`sharegroup` in de jobspec, of een board dat yield-idle
vraagt) is het een yield naar de kern, en de switcher geeft de core aan de
volgende bewoner. Bij de wektijd, of eerder bij de deurbel (een frame in de
RX-ring), komt de app terug.

De kern verdeelt de tijd dus eerlijk tussen bewoners die slapen. Wat hij op
arm64 niet kan, is tijd afpakken van een bewoner die niet slaapt: er is geen
preëmptie, een beurt duurt tot de executor idle is. Op riscv64 (de LicheeRV,
alle apps op één hart) neemt de switcher sinds 02-10 na 10 ms de beurt af
als er buren zijn; dat houdt de node bereikbaar, maar een app die daarop
leunt, verbrandt nog steeds de tijd van zijn buren. Daar volgen twee regels
uit:

1. Elk dutje (`after(5 ms)` om te kijken of er al iets is) is een wek van het
   slot, een volle ronde van alles wat klaarstaat, en pas daarna weer
   loslaten voor hooguit die 5 ms. Tien taken die zo dutten zijn tweeduizend
   wekken per seconde. Op een eigen core kost dat stroom; op een gedeelde
   core kost het de buren hun beurt.
2. Een lange synchrone berekening in één poll (crypto, een groot JSON-document
   serialiseren) houdt de core vast tot hij klaar is. De buren zien dan
   niets, ook niet hun hartslagen.

Hoe dat eruitziet als het misgaat, gemeten op de LicheeRV op 02-10-2026 met
drie Rust-slots in één sharegroup: een lege 404 van de controller duurde via
het LAN 0,2 tot 2 s en één keer meer dan 20 s; elke 8 tot 11 minuten verloren
tien plugins tegelijk hun hartslag (5 s) naar de controller. De Go-voorganger
op dezelfde node en dezelfde kern had daar nooit last van. Niet omdat Go
sneller is, maar omdat Go blokkeerde op het echte ding: een `Read`, een
`select` op channels, een timer voor de eerstvolgende deadline. De Rust-port
keek elke 1 tot 5 ms of er al iets lag.

## Welke core

Waar Hop zelf woont zegt de config (de bordlaag in `image/cfg`,
[boards.md](boards.md), "De config"): `hopos.hop.sharegroup=system` (de
OS-core naast de kern, de standaard waar de kern zijn core deelt) of een
andere naam, `hop` in de bordlagen (een eigen app-core; jobs met
`"tags":{"sharegroup":"hop"}` delen hem en krijgen op last hun deel van de
beurten, zoals in elke sharegroup). Hop krijgt die naam als
`HOPOS_HOP_GROUP` en telt haar vrij in zijn planning. Met een eigen groep
kiest `hopos.hop.core-class=small|mid|big` de soort core: de eerste core
van de groep komt uit die klasse als er een vrij is, anders uit elke
(`Placement::prefer` in `kern/src/pool.rs`; dezelfde namen als
`tags.core-class` van een job). Op de M4 is small een E-core, op de O6N
een A520; borden zonder klassen (de Pi's, de Radxa, de Altra, de LicheeRV)
nemen de eerste vrije core, en op de OS-core (`system`) kiest
`hopos.oscore` de core. Op de console: `HOPOS_HOP_GROUP`,
`HOPOS_HOP_CLASS` en `class=` achteraan `HOPOS_HOP_START`.
Een warme flip verplaatst Hop niet (`HOPOS_HOP_RESUMED`): de nieuwe kern
neemt zijn groep en core over, de bootregel zegt `(carried over the flip)`,
en vraagt de config van de nieuwe kern een andere groep, dan zegt
`HOPOS_HOP_GROUP_COLD` dat die pas bij de volgende koude start geldt.

Welke core een slot krijgt, kiest de kern (`kern/src/pool.rs`): zonder
`sharegroup` een eigen core (of `cores` aaneengesloten), met een
`sharegroup` de cores van die groep. De core van de kern zelf is ook
deelbaar, als groep `system`: de kern is er de vaste bewoner met voorrang
en een bewoner krijgt de core in zijn idle (PORT.md beslissing 2). Daar
komt een job met `"tags":{"sharegroup":"system"}` (één core, geen SMP), en
"waarever" ook een job zonder groep van één core die geen vrije eigen core
vindt en geen `core-class` vraagt die de OS-core uitsluit, met één regel:
`slot N: no free app core, joining the system group on the OS core
HOPOS_PLACE_SYSTEM`. Een job die een andere groep noemt, blijft bij zijn
groep. Op de M4 deelt de kern zijn core niet (de EL2-smaak van Apple), dus
daar is er geen `system`. Of Hop zo'n job uitstuurt, beslist Hop: hij plant
tegen `HOPOS_CORES` (de eigen app-cores die hij uitdeelt, min de zijne als
hij er een bezet; de OS-core telt niet mee), en `HOPOS_SYSTEM_CORE=1` zegt
hem dat de kern zijn core deelt. Dan kost een job in `system` hem geen core
en past hij altijd (geheugen telt wel), net als een job in `hop` op Hop's
eigen core (op elke node); een job zonder groep van één core zonder
`core-class` laat hij bij een volle node door naar de terugval, en telt hem
niet meer zodra de kern core 0 meldt. Met `HOPOS_SYSTEM_CORE` mag
`HOPOS_CORES` 0 zijn (de LicheeRV: Hop op de enige app-core); zonder maakt
Hop er minstens 1 van. Een weigering is één regel per job op de console:
`hop: job NAAM refused here: no capacity (cpu; ...) HOP_NO_CAPACITY`.

Naast de cores zegt de kern ook of het board koud kan flippen:
`HOPOS_COLD_FLIP=yes|no|fresh` (`hopos/src/flip.rs`, `COLD_FLIP`, dezelfde
waarde waarmee de kern zelf weigert). `no` op een board zonder PSCI (de M4):
de app-cores gaan niet uit. `fresh` op een board waar CPU_OFF niet terugkeert
(de Pi 5): koud alleen zolang er nog nooit een app-core draaide. Anders `yes`.
Hop beslist ermee vóór hij een taak stopt: bij `no`, en bij `fresh` zodra hij
ooit iets op een app-core zette (of zelf op een app-core woont), is een koude
flip meteen een 502 met "ask warm", zonder dat er een taak stopt.

## De regel

Een taak wacht op precies de dingen die hem werk kunnen geven, met één
`select`, en op niets anders:

```rust
loop {
    match select(self.stop.wait(), self.conn.readable()).await {
        Either::Left(()) => return,          // Drop ruimt op.
        Either::Right(Err(e)) => return self.fail(e),
        Either::Right(Ok(())) => {}
    }
    while let Some(frame) = self.read_frame()? {   // leest tot WouldBlock
        self.handle(frame).await?;
    }
}
```

Wat je daarvoor hebt, allemaal uit applib en de `sync`-crate:

| Wachten op | Bouwsteen |
| --- | --- |
| bytes op een TCP-verbinding, zonder ze al te lezen | `TcpStream::readable()` (de read zelf blokkeert ook netjes: `read()` wacht via de waker van Lean) |
| een datagram | `UdpSocket::readable()`, of `recv_from()` |
| een nieuwe verbinding | `TcpListener::accept()` |
| een bericht van een andere taak | `Channel<T, N>` en `Mailbox<T, N>` (`recv().await`) |
| een bel van een andere taak | `Signal::wait()` (level-triggered: tien bellen tegelijk zijn er één) |
| een echte deadline: hartslag, time-out, keepalive | `exec.after(d)` of `exec.until(at)` |
| een vangnet dat alleen telt zolang de core toch wakker is | `exec.after_deferrable(d)`: vuurt in de eerste ronde na zijn deadline, maar wekt een slapende core niet |
| twee of meer van het bovenstaande | `select(a, b)`, genest voor meer |
| de stop van de app | `app.stopped()` of `app.stop().wait()`: de stopbel, zie hieronder |

### De stop

Een `DELETE` van de job is geen kill: de kern zet de kill-vlag met een
termijn erin (Hop vraagt 3 s), en applib maakt daar de stopbel van de app
van, `app.stop()` (een `sync::Stop`, de tegenhanger van `close(stop)` en
`context.WithCancel`). De app heeft dan die termijn min 500 ms (het
net-afscheid en één hartslag) om af te ronden: de database dicht, de
sessie beëindigd, de tunnel dicht, een laatste regel. Tijdens die gratie
heeft hij zijn net en de system-API nog. Keert `main` terug, dan doet de
main-schil het net-afscheid (elke verbinding dicht, elke FIN bevestigd) en
de exit. Twee regels in de hoofdlus zijn genoeg:

```rust
async fn app_main(app: &'static App) {
    let db = Db::open(app).await;
    // De hoofdlus tot de kern vraagt te stoppen.
    select(app.stopped(), serve(app, &db)).await;
    db.close().await;                     // de opruiming, binnen de gratie
}                                         // terug: applib doet shutdown(0)
```

Op de console: `HOPOS_APP_STOP grace_ms=2500` bij het verzoek,
`HOPOS_APP_SHUTDOWN` bij het afscheid, en `HOPOS_SLOT_STOPPED` als de kern
de partitie vrijgeeft. Een app die de bel negeert, krijgt na de gratie
`HOPOS_APP_STOP_FORCED` en hetzelfde afscheid; wie ook dan niet parkeert,
wordt na de termijn ingetrokken (`HOPOS_FATAL` als zelfs dat niet pakt: de
kern kan de isolatie dan niet bevestigen, en de watchdog reset de node). Een `stacktask` breekt af door hem te droppen: `select(app.stopped(),
task)` geeft de C-kant `Cancelled` bij zijn volgende `wait`.

`after(d)` is dus een deadline, geen peiling. Als je `after(d)` in een lus
schrijft met een `d` kleiner dan de termijn die je werkelijk bewaakt, kijk je
rond: dan ontbreekt er een `readable()`, een `Signal` of een `Channel` in de
`select`.

### Lange berekeningen

Een handshake, een PBKDF2, een document van een megabyte: knip het op en zet
`yield_now().await` tussen de stukken. Een netwerkrondreis is al een
`.await` en dus al een knip; de rekenstap ertussen hoort kort te blijven. De
maat: één poll van één taak blijft onder een milliseconde op het traagste
board waar de app voor bedoeld is. Wat langer duurt, is werk voor een
`stacktask` (zie [stacktask.md](stacktask.md)) of voor meer `yield_now`.

### Pools, geen taak per verbinding

Het aantal taken staat vast bij de compiler (het handboek, §2). Een werker
wacht achter zijn deur; een acceptor deelt uit (`sync::Doors`, zoals in
welcome, bench en vitals). Een pool die vol is, laat de verbinding wachten
in de accept-wachtrij, en de acceptor wacht op de eerste werker die zich
vrij meldt (`Doors::place`); hij gaat niet rondkijken of er al een werker
vrij is.

### Veel kleine lezingen: bundelen

Elke call naar de kern is een rondreis door de OS-core en houdt één
opdracht in de lucht; een SSD haalt zijn snelheid pas met veel tegelijk.
Wie veel kleine lezingen doet (een database, een index), bundelt ze, zoals
io_uring: `Client::read_many(path, &mut ops)` zet tot `sys::MAX_READS` (16)
lezingen uit één bestand in één call (`OP_READ_MANY`), de kern zet ze in één
batch op het device en antwoordt één keer, elke lees in haar eigen buffer
met haar eigen uitkomst (`ReadOp::got`; een blokfout in de ene laat de
andere staan). Een bundel wacht op zijn traagste lees: wie de schijf vol wil
houden, houdt twee bundels in de lucht met een tweede `Client` (een eigen
verbinding; een app heeft er twee) en wacht ze samen af. GEMETEN 04-10 op
de Altra (SN770, vitals `rand` met `depth` en `bundles`): één app 18.800
lezingen per seconde één voor één, 100.000 met bundels van 16, 159.000 met
twee bundels; vier apps met bundels van 16 samen 249.000, acht 305.000
(één voor één: 54.000 en 85.000). Op de M4 (ANS): één app 8.400, 56.000 en
80.500; zes apps met bundels van 16 samen 171.000.

## De meetlat

Zonder cijfers is "de app slaapt" niet te onderscheiden van "de app dut".
Elke app publiceert twee tellers op zijn control-page, zonder er iets voor te
doen: `CTRL_IDLE` (de ticks van de architectuurteller die de slaper weg was,
alle cores van de app bij elkaar) en `CTRL_WAKES` (hoe vaak de slaper is
aangeroepen). De kern geeft ze in `SLOT_STATUS`, en Hop rekent: het
cpu-percentage van een taak in zijn API (`cpu_percent` per taak,
`kern_cpu_percent` en `hop_cpu_percent` per node in `GET /v1/agents`) is
"100 min idle" over zijn eigen poll-interval. Dat is de enige meetlat; de
consoleregel `HOPOS_SLOT_LOAD` van de kern rekende hetzelfde nog eens en is
op 01-11 weg (sequence-hopos.md §14.6).

Lees de twee zo:

- **idle** is de tijd die de app de core niet nodig had. Op ijzer zit een
  stille app boven de 95%; een app die werkt zakt, en dat hoort: de vraag
  is of hij weer stijgt als het werk klaar is. De norm geldt alleen op
  ijzer: op QEMU-TCG is `WFE` een no-op en meet een eigen core ~0%, dus een
  QEMU-run is op dit getal nooit rood.
- **wakes** is hoe vaak de app wakker werd. Een stille app met één hartslag
  van 50 ms (de `watch`-taak van applib) en een paar echte timers zit in de
  tientallen per seconde. Honderden per seconde zonder verkeer is dutten;
  duizenden is rondkijken.
- Op QEMU-TCG zegt daarom alleen `wakes` iets, behalve op een gedeelde
  core: daar telt de yield wél als idle, ook onder QEMU
  (`tools/qemu-test-share.sh`).
- De kern zelf staat er als slot 0: `SLOT_STATUS` van slot 0 geeft hem als
  levende app op de OS-core met één core, met de slaaptijd van zijn executor
  (dezelfde als `busy_ms` van `HOPOS_TICK`) als idle, de heap in gebruik als
  geheugen en de tik als hartslag. Zo toont Hop per node ook de kern. De
  beurten van Hop op de gedeelde OS-core tellen daar als idle van de kern;
  ze staan in slot 1.

In de app zelf staat hetzelfde fijner: `EXEC.get().stats` (`rounds`, `polls`,
`sleeps`, `slept_ns`). Een app die een eigen diagnoseregel heeft, zet die
getallen erin.

## Het Go-equivalent, voor wie van daar komt

| Go op tamago | Rust op applib |
| --- | --- |
| `conn.Read(buf)` blokkeert de goroutine | `stream.read(&mut buf).await`, of eerst `stream.readable().await` |
| `select { case <-ch: case <-time.After(d): }` | `select(rx.recv(), exec.after(d))` |
| `time.Sleep(d)` | `exec.after(d).await`, en alleen als `d` een echte termijn is |
| een goroutine per verbinding | een werker uit een vaste pool |
| `runtime.Gosched()` | `yield_now().await` |
| de runtime parkeert als alles blokkeert | de executor idlet als geen taak een bit heeft; applib yieldt of `WFE`'t |

Go's runtime deed het loslaten voor je: zolang je blokkeerde, sliep de app.
Hier is het hetzelfde, met één verschil: een `after(1 ms)` is in Go ook
mogelijk, maar daar schrijft niemand hem omdat `Read` blokkeert. In Rust ligt
`poll::once(read)` plus `after(1 ms)` voor de hand als je een niet-blokkerende
lus vertaalt. Dat is de valkuil, en `readable()` is de uitweg.

## Lessen uit de Stulp-port (3 en 4 oktober 2026)

Stulp (de huisautomatisering, tien plugins in één bundel-slot plus de
controller in een tweede) ging in één dag van "alles is traag, 502's,
Matter valt om" naar stabiel op de LicheeRV. Slapen op gebeurtenissen was
de eerste stap. Wat daarna nog misging, en geen van die dingen was de
scheduler:

**Een late hartslag is niet dood.** Een plugin die na 5 s zonder antwoord
de verbinding sloot, kostte bij het herverbinden ~70 inits en tientallen
Matter-handshakes; die kosten maakten de volgende hartslag weer te laat.
Een gemiste hartslag is nu een logregel; pas na 5 minuten volledige stilte
gaat de verbinding dicht. Hoe duurder herverbinden is, hoe ruimer de grens.

**Geef de executor terug, ook als alles klaar is.** Een protocollus waarvan
elke `.await` meteen `Ready` is (een volle inbox, een kanaal met werk) yieldt
nooit vanzelf. De SDK van Stulp geeft na 32 beurten verplicht
`yield_now()` (het coöperatieve budget), los van wat er nog ligt.

**Meet per taak, niet per slot.** De idle van het slot zei "slot 3 is druk";
pas een meting per plugintaak (`busy_ms` per taak en een regel voor elke
poll boven 200 ms, met de naam van de taak) wees de Matter-plugin aan met
polls van ~290 ms, honderden keren. Meet ook wat vóór het werk gebeurt: de
wachttijd van een verbinding in de rij voor een werker stond nergens, dus
een volle pool was onzichtbaar.

**Kopieer geen hele staat per gebeurtenis.** JSON van een plugin-staat naar
tekst en terug kostte op de C906 ~300 ms, en de pool deed dat twee keer per
lampcommando, voor elke werker die achterliep. Geef werkers de wijziging
door (meestal één apparaat), niet de staat. Let ook op de bouwstenen:
`json::set` bouwde het object opnieuw en kopieerde elk ander veld met alles
eronder, dus één apparaatupdate kopieerde de hele staat. Stulp wijzigt nu
ter plekke via `Object::get_mut`, `insert` en `remove`, voorlopig als
overlay op `types` van Hop; die horen in Hop zelf.

**De allocator telt mee.** Een first-fit-lijst met duizenden kleine vrije
blokken maakte elke allocatie een wandeling: 505 ns per vrijgave plus
allocatie bij 4547 vrije blokken, en op de node bootstraps van 250 ms.
Exacte klassen tot 1 KiB met een bitmap van niet-lege klassen maakte het
53 ns ([heap.md](heap.md), de commits van 03-10).

**Handshakes zijn duur; deel ze.** Op de C906 kost een Matter-CASE 0,7 tot
1,7 s (P-256) en een TLS-handshake ~100 ms (na de snellere AES en GHASH in
leantls: seal van 10 naar 232 MB/s). Drie gevolgen:
- Eén sessie per apparaat. Met een sessie per werker betaalde het eerste
  commando naar een lamp op elke werker eerst een volle handshake, en
  verdrongen de sessies elkaar op het apparaat; de Go-versie deelde er één,
  en voelde daarom direct. Zie "Eén eigenaar per peer" hieronder.
- Hergebruik verbindingen. Een HTTPS-client die elke poll opnieuw
  handshaket (TaHoma: 134 keer in een kwartier) verbrandt rekentijd.
- Een onbereikbaar apparaat krijgt een backoff (Stulp: 1 minuut,
  verdubbelend tot 30 minuten), geen poging per minuut met een time-out
  van 40 s.

**Eén eigenaar per peer.** Werk parallel per peer (een apparaat, een
server), niet per werker. Geef elke peer één vaste taak (bijvoorbeeld zijn
id modulo het aantal werkers) die zijn verbinding of sessie bezit en al het
verkeer ernaartoe doet: abonnement, herverbinden en verzoeken. Verzoeken
naar verschillende peers lopen dan nog steeds tegelijk. Stulp deed eerst een
sessie per werker en betaalde zo bij elk eerste Matter-commando een
handshake; met een eigenaar per node had geen enkel commando er nog een.

**Geef door vóór je op een bevestiging wacht.** Een betrouwbaar protocol
(MRP, een eigen ACK-laag) bevestigt je antwoord pas na een rondreis. Wacht
daar niet op voordat je de ontvangen gegevens verwerkt: verstuur het
antwoord, geef de gegevens door, en laat de bevestiging op de achtergrond
binnenkomen. Stulp wachtte na elk Matter-rapport op die ACK (op Thread 550
tot 600 ms), en elke melding, ook een beweging, liep zoveel achter.

**Meer parallel is niet gratis.** Werkers op één executor verdelen het
wachten, niet de rekentijd: wat elke werker per gebeurtenis kost, betaal je
zo vaak als er werkers zijn. In Stulp startten
acht werkers per rapport een taak die met een kopie van de staat begon, en
zakte het slot van boven de 90% naar 11 tot 40% idle. Meet de idle van het slot
opnieuw na elke stap naar meer gelijktijdigheid; doe kort werk direct en geef
alleen lang, afbreekbaar werk een eigen taak.

**Meet een keten per schakel.** Een vertraging die de gebruiker voelt, zit
zelden op één plek. Geef elke schakel een tijdregel (wachten in een rij,
verwerken, verbinden, de rondreis, het doorgeven), dan krijgt elk vermoeden
een getal. Bij Stulp ("beweging naar lamp duurt 5 s") viel de rij zo af
met 0 ms, en bleken de handshake (0,7 tot 1,7 s) en het wachten op een
ACK (~600 ms) de grote posten.

**Laat levenscyclus geen lopend werk afbreken.** Het onderhoud van Matter
werd door elke `device.init` geannuleerd, midden in een handshake (137 van
173 handshakes na een start). Wacht tot de start stil is (Stulp: 3 s zonder
init), en maak een init geen barrière voor commando's.

**Elk faalpad een logregel.** Het camerabeeld was een stille 502:
`leanhttp::get` eist een `Content-Length`, en een livestream of een bron
die na het antwoord sluit heeft die niet (gebruik `leanhttp::fetch` en
controleer de status zelf). Zonder regel zag de console alleen een
geslaagde callback en daarna niets.

**Een volle pool laat wachten, begrensd.** Een verbinding weggooien als alle
werkers bezet zijn, is een 502 voor de browser. Een rij ervoor is beter,
maar niet langer dan de klant zelf wacht: de tunnel geeft na 95 s zonder
antwoordkop op, dus Stulp sluit wat langer dan 90 s in de rij lag.

**Timers zijn schaars.** Een executor heeft er 32 (`applib::rt`, `TIMERS`).
Een acceptor of werker die altijd een timer open heeft, eet daarvan; gebruik
er alleen een zolang er iets te bewaken is.

**Een sluitrace is geen fout.** Een write of close voor een stream die al
weg is, is normaal na een herverbinding. Stulp maakte daar een fatale fout
van, en de UniFi-plugin viel elke keer om.

**Kies de core met een meting.** Op de LicheeRV is de grote core die van de
kern (`system`); de bundel met plugins daar had de helft over, verplaatst
naar de kleine core zat hij op 100%. Kijk naar de idle van het slot vóór en na
een verhuizing, niet naar wat de naam van een hart doet vermoeden.

## Checklist voor de review

- Geen `after(d)` in een lus met een `d` korter dan de termijn die de lus
  bewaakt. Elke `after` noemt in zijn commentaar welke termijn hij is.
- Elke lus die op I/O wacht, wacht via `readable()`, `accept()`, `recv()` of
  `Signal::wait()`, niet via een niet-blokkerende poging plus een dutje.
- Een synchrone stap die op het traagste board meer dan een milliseconde
  kost, heeft een `yield_now` of een `stacktask`.
- Het `cpu_percent` van de taak bij Hop op een stille node: onder de 5%.
  Zet dat getal in het testlogboek van de app.
- Op een gedeelde core: de 404 van de buurman blijft onder de 50 ms terwijl
  jouw app werkt.
- Een gemiste hartslag sluit niets af; een herverbinding is duurder dan
  wachten.
- Geen volledige kopie van een groot document per gebeurtenis; werkers
  krijgen de wijziging.
- Elke taak heeft een eigen meting (bezette tijd, lange polls met naam), en
  elke wachtrij meet zijn wachttijd.
- Elke foutstatus die de app teruggeeft, heeft een consoleregel met de reden.
- De hoofdlus wacht ook op `app.stopped()`, en wat open is (een database,
  een sessie, een tunnel) gaat dicht vóór `main` terugkeert.
- Sessies en verbindingen naar hetzelfde apparaat worden gedeeld en
  hergebruikt; een onbereikbaar apparaat krijgt een backoff.
- Elk apparaat (peer) heeft één eigenaar-taak; parallel per apparaat, niet
  per werker.
- Gegevens gaan door zodra ze binnen zijn; een protocol-ACK wordt niet
  synchroon afgewacht.
- Na meer gelijktijdigheid: het `cpu_percent` opnieuw gemeten.
- Een keten met merkbare vertraging heeft een tijdregel per schakel.

Zie ook: het Rust-handboek (`rustdoc/README.md`, §2 Taken en §4 De executor),
[stacktask.md](stacktask.md) voor synchrone code op een eigen stack,
[go-apps.md](go-apps.md) voor apps die in Go blijven, en
`tools/qemu-test-share.sh` voor de rotatie op een gedeelde core.
