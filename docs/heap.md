# Gedeelde allocator, 1 oktober 2026

`heap` bevat de allocator met vrije lijsten en grenslabels die eerder in
`applib::heap` stond. Apps leveren hun SMP-core-identiteit; `board::heap`
gebruikt dezelfde implementatie voor de kern. Alleen de primaire kernexecutor
alloceert; ISR's en geparkeerde secundaire cores doen dat niet.

Aanleiding: tijdens de Lumen-proef op de O6N stopte OP_SYNC met
`out of memory (483328 bytes)`. De oude kernallocator nam uitsluitend het
laatst uitgegeven blok terug. Bij door elkaar lopende metadata-, netwerk- en
I/O-allocaties bleef vrijgegeven geheugen bezet. De TCP-console viel daarna
ook uit. Alleen meer kern-RAM geven lost die groei niet op.

De gedeelde allocator voegt aangrenzende vrije blokken samen en respecteert
het bestaande RAM-plafond. Uitlijning is begrensd op 4096 bytes, zoals in de
app-allocator. Er is geen tweede kopie van het algoritme. Het publieke
`applib::heap::Heap`-contract blijft via een type-alias bestaan; de globale
allocator blijft eigendom van de betreffende binary.

Validatie (01-10): tien allocatortests, inclusief 80.000 deterministische gemengde
bewerkingen, vier gelijktijdige threads en vrijgave door een andere thread.
De board-regressietest herhaalt 2.000 metadata/netwerkcycli in 4 MiB; na elke
cyclus is alles terug en alle 6.000 allocaties slagen. Board en applib-tests
slagen, evenals Clippy. Lumen draait met deze kernel en app in QEMU: 43
controles voor HTTP, WebDAV, beheer en persistentie na recreate slagen.
Ook 187 kern-tests plus twee doc-tests slagen. De QEMU-warmflip behoudt Hop,
een bestaande uitgaande TCP-verbinding, alle vier NAT-flows en de HopFS-generatie;
na de flip kan hij opnieuw een app plaatsen. De rebootproef vindt de zwarte doos.
Die flipproef gebruikte `HOP_PATCH=0 HOP_DIR=/tmp/lumen-hop-hardware` om de
bekende Hop-ELF te behouden; de reguliere patch-helper heeft met deze oude Hop-pin
een Lean-versiemismatch en is niet voor deze proef gebruikt.

Op de fysieke O6N had de oude kernel zijn heap al uitgeput toen de fix klaar
was. Na een fysieke herstart zijn de bundels `lumen-heap` en vervolgens
`lumen-heap-recover` bevestigd geland. Lumen-kandidaat 15 hervat vanaf een
geverifieerd checkpoint en schrijft weer nieuwe beelden naar de NVMe.
De tweede flip liet de bestaande VPU-herstelroutine een vastgelopen sessie
opruimen. Langdurige hardwarevalidatie van de geheugenfix loopt nog.

## Sinds 1 oktober

- **Exacte klassen** (9107dd6, 03-10). Tot en met 1 KiB (`SMALL_LIMIT`)
  heeft elke blokmaat een eigen vrije lijst, per korrel van 16 bytes;
  daarboven één klasse per macht van twee. Een bitmap van niet-lege klassen
  laat het zoeken meteen naar de eerste klasse springen die groot genoeg
  is, en `walk` controleert die bitmap mee. Aanleiding: met alleen klassen
  per macht van twee liep een vraag van 48 bytes langs elk vrij blok van 32
  in dezelfde lijst. JSON-werk laat daar duizenden van achter; Stulp mat op
  de host 505 ns per vrijgave plus allocatie bij 4547 vrije blokken, en op
  de LicheeRV werd dat een bootstrap van 250 ms en callbacks van een halve
  seconde. Nu is dat een lijstkop pakken.
- **Het plafond is het gebied.** `Heap::set_ceiling` en `cpu::memlimit`
  zijn weg (e97b4fc, 01-10): `init` zet het plafond op de maat van het
  gebied, en een lagere grens bestaat niet meer. De grens van een app is
  zijn partitie.
- **Toetsen** (24-10): `cargo test -p heap` draait negen toetsen en twee
  metingen die alleen op verzoek lopen (`json_like_churn_cost` en
  `stulp_shape_cost`, `#[ignore]`, in release). De regressietest van
  `board::heap` (2.000 cycli in 4 MiB) staat er nog.

Herhalen:

```sh
cargo test -p heap -p board -p applib
cargo clippy -p heap -p board -p applib --all-targets -- -D warnings
GUI=1 HOPOS_STAMP=lumen-heap FEATURES=media sh image/flip-bundle.sh o6n
```
