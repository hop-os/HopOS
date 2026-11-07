# HopOS v3: de volgorde en de afwegingen

Wat HopOS doet, in de volgorde waarin het gebeurt, met per stap waarom het
zo is en of de volgorde verplicht is. Het doel is het ontwerp na te lopen:
is elke keuze logisch, en waar kan het simpeler. Dit is deel 1 van drie:
eerst HopOS (deze repo), dan LEAN, dan HOP.

Stand: main op 2d1ed1f (24-10-2026). Uit de code gelezen, niet uit de
andere docs; waar die afwijken, staat het in §12. Regelnummers verschuiven,
dus de verwijzingen noemen pad en functie.

**Buiten beschouwing:** de drivers (`driver/*`) en de boards voor zover ze
ijzer bedienen. Ze komen alleen voor waar het contract met de kern loopt. De
TCP/IP-stack (leannet) en Hop zelf komen in deel 2 en 3; hier alleen hoe de
kern ze gebruikt.

**Hoe te lezen.** Elke sequence is een genummerde lijst in de echte
volgorde. Bij een stap staat:

- **Waarom**: de reden uit de code of de commit (met datum of hash). Staat
  er geen, dan zegt de stap dat: *geen reden gedocumenteerd*.
- **Verplicht**: de volgorde moet zo, met wat er misgaat als je hem omdraait.
- Een verwijzing als **[F3]** of **[S7]** wijst naar de lijst in §11
  (F = mogelijke fout, S = kandidaat om te schaven).

Zekerheid in §11: **H** = beide kanten in de code gelezen, **M** =
waarschijnlijk, **V** = vermoeden. "Zelf getoetst" betekent dat de bewering
na het lezen nog een keer apart tegen de code is gehouden.

## Inhoud

1. Het hele verloop op één pagina
2. Stroom aan tot `kmain`
3. `kmain` tot de executor draait
4. Eén ronde van de executor, en de OS-core als gastheer
5. Hop: plaatsen, bewaken, herstarten, vallen
6. Een app van `START_SLOT` tot zijn eerste instructie
7. Een app draait: net, system-API, opslag, slaap
8. De stop
9. hopfs: mount, call, commit, sync
10. De kern-flip
11. Te wegen: fouten en kandidaten
12. Docs en commentaar die achterlopen
13. Het voorstel: wachten, bellen, hard falen, LEAN
14. Het doel: het HOK en het NETWERK (besluiten van 01-11, nulmeting, werklijst)

---

## 1. Het hele verloop op één pagina

```
firmware ─► boot-stub (cpu::boot | uefi entry | apple head | riscv boot)
            stack, BSS, [vectoren, EL2-regime, MMU]
         ─► kmain: console, bunny, EL2-toets, heap, feitenpagina, discover
         ─► move_to_os_core  (start de OS-core, boot-core wacht in el2::hold)
         ─► setup  (in een eigen frame dat weg is vóór de executor draait)
              flip::land ─ klok ─ conport ─ wandklok ─ zaad ─ Privilege
              opslag (probe, mount, actor, committer) ─ codec ─ System
              interrupts ─ tick ─ watchdog ─ telemetrie ─ load
              node-config ─ NIC (pomp, switch, host-taak: DHCP → stack → listeners)
              USB ─ flip-taak ─ slots (kooi, lifecycle, servicers, supervisor Hop)
              OsCore + zelftest ─ sleeper.host
         ─► exec.run(doorbell(sleeper))
              idle = beurten voor Hop (en groep `system`) op de OS-core
```

Daarna gebeurt alles als taak op de executor van de OS-core. Er is één
executor in de kern en één per core van elke app. De app-cores draaien geen
kern-code. Ze draaien alleen de switcher, die in de plan-regio staat.

De vier dingen die overal terugkomen, en die in de stappen hieronder steeds
de reden zijn:

1. **Eén eigenaar per staat.** Die eigenaar is een taak. Wie iets wil,
   stuurt een bericht (rustdoc §1).
2. **Alles faalbaar vóór het startschot, niets erna.** Een start die faalt
   vóór de dispatch draait terug. Een dispatch met onbekende uitkomst gaat
   in quarantaine.
3. **Luid falen, en de watchdog reset.** De kern reconcilieert geen staat
   (`kern/src/slots.rs`, moduledoc). Een falende Hop is een falende node
   (23-10).
4. **Een gebeurtenis, geen klok.** Taken slapen op een bel of een deadline.
   Waar dat niet lukt, is het een kandidaat in §11.

---

## 2. Stroom aan tot `kmain`

Er zijn vier ingangen. Alle vier eindigen in `kmain(x0, el)` in
`hopos/src/main.rs`. Een flip komt door dezelfde ingang binnen als stroom
aan, met x0 = de feitenpagina op de boot-scratch (O6, 01-11,
`docs/flip.md`): de handoff, of nul, en wat de firmware bij stroom aan gaf.
`kmain` leest de pagina vóór `discover` (`flip::facts`), en bouwt hem bij
stroom aan opnieuw, met handoff nul. Een koude boot is een boot zonder
handoff.

### 2.1 `cpu/src/boot.rs` `_start` (qemuvirt, rk3566; de Pi via `_pi_start`)

1. **De poort** (`admits`). Een core met affiniteit ≠ 0 parkeert voorgoed
   in een wfe-lus, zonder stack. Behalve als x0 = de feitenpagina
   (`__hopos_facts`, een absoluut symbool van de binary): een
   register-vergelijking, geen lees.
   - **Waarom:** een firmware die alle cores loslaat, geeft zo toch één kern.
     Een flip komt van de OS-core, en die is niet per se core 0 (29-09).
     Geen firmware geeft dat adres in x0 (de DTB, of 0).
2. x20 = x0, x21 = CurrentEL, sp = `__stack_top`, BSS op nul.
3. Is het EL niet 2, dan meteen naar `kmain`. Die meldt het luid en
   parkeert.
4. VBAR_EL2, HCR_EL2 (RW|IMO|FMO|AMO), MAIR, TCR (39 bit, 4 KB).
   - **Waarom IMO:** zonder IMO wordt een IRQ op EL2 nooit genomen.
   - De plek van VBAR (vóór de MMU; UEFI en Apple doen het erna): *geen
     reden gedocumenteerd* [S1].
5. **De wachtpagina onder de stack**, met de MMU nog uit.
   - **Waarom:** een stackoverloop schreef stil in `.bss` (5682072, 30-09).
   - **Verplicht:** vóór SCTLR.M=1, want dan is break-before-make niet nodig.
6. TTBR0, `tlbi`, SCTLR (M|C|I|SA), `kmain`.

De Pi heeft een eigen stap ervóór, `board/raspi/src/arch.rs` `_pi_start`:

- x0 bewaren in x20 (de DTB, of na een sprong de feitenpagina), en
  ongeschonden door naar `_start`.
- Een 'P' op de UART.
- De Linux-`init_el2`-set: MDCR, CPTR, HSTR, VTTBR=0, CNTHCTL, CNTVOFF.
- `dc ivac` boven 0x80000.
  - **Waarom:** schone firmware-regels zouden de walker na MMU-aan voeden.
  - **Verplicht:** vóór de MMU.

### 2.2 UEFI (generiek, O6N, Altra): `board/uefi/src/entry.rs`

1. **x0 = de feitenpagina is een flip** (geen ImageHandle: de pagina ligt
   in het kernvenster dat de stub pas daarna alloceert). Dat wordt getoetst
   vóór de eerste store, omdat de stack van de oude kern op onze code kan
   liggen.
2. **Koude weg:**
   1. Zelf-relocatie, dan de BSS.
   2. `hopos_efi_main`, met de boot services nog levend: console, de
      firmware-watchdog uit, `prepare`.
   3. `prepare` doet, in deze volgorde: GOP, ACPI, de feiten, het
      kernvenster `allocate_at`, `hopos.cfg`, de staging en de memory map.
   4. `build_map` maakt een 48-bit identity map.
   5. `go`: de console naar stil, `ExitBootServices`, de feitenpagina
      publiceren (magic als laatste), de tabellen naar DRAM.
   - **Waarom de console stil:** een print kan alloceren en dan wordt de
     MapKey ongeldig.
   - **Waarom eigen relocatie:** EDK2 image-protection en een lege
     relocatie-directory.
3. **`uefi_enter_kernel`:**
   1. DAIF dicht, MMU uit.
   2. **HCR vóór TCR/SCTLR.** **Verplicht onder VHE:** E2H bepaalt de
      lay-out van die twee.
   3. MDCR, HSTR, CPTR, CNTHCTL, CNTVOFF.
   4. Afhankelijk van de feature-registers: FGT, HCRX, MPAM, ICC_SRE en
      ICH_HCR.
   5. MAIR, TCR, TTBR0, SCTLR, VBAR, sp.
   6. `kmain(0, el)`: er is geen DTB.
4. **Flip-weg:** relocatie en BSS, de magic van de UEFI-feiten
   (`FLIP_FACTS_PA`), en het regime van de koude boot terugzetten. Dan
   dezelfde enter. Na MMU-aan zet `hopos_efi_flip_facts` de statics terug,
   en `kmain` krijgt de feitenpagina in x0.

UEFI heeft geen wachtpagina onder de stack. Dat is gedocumenteerd in
`board/uefi/src/boot.rs` [S2].

### 2.3 Apple (Mac mini M4): `board/apple/src/head.rs`

1. **`stub_reset`:** elke core uit reset wacht in WFE op een brievenbus met
   zijn affiniteit, en bevestigt met -1.
   - **Waarom -1:** E-core 0 heeft affiniteit 0.
   - **Waarom geen exclusives:** met de MMU uit zijn die onvoorspelbaar.
2. **`stub_entry`:** iBoot springt op een willekeurig adres. De stub kopieert
   zichzelf naar het linkadres, met `dc civac` op bron en doel.
3. **`_start_apple`:**
   1. BSS en stack vegen (de firmware schreef met caches aan), BSS op nul.
   2. HCR met E2H (dat is daar RES1), CNTHCTL.
   3. `apple_early` met de MMU uit: na een sprong de boot_args uit de
      feitenpagina, de dockchannel en de map met wachtpagina.
   4. MAIR, TCR, TTBR0, SCTLR, VBAR, `kmain` met de x0 van de ingang.

### 2.4 riscv64 machine mode: `cpu/src/riscv/boot.rs`

1. Elk hart: `mie`=0, `mtvec`, het T-Head-regime (caches aan).
   - **Waarom het T-Head-regime:** de C906L komt kaal uit reset, ~77×
     trager (18-08).
2. Hart 0: sp, BSS, `kmain(a1, 3)`. Er is geen MMU en geen wachtpagina.
3. De andere harts gaan in een postvak in `.data`.
   - **Waarom `.data`:** hart 0 wist de BSS terwijl de andere harts kijken.
4. riscv64 flipt niet (`flip::FLIPS`) en heeft geen feitenpagina: `kmain`
   leest er niets, dus een oud blob wordt er nooit gelezen (F17 is weg,
   01-11).

### 2.5 Te wegen

- **Vijf EL2-ingangen, vijf verschillende saneringen [S1].** `cpu::boot`
  zet alleen het regime en vertrouwt de firmware voor CPTR, MDCR, VTTBR en
  CNTHCTL. De Pi doet de Linux-set, UEFI nog meer. De verhuisde kern
  (`hopos_smp_entry`) zet niets naast het regime. Dezelfde letterlijke
  waarden (HCR, SCTLR 0x30c50830, CPTR 0x33ff) staan op vier tot vijf
  plekken.
- **Eén flip-ingang [S3, gedaan 01-11]:** x0 = de feitenpagina op elk
  arm64-board; riscv flipt niet.

---

## 3. `kmain` tot de executor draait

Alles in `hopos/src/main.rs`, synchroon, vóór de eerste ronde van de
executor.

### 3.1 `kmain`

1. **Console-sink, conport-ring, de bunny, de runtime-regel.**
2. **`board.privilege(el)`**, en anders parkeren.
   - **Waarom vóór alles:** de kooi is een invariant, geen optie. Zonder
     EL2 (riscv: M-mode) start de kern niet.
3. De firmware-regel, **`init_heap`**, **`discover(dtb)`**.
   - **Verplicht vóór de verhuizing:** `start_one` alloceert stack en
     handoff op de heap, en `discover` levert de cores en hun mpidr.
4. `conport::here()`.
   - **Waarom:** op de M4 kent `this_core` de cores pas na de ADT (01-10).
5. **`move_to_os_core`.** Wijst `hopos.oscore` een andere core aan:
   1. `cpu::smp::start_one` start die core met het levende regime (MAIR,
      TCR, TTBR0, HCR, VBAR, SCTLR) en een stack van 256 KiB uit de heap.
   2. De boot-core gaat in `cpu::el2::hold` en wacht tot de kooi-regio er is.
   3. Daarna wordt hij een geparkeerde app-core (`release_held` in
      `cage::new`).
   - **Waarom (PORT.md beslissing 2, 30-09):** het OS plus Hop op één
     gekozen core. De verhuizing gebeurt vóór er één lijn, timer of bewoner
     aan de boot-core hangt.
   - **Waarom het regime van de bron en niet afgeleid:** de hoge periferie
     van de Altra (17-07).
   - De verhuisde stack heeft geen wachtpagina [S2]. Stopt `slots::start`
     vroeg (een geweigerd plan), dan wordt de wachtende boot-core nooit
     vrijgegeven, zonder regel [F13].
6. **`boot` → `setup`, dan `exec.run(doorbell(sleeper))`.**
   - **Waarom twee functies:** de les van 30-09. Het frame van `setup` was
     125 tot 200 KB en bleef onder `exec.run` staan. Op de Altra liep het
     zonder wachtpagina over `.bss` heen. Nu bouwt `setup` alles in een
     eigen frame (`#[inline(never)]`), en elke `start` die een future bouwt
     ook.

### 3.2 `setup`, stap voor stap

| # | Stap | Waarom hier | Verplicht? |
| --- | --- | --- | --- |
| 1 | `flip::land(dtb)`: het handoff-blob lezen, wissen en ontcijferen (§10.4) | "als eerste na de heap"; feitelijk staat `discover` ervóór [§12] | Vóór alles wat staat bouwt die het blob kan dragen (klok, Privilege, opslag, slots) |
| 2 | Na een landing: `watchdog::pet_now` | De teller van de vertrekkende kern loopt door (Pi 5, 30-09) | Zo vroeg mogelijk |
| 3 | `exec.set_clock`, de regel `HOPOS_BOOT gen=` | | |
| 4 | `conport::here`, de console op het glas, spawn `conport::drain` | Een regel wacht niet op de baudrate (7,7 ms per regel op 115200, 04-10) | Vóór er taken loggen |
| 5 | Wandklok: de offset uit de flip, anders vast op 29-09-2026 | Hop stempelt vanaf zijn eerste regel; 1970 breekt de leader | Vóór de eerste kooi (`CTRL_WALL_OFF` bij de bouw) |
| 6 | `seed::init`: de DRBG gezaaid (board, anders CPU/jitter) | Een app heeft zaad nodig vóór hij een verbinding heeft | Vóór de eerste kooi |
| 7 | `staged_role` en het **Privilege-token** voor slot 1 (ook als het blob slot 1 draagt) | PORT.md beslissing 1: het token bestaat vóór de listener; de M4 van 03-10 (Hop ingebakken, de bundel niet) | Vóór `system()`: het token zit vast in `System` |
| 8 | `bench::start(storage::probe())`, dan `storage::start` (mount met `block_on`, actor, committer) | De bench leent de schijf vóór de mount; de mount mag wachten, "er is nog niemand die stil zou staan" | Vóór `system()` (`with_fs` wordt bij de bouw gekozen) |
| 9 | `codec::up` | De firmware-blobs staan op het volume | Na de opslag |
| 10 | `system(privilege, fs, slot_count)`: één `System`, gelekt | Niet `const`, dus geen `static` | |
| 11 | `start_interrupts`, spawn `irq_dispatch`, `wire_disk` | De bench en de mount pollden; de lijn heeft de controller nodig | Vóór de zelftest van de OS-core (stap 19) |
| 12 | spawn `tick`, `watchdog::start`, `telemetry::start`, `load::start`, gui-status | | |
| 13 | De node-config: `NodeCfg::new(config, bootargs)`, op QEMU de bank eronder; conport, replay, tick, asid | De console over TCP kiest uit dezelfde config, ook na een flip zonder herplaatsing | Vóór het net |
| 14 | `probe_nic` → `nic_up` → `net::start` (§3.3), of `nic_retry` | Geen net is een board, geen fout; een NIC die faalt, wordt opnieuw geprobeerd | |
| 15 | `optical::start`, `gui::start_usb_input` | De stroom naar de display-app loopt over de switch | Niet hard: de input-listener pollt tot de stack er is |
| 16 | `flip::start`: de flip-taak en de guard, en na een landing `HoldAdoption` in de brievenbus van de switch | De claim op de node-poorten moet vóór het eerste frame van de pomp staan | **Ja**, anders RST op levende verbindingen (§10.4) |
| 17 | `slots::start` (§5, §6): de kooi, de partitiepool, de lifecycle-actor, een servicer per slot, de supervisor van Hop | | Na de interrupts (de zelftest), vóór `OsCore::new` (`init_region` wist sched-blok 0) |
| 18 | `board.sleeper()` | | |
| 19 | `slots::os_core()`: `OsCore::new` en de zelftest (timer, yield, kick), dan `sleeper.host` | De idle van de kern wordt de rotatie over Hop | Na stap 11 en 17 |

Wat opvalt in deze volgorde:

- **De watchdog (12) leest `hopos.wd` vóór er een `NodeCfg` is (13).** Hij
  gebruikt `Board::boot_param`. Dat geeft dezelfde lagen, behalve de bank
  van QEMU. Er zijn dus twee wegen naar een sleutel [S4].
- **`watchdog::start` beslist "Hop woont hier" met `staged_role()`**, niet
  met de adoptie-logica van stap 7. Na een flip op een M4 met Hop
  ingebakken in de kaart-kern zegt de staging "app". Dan kijkt de watchdog
  niet naar Hop's hartslag. Dit is dezelfde fout die bij het token al
  gerepareerd is [F6].
- **Een mislukte zelftest van de OS-core host toch** (`Ok(os)` in beide
  architecturen). Alleen `Err` laat de kern zijn core voor zichzelf houden
  [F14].
- **Apple (`SHARES_OS_CORE = false`) bouwt, test en host toch een OsCore.**
  Elke slaap loopt dan over een lege lijst [S5].
- **De NIC-probe spint 8 s (O6N 12 s) in `setup` en 7 à 8 s per retry op de
  executor.** Bij een retry staan de switch, Hop en alle taken zo lang stil
  [S6].

### 3.3 Het netwerkvlak: `nic_up` → `net::start` (`hopos/src/net.rs`)

1. `UPLINK_MAC`. De ingress- en egress-rij splitsen. Twee host-ringen in
   de kern-heap.
2. `Switch::new` en `attach_host` (poort 0).
3. **De pomp eerst, dan de switch.** Na de switch volgt `SWITCH_UP = true`.
   - **Waarom de pomp eerst:** "de pompen eerst" (rustdoc §4): de
     spawn-volgorde is de poll-volgorde. Dat geldt alleen binnen
     `net::start`: de opslag, IRQ, tick en watchdog zijn al eerder gespawnd,
     en een vrijgekomen taakslot hergebruikt zijn index.
   - **Waarom `SWITCH_UP` na de spawn:** wie op een bevestiging wil wachten,
     kijkt eerst hier.
4. `flow_expiry`: elke 30 s een veeg.
5. De host-taak `Node::run`, in deze volgorde:
   1. **De DHCP-lease**, rauw over de host-ringen, eindeloos met 5 s
      tussen de pogingen.
   2. **`SetUplink` met ack.**
   3. **`Stack::new`.**
   4. `UPLINK_IP` (het seintje voor `wait_uplink`).
   5. De DHCP-keeper, de system-listener en de console-listener.
   6. De host-port-lus.
   - **Verplicht: de lease vóór de stack.** De stack kan niet van adres
     wisselen (Go: `hopnet.Up`).
   - **Verplicht: `SetUplink` vóór de stack.** Pas dan scheidt de NAT de
     gepubliceerde poorten van het verkeer voor de node zelf.
   - De onderlinge volgorde van keeper, system-listener en console-listener:
     *geen reden gedocumenteerd*. Ze zijn onafhankelijk.
6. In `boot` wikkelt `net::doorbell` de slaper.
   - **Waarom:** een kick van een app haalde de OS-core uit WFE, maar
     niemand belde de switch. Elk frame wachtte dan op de failsafe van 1 ms
     (dbc522f, 30-09: 875 → 11 failsafe-wekken/s).

**Het gevolg van stap 5: de system-API bestaat pas na een DHCP-lease.**
Vóór de lease gooit de DHCP-lus elk frame weg dat geen uplink-frame is, ook
een system-API-frame van een app (zelf getoetst). De listener bestaat dan
ook nog niet. Hop start na 10 s ook zonder lease, en sinds 323465f is een
mislukte call bij zijn start fataal [F7].

---

## 4. Eén ronde van de executor, en de OS-core als gastheer

### 4.1 De executor (`executor/src/lib.rs`)

1. **`spawn`:**
   1. `try_box` zet de future op de heap.
   2. De box gaat de spawn-brievenbus in, die even groot is als de tabel.
      - **Waarom:** de Altra spawnde 128 servicers vóór de eerste ronde
        (03-10).
2. **`step`**, in deze volgorde:
   1. **`drain_spawns`:** elke verse taak krijgt het eerste vrije slot en
      zijn ready-bit. Zonder slot wordt hij gedropt en telt `dropped`. De
      `spawn` gaf toen al `Ok`.
   2. **`expire_timers`.**
   3. **Per woord van `ready`:** eerst een kale load, dan `swap(0)`, dan per
      bit `poll`.
      - **Waarom:** 512 swaps per ronde kostten tot 03-10 de rtt (359 → 108
        µs).
      - **Waarom `polling`:** een geneste ronde gooide een verse spawn weg
        (29-09).
3. **`run`:** een ronde met werk gaat meteen door. Anders volgt
   `sleeper.sleep(now, next_deadline, has_ready)`.
4. **Een wek** is `fetch_or` op het bit plus `dsb sy; sev`. Dat mag vanuit
   een ISR of een andere core.
5. **Een uitstelbare timer** telt niet mee in de deadline en wekt dus geen
   slapende core.
   - **Waarom:** de OS-core ging van 710 naar 210 wekken/s (262ea1f).

Te wegen:

- De moduledoc zegt "pollen, dan timers". De code doet spawns, timers,
  pollen [§12].
- Taken in een box op de heap gaat tegen rustdoc §2/§4 ("statische taken").
  Dat is bewust en gedocumenteerd [S7].
- applib meldt `dropped` nergens. In een app verdwijnt een taak die geen
  slot krijgt dus stil [F15].

### 4.2 De slaap van de OS-core (`cpu/src/idle.rs`, `cpu/src/el2/oscore.rs`)

1. **`Doorbell::sleep`** (`net/src/switch.rs`) toetst binnen `ready()` ook de
   TX-ringen, met de maskers dicht, en ná de beurt nog een keer.
   - **Waarom:** dit dicht de race van de verloren wek.
2. **`ArmSleeper::sleep`:**
   1. Maskers dicht, `ready()`. Is er werk, dan meteen terug.
   2. Anders `resident` met deadline = min(until, nu + 10 ms).
   - **Waarom de cap van 10 ms:** "geen deadline" als oneindig lezen is een
     hang.
3. **`OsCore::run` → `next`:** round-robin over de bewonerslijst. Due
   betekent:
   - BootPending;
   - Saved met een verstreken wektijd;
   - een kick, of RX voorbij de gewapende drempel.

   **Hier en alleen hier** gaat een ingetrokken bewoner dood (`CTX_REVOKE`,
   het KVM-model, a5885f7).
4. **`turn`**, in deze volgorde:
   1. VTTBR van de eenheid, met TLBI en `ic iallu` alleen bij een verse
      bewoner. **Waarom:** de entries zijn VMID-getagd (Altra 15-07).
   2. CNTHP op de deadline.
   3. `listen(true)`: de switcher stuurt een SGI in plaats van een SEV.
      **Waarom:** tijdens een beurt hoort de kern geen SEV.
   4. Lazy FP. **Waarom:** 66 stores op Device per kick (O6N, b6878f9).
   5. `eret` naar EL1 van de bewoner.
5. **Terug** via een eigen vectortabel (`settle`):

   | Route | Wat de bewoner wordt | Wat de kern ziet |
   | --- | --- | --- |
   | HVC #0 | Dead | Exit |
   | HVC #1 | Saved, wektijd = x1 | Yield |
   | andere HVC | yield naar nu | Yield |
   | andere sync | fault-rapport op de control-page, Dead | Fault |
   | IRQ of FIQ | Saved | Timer, Ipi of Irq |

6. **`Turn::Ran`:** terug naar de executor, want de kern gaat altijd voor.
   **`Turn::Idle`:** slapen tot de vroegste wektijd (WFI of WFE, cap 10 ms).

Het resultaat: Hop draait in de idle van de kern. Elke interrupt, kick of
timer geeft de core terug. Er is geen preemptie, behalve de 10 ms-cap per
beurt.

Te wegen:

- De `slept_ns` van de executor meet de wandtijd om `sleep` heen, dus de
  beurten van Hop tellen mee. Slot 0 `idle_ns` in `SLOT_STATUS` noemt de
  draaitijd van Hop "idle van de kern" [F16].
- Er zijn vier FP-beleiden en drie TLB-beleiden over arm en riscv, en
  OS-core en app-core. Elk heeft zijn reden, maar het is veel om te dragen
  [S8].

### 4.3 De interruptketen

1. **De vector** (`cpu/src/vectors.rs`) redt de registers, zet `I` dicht en
   doet `IRQ_PENDING.set()`, dan `eret`. Geen werk in exception-context
   (Ampere 19-09, M4 21-09).
2. **De taak `irq_dispatch`** doet `board.dispatch_interrupts()`: claim, de
   `IrqAck` van de driver, de bel, complete. Daarna het masker open.
3. Een onbekende lijn gaat uit. Bij `STRAY_LIMIT` gaat een level-lijn zonder
   ack uit.

---

## 5. Hop: plaatsen, bewaken, herstarten, vallen

Hop is de bewoner van slot 1. De kern is zijn supervisor:
`hopos/src/slots.rs` `HopNode::supervise` en `Supervisor`.

### 5.1 De voorbereiding in `slots::start`

1. **`init_hop_group`:** de sharegroup van Hop.
   - Zonder sleutel: `system` (de OS-core) waar de kern deelt, anders `hop`.
   - Na een warme flip is zijn meegekomen groep de waarheid; een nieuwe
     config geldt pas bij een koude start.
2. **Het plan**, dan **de kooi:** `cage::new` (switch-code installeren,
   app-cores klaarzetten, de boot-core parkeren) of `cage::adopt`.
3. **De partitiepool:** het vrije DRAM minus kern en DMA.
4. **De lifecycle-actor**, gebouwd buiten zijn future en in een `Box`.
   - **Waarom:** het frame van 57 KB reserveerde zich elke poll opnieuw
     (30-09), en twee kopieën brachten de boot-stack over zijn wachtpagina
     (02-10).
5. **Een servicer per slot**, uit een vaste pool.
   - **Waarom:** er wordt nooit per start gespawnd.
6. **De supervisor**, of op QEMU met de rol `app` `place_first`.
   `place_first` is testmachinerie in de productiebinary [S9].

### 5.2 De eerste plaatsing (`supervise(None)` → `put`)

1. **`wait_uplink`:** pollt elke 50 ms op `UPLINK_IP`, hoogstens 10 s.
   Tijdens `nic_retry` schuift de termijn op.
   - **Waarom:** Hop meldt zijn node-IP aan de leader (Go, 19-09).
   - Dit is een poll waar een bel kan [S10].
2. **`spec`:**
   - De env uit `NodeCfg` plus de feiten van de kern: node-IP, DNS, cores,
     groep, koude flip, pool minus de codec-arena, `HOP_MEM`.
   - De plaatsing: `Placement::hop` plus `hopos.hop.core-class` als
     voorkeur.
3. **`set_resident`** vóór de plaatsing.
   - **Waarom:** zodra de poort aan de switch hangt, mag de switch niet op
     Hop's ring wachten. Hop's consument draait pas als de executor afgeeft
     (30-09).
4. **`place`:** dezelfde weg als elke start (§6), met het hele image in één
   brok, volume `/hop → /volumes/hop` en poorten 8080/9080.
5. `HOPOS_HOP_START`.

### 5.3 Bewaken (`Supervisor::watch`)

1. Elke seconde `Request::Status` aan de lifecycle-actor. Een regel per
   wissel, om de 30 s de hartslag.
2. Een fault (EL2, of EL1 via applib), of een exit die niemand vroeg, is
   `Crashed`.
3. Wakker op `ASK_BELL` (een HOP_RESTART) of de volgende ronde.

Er is geen bel voor de exit van een bewoner. Daarom pollen `watch` (1 s) en
`wait_leave` (50 ms) [S10].

### 5.4 Herstarten op verzoek (`HOP_RESTART`)

1. **De system-API** toetst de velden (vlaggen, termijn ≤ 30 s, som) en of
   de stroom compleet en rauw is.
2. **De haak `hop_restart`**, synchroon:
   1. Is er een herstart mogelijk (`SAME`)? Is er geen al lopende?
   2. Een nieuw image: sha256, `check_image` (de ELF-toets van de
      plaatsing), en een kopie in de kern-heap.
   3. Dan `ASKED` en `ASK_BELL`.
   - **Waarom de heap:** het slot van het image moet terug in de pool,
     slots zijn schaars. De staging is per board anders. De heap is ruim
     (2,4 MB voor Hop tegen 40-256 MB kern-RAM).
   - **Verplicht: synchroon.** Na `Ok` geeft de system-API het slot terug.
     Wat de haak nodig heeft, moet dan al gekopieerd zijn.
3. **Hop schrijft zijn antwoord en zijn staat weg** (`STATE_PUT`) en keert
   terug uit `main`.
4. **`after_ask`:** wacht hoogstens de termijn tot Hop zelf vertrok, dan de
   stop-bel met 1 s als vangnet.
   - **Waarom:** tot 22-10 een vaste halve seconde. Een Hop die nog met SNTP
     bezig was, schreef zijn 202 nooit (curl HTTP 000).
5. **`replace`**, in deze volgorde:
   1. De oude stoppen. Dat moet bevestigd zijn, anders geen nieuwe.
   2. `drop_streams`: de open stromen van de oude terug in de pool.
   3. De staat die klaarligt melden (`HOPOS_HOP_STATE kept=`).
   4. `put`.
   - **Verplicht (323465f):** de nieuwe Hop pas na de bevestigde stop van de
     oude.
   - **Verplicht:** zijn system-verbindingen worden toegelaten vanaf zijn
     eerste instructie. De generatie staat vóór het startschot (`register`
     vóór `dispatch`), en de verbindingsteller hoort bij een levensduur.
     Daarom wacht Hop niet en probeert hij niet opnieuw. Toets:
     `a_resident_is_admitted_from_its_first_instruction`.
6. Het nieuwe image wordt het huidige. Er wordt geen vorig bewaard.

### 5.5 Vallen

Een fault, een ongevraagde exit, of een nieuw image dat niet plaatst:

1. De diagnoseregel (`HOPOS_HOP_FAULT`, `_EXIT`, `_FAIL`).
2. `stay_down`: `HOPOS_HOP_DOWN`. Hop blijft liggen zoals hij viel.
3. Zijn hartslag stopt. De canary van de watchdog (een verse TCP-dial naar
   10.100.0.2:8080) faalt.
4. Na `STALE_NS` (15 s) houdt het beleid de pets in en reset de hardware.
5. De koude boot start de Hop van het bootmedium.

**Waarom (Derek, 23-10):** falen gaat naar het oude. Is het ijzer de
oorzaak, dan helpt een verse start in dezelfde boot niet, en een koude boot
ontdoet de node van zijn toestand.

### 5.6 De watchdog (`kern/src/watchdog.rs`, `hopos/src/watchdog.rs`)

1. **Fase 1, boot-guard:** wapenen en blind aaien tot het eerste
   levensteken. Het levensteken is een geslaagde self-dial plus een Hop die
   slaat.
2. **Fase 2, live:** aaien alleen op bewijs. Ontbreekt het, dan geen pets
   meer, en de hardware reset.
3. **Een flip-boot:** hoogstens 120 s blind.
   - **Waarom:** de M4 bleef op 06-09 zeven minuten donker.
4. **Waarom het bewijs een nieuwe verbinding is:** op 02-08 waren nieuwe
   verbindingen en ICMP dood terwijl alle interne lussen gezond waren.

Te wegen:

- **Een Hop die al bij zijn eerste start valt, wordt nooit gereset** (zelf
  getoetst) [F5]. Zonder eerste levensteken blijft het beleid in fase 1 en
  aait het blind. Fase 1 is zo bedoeld voor "een bring-up die leeft maar
  geen net krijgt". Maar zo staat "een falende Hop is een falende node" alleen
  ná het eerste levensteken. Het alternatief, ook in fase 1 resetten als
  Hop viel, geeft een resetlus als het image op het bootmedium zelf kapot
  is. Dat is een keuze, geen fout, maar hij is nergens uitgeschreven.
- **Twee boot-guards van 120 s** naast elkaar [S11]:
  - `flip::GRACE` eist adoptie plus net en doet daarna een PSCI-reset;
  - het watchdog-beleid eist een dial plus de hartslag en houdt daarna de
    pets in.

  Ze hebben elk een eigen klok en een eigen startpunt.
- **De supervisor, de late `republish` en de boot-plaatsing delen één
  antwoordplek, `BOOT_REPLY`** [F4]. `Oneshot` kent één wachter, en een
  tweede `recv` verdringt de waker van de eerste. Valt de eenmalige
  `republish` (na een warme flip met een NIC die pas bij een retry opkwam)
  samen met een `status` van de supervisor, dan kan de supervisor voorgoed
  hangen. Daarmee hangt elke volgende HOP_RESTART.

---

## 6. Een app van `START_SLOT` tot zijn eerste instructie

Wie wat bezit:

| Eigenaar | Staat |
| --- | --- |
| `System` | lopende image-stromen, het Privilege-token, Hop's staat |
| `Lifecycle` | partities, cores, bewoners, de kooi, de grants |
| `Servicers` | de leesbare tabel van levende generaties, mounts, jobs |
| `Kooi` | per gebouwde kooi de control-page, core en breedte |
| `Switch` | poorten, ringhelften, NAT |

### 6.1 De claim: `START_SLOT` (`kern/src/system.rs` `System::start`)

1. **Velden toetsen:** image-maat ≠ 0, env ≤ `CTRL_ENV_MAX`,
   `memory_limit` > `ABI_TAIL`, plaats in de stromentabel (8).
2. **Een rauwe stroom** (`FLIP_BUNDLE_JOB`, `HOP_IMAGE_JOB`) krijgt de
   plaatsing van Hop, en de kern bemeet de partitie zelf.
   - **Waarom de plaatsing van Hop:** een bundel draait nooit. Met één
     app-hart bezet weigerde de reservering (02-10).
   - **Waarom zelf bemeten:** 4 KiB te weinig na de guard-pagina (O6N 30-09).
3. **`free_slot`:** één vraag `FirstEmpty` aan de actor, en de slots van
   Hop en van lopende stromen slaat hij over.
4. Poorten (ontdubbeld) en volumes (`mount_table`).
   - **Waarom de volumes hier geweigerd worden:** wat de flip niet kan
     dragen (`MAX_FLIP_PATH`), zou anders pas bij een latere flip een
     weigering geven.
5. **`Request::Claim` → `Lifecycle::claim`:**
   1. `mount_table` opnieuw.
   2. Slot in bereik en niet bezet.
   3. **`place_anywhere`** (`kern/src/pool.rs`):
      - dedicated, of een sharegroup, of de OS-core als groep `system`;
      - een dedicated job van één core die geen vrije app-core vindt, komt
        in `system` terecht.
   4. Dedicated cores moeten Off zijn.
   5. **`parts.alloc`:** best-fit op 2 MiB, de hoogste basis.
   6. **De scrub in brokken van 4 MiB, met een yield ertussen.**
      - **Waarom:** een ononderbroken veeg verhongert de netstack (12 s in
        Go).
   7. Generatie+1, `Resident{Streaming}`, en de `ImageGrant` terug.
   - **Waarom twee stappen (claim, dan arm):** de download duurt minuten en
     mag de actor niet vasthouden. Grote data loopt niet door de kern; de
     grant wel.
6. `Placer::for_grant`, de logring van het slot leeg, `put_stream`.

Een geweigerde claim laat niets achter (`places.release` op elk faalpad).

### 6.2 Het stromen: `STREAM_IMAGE` → `Placer`

1. **Per brok:** de offset moet gelijk zijn aan wat er binnen is.
2. **Tot de program headers compleet zijn:** bufferen (hoogstens 64 KiB).
   Daarna routeert elke byte meteen naar zijn eindadres:
   - binnen een PT_LOAD naar de partitie;
   - de staart van het bestand (sectieheaders, symbooltabel) naar een
     exact passende schrapruimte bovenin het app-RAM.
   - **Waarom:** geen kopie van het image op de heap.
   - **De enige aanname:** PT_LOAD in oplopende bestandsoffset. Anders
     faalt het luid.
3. **Na de laatste byte, `Placer::finish`:**
   1. De symbolen lezen, in brokken uit de partitie.
      - **Waarom brokken:** een riscv-welcome draagt 42.000 symbolen;
        "out of memory (1012680 bytes)" op de LicheeRV, 03-10.
   2. `abi::place::build`: het plan met álle toetsen. Het controleert dat
      plan en stroom overeenkomen.
   3. BSS nullen, patchen (RamStart, RamSize, SlotHint), de schrapruimte
      wissen, cache vegen.
4. **De env**, langs de grant-aanbieder (`Request::Env`), op de
   control-page.
5. **`Request::Arm`.**

Een rauwe stroom blijft in de tabel liggen tot `FLIP` of `HOP_RESTART` hem
opeist.

### 6.3 Wapenen: `Lifecycle::arm` (`kern/src/slots.rs`)

| # | Stap | Faalt het, dan |
| --- | --- | --- |
| 1 | De grant hoort bij de stroom van deze levensduur | Niets aanraken (fail-closed) |
| 2 | **Poorten publiceren** (`Cage::publish` → switch, tcp+udp, met ack) | Unpublish en abort: er draaide niets |
| 3 | **`cage.build`** (§6.4) | Unpublish en abort |
| 4 | `grants.arm` (het glas de kooi in) | Unpublish, detach en abort |
| 5 | **`register`**: mounts, job, ctl-basis/maat, **de generatie in `Servicers`**, de servicer start | |
| 6 | **`cage.dispatch`** | `NEVER_RAN`: alles terug en de core uit de plaatsing tot een koude boot. Anders quarantaine |
| 7 | `boots`: wachten tot de bewoner niet meer boot-pending is (gedeelde core: na 2 s de vasthouder offeren, nooit Hop en nooit de OS-core) | Quarantaine |

- **Verplicht: poorten vóór de bouw.** Een poort van een ander slot laat de
  start falen terwijl er nog niets draait (Go `armSlot`).
- **Verplicht: `register` vóór `dispatch`.** De app is toegelaten tot de
  system-API vanaf zijn eerste instructie (323465f).
- **Verplicht: na een geslaagde dispatch geen faalbare stap meer.**
- **Waarom `NEVER_RAN` geen quarantaine is:** gemeten 01-10 op de M4: één
  POST, zes slots in quarantaine, elke flip geblokkeerd.
- **Waarom offeren (reclaim):** in Go (LicheeRV, 14/15-08) hield een buur die
  nooit yieldt de core urenlang vast, en elke plaatsing was dood.

### 6.4 De kooi bouwen en starten (`hopos/src/kooi.rs` + `cage.rs`)

**`Kooi::build`:**

1. **`roster::forget`** op elke core.
   - **Waarom:** een dode byte op een oude core zou een nieuwe levensduur
     twee keer starten.
2. Het ctx-blok op nul (staat Empty).
3. **`Isa::build`:**
   - arm: de stage-2-tabel in het kooiblok, dan `dev::pull` (clean+inv).
     **Verplicht:** de walker leest cacheable, de kern schreef Device.
   - op de OS-core: `el2::prepare`, de eerste beurt zonder trampoline.
   - de woorden van de trampoline en de wek-keten.
4. **`arm_page`:**
   - ENTRY, SLOT, CORES, STATUS=Booting, WALL_OFF en HART;
   - het zaad;
   - de page naar DRAM;
   - drie ringen `init` (outbox, net-TX, net-RX).
5. De ctx-woorden, `mb`, en **`attach`**: de frame-ringen aan de switch
   (`try_send`, de ack pas bij de volgende attach).
6. `clock::attach(slot, ctrl)`: de control-page in de tabel van klok,
   telemetrie, zaad en watchdog.

**`Kooi::dispatch`:**

- **OS-core:** `el2::host`, de bewonerslijst, dan BootPending.
- **App-core:**
  - koud: CPU_ON;
  - geparkeerd: de mailbox (eerst MBOX_PC, dan MBOX_CTX) plus SEV;
  - al draaiend (sharegroup): `join` in de rotatie.

  Daarna `refresh_shared`.
- `HOPOS_SLOT_START`.

**De trampoline** (`cpu/src/el2/switch.rs`), in deze volgorde:

1. VBAR = CTRL_VEC_PA. **Waarom eerst:** elke exception hierna rapporteert.
2. TPIDR_EL2 en sp.
3. VPIDR/VMPIDR. **Waarom:** ze zijn UNKNOWN (M4 03-09).
4. VTCR met PS = min(PARange, 44). **Waarom:** de Altra, 15-07.
5. VTTBR met de VMID, TLBI.
6. HCR = RW|TSC|FMO|VM, zonder IMO en zonder TWE. **Waarom:** de hard-kill
   is stage-2, de wissel een expliciete yield.
7. SCTLR_EL1 schoon. **Waarom:** nooit erven (Pi 5, 10-07).
8. CPTR, CNTHCTL, `eret` naar EL1.

### 6.5 De eerste instructies van de app (`applib/src/rt.rs`)

1. **`_start`:**
   1. DAIF dicht, FP aan.
   2. **VBAR_EL1 = de vectoren van applib.**
      - **Verplicht vóór alles wat kan faulten:** de Pi 5 sprong naar een
        lege VBAR (30-09).
   3. sp.
   4. **De stage-1 bouwen met de MMU uit:** RAM write-back, control-page
      Normal-NC, ringen write-back.
   5. MMU aan.
   6. **sp opnieuw.**
   7. **`.bss` vegen ná MMU-aan**, dan het boot-woord.
2. **`rt::run`**, in deze volgorde:
   1. De gepatchte symbolen lezen.
   2. **`App::new`:** de staart, de outbox, de control-page, de env.
   3. **`APP` zetten.** Vanaf hier werkt `log!`, ook in een panic.
   4. **De heap**, tot top − 256 KiB.
   5. **`clock::adopt_timebase`.** **Verplicht vóór de eerste kloklees:** de
      LicheeRV liep 2,5× te traag.
   6. De event-stream.
   7. **`announce`:** eerst `CTRL_RAM_SIZE`, dan `STATUS=Ready`.
      **Verplicht:** wie op READY wacht, ziet de maat al.
   8. `smp::init`.
   9. Spawn `bring_up` (bij SMP), `watch`, en `main` plus `shutdown`.
   10. `exec.run(AppSleeper)`.
3. Het netwerk komt pas op als `main` `appnet::up` roept (§7.1).

Te wegen:

- **Geen wachtpagina tussen de stack van de app en zijn heap.** Een
  overloop schrijft stil in de heap [S2].
- **De MMU-aanzet staat twee keer in asm:** in `_start` en in de
  SMP-ingang [S12].

---

## 7. Een app draait

### 7.1 Het net van de app (`applib/src/appnet.rs`)

1. `appnet::up` doet:
   1. De NIC op de ringen, met de coherentie uit de MMU-staat.
   2. Een budget.
   3. Zaad.
   4. DNS uit de env.
   5. Een leannet-stack: IP 10.100.0.1 + slot in een /16 (slot 1 is 10.100.0.2), MAC 02:00:00:00 met de zestien bits van het slot.
   6. De gateway 10.100.0.1 als statische buur, zonder ARP.
   7. De pomp.
2. **Een frame naar buiten:**
   1. De app schrijft in zijn TX-ring.
   2. Bij de overgang van leeg naar niet-leeg kickt hij: SEV plus HVC #6.
      Op de OS-core gebeurt dat pas bij de idle-yield.
   3. De switch ziet het via de deurbel.
3. **De switch** (`net/src/switch.rs` `switch_pass`) doet per ronde:
   1. Eerst poort 0. *Geen reden gedocumenteerd waarom eerst.*
   2. Dan de slots, elk hoogstens 64 records.
   3. Dan hoogstens 64 ingress-frames.
4. **Per frame van een slot, in deze volgorde:**
   1. **De bron-MAC en het bron-IP moeten van het slot zijn.**
      - **Waarom:** daardoor is het remote IP aan 10.100.0.1 een
        betrouwbare capability. Daarop rust de toelating van de system-API
        en het Privilege.
   2. Broadcast en ARP voor de gateway: de switch antwoordt zelf.
   3. Naar een ander slot: relay, één kopie van ring naar ring.
      - **Waarom één kopie:** op de M4 was de switch 130 van de 137 ms bezig
        (01-10).
   4. Naar 10.100.0.1: poort 0.
   5. Een antwoord van een gepubliceerde poort: SNAT.
   6. Het eigen node-IP: hairpin.
   7. De rest: masquerade naar de uplink.
5. **Naar binnen:**
   1. De NIC, dan de pomp (`rx_pass`, ≤ 32).
   2. De ingress, dan `uplink_in`.
   3. Multicast naar alle slots, IPv6 op een slot-MAC naar dat slot.
   4. `Nat::inbound` (een antwoord op een masquerade-flow, of DNAT).
   5. De rest naar poort 0.
6. **Backpressure:**
   - Een volle egress zet Xoff, en dan leest de switch niets meer. Dat
     geldt ook voor slot-naar-slot en de system-API.
     - **Waarom:** "zoals Linux één rij per device". Tot 24-10 dropte de
       switch hier (ORION ~17 MB/s).
   - Een volle RX-ring van een slot: één wek, 10 ms spinnen, dan droppen.
     Poort 0 en een OS-core-bewoner droppen meteen.
     - **Waarom:** hun consument draait op déze core (soak 30-09).

### 7.2 Een system-API-call (`kern/src/system.rs`, `hopos/src/net.rs`)

1. **TCP naar 10.100.0.1:10100.** Het antwoord van de node-stack loopt via
   de ARP van de LAN-gateway, omdat 10.100.0.x buiten het subnet van de
   lease valt [S13].
2. **`system_listener`:**
   1. `accept`.
   2. De self-dial van de watchdog wordt stil gesloten.
   3. **`admit`:**
      - het slot uit het bron-IP;
      - een levende generatie in `Servicers`;
      - hoogstens 2 verbindingen per levensduur (Hop 3), als één atomic
        met de generatie erin (fb81b13).
   4. **`DOORS.hand`** aan een vrije werker (13), anders meteen weigeren.
   - **Waarom de listener nooit leest:** een half-open verbinding hield de
     listener tot 29-09 voor altijd vast.
3. **`System::serve` → `frames`**, per frame:
   1. De kop en de payload.
   2. **`alive()`:** de generatie opnieuw toetsen.
   3. Dispatch:
      - `Kind::Log` (Go-apps);
      - fs- of device-op;
      - store-op;
      - codec-op;
      - anders `call` met `PrivOp`, alleen als `privilege.slot == slot`.
   4. Antwoord.
4. **De bewaking:** elke wachtende read of write tikt per 100 ms
   (`LIFE_TICK`). Is de generatie weg, dan `Evicted`. Na 300 s stilte
   `Idle`.
   - **Waarom pollen:** de stopbel van de servicer heeft één wachter [S10].
5. **De werker:** eerst `close`, dan `drop(job)`, dan `DOORS.free`.
   - **Verplicht:** een nieuwe verbinding van het slot vindt de oude nooit
     half open.

Te wegen:

- `DOORS.hand` deelt uit over alle 13 deuren, ook als `spawn_workers` na een
  OOM eerder stopte. Een verbinding op een deur zonder werker hangt dan
  voorgoed, met zijn plaats (zelf getoetst) [F10].
- **Poort 5555 luistert ook op 10.100.0.1 zonder bronfilter**, en de ring
  bevat de logregels van alle apps. Een app leest dus de logs van zijn buren
  als de console aan staat. Dat geldt standaard bij `hopos.insecure=1` [F9].
- **De draadvorm van de system-API staat twee keer:** `sys::Req`/`Resp` in
  applib spiegelt `abi::hopabi`. `payload_len` herhaalt de toetsen [S14].
- **Het adres van de kern staat op vijf plekken:** `HOST_IP4`,
  `systemapi::ADDRESS`, `sys::ADDRESS`, `appnet::HOST`, `net::host_ip()`
  [S14].

### 7.3 Een bestandscall: zie §9.2. Een store-op (`kern/src/store.rs`)

1. **De app:**
   1. `OP_STORE_*` met de naam en het lokale pad.
   2. `valid`: de jobnaam komt uit de lifecycle, niet van de app.
   3. `submit` in een van 8 plaatsen. Vol geeft meteen een fout.
2. **De app wacht:** `select(done, 100 ms)`, en dan `check`:
   - klaar;
   - de app leeft niet meer;
   - 30 s niet opgehaald;
   - 14 min oud;
   - de Hop die hem ophaalde is weg, dan terug in de rij.
3. **Hop:**
   1. `NEXT_STORE` (wachten ≤ 5 s).
   2. `STORE_READ`/`STORE_WRITE`. Dat zijn `FsCall`'s met het slot en de
      generatie **van de app**, dus langs zijn mount-tabel.
   3. `STORE_DONE`.
4. **Waarom 8 plaatsen:** elke wachtende call houdt een verbindingstaak
   vast.

### 7.4 De outbox en de servicer (`kern/src/slots.rs` `servicer_task`)

1. Per slot leest één vaste taak de outbox van de levensduur:
   - logregels naar de console en `NEXT_LOG`;
   - SMP-verzoeken als `Request::Smp` naar de lifecycle.
2. **16 records per burst, dan een yield.**
   - **Waarom:** een app die zijn outbox volschreef, hield de OS-core vast
     (soak 30-09).
3. **De tik:** uitstelbaar elke 2 ms, plus een vangrail van 10 ms op een
   raster.
   - **Waarom het raster:** de servicers van alle slots kosten samen één wek
     per 10 ms (Linux `round_jiffies`).
4. **Bij een stop:** eerst de outbox leeg, dan pas weg.
   - **Waarom:** het afscheid van de app haalde de console anders niet
     (05-10).

### 7.5 De slaap van de app (`applib/src/sleep.rs`)

1. De RX-deurbel wapenen (`CTRL_RX_DOOR`), en dan **opnieuw kijken**.
   - **Verplicht:** een frame net vóór het wapenen.
2. **Yield (HVC #1 met de wektijd)** als de core gedeeld wordt, bij Apple,
   op riscv en op de OS-core. **Anders WFE** tot het werk komt of de
   deadline.
   - **Waarom WFE op Apple niet:** daar slaapt WFE niet (02-09).
   - **Waarom de WFE-lus binnen:** een wek zonder werk gaat terug de WFE in
     en wordt geen ronde (262ea1f).
3. `CTRL_IDLE` en `CTRL_WAKES` publiceren, de deurbel ontwapenen.
   - **Waarom:** de dvfs en de meetlat van docs/apps.md.

---

## 8. De stop

### 8.1 De kern-kant (`Lifecycle::stop`)

1. Een slot in de stroom: weigeren. Eerst Abort; Hop serialiseert dat.
2. **`request_exit`:** `CTRL_KILL` = de termijn in ms (≥ 1), `refresh_shared`
   en een kick.
3. **`unpublish`:** de deuren dicht, ook zonder poorten (flows weg).
4. **`wait_quiet`:** elke 10 ms, hoogstens de termijn.
5. Niet stil: **`revoke`** plus een kick, en nog 1 s.
6. **`evict`:** de generatie uit `Servicers`. Een open system-verbinding
   ziet dat binnen 100 ms. Daarna wacht `evict` tot de servicer weg is.
7. **`detach`:** de ringen van de switch, met ack.
8. Stil: grant vrij, partitie vrij, cores vrij, `HOPOS_SLOT_STOPPED`. Niet
   stil: quarantaine.

- **Verplicht: de vlag vóór net, generatie en outbox weg.** Tot 05-10 ging
  het andersom, en de app had niets meer om netjes mee af te sluiten. Nu is
  het SIGTERM vóór SIGKILL (b1ae2a8).
- **Verplicht: `detach` vóór de partitie vrijkomt.** Daarna schrijft niemand
  meer in die staart.

### 8.2 De app-kant (`applib/src/rt.rs` `watch`, `app.rs`)

1. **De hartslag (elke 50 ms)** ziet `CTRL_KILL`. De kick van de kern helpt
   niet, want de WFE-lus leest het woord niet [S15].
2. **`request_stop`:** de gratie is de termijn min 500 ms, dan `Stop::set`
   en `HOPOS_APP_STOP`.
   - **Waarom 500 ms:** één slag, het net-afscheid en de poll van de kern.
3. **`main` keert terug**, dan `App::shutdown`:
   1. `Net::shutdown(200 ms)`: FIN's.
   2. `HOPOS_APP_SHUTDOWN`.
   3. `mark_exited`: de exit-code, dan `Exited`, dan `dsb`.
   4. HVC #0.
   - **Waarom het net-afscheid:** `exit` parkeerde de core met een zendbuffer
     die niemand bereikte (29-09).
4. **Na de gratie zonder terugkeer:** `HOPOS_APP_STOP_FORCED` en hetzelfde
   afscheid, met exitcode 0.

Te wegen:

- **Na een gewone stop wijst de tabel van control-pages nog naar de
  vrijgegeven partitie** (zelf getoetst) [F1]. `clock::detach` staat alleen
  in `Kooi::revoke`. Een app die binnen zijn termijn stil is, wordt nooit
  ingetrokken, dus zijn ingang in `clock::CTRL` blijft staan. Dan gebeurt
  elke seconde dit:
  - de telemetrie-tik schrijft `CTRL_TEMP` en het zaad (seqlock, 32 bytes en
    de bron) op dat fysieke adres;
  - `load` leest het;
  - `clock::set` schrijft er de wandklok in;
  - de watchdog leest er de hartslag van Hop.

  Gaat die partitie, of een stuk ervan, naar een andere app, dan schrijft de
  kern elke seconde in diens geheugen. Dat duurt tot hetzelfde slot opnieuw
  gebouwd wordt.
- **Secundaire SMP-cores krijgen geen gratie.** Bij de vlag parkeren ze
  meteen, terwijl `main` nog afrondt (zelf getoetst) [F11].
- **Een geforceerde stop meldt exitcode 0**, net als een nette [S15].

---

## 9. hopfs: mount, call, commit, sync

### 9.1 De mount (`hopos/src/storage.rs`, `kern/src/hopfs.rs`)

1. `Queue::new(disk, ExecPace)`: één wachtrij voor de hele levensduur van de
   kern.
2. **Stateless tenzij `hopos.storage=stateful` of een landing.**
   - **Waarom:** zoals in Go begint een device schoon. `landed` en niet
     `jumped`, want riscv telt elke boot als sprong.
3. **`block_on(Fs::mount)`**, vóór `exec.run`.
4. **De mount:**
   - De plekgrootte is min(64 MiB, schijf/64). Is de schijf te klein, dan
     `Volatile`.
   - Stateless: beide koppen op nul.
   - Anders beide plekken lezen en hashen. De hoogste geldige generatie
     wint.
   - Een boom die niet ontcijfert, wordt een lege boom met `dirty` [F12].
5. De actor, de committer en de stats.

### 9.2 Een bestandscall (`kern/src/rpc.rs` `FsActor`)

1. **`fs_call`:** de buffers van de verbinding gaan als waarde de actor in,
   en komen met het antwoord terug.
   - **Waarom:** geen lening over een `.await` en geen allocatie per call.
2. **`admit_call`**, de regels:
   - **Bevroren:** `Busy`.
   - **Per slot één call.** Lezingen mogen naast elkaar. **Waarom:** de app
     wacht toch op zijn antwoord.
   - **Een nieuwe levensduur, `REMOVE` en `TRUNCATE`:** alleen bij een lege
     pool. **Waarom:** ze geven nodes en blokken vrij.
   - **`OP_SYNC`:** niet tijdens een vastlegging.
   - **Wat wacht, blijft vooraan, en alles erachter wacht ook.** **Waarom:**
     de volgorde van binnenkomst is de volgorde van beginnen.
   - **Eén schrijver per bestand.**
3. **`plan` (synchroon):**
   1. De generatie moet levend zijn.
   2. **`prepare`**, één keer per (slot, generatie): `/.tasks/slot<N>` weg
      en opnieuw aanmaken, en de mappen van de volumes aanmaken.
   3. **`resolve`:** het langste volume-prefix, anders de eigen root. Een
      `..` is een weigering.
4. **De I/O als future** in een pool van 16:
   - `read_step` met de boom kort geleend, dan de await;
   - `write_step`: een verse run voor ongemapte blokken, gemapte blokken
     ter plekke.
   - **Verplicht: de mapping pas ná de data** (`write_landed`). Omgedraaid
     ziet een lezer of commit de bytes van een vorige eigenaar.
5. **blkdev `Queue`:** de eerste wachter is de pacer en wekt de anderen.
   - **Waarom:** 16 pollende wachters zijn 16 keer het device lezen
     (01-10: 11.888 tegen 175.055 lezingen/s).

### 9.3 De commit

1. **De committer:** `select(Servicers::changed, 10 s)`. Een stop geeft
   meteen een commit.
   - **Waarom:** wat een app schreef, overleeft dan een stroomuitval.
2. **`commit_begin` (synchroon):**
   1. De hele boom encoderen.
   2. **Doel = de andere plek**, met generatie+1.
   3. De vrijgaven apart nemen.
3. **De I/O: FLUSH, de body, de kop, FLUSH.**
4. **`commit_end`:** pas dan de vrijgaven echt vrij.

Verplichte volgordes (crash-consistentie):

- **FLUSH vóór de boom.** De blokken waar de nieuwe boom naar wijst, moeten
  duurzaam zijn. Anders wijst de boom na een stroomuitval naar oude bytes
  van andere apps.
- **Altijd de andere plek.** Anders blijft er bij een scheur niets geldigs
  over.
- **De vrijgaven pas na de tweede FLUSH.** Anders hergebruik je een blok
  waar de op schijf geldende boom nog naar wijst.
- **Eén commit tegelijk.**
- **Body vóór kop is géén duurzaamheidsvolgorde.** Er zit geen FLUSH
  tussen; de SHA-256 beslist.

### 9.4 `OP_SYNC` en de belofte

1. De sync gaat pas van start als elke eerdere call van dit slot terug is
   van het device.
2. Daarna een commit (of alleen een FLUSH). De gemelde generatie is
   duurzaam.
3. **Niet beloofd:** de schrijfs van andere apps die in de lucht zijn.

Te wegen:

- **De standaard (`image/cfg/default.cfg`) is `stateless`** (zelf getoetst)
  [F8]. Een koude boot wist dan alles, ook de volumes. Commit en `OP_SYNC`
  beloven dus een duurzaamheid die alleen een flip overleeft.
  `docs/storage-sync.md` zegt dat niet; de sync-toets boot `stateful`.
- **Head-of-line-blokkering over alle slots** [S16]. Eén wachtend bericht
  vooraan houdt de hele rij op:
  - een tweede commit;
  - een sync tijdens een commit;
  - `REMOVE`, `TRUNCATE` of de eerste call van een levensduur bij een
    niet-lege pool.

  Het contract vraagt alleen volgorde per slot. Op virtio-blk (diepte 1)
  lopen calls niet echt naast een commit, en de slowdisk-toets toetst geen
  bestandscalls.
- **`OP_SYNC` is een globale commit:** de hele boom, twee FLUSH'en. Het pad
  is alleen een toegangstoets [S16].
- **De root van een gestopte app wordt lui gewist**, pas bij de eerste call
  van de volgende bewoner [§12].

---

## 10. De kern-flip

Kern A vervangt zichzelf door kern B, zonder de apps te stoppen (warm) of
met een stop (koud).

- Het beleid staat in `hopos/src/flip.rs`.
- Het mechanisme staat in `kern/src/kernflip.rs`.
- De sprong staat in `cpu/src/el2/chain.rs`.
- De procedure staat in `docs/flip.md`.

### 10.1 De aanvraag (alles waar "nee" nog een antwoord aan Hop is)

1. Hop reserveert een rauw slot (`FLIP_BUNDLE_JOB`), stroomt de bundel en
   stuurt `PrivOp::Flip` met de sha.
2. **De system-API:**
   1. De som.
   2. Een onbekende vlag is een weigering. **Waarom:** liever nee dan een
      warme flip als er iets anders gevraagd was.
   3. De stroom moet compleet zijn.
   4. De haak (synchroon).
   5. **Het slot gaat altijd terug.**
3. **`prepare_inner`**, in deze volgorde:
   1. sha256 over de bundel.
   2. `Bundle::parse` (HOPRELO1, versie, `flat_size` ≤ 64 MiB, entry,
      relocaties).
   3. `FLIP_ABI` = 3.
   4. Warm op riscv: weigeren.
   5. Koud zonder PSCI: weigeren.
   6. Koud met `no_way_back` (Pi 5): weigeren.
   7. Warm: **de som van de switch-code moet gelijk zijn**, want de
      bewoners draaien in die kopie.
   8. Een DTB op x0.
   9. De maat.
   10. `stage_slot`: het beeld gaat achter de gestagede Hop.
   11. De recorder.
   12. `flatten`.
   13. `relocate`.
   14. De config meenemen (een leeg venster krijgt het onze).
   15. `PENDING` en `BELL`.
   - **Verplicht: de som vóór het parsen.** Bytes van het net worden niet
     geïnterpreteerd voordat ze vertrouwd zijn.
   - **Verplicht: alle weigeringen hier, vóór er iets bevroren of gestopt
     wordt.** Alleen hier hoort Hop de weigering als antwoord (502). De Pi
     5-bug van 03-10 was precies een weigering ná een 202.

### 10.2 Warm (`capture_and_jump`)

1. 500 ms wachten, zodat het antwoord naar Hop nog de draad op gaat.
2. **hopfs bevriezen en committen** (`ACTOR_WAIT` 2 s).
3. **De NAT vangen:** eerst de masquerade dicht, dan lezen.
   - **Verplicht:** omgekeerd komt er een flow bij die het blob mist.
4. **USB stil.** **Waarom:** DMA-masters op de RP1 schreven door de
   link-reset van kern B heen (Pi 5, 30-09).
5. **`Lifecycle::snapshot`**, het laatste `.await`. Het weigert bij
   quarantaine, een lopende stroom, een dode kooi of `MAX_FLIP_*`.
6. **`handoff_and_jump`:**
   1. De klok vol.
   2. Een aai van de watchdog, zo laat mogelijk.
   3. Het blob op `FLIP_HANDOFF_PA`.
   4. **Het pointer/magic-paar als laatste**, in een eigen cacheregel.
   5. `conport::flush`.
   6. `chain`.
   - **Verplicht: het paar ná het blob.** Een reset ertussen mag geen
     geldig paar laten dat naar een half blob wijst.
7. **De trampoline:**
   1. Een eigen dump-VBAR.
   2. MMU en caches uit.
   3. `dc civac` over beide vensters.
   4. De kopie.
   5. `ic iallu`.
   6. `br entry` met x3 = `FLIP_ENTRY`.

   De app-cores draaien intussen door in hun partities, op de switch-code
   in de plan-regio. Hop (op de OS-core) staat stil tot kern B hem
   terugzet.

- De volgorde hopfs vóór NAT vóór USB: *geen reden gedocumenteerd* voor die
  plek. Een aannemelijke reden: de commit duurt, en zo blijft het venster met
  een dichte masquerade kort.

### 10.3 Koud (`"cold":true`)

1. Hop stopt eerst zelf zijn taken.
2. hopfs bevriezen, dan meteen `cold_jump`, **zonder NAT en zonder USB
   stil** (zelf getoetst) [F18].
3. De bewoners op een app-core stoppen.
4. **De app-cores uit:** een uit-stub met `CPU_OFF` op `FLIP_TRAMP_PA`, en
   wachten op AFFINITY_INFO OFF.
   - **Waarom:** kern B doet CPU_ON. Een core die nog in de oude parkeerlus
     staat, geeft ALREADY_ON midden in overschreven code (29-09).
5. Dezelfde `handoff_and_jump` met `cold = true`.

### 10.4 De landing in kern B

1. **`kmain`:** console, EL2, heap, `discover`, `move_to_os_core`. Dan
   `setup`, en dan pas **`flip::land`**:
   1. **Het paar wissen en naar DRAM vóór enig vertrouwen.**
      - **Waarom:** de nul bleef in de cache en het paar overleefde een
        watchdog-reset (O6N 04-10, d7b82c9).
   2. Zonder `jumped` is het blob `Stale`.
   3. Het blob ontcijferen.
   4. Koud: geen bewoners. Warm: de bewoners, de wandklok en de NAT.
   5. Een fout in het blob: reset. **Waarom:** een koude boot zou de regio's
      van levende bewoners vrij noemen.
2. **Het Privilege** voor slot 1 als het blob hem draagt.
3. **De opslag stateful:** de mount vindt de bevroren generatie.
4. **`watchdog::start` met `arm_boot_guard`**: 120 s.
5. **`flip::start`:**
   1. De guard.
   2. **`hold_ports`:** `HoldAdoption` in de brievenbus van de switch, vóór
      de eerste switch-ronde.
   3. `restore_nat`.
   - **Verplicht:** een antwoord van een oude peer zou anders bij de
     node-stack vallen, en die doodt de verbinding met een RST (9bf3c74).
6. **`slots::start`** met de adoptie:
   1. `cage::adopt`: niets schrijven, de som moet gelijk zijn.
   2. Per bewoner `adopt_slot`: `rehost` op core 0, `attach`, de klok, vers
      zaad.
   3. **`lc.adopt`:** eerst álle eigendomsclaims, dan de grants, dan pas de
      servicers.
   4. `republish`.
   5. `HOPOS_HOP_RESUMED`.
   - **Verplicht:** geen servicer vóór alle oude claims terug zijn (E8).
7. **`restore_nat`** wacht op `ADOPTED`, dan `RestoreNat`, en **altijd**
   `FinishAdoption`.
   - **Verplicht:** `restore` houdt alleen flows van aangesloten slots. Een
     vroege vrijgave laat een nieuwe mapping het oude tupel verbergen.
8. **De guard:** elke 250 ms adoptie plus net, en anders na 120 s reset.

Te wegen:

- **Na een warme flip wist de eerste bestandscall de eigen root van elke
  geadopteerde app** (zelf getoetst) [F2]. De adoptie geeft elke bewoner
  een nieuwe generatie. De nieuwe opslag-actor begint met een lege
  `prepared`. Daardoor ziet `prepare` een "nieuwe levensduur" en wist hij
  `/.tasks/slot<N>`, terwijl de app gewoon doordraait. Volumes blijven.
- **Een bevriezing die de 2 s mist, wordt later alsnog uitgevoerd en nooit
  ontdooid** [F3]. Het `Freeze`-bericht staat al in de rij, en `frozen.fs`
  wordt alleen bij `Ok` gezet. Daarna is elke bestandscall `Busy` tot een
  reboot. `SnapshotNat` heeft hetzelfde patroon.
- **Een tweede FLIP tijdens `JUMP_DELAY`** legt opnieuw plat in dezelfde
  staging. Er is geen "flip bezig"-weigering; Hop serialiseert nu [F19].
- **Een mislukte `adopt_slot` wordt alleen gelogd.** `ADOPTED` kan daarna
  toch de guard tevreden stellen, met een slot zonder ringen [F20].
- **Dode velden en stappen:**
  - `SUM` (geschreven, nooit gelezen; zelf getoetst);
  - `Handoff.bundle_sum`, `.window`, `.total`;
  - twee nulwoorden in het blob;
  - vier recorder-stappen zonder schrijver;
  - `facts::PSCI_HVC`.

  Zie [S17].
- **De handoff-pointer is altijd `FLIP_HANDOFF_PA`.** Alleen de magic telt
  [S17].

---

## 11. Te wegen: fouten en kandidaten

### 11.1 Mogelijke fouten (F)

Gerangschikt op wat ik eerst zou bekijken. "Zelf getoetst" = na het lezen
nog apart tegen de code gehouden.

| # | Wat | Waar | Zeker |
| --- | --- | --- | --- |
| F1 | **Na een gewone stop schrijft de kern elke seconde in de vrijgegeven partitie**: `clock::detach` staat alleen in `revoke`, dus `clock::CTRL` blijft wijzen; telemetrie (temp, zaad) en `clock::set` schrijven, load en watchdog lezen. Een nieuwe huurder van dat geheugen krijgt de bytes in zijn RAM | `hopos/src/clock.rs`, `kooi.rs` (`revoke`), `telemetry.rs` (`thermal`), `seed.rs` (`refresh`) | H, zelf getoetst |
| F2 | **Een warme flip wist bij de eerste bestandscall `/.tasks/slot<N>` van elke doorlopende app** (nieuwe generatie bij adoptie, lege `prepared` in de nieuwe actor) | `kern/src/slots.rs` (`adopt`), `kern/src/rpc.rs` (`prepare`, `admit_call`) | H, zelf getoetst |
| F3 | **Een te late bevriezing (> 2 s) blijft voorgoed staan**: `Freeze` staat al in de rij, `thaw` alleen als `frozen.fs`. Daarna is elke bestandscall `Busy`. `SnapshotNat` idem (de masquerade dicht) | `hopos/src/flip.rs` (`capture_and_jump`, `Frozen::thaw`), `kern/src/rpc.rs` | H |
| F4 | **De supervisor van Hop deelt `BOOT_REPLY`** met de late `republish` en de boot-plaatsing; `Oneshot` heeft één wachter, dus de supervisor kan voorgoed hangen in `status()` | `hopos/src/slots.rs` (`ask`, `republish`) | M (mechanisme H, kans klein) |
| F5 | **Een Hop die al bij zijn eerste start valt, wordt nooit gereset**: zonder levensteken blijft het beleid in fase 1 en aait blind. Bewust (een bring-up zonder net blijft staan), maar dat staat haaks op "een falende Hop is een falende node" | `kern/src/watchdog.rs`, `hopos/src/watchdog.rs` | H (mechanisme), keuze |
| F6 | **De watchdog kiest "Hop woont hier" met `staged_role()`**, niet met de adoptie: na een flip met Hop ingebakken (M4) wordt Hop's hartslag niet bewaakt. Dit is de fout die bij het token al gerepareerd is | `hopos/src/watchdog.rs` (`start`), vergelijk `main.rs` (`hop_adopted`) | M |
| F7 | **De system-API bestaat pas na een DHCP-lease**: vóór de lease gooit de DHCP-lus app-frames weg en is er geen listener. Hop start na 10 s ook zonder lease, en een mislukte call bij zijn start is fataal | `hopos/src/net.rs` (`dhcp`, `Node::run`), `slots.rs` (`wait_uplink`) | H (mechanisme, zelf getoetst), M (gevolg) |
| F8 | **De standaard is `stateless`**, terwijl commit en `OP_SYNC` duurzaamheid melden; een koude boot wist ook de volumes | `image/cfg/default.cfg`, `storage.rs`, `docs/storage-sync.md` | H, zelf getoetst |
| F9 | **Poort 5555 is bereikbaar vanuit elk slot** (luistert ook op 10.100.0.1, geen bronfilter) en draagt de logregels van alle apps | `hopos/src/net.rs` (`console_listener`) | M-H |
| F10 | **`DOORS.hand` kiest uit alle 13 deuren**, ook als `spawn_workers` na een OOM eerder stopte: die verbinding hangt voorgoed | `sync/src/doors.rs`, `hopos/src/net.rs` (`spawn_workers`) | H (alleen bij OOM), zelf getoetst |
| F11 | **Secundaire SMP-cores parkeren meteen bij de kill-vlag**, zonder de gratie die `main` krijgt (van vóór de nette stop van 05-10) | `applib/src/smp.rs` (`gone`) | H, zelf getoetst |
| F12 | **Een boom die niet ontcijfert, laat ook de oudere geldige plek verloren gaan**: de mount probeert de andere niet, en de eerste commit overschrijft hem met een lege boom | `kern/src/hopfs.rs` (`mount`) | H |
| F13 | **De boot-core van een verhuisde kern blijft voorgoed wachten** als `slots::start` vóór `cage::new` stopt (een geweigerd plan): geen timeout en geen regel | `hopos/src/slots.rs` (`start`), `cpu/src/el2/oscore.rs` (`hold`) | M |
| F14 | **Een mislukte zelftest van de OS-core host toch** (beide architecturen geven `Ok(os)`) | `hopos/src/slots.rs` (`arch::os_core`), `cage_riscv.rs` | H |
| F15 | **applib meldt `executor.dropped` nergens**: een app-taak die geen slot krijgt, verdwijnt stil (de kern meldt het wel) | `applib/src/rt.rs`, `executor/src/lib.rs` | H |
| F16 | **Slot 0 `idle_ns` telt de beurten van Hop als idle van de kern** (`slept_ns` meet de wandtijd om `sleep`) | `hopos/src/main.rs` (`Hooks::kern`), `cpu/src/idle.rs` | M |
| F17 | **riscv kent geen flip-merkteken** (`jumped()` is altijd waar): een firmware-boot na een dood tussen sprong en landing vindt een oud koud paar en "landt" | `hopos/src/flip.rs`, `cpu/src/riscv/boot.rs` | M |
| F18 | **De koude flip slaat `quiesce_usb` over**: op de Pi 5 (koud toegestaan zolang er geen app-core draaide) springt hij met draaiende xHCI's, het geval van fc66084 | `hopos/src/flip.rs` (`capture_and_jump`) | H (pad, zelf getoetst), M (impact) |
| F19 | **Geen "flip bezig"-weigering**: een tweede FLIP in de 500 ms legt opnieuw plat in dezelfde staging | `hopos/src/flip.rs` (`PENDING`) | M |
| F20 | **Een mislukte `adopt_slot` is alleen een regel**; `lc.adopt` kan slagen en `ADOPTED` de guard tevreden stellen met een slot zonder ringen | `hopos/src/slots.rs` (`start`) | M |
| F21 | **Commando's aan de switch zonder switch**: `attach` en `unpublish` doen `try_send` ook zonder NIC; de brievenbus (16) loopt vol, en een late `nic_retry` voert oude opdrachten uit | `hopos/src/kooi.rs` (`attach`, `unpublish_ports`) | H |
| F22 | **Een mislukte unpublish bij een stop** (volle brievenbus) laat pubs en flows staan voor de volgende huurder | `hopos/src/kooi.rs` | M |
| F23 | **De store-ring is rood sinds 323465f** (ook op een schone 2d1ed1f, 24-10 getoetst): Hop opent bij zijn start vier system-verbindingen tegelijk (de hoofdverbinding, de store-taak, de kloktaak, en een losse in `init_specs` voor één stat van `/hop/init-jobs.json`) tegen `MAX_HOP_CONNS` = 3. De vierde wordt geweigerd, en sinds 323465f is dat fataal (`HOP_KERNEL_GONE step=init`, exit 3). De kloktaak opent bovendien per SNTP-ronde een nieuwe verbinding. **Gerepareerd 25-10 in hop (werkboom op 2364cc7):** de kloktaak hield zijn verbinding het hele uur open (`sys` viel pas na `exec.after(wait)` weg, tegen zijn eigen doc in) en sluit hem nu vóór de wacht; `init_specs` loopt één keer vooraan in de reeks over de hoofdverbinding (`sys.conn_mut()`), vóór de `Node`. Zo houdt Hop er twee vast (hoofd, store) en de klok even een derde | hop: `agentd-hopos/src/main.rs` (`init_specs`, de kloktaak, `store_task`); kern: `kern/src/system.rs` `MAX_HOP_CONNS` | H, zelf getoetst |

### 11.2 Kandidaten om te schaven (S)

Waar het ontwerp twee dingen doet waar één kan, of een klok gebruikt waar
een bel kan. Elk punt heeft één regel richting; uitwerken is de volgende
stap.

| # | Wat | Richting |
| --- | --- | --- |
| S1 | Vijf EL2-ingangen met vijf saneringen; HCR/SCTLR/CPTR/VTCR als literals op 4-5 plekken; de plek van VBAR wisselt | Eén `el2_init` en één set constanten |
| S2 | Geen wachtpagina op UEFI, riscv, de verhuisde kern-stack en tussen de stack en heap van een app | Overal één ongemapte pagina |
| S3 | Drie flip-merktekens (x3, x1+feitenpagina, geen) en twee boot-guards van 120 s | Eén protocol en één guard |
| S4 | Twee wegen naar een config-sleutel: `Board::boot_param` (watchdog, bench, codec) en `NodeCfg::one` | `NodeCfg` eerder in `setup`, en alleen die |
| S5 | Apple bouwt, test en host een OsCore die niemand deelt | Niet bouwen als `SHARES_OS_CORE` onwaar is |
| S6 | De NIC-probe spint 8-12 s, ook in `nic_retry` op de executor | De linkwacht als taak met een deadline |
| S7 | Taken in een box (handboek zegt statisch); de handboekregel "het enige `unsafe impl Sync` is Local" klopt niet (Oneshot, Mailbox, spsc, AtomicWaker, HeapLock, Reg) | Handboek of code bijstellen |
| S8 | Vier FP-beleiden en drie TLB-beleiden (arm/riscv × OS-core/app-core) | Uitschrijven als één tabel; kijken of riscv-app-harts ASID's kunnen krijgen |
| S9 | `place_first` (QEMU-testpad) in de productiebinary | Achter een feature, of als rol van appspike |
| S10 | **Pollen waar een bel kan**: `wait_uplink` (50 ms), `watch` (1 s) en `wait_leave` (50 ms) van de supervisor, `LIFE_TICK` (100 ms per verbinding), het store-wachten (100 ms), de input-listener (100 ms), de console-lezer (100 ms). Twee oorzaken: geen bel voor "de exit van een bewoner" en `Servicers::changed` heeft één wachter | Een `WakerSet`/`Stop` per levensduur of een bel per slot, en een `Signal` bij `UPLINK_IP` |
| S11 | `flip::GRACE` naast `FLIP_GRACE_SECS` (twee klokken, twee voorwaarden, twee resets) | Eén guard |
| S12 | De MMU-aanzet van applib twee keer in asm (`_start` en de SMP-ingang) | Eén macro |
| S13 | De antwoorden van de kern aan apps hangen aan de ARP van de LAN-gateway; `classify` overschrijft de MAC's daarna toch | Het slot-LAN als eigen route in de stack |
| S14 | De draadvorm van de system-API in abi én applib (`sys::Req`/`Resp`, `payload_len`); het kern-adres op vijf plekken; het adresplan in abi, net, applib, `slot_from_remote` en `gw` | abi als enige waarheid, ook in applib |
| S15 | De stop-kick wekt de primaire niet (de WFE-lus leest `CTRL_KILL` niet); een geforceerde stop is exit 0; `KILL=1` dubbelzinnig | `CTRL_KILL` in `woke()`, een eigen exitcode |
| S16 | hopfs: head-of-line over alle slots; `OP_SYNC` is een globale commit; een commit tijdens een commit wacht vooraan | Wachten per slot; commits samenvoegen (`dirty` draagt ze al) |
| S17 | Flip-resten: `SUM`, `bundle_sum`, `window`, `total`, nulwoorden, recorder-stappen zonder schrijver, `PSCI_HVC`; de handoff-pointer draagt geen informatie; relocaties twee keer getoetst | Opruimen (let op oudere kernen voor het blob-formaat) |
| S18 | Dubbele intrekking op de arm-OS-core (`recall` plus stage-2 nullen plus `tlbi alle1is`); `CTX_REVOKE` heeft op arm-app-cores geen lezer; `tlbi alle1is` raakt alle VMID's | Per VMID intrekken; op core 0 alleen `recall` |
| S19 | `max_slots` drie keer afgeleid (`slot_count`, `os_plan`, de switch) | Opgelost in v3.2.0: er is geen maximum aantal slots meer, dus ook geen `max_slots` (de kooi-blokken komen uit de pool) |
| S20 | DHCP op twee manieren (rauw voor de eerste lease, dan een socket) | Volgt uit "de stack kan niet van adres wisselen"; samen met S13 bekijken |
| S21 | De `Isa`-takken in `flip.rs` (vier paren) en de `cfg`-bos in hopos (watchdog, telemetrie, bench) | Als data of trait van het board |

### 11.3 Wat in deze ronde logisch bleek

Om de lijst eerlijk te houden: de meeste volgordes zijn verplicht en goed
beredeneerd, met de les en de datum in de code. Zo gelezen staan ze stevig:

- Het startpad in twee stappen (claim, arm) en de typestate van de partitie
  (`Free` → `Owned` → `Quarantined`).
- Poorten vóór de bouw, `register` vóór `dispatch`, en niets faalbaars na de
  dispatch.
- De stop als SIGTERM vóór SIGKILL, met `detach` vóór de partitie vrijkomt.
- De commit van hopfs: data, dan mapping, dan FLUSH, dan de andere plek,
  dan de vrijgaven.
- De flip: alle weigeringen vóór het bevriezen, het paar ná het blob, de
  claim op de poorten vóór de eerste switch-ronde, en de adoptie eerst
  eigendom en dan diensten.
- De bron-toets van de switch als fundament van de identiteit
  (bron-IP → slot → generatie → Privilege). Er is geen tweede identiteitspad.

---

### 11.4 Stand na de reparaties van 24-10

Gerepareerd, niet gecommit:

| # | Wat |
| --- | --- |
| F1 | `clock::detach` in `Kooi::detach`, bij elke stop. De watchdog houdt zijn tolerantie van 15 s als de page even weg is |
| F2 | `Life { generation, adopted }` in de servicer-tabel. Een geadopteerde levensduur houdt zijn root (`HOPOS_FS_ROOT_KEPT`), met een toets die faalt zonder de fix |
| F4 | `republish` heeft een eigen antwoordplek (`REPUBLISH_REPLY`) |
| F6 | `setup` beslist één keer of er een Hop is (`hop_here`), voor het token én de watchdog |
| F10 | `Doors::staffed`: deuren zonder werker blijven dicht (zie §13.8, terug te snoeien) |
| F11 | Secundaire SMP-cores parkeren pas bij `Exited`, niet bij de kill-vlag |
| F15 | applib meldt `HOPOS_TASK_DROPPED`, en ook `HOPOS_TIMER_OVERFLOW` (zie §13.8) |
| F18 | De koude flip zet de USB stil, als laatste `.await` vóór de sprong |
| F19 | Eén staat `Phase::{Idle, Pending, Jumping}`. Een tweede FLIP is `HOPOS_FLIP_BUSY` |
| §12 | De docs en het commentaar zijn bijgewerkt, ook `description.md` §8 (bijna alles was op 04-10 al opgelost) |

Getoetst op 24-10:

- **`tools/gate.sh`:** groen in 220 s.
- **`tools/qemu-all.sh`:** 32 van de 33 groen. Alleen `store` is rood, en die
  is ook rood op een schone 2d1ed1f (F23). Hij is niet veroorzaakt door de
  reparaties.
- **25-10, F23 in hop:** de gate van hop groen; met `HOP_REV=worktree`
  `store`, `hop`, `hop-restart`, `hop-fail`, `welcome`, `flip` en
  `riscv-hop` groen, zonder één `HOPOS_SYSTEM_REFUSED`. Let op: zonder
  `HOP_REV=worktree` bouwen de ringen HEAD van hop, dus `qemu-all` blijft
  rood op `store` tot de reparatie in hop gecommit is.

## 12. Docs en commentaar die achterlopen

De code wint. Dit is wat bij het lezen afweek. Alles hieronder is op 24-10 nagekeken en rechtgezet; drie punten van de eerste lijst bleken zelf niet te kloppen en staan hier verbeterd.

**`docs/description.md` (04-10)**

| Plek | Wat er staat | Hoe het nu is |
| --- | --- | --- |
| §8.2, §8.16 | `NatSnapshot`, `Handoff.agent` | Weg (c6e0bf8, f1917f0) |
| §8.5 | gicv3 implementeert `Controller` niet | Wel, sinds 8e8cea4 |
| §8.11 | `Paced`, `start`/`poll_done`, `Fs::read_at`, Hop's poorten via `net::publish` | Weg sinds 110b945 |
| §8.13 | `bounded::Text` bestaat niet | Bestaat |
| §5.1 | `Pool` | Heet `Futures` |
| §4.4 | De app draait twee taken | Tot drie plus de pomp |
| §4.1 | Alle vier de ingangen zetten vectoren en MMU | riscv zet `mtvec` en de FPU, maar geen `__hopos_vectors` en geen MMU |
| §4.8 | De root wordt per levensduur gewist | Lui, bij de eerste call van de volgende |

**`docs/flip.md`**

- "een vaste staart van een paar KiB": `HANDOFF_TAIL` is 256 KiB. De 1 MiB in flip.md klopt wel: dat is de staat van Hop (`HOP_STATE_MAX`).
- De kerneltabel mist de M4.
- De assertie van de zwarte doos staat in de board-slots.

**`docs/heap.md`** mist de exacte klassen (9107dd6).

**Commentaar in de code**

| Plek | Wat er staat | Hoe het nu is |
| --- | --- | --- |
| `hopos/src/main.rs`, `flip.rs` | `flip::land` "als eerste na de heap" | `discover` en `move_to_os_core` gaan ervóór |
| `kern/src/slots.rs` (`try_conn`) | "Tot 25-10" | De commit is van 24-10 |
| `kern/src/cage.rs` (`CoreClass`) | Hop vraagt `Small` | `Placement::hop` vraagt geen klasse |
| `heap/src/lib.rs` | Belooft een plafond onder het RAM | `set_ceiling` is weg; het plafond is het gebied |
| `executor/src/lib.rs` | "pollen, dan timers" | Spawns, timers, pollen |
| `hopos/src/net.rs`, `kern/src/rpc.rs`, `flip.rs` | De heap is een bump-allocator | Hij heeft vrije lijsten sinds 29-09 |
| `hopos/src/conport.rs` | Een ring van 64 KiB | 256 KiB |
| `net/src/nat.rs` | `Nat::from_slot` | Heet `slot_reply` |
| `blkdev/src/lib.rs` | De mount gebruikt `Spin` | `ExecPace` |
| `tools/qemu-test-slowdisk.sh` | `InFlight` | Weg |
| `abi/src/layout` | `SCHED_CURRENT` en `SCHED_ROTOR` zijn "alleen RISC-V" | Arm gebruikt ze ook |
| `cpu/src/el2/stage2.rs` | "de TLBI via de HVC" | Er is geen HVC meer |
| `cpu/src/irq.rs` | "SPSR_EL1.I" | Moet SPSR_EL2 zijn |
| `applib/src/rand.rs` | "geen SHA-256, derde kopie" | `abi::sha256` is de ene kopie |
| `board/uefi/src/el2.rs` | `HCR`, `SCTLR` en `CPTR` worden door de ingang gebruikt | Voor nVHE alleen door de tests (de ingang schrijft literals); de VHE-tak laadt ze wel |



---

## 13. Het voorstel: wachten, bellen, hard falen, LEAN

Dit is het voorstel om morgen (25-10) door te lopen. Het staat nog niet
vast. Het rust op §11 en op Dereks regels van 24-10:

- Bellen alleen voor het LAN en het interne LAN. De schijf hoort erbij,
  want reads komen daar ook uit, en zonder bel weet je niet wanneer iets
  klaar is.
- Goed wachten, en anders hard falen, overal. Geen honderd retries.
- LEAN: alleen code die nodig is en een reden heeft. De backbone eenvoudig
  houden, anders wordt de flip alleen maar lastiger.

Per punt staat wat er nu is, wat ik zou doen, wat er daardoor weg kan, en
wat het voor de flip betekent. Wat ik niet zelf kan beslissen, staat in
§13.9.

### 13.1 De vier regels

**R1. Bellen.** Een bel alleen voor wat van buiten de kern binnenkomt:

1. de NIC (het LAN);
2. de kick van een app-core (het interne LAN: de slot-ringen);
3. de completion van de schijf;
4. de timer van een deadline.

Verder niets. Binnen de kern is alles een volgorde, of een call met een
antwoord.

**R2. Wachten.** Elke wacht is één bel of één antwoord, plus één termijn.
- Geen vangrail náást een bel. Een verloren wek is een fout die we
  repareren, niet afdekken.
- Waar het ijzer geen bel heeft (de link van de PHY, het staatswoord van een
  andere core), is er een begrensde lees binnen de termijn, met de reden
  erbij.

**R3. Falen.** Mist een termijn, breekt een invariant van de kern, of faalt
het ijzer, dan volgt één regel `HOPOS_FATAL <reden>` en `fatal()`: de core
parkeert, de pets stoppen, de watchdog reset.
- Geen retry, geen terugvalstand. De koude boot is de enige retry.
- Een fout van een app blijft een antwoord aan die app, of een stop van dat
  slot. Behalve Hop: Hop vallen is de node vallen.

**R4. LEAN.** Code zonder gebruiker of zonder reden gaat weg. Een tweede weg
naar hetzelfde gaat weg. Een vangrail, retry of terugval mag alleen blijven
met een reden die hier staat.

### 13.2 Hard falen: het mechanisme

| Nu | Voorstel |
| --- | --- |
| Een panic parkeert (`HOPOS_PANIC`); daarnaast tientallen `*_FAIL`-regels waarna de kern doordraait | **`fatal(reden) -> !`** in `cpu`, naast de panic-handler; dezelfde weg (emergency-console, `park`). Elke `*_FAIL` waarna nu wordt doorgedraaid, wordt per stuk `fatal` of verdwijnt (§13.3-13.7) |
| De watchdog wordt gewapend in stap 12 van `setup`, ná de mount: een fout in de mount parkeert voorgoed zonder reset | **De watchdog als eerste in `setup`.** De hardware staat er na `discover`. De synchrone stappen van `setup` moeten dan binnen de timeout van de hardware vallen; daarom de link-wacht uit `setup` (§13.3) |
| Fase 1 aait op een koude boot blind zonder termijn (F5); daarnaast een eigen flip-guard (`flip::GRACE`, 120 s, PSCI-reset) | **Eén guard voor elke boot:** levensteken (een verse dial plus de slag van Hop) binnen de termijn, anders geen pets meer. `flip::guard`, de PSCI-reset en de tweede klok vallen weg (S3, S11) |
| QEMU virt is onbewaakt zonder `WDT=1` | Zie §13.9 vraag 2 |

Voor de flip: één guard en één reset-weg is minder om over te dragen.

### 13.3 Interrupts, bellen en de boot als één reeks

**Wat blijft (R1):**

- de NIC-lijn;
- de kick-SGI van de app-cores (`KICK_SGI`, HVC #6 en de yield);
- de schijf-lijn (virtio-blk, NVMe met MSI-X);
- de timer-PPI's (de deadline van de executor, CNTHP voor de beurt).

Dat is al wat qemuvirt nu aanzet (`board/qemuvirt/src/lib.rs`
`start_interrupts`).

**Wat nu ernaast staat, en weg kan:**

| Vangrail of terugval | Waar | Voorstel |
| --- | --- | --- |
| `start_interrupts` faalt, en dan door op "de vangrail van de slaap" (`HOPOS_IRQ_FAIL`) | `main.rs` `setup` | `fatal` |
| De schijf-lijn geweigerd, en dan de vangrail van 10 ms | `board/qemuvirt` `start_interrupts`, `blkdev` | `fatal` |
| `IRQ_GUARD` 10 ms naast de bel | `net/src/pump.rs`, `blkdev/src/queue.rs` | weg |
| De switch-failsafe 1 ms (`after_deferrable`) | `net/src/switch.rs` | weg |
| `SERVICER_TICK` 2 ms en `SERVICER_GUARD` 10 ms | `kern/src/slots.rs` `serve` | weg; de servicer wordt door de kick gewekt (hieronder) |
| `TURN_CAP_NS` en `WFI_CAP_NS` 10 ms | `cpu/src/el2/oscore.rs`, `cpu/src/idle.rs` | weg; de beurt eindigt op de deadline van de executor of een bel (de executor heeft altijd de tik van 1 s) |
| De poll van 300 µs/200 µs zonder lijn | pomp, blkdev | alleen waar het board geen lijn heeft, met de reden; zie vraag 3 |

**De volgorde voor de vangrails.** Eerst één soak met de tellers die er al
zijn (`sw(door= timer=)` in `HOPOS_TICK`), en daarna weg. Vond een vangrail
werk, dan is dat een verloren wek. Die repareren we vóór hij weg gaat. Dat
is één meting (zie het geheugen "één run per punt"), geen tussenvorm in de
code.

**De kick als de bel van het interne LAN.** Nu luidt alleen de yield en
HVC #6 de kern. Voorstel:

- De switcher kickt de OS-core ook bij HVC #0 (exit) en bij een fault. Dat
  is één regel in de exit-tak van `\p_entry`, en op riscv in ecall a7=1.
- Bij elke kick kijkt de deurbel (`Doorbell::sleep` `ready()`) naar de
  TX-ringen én de outbox van elk slot.

Dan wordt het volgende sequentieel of wacht het op die ene bel:

- de servicer, zonder eigen tik;
- `wait_quiet` van de stop, op de kick van de exit;
- het zien van een gevallen Hop (§13.4);
- het SMP-verzoek, dat nu via de servicer loopt.

**De boot als één reeks.** In `Node::run` (`hopos/src/net.rs`) achter elkaar:

1. de lease;
2. `SetUplink`;
3. de stack;
4. de keeper, de system-listener, de console-listener en de input-listener;
5. **de plaatsing van Hop**, als stap in dezelfde reeks.

Er is dan niets meer om op te wachten. Wat weg kan:

| Weg | Waarom |
| --- | --- |
| `wait_uplink` en `UPLINK_WAIT` (10 s), `HOPOS_HOP_NO_UPLINK` | Hop start pas ná de lease, in de reeks |
| `nic_retry` en `NIC_RETRY` | Een NIC die er is maar faalt, is `fatal`; de reset is de retry. Dereks eis van 19-09 ("geen 100 dode nodes") blijft gedekt: de node komt op zodra de kabel er is. Een board zonder NIC (`Ok(None)`) blijft gewoon |
| De eindeloze lease-lus (elke 5 s) | Eén DHCP-sessie (de hertransmissies van het protocol zitten in leandhcp) binnen de termijn van de guard; daarna houdt de guard de pets in |
| De proeflease van de keeper bij een verlopen lease (`HOPOS_DHCP_RETRY`) | `fatal`; een NAK en een ander adres doen al `request_reset` |
| De 100 ms-poll van de input-listener op de stack | Start in de reeks |
| De 100 ms-poll van de console-lezer | `conport::drain` schrijft in dezelfde ronde ook naar de TCP-lezers |
| De link-wacht van 8-12 s als spin in `probe_nic` | De probe zonder spin; de link als begrensde lees (de PHY heeft geen bel, R2) in de reeks, vóór de lease |

Ook `fatal` in plaats van doordraaien, op drie plekken in `main.rs` en
`slots.rs`:

- `HOPOS_OSCORE_FALLBACK`;
- `HOPOS_OS_SELFTEST_FAIL`, de zelftest die nu toch host (F14);
- `HOPOS_OS_CORE_FAIL`.

Voor de flip:

- Minder taken die tussen "klaar" en "nog niet" zitten.
- Alle lijnen zijn LAN, kick of schijf, en die drie kent de flip al
  (`HoldAdoption`, de ringen, hopfs bevriezen).

### 13.4 Hop

| Nu | Voorstel |
| --- | --- |
| `Supervisor::watch` vraagt elke seconde `Status`; `wait_leave` elke 50 ms; `crashed`, `stay_down`; dan wacht de node 15 s op de canary | **Hop valt, dan meteen `fatal`.** Op de OS-core ziet `settle` de exit of fault op het moment zelf (`Back::Exit`/`Fault`); op een app-core komt de kick van de exit (§13.3). Was de exit niet gevraagd: de diagnoseregel (`HOPOS_HOP_FAULT`/`EXIT`), `HOPOS_HOP_DOWN`, `fatal`. Geen statuspoll, geen 15 s |
| `HOP_RESTART`: wachten op vertrek (poll), de bel als vangnet, `replace` | Dezelfde reeks, zonder poll: de stop met de termijn (die wacht op de kick van zijn exit), dan `drop_streams`, dan plaatsen. Mislukt een stap, dan `fatal` (zoals nu, maar direct) |
| F5: een Hop die bij zijn eerste start valt, wordt nooit gereset | Opgelost door de valdetectie hierboven, en door de termijn van de guard (§13.2) |
| `place_first`, `run_once`, `watch_first` (de QEMU-rol `app`) in de productiebinary | Weg uit hopos; de toetsen die hem nodig hebben (`qemu-test.sh`, `-fault`, `-nvme`, riscv) plaatsen appspike via Hop, zoals `qemu-test-hop.sh` al doet. Zie vraag 7 |
| De store-ops die de vorige Hop ophaalde, gaan terug in de rij | Zie vraag 5 |

Voor de flip: een meegekomen Hop heeft geen supervisor-staat meer om terug
te zetten. Alleen "wie is Hop" (slot 1) en zijn spec blijven.

### 13.5 Slots

| Nu | Voorstel | Reden |
| --- | --- | --- |
| Een stop die niet stil wordt, ook niet na de intrekking: quarantaine (`Partition<Quarantined>`, `Held::Quarantined`, `confirm`, `HOPOS_PART_QUARANTINE`) | `fatal` | De kern kan de isolatie niet meer bevestigen; doordraaien met een slot dat misschien nog loopt, is een terugvalstand. Weg: de quarantaine-typestate en zijn paden; de partitie heeft dan alleen `Free` en `Owned` |
| Een dispatch met onbekende uitkomst (`boots` false, dispatch-fout): quarantaine | `fatal` | Idem |
| `NEVER_RAN`: de core uit de plaatsing tot een koude boot (`Places::retire`) | `fatal` | De koude boot doet hetzelfde, schoon. Weg: `retired` |
| Reclaim: na 2 s de vasthouder van een gedeelde core offeren | **blijft** | Dit is beleid voor apps (een buur die nooit yieldt), geen fout van de kern |
| `LIFE_TICK` (100 ms per open system-verbinding), `IDLE_TIMEOUT` (300 s), het store-wachten op 100 ms | De stop breekt als stap de TCP-verbindingen van dat slot in de node-stack af; de werker ziet dat als een gewoon LAN-event | Er hoeft niemand meer te kijken of zijn generatie leeft. **Afhankelijk van leannet:** een "breek alle verbindingen van dit adres af" (deel 2) |
| `Servicers::changed` als bel naar de committer | De stop doet de commit als laatste stap, met ack | Een volgorde in plaats van een bel |
| `snapshot` weigert quarantaine | Vervalt met de quarantaine | |

Voor de flip: zonder quarantaine is elke bewoner levend of weg. `snapshot`
heeft dan één weigering minder, en `validate_adoption` één toestand minder.

### 13.6 hopfs

| Nu | Voorstel |
| --- | --- |
| Een mount die een boom niet ontcijfert: een lege boom met `dirty` (F12); een I/O-fout telt als "ongeldige plek" | `fatal`. Alleen "geen enkele geldige plek op een lege schijf" is een verse fs |
| Een I/O-fout van het device gaat als fout terug naar de app | `fatal` (het ijzer faalt) |
| Een commit die faalt: `dirty` terug, opnieuw proberen, na drie regels stil (O15) | `fatal` |
| De `Volatile`-stand (schijf te klein voor twee plekken) | weg: `fatal` bij de mount |
| Geen schijf: geen fs, calls luid geweigerd | **blijft** (een board zonder schijf is geen fout: de Pi's, de Radxa, de LicheeRV) |
| Bevriezen voor de flip met 2 s, en een `Freeze` die blijft liggen (F3) | Wachten op het antwoord, met een ruime termijn; gemist is `fatal` (§13.7) |
| Een tweede commit tijdens een commit wacht vooraan in de rij (S16) | Wegvallen: `dirty` draagt hem al naar de volgende |
| De rij is één FIFO over alle slots (head-of-line) | **blijft** (LEAN); alleen de commit-dubbel valt weg |
| De standaard `hopos.storage=stateless` (F8) | Zie vraag 8 |

Voor de flip: een hopfs die alleen "goed" of "dood" kent, heeft bij het
bevriezen geen halve toestand meer.

### 13.7 De flip

| Nu | Voorstel |
| --- | --- |
| Na de 202 kan het nog misgaan: de freeze haalt de termijn niet, de NAT-snapshot faalt, `snapshot` weigert, `encode` is te groot, de layout van `chain`. Dan `Frozen::thaw` en doorgaan (de node draait door, Hop hoort het alleen via de console) | **Na de 202 is er geen weg terug:** óf de sprong, óf `fatal`. Alles wat kan weigeren, schuift naar `prepare`, vóór de 202: een `Request::Snapshot` als proef (weigert bij een stroom of dode kooi), de maat van het blob vooraf, en de layout van `Jump::check`. Weg: `Frozen`, `thaw`, `FinishAdoption` als ontdooier, `cores_back`/`unpark_after_flip` |
| Koud: `cores_off` faalt, dan `cores_back` | `fatal` |
| Twee guards (zie §13.2) | Eén |
| `SUM`, `bundle_sum`, `window`, `total`, de nulwoorden, vier recorder-stappen zonder schrijver, `PSCI_HVC` (S17) | Weg (met een `FLIP_ABI`-stap als het blob-formaat daardoor verandert) |
| De koude weg slaat `quiesce_usb` over (F18) | Gerepareerd (24-10): de USB gaat stil na `stop_residents` en `cores_off`, als laatste `.await` vóór de sprong |
| Drie flip-merktekens (x3, x1=0 plus de feitenpagina, geen op riscv) | Zie vraag 6 |

### 13.8 LEAN: weg zonder vervanging

Uit de leesronde. Per stuk eerst een grep op gebruikers, dan pas weghalen.

- **Alleen voor de toetsen, maar in de productiemodules:**
  - `FsActor::handle`, `kern_read`, `commit`, `freeze`, `into_fs`, `fs`,
    `tree`;
  - `Desk::frozen_after`;
  - `hopfs::path`, `Fs::list_n`, `Queue::in_flight`.

  Naar `#[cfg(test)]`, of de toetsen via `admit` laten lopen. Dat laatste
  toetst dan ook de regels.
- **Dode meetlatten:**
  - `ArmSleeper`/`RvSleeper::stats`;
  - `CTX_SLEEPS`/`CTX_WAKES` (de switcher schrijft, niemand leest);
  - `SCHED_ROTOR` op arm;
  - `host_tx_drops`, `uplink_rx_drops`.
- **Dood of dubbel:**
  - `CTX_REVOKE` op arm-app-cores (geen lezer);
  - de onbereikbare `|| src == 0` in `forward`;
  - de `.L_s2none`-tak in `\p_smp`;
  - `el1_timer_access` naast `merge_event_stream`;
  - de ongebruikte nVHE-constanten in `board/uefi/src/el2.rs`;
  - `Nat::flow_count`, `Switch::nat`, `Switch::is_attached`,
    `Pump::is_holding`, `HostPort::stack` buiten de toetsen;
  - `FlowTable::maybe_compact`.
- **Apple:** geen OsCore bouwen, testen of hosten als `SHARES_OS_CORE`
  onwaar is (S5).
- **Een tweede weg naar hetzelfde:**
  - `Board::boot_param` naast `NodeCfg::one` (S4): `NodeCfg` vroeger in
    `setup`, en alleen die;
  - `max_slots` op drie plekken (S19; weg in v3.2.0);
  - het kern-adres op vijf plekken, en de draadvorm in applib naast abi
    (S14).
- **De literals van het EL2-regime** op vier tot vijf plekken: één set
  constanten (S1). Of één `el2_init` voor de vijf ingangen: dat is groter
  werk (asm), en hoort bij de flip-ingangen (vraag 6).

**Terug te snoeien na de beslissing, uit de reparaties van 24-10:**

- `Doors::staffed` (F10, `sync/src/doors.rs`) dekt een half gespawnde
  werkerpool af. Onder R3 is een spawn die faalt `fatal`, en dan gaat
  `staffed` weer weg.
- De melding van `timer_overflows` in applib (F15, `Losses`/`Lost` in
  `applib/src/rt.rs`) is meer dan gevraagd. De regel voor `dropped` volstaat.

### 13.9 Vragen die ik niet zelf kan beslissen

1. **De termijn van de guard.**
   - Voorstel: 120 s, zoals nu de flip, voor elke boot.
   - Langer als DHCP of de link op sommige netten traag is.
2. **QEMU zonder watchdog.** Zonder `WDT=1` parkeert een `fatal` voorgoed.
   Twee opties:
   - **a.** Elke ring met `WDT=1` (`tools/lib.sh`).
   - **b.** `fatal` doet een PSCI-reset als er geen watchdog gewapend is.

   Voorstel: **a**, want b is een tweede weg.
3. **Boards zonder lijn:** de NIC van de Pi 4 (GENET), de schijf van de M4
   (ANS) en die van riscv-virt pollen nu. Twee opties:
   - **a.** Eerst de lijn bedraden (board-werk).
   - **b.** Pollen toestaan als feit van het board, met de reden in het
     board (R2), nooit naast een bel.

   Voorstel: b nu, a als board-werk op ALLES.md.
4. **USB en de codec.** usbin pollt elke 4 ms, de codec elke 1 ms. Ze zijn
   geen LAN. Twee opties:
   - **a.** Poll houden: het zijn optionele vlakken (`gui`, `media`), en een
     poll houdt de flip eenvoudiger dan een extra lijn.
   - **b.** Naar interrupts (xHCI, de VPU op intid 358).

   Voorstel: **a**, met de reden in de code.
5. **De store-ops van een vertrekkende Hop.** Nu gaan ze terug in de rij.
   Voorstel: falen, zodat de app een fout krijgt. Een herstart van Hop is
   gevraagd en zeldzaam.
6. **De flip-ingangen.** UEFI heeft de feitenpagina nodig (geen DTB), de
   rest kan x3. Twee opties:
   - **a.** Zo laten.
   - **b.** Eén protocol: overal de feitenpagina, of overal x3 plus de
     pagina.

   Voorstel: a tot de flip zelf aan de beurt is.
7. **De QEMU-rol `app` (`place_first`).** Zes toetsen leunen erop
   (`qemu-test.sh` met de varianten, `-fault`, `-nvme`, riscv). Twee
   opties:
   - **a.** Ombouwen naar Hop.
   - **b.** Houden achter een feature.

   Voorstel: **a**. Een productiebinary zonder testpad, en de toetsen lopen
   de echte weg.
8. **De standaard van hopfs.** Voorstel: `stateful` als standaard, en
   `stateless` als expliciete keuze per node. Met `stateless` belooft
   `OP_SYNC` niets over een koude boot heen.
9. **De ene retry van applib.** Een transportfout wordt één keer herhaald
   met een nieuwe seq (`applib/src/sys.rs`, de flip van 06-09). De reden:
   na een warme flip is de node-stack nieuw, dus elke open
   system-verbinding is weg.
   - Houden, als de enige retry met een reden?
   - Of: een call die tijdens een flip in de lucht was, faalt, en de app
     beslist.

   Voorstel: houden. Hij hoort bij de warme flip, niet bij een fout.
10. **De outbox.** Een derde ring naast de twee net-ringen, met een eigen
    lezer. Twee opties:
    - **a.** Houden, gewekt door de kick (§13.3).
    - **b.** Logregels over het LAN (`Kind::Log`, wat de Go-apps al doen),
      en de outbox alleen voor de laatste woorden (een panic of fault vóór
      de exit), gelezen in de stop-reeks.

    Voorstel: a. Een app logt al vóór zijn net op is, en b raakt elke app.

### 13.10 Volgorde van uitvoeren (voor de agents)

*Vervangen door §14.8 (01-11).*

Elke fase eindigt met `tools/gate.sh` en `tools/qemu-all.sh`, en daarna
`target/` opruimen. Per fase krijgt elke agent zijn eigen bestanden.

1. **Fundament** (één agent, alles hangt eraan):
   - `fatal()`;
   - de watchdog als eerste;
   - één guard met termijn;
   - de QEMU-ringen met `WDT=1` (als vraag 2 a wordt).
2. **Parallel:**
   - **a. De boot als reeks plus het net:** `hopos/src/main.rs`,
     `hopos/src/net.rs`, `net/src/pump.rs`. `nic_retry` en `wait_uplink`
     weg, de keeper hard, de link-wacht.
   - **b. hopfs:** `kern/src/hopfs.rs`, `rpc.rs`, `hopos/src/storage.rs`.
     Mount, I/O en commit hard, `Volatile` weg, de commit-dubbel weg.
   - **c. Slots en Hop:** `kern/src/slots.rs`, `partmem.rs`, `pool.rs`,
     `hopos/src/slots.rs`, `kooi.rs`, de exit-kick in `cpu/src/el2/switch.rs`
     en `cpu/src/riscv/switch.rs`. Quarantaine, retire, supervisor-poll en
     `place_first` weg; de servicer en `wait_quiet` op de kick.
   - **d. De flip:** `hopos/src/flip.rs`, `kern/src/kernflip.rs`. Na de 202
     geen weg terug; de proeven naar `prepare`; S17.
3. **De vangrails:** één soak met de tellers, de verloren wekken repareren,
   dan de vangrails weg.
4. **LEAN-opruiming (§13.8) en de docs.**

Na fase 2 komt deel 2 (LEAN): de verbindingen van een adres afbreken
(§13.5) is daar het eerste.

---

## 14. Het doel: het HOK en het NETWERK

> "RUST (en GO) draaien gewoon in het HOK wat we ze geven, het enige wat wij
> moeten maken is het HOK en we faciliteren NETWERK, punt." (Derek, 01-11)

Elke regel code moet zich verantwoorden. De richting is het OS van ooit:
10k + 10k. Dit hoofdstuk legt de besluiten van 01-11 vast, de nulmeting, de
vorm die we zoeken, en de werklijst voor de agents. Daarna zoomen we per
element in, met de vraag "wat willen we bereiken, en hoe simpeler". Het
eerste element is de **flip**.

### 14.1 Besluiten van 01-11

Over de vragen van §13.9 (O), deel 2 (L, `lean/docs/sequence-lean.md`) en
deel 3 (H, `hop/hop/docs/sequence-hop.md`):

| # | Besluit |
| --- | --- |
| O1 | Eén guard van 120 s voor elke boot |
| O2 | De watchdog is verplicht, overal, ook op QEMU. Geen testpad, geen optie: elke ring draait met de watchdog |
| O3 | Altijd een interrupt. De boards zonder lijn krijgen hun lijn (GENET op de Pi 4, ANS op de M4, virtio-blk op riscv-virt) |
| O4 | USB (4 ms) en de codec (1 ms) blijven pollen, maar DUIDELIJK bijgehouden: één tabel "wat pollt en waarom", en verder pollt niets |
| O5 | De store-ops van een vertrekkende Hop falen; de app krijgt een fout |
| O6 | Generiek: overal dezelfde ingang met de feitenpagina, x3 weg |
| O7 | De QEMU-rol `app` (`place_first`) weg; de toetsen lopen via Hop |
| O8 | Compute is stateless, en dat blijft de standaard: zo ontwikkelen we echt stateless |
| O9 | De ene retry van applib blijft (hij maakt de warme flip mogelijk), met zijn plaats in het verbindingenbudget: de klok van Hop gaat over de hoofdverbinding |
| O10 | De outbox blijft, met één lezer die door de kick gewekt wordt, zonder poll |
| L1 | Het TLS 1.2-werk gaat erin, als aparte toevoeging (los aan of uit te zetten) |
| L2 | De GET-retry van leans3http weg: dat is applicatiecode |
| L3 | De ISS als sleutelhash in leannet |
| L4 | Een afwijking van de RFC alleen met een reden in KAM, anders goed zetten (RTO_MIN, TIME-WAIT en de SYN-ladder hebben een reden; FIN-WAIT-2 naar 60 s; CLOSE-WAIT weg zodra er een abort is) |
| L5 | KAM naar Rust |
| L6 | DHCP blijft op vaste rondes, met de reden in KAM |
| L7 | De abort per adres (`abort_peer(ip)`) |
| L8 | Eerst een lijst van wat weggaat; Derek ziet hem vóór er iets weggaat |
| L9 | De uitgaande GOAWAY van leanh2 weg |
| H1 | Het gat na een warme flip: de lease gaat mee in de handoff, de listener staat er vóór Hop een beurt krijgt; daarna is elke call zonder antwoord fataal |
| H2 | Een configfout is `fail(config)` |
| H3 | `Kept::Unreadable` is hard falen, verder niets |
| H4 | Een leider die zijn staat niet kan lezen, leidt niet (ook `agentd`) |
| H5 | De nieuwe Hop vertrouwt de klok van de kern |
| H6 | Geen compat-code |
| H7 | Init-jobs die er zijn maar niet kloppen: hard |
| H8 | `NEXT_LOG` in de lange wacht van de store-taak |
| H9 | Exitcodes per soort, vastgelegd in `abi` |
| Flip | Warm, één flip, op een chirurgische manier (§14.4) |
| Opslag | Blijft: SQLite moet erop kunnen (en dus bestanden), op NVMe |
| Media | Helemaal los: headless, `gui` en `media` als losse flags (§14.3) |
| riscv | Blijft |
| Meten | Op één manier (§14.6) |
| Console over TCP | Blijft (geen kabels zodra er netwerk is), maar kleiner (§14.5) |

### 14.2 De nulmeting (01-11)

Regels **code**: zonder commentaar, zonder lege regels, zonder toetsen
(`tests*.rs`, `tests/` en de blokken na `#[cfg(test)]`). Gemeten met
`tools/loc.py`. Elke agent meet zijn deel ervoor en erna.

De backbone van hop-os: **41.360**.

| Deel | Code | Hoort bij |
| --- | ---: | --- |
| slots, kooi, geheugen (kern + hopos) | 6.213 | HOK |
| cpu aarch64 | 5.444 | HOK |
| cpu riscv | 3.082 | HOK |
| applib | 2.551 | HOK (de app-kant) |
| abi (layout, plaatsing) | 1.712 | HOK |
| executor, heap, sync | 1.845 | fundament |
| switch, NAT, node-stack, appnet | 6.005 | NETWERK |
| system-API | 3.141 | NETWERK (het hok beheren) |
| opslag: hopfs, store, blkdev | 4.047 | opslag |
| scherm, media, apparaten | 2.446 | los (§14.3) |
| flip (`flip.rs`, `kernflip.rs`) | 1.741 | plus verspreid over ± 30 bestanden |
| meten: bench, hopcost, telemetrie, load | 1.174 | één manier (§14.6) |
| entropie | 810 | klein houden |
| console over TCP | 432 | kleiner (§14.5) |
| toetsen met een codenaam (`*_tests.rs`, `testutil`) | 717 | geen code |

Verder: driver 16.871, board 14.746, fw/gui/media 6.714. lean 23.035
(werkboom met het TLS 1.2-werk). Hop op HopOS ± 20.900.

`hopos` heeft 8.046 regels code en geen eigen toets: dat is de lijm die
alleen de ringen toetsen.

### 14.3 Headless, `gui` en `media`

Lumen bewijst het: media zonder scherm (Blu-ray lezen, de hardwaredecoder,
encoderen, bewaren op NVMe). Nu trekt `media` toch de hele `gui` mee
(`hopos/Cargo.toml`: `media = ["gui", …]`), omdat de optische drive een
USB-apparaat is en de USB-stack onder `gui` staat (`hopos/src/optical.rs`
gebruikt `gui_usbin::storage`). USB is geen GUI, het is een bus.

| Build | Inhoud |
| --- | --- |
| headless (geen flag) | het HOK, het NETWERK, opslag |
| `gui` | framebuffer, fbgrant, HID, plus de bouwsteen `usb` |
| `media` | codec/VPU, optische drive, plus de bouwsteen `usb` |

- `gui` en `media` staan los; geen van beide trekt de ander mee.
- `usb` is een interne bouwsteen (xhci, dwc3, mass-storage, de poll), geen
  keuze voor de gebruiker.
- De toets: een headless build bevat geen regel scherm, media of USB.

### 14.4 De flip: warm, één, chirurgisch

**Nu** zijn er acht: warm, koud, mismatch, oscore, de ingang van de rpi4,
uefi (de feitenpagina), riscv (koud) en de M4. Ongeveer dertig bestanden
weten van de flip: `kern/src/slots.rs`, `system.rs`, `rpc.rs`,
`partmem.rs`, `grants.rs`, `watchdog.rs`, `nodecfg.rs`, `net/src/nat.rs`,
`switch.rs`, `cpu/src/boot.rs`, `smp.rs`, `el2/chain.rs`, `el2/dispatch.rs`,
`abi/src/layout`, `hopos/src/main.rs`, `slots.rs`, `storage.rs`, `gui.rs`,
`cage_riscv.rs` en een `slots.rs` per board.

**De vorm die we zoeken:** de flip is een gewone boot, plus een handoff.

1. **Eén ingang.** Kern B start via dezelfde ingang als bij stroom aan, met
   dezelfde feitenpagina (O6). De pagina heeft één veld meer: waar de
   handoff ligt, of nul.
2. **Eén reeks.** De boot loopt altijd: lease, uplink, stack, listener,
   NAT, slots, Hop. Per stap één tak: leeg beginnen, of overnemen uit de
   handoff.
   - lease: DHCP, of uit de handoff (H1);
   - NAT: leeg, of de gepubliceerde poorten en flows;
   - slots: leeg, of geadopteerd (partitie, core, generatie);
   - Hop: plaatsen, of adopteren met zijn blob.
3. **Elk onderdeel kent alleen zijn eigen stuk:** `capture` aan de kant van
   kern A, `restore` aan de kant van kern B. Alleen de flip zelf weet dat er
   een flip is. Nergens anders een `if flip`.
4. **De acht vallen samen.** Koud is een boot zonder handoff. Mismatch is
   een handoff met een andere versie, en dus koud. oscore, rpi4, uefi, riscv
   en de M4 gaan door dezelfde ingang.
5. **Wat een warme flip niet overleeft:** de open TCP-verbindingen van de
   node-stack. Daarvoor is de ene retry van applib (O9).
6. **Na de 202 geen weg terug** (§13.7): kern A geeft niets meer terug aan
   Hop zodra hij springt; mislukt de landing, dan reset de ene guard (O1).

Nog na te gaan bij het inzoomen: de feitenpagina op de M4 en de Pi, en wat
de zwarte doos (§14.5) in de handoff doet.

### 14.5 Console over TCP: één ring

**Nu** gaan dezelfde console-bytes naar drie ringen: de conport-ring
(256 KiB, `hopos/src/conport.rs`), `KERN_LOG` voor Hop (`NEXT_LOG` 0,
64 KiB, `kern/src/system.rs`) en de zwarte doos van de flip
(`flip::black_box`, die een reset overleeft). Elke TCP-lezer pollt elke
100 ms (`kern::conport::POLL`).

**Doel:** één ring, op de vaste plek van de zwarte doos (hij overleeft een
reset en een flip), met drie lezers: de UART, TCP en Hop (`NEXT_LOG` 0).
De lezers worden door de schrijver gewekt, geen poll. Weg: twee ringen, de
tee naar drie plekken, de poll. Richting: ± 150 regels in plaats van ± 430.

### 14.6 Meten: één manier

De app publiceert zijn tellers op zijn control-page (`CTRL_IDLE`,
`CTRL_WAKES`), de kern geeft ze in `SLOT_STATUS`, Hop rekent en toont
(`KernSys::cpu_pct`). Dat is de manier.

Weg:

- `HOPOS_SLOT_LOAD`: dezelfde berekening nog eens in de kern, op de console
  (`kern/src/load.rs`, `hopos/src/load.rs`);
- `cpu/src/hopcost.rs`: de "tijdelijke meetlat" van 03-10;
- `hopos/src/bench.rs` uit de productiekern (nvmebench, wdtest, idlestat):
  wat nodig blijft, wordt een app die via de system-API meet.

Blijft: de temperatuur, één woord op de control-page van Hop.

### 14.7 Hard falen en interrupts

§13.1 tot en met §13.6 staan, met de besluiten van §14.1 erin:

- `fatal()`: één regel `HOPOS_FATAL <reden>`, parkeren, de watchdog reset;
- de watchdog als eerste in `setup`, één guard van 120 s voor elke boot
  (`flip::guard` en de tweede klok weg);
- elke QEMU-ring met de watchdog (O2);
- quarantaine en retire worden `fatal`, `LIFE_TICK` weg (§13.5);
- hopfs hard (§13.6);
- de vangrails naast een bel weg (§13.3);
- de lijnen van O3, de tabel van O4.

### 14.8 Werklijst (01-11)

Elke agent werkt in een eigen git-worktree naast de repo, op een eigen
branch vanaf de commit met §14. Hij meet zijn deel ervoor en erna met
`tools/loc.py`, draait de gate en de ringen van zijn deel, ruimt daarna
`target/` op, en commit lokaal (geen push, geen tag, geen Claude in het
bericht). Daarna zoomen we per element samen in en voegen we samen.

| Agent | Wat | Eigenaar van |
| --- | --- | --- |
| **A. Flip** | §14.4: één ingang met de feitenpagina, de boot als één reeks met per stap leeg of uit de handoff, `capture`/`restore` per onderdeel, de lease in de handoff (H1) | `hopos/src/flip.rs`, `kern/src/kernflip.rs`, de bootreeks in `hopos/src/main.rs` en `net.rs` (`Node::run`), de adoptie in `kern/src/slots.rs` en `net/src`, de ingangen in `cpu/` en `board/` |
| **B. Console en meten** | §14.5 en §14.6, en de kern-kant van H8 (één lange wacht voor store en log) | `hopos/src/conport.rs`, `kern/src/conport.rs`, de zwarte doos, `KERN_LOG`, `load.rs`, `bench.rs`, `hopcost.rs`, `kern/src/store.rs` |
| **C. Headless, `gui`, `media`** | §14.3 | de features in `hopos/Cargo.toml` en de boards, `gui/`, `media/`, `hopos/src/gui.rs`, `optical.rs`, `codec.rs` |
| **D. Hard falen** | §14.7 behalve de interrupts: `fatal()`, de watchdog eerst, de ene guard, de watchdog in elke ring, quarantaine en retire, hopfs hard, O5 | `kern/src/watchdog.rs`, `hopos/src/watchdog.rs`, de quarantaine in `kern/src/slots.rs`, `kern/src/hopfs.rs`, `rpc.rs`, `hopos/src/storage.rs`, `tools/lib.sh` |
| **E. Interrupts en de kleine punten** | O3, O4, O7, O10, de vangrails naast een bel (§13.3) | `board/*/src/lib.rs` (`start_interrupts`), de drivers voor de lijnen, `net/src/pump.rs`, `blkdev/src/queue.rs`, de servicer in `kern/src/slots.rs`, `place_first` en de QEMU-toetsen |
| **F. LEAN** | L1 tot en met L9 en de F's van deel 2, in de werkboom van lean (het TLS 1.2-werk staat daar ongecommit) | `lean/` |
| **G. HOP** | H2 tot en met H7 en H9, de F's en S's van deel 3 (één `fail`, de klok over de hoofdverbinding, de veger, de staat één keer schrijven) | `hop/hop/` |

Waar twee agents hetzelfde bestand raken (`main.rs`, `kern/src/slots.rs`),
houdt ieder zich aan zijn eigen functies; de samenvoeging doen we bij het
inzoomen.

### 14.9 Daarna: inzoomen, element voor element

Per element: wat willen we bereiken, wat is de kleinste vorm, en hoeveel
regels kost het. Volgorde: **de flip**, dan het hok (slots en kooi), het
netwerk, opslag, de console, meten, en de rest van §14.2.

## 15. Inzoomen: de besluiten (07-11)

Na v3.2.0-rc5 (5f4179b, qemu-all 33 van 33), vóór v3.2.0: per element één
kaart (wat willen we, wat staat er, wat kan weg), Derek beslist per punt.
Alles wordt daarna in één keer gebouwd en getoetst, dan rc6 op ijzer, dan
v3.2.0. Meegenomen: wat er nog staat uit de leesreview van Codex
(1 november, gelezen tegen 4461670; de rest daarvan is gedaan door de
agents van §14.8).

### 15.1 De flip

Stand: `hopos/src/flip.rs` 451 regels code (was 1.042), `kern/src/kernflip.rs`
596, `cpu/src/el2/chain.rs` 382, `board/uefi/src/flip.rs` 194. Doel: kern B
neemt over wat leeft (hokken, volumes, lease, klok), de rest is een gewone
boot; na de 202 is elke fout fataal.

| # | Besluit (Derek, 07-11: ja op alle vijf) |
| --- | --- |
| F1 | De vluchtrecorder weg (`Stage`, `stage`, `archive_stage`, `take_last_flip`, `take_archived`, `mark_early_boot`, `FLIP_RECORDER_PA` per board en hun aanroepen). De zwarte doos (de console-ring die een reset overleeft) zegt wat kern B als laatste zei; de stappen vóór de 202 zijn een weigering en geen dood. |
| F2 | De M-mode-trampoline van riscv64 in `cpu/src/el2/chain.rs` weg: riscv flipt niet (geen extra code voor riscv, 01-11). |
| F3 | Bundelvorm 1 (`BUNDLE_HEAD` 56) weg: zo'n bundel komt van 3.0.x en valt al op `FLIP_ABI`. |
| F4 | Hop: de `cold`-vlag uit de flip-API (`api/src/node.rs`); er is geen koude flip, de parser weigert een onbekend veld. |
| F5 | Radxa en M4: de trampoline op een vaste pagina, niet op de bovenste heap-pagina (de heap één pagina korter), zoals `FLIP_TRAMP_PA` op de andere boards. |

Gehouden: de toets van de switch-code, `Jump::check` vóór de 202, de
500 ms tot de sprong, de lease in het blob, de feitenpagina en de
adressen per board. Het risico zit in het ijzer, niet in de code: de ene
ingang is alleen op QEMU bewezen. De ronde van rc6 doet per board een
koude boot en één warme flip.

### 15.2 Het hok

Stand: ~9.500 regels code. kern 2.569 (de lifecycle-actor `slots.rs`
1.297, partities, core-plaatsing, kooi-geheugen, grants), hopos 2.575 (de
board-lijm en de supervisor van Hop `slots.rs` 944, `kooi.rs` 717,
`cage.rs`, `cage_riscv.rs`, `glue.rs`), cpu arm 2.851 (`switch.rs` 985,
`oscore.rs` 1.074, dispatch, stage-2, de bewonerslijst), cpu riscv 1.492
(switcher 875, OS-core 617). Doel: een app in een hok, op een core die
gedeeld mag worden; hard stoppen; geen maximum aantal slots.

| # | Besluit (Derek, 07-11: ja op alle drie) |
| --- | --- |
| K1 | De OS-core-rotatie op riscv64 weg (`cpu/src/riscv/oscore.rs` en de riscv-tak in de lijm). Geen riscv-board gebruikt hem: de LicheeRV zet Hop op het app-hart (`sharegroup=hop`). De QEMU-ring `riscv-hop` zet Hop dan ook op het app-hart. |
| K2 | Het herstel van de registers één keer in de arm-assembly (`oscore.rs` en `switch.rs` deelden blokken van 35 en 19 gelijke regels): één fragment, offsets uit `abi::layout`. |
| K3 | `cage_riscv.rs` rekent met `abi::checksum::Fnv64` in plaats van een eigen FNV. |

Gehouden: de OS-core-rotatie op arm (Pi 4, Pi 5, Radxa: Hop naast de kern
houdt 3 van 4 cores vrij), het kooi-geheugen en de VMID bij de plaatsing
(de prijs van geen maximum), de supervisor van Hop (een vallende Hop is
al fataal; hij doet alleen `HOP_RESTART`), de grants (gui). Toetsen in
rc6: de riscv-ringen (K1), de arm- en VHE-ringen (K2), op ijzer de Pi
(Hop op de OS-core) en de LicheeRV.

### 15.3 Het netwerk

Stand: `net/` 2.489 regels code (switch, NAT, conntrack, pomp, ringen),
`hopos/src/net.rs` met `input.rs` 1.075 (lease, node-stack, keeper),
`driver/nic` 5.007 (zeven drivers), applib appnet en net 1.313,
`applibhttp` 410. Doel: de node aan het LAN, elk hok erachter met NAT en
gepubliceerde poorten, apps die zelf TCP en HTTP doen. Het plafond zit
bewust bij de OS-core (`docs/apps.md`).

| # | Besluit (Derek, 07-11: ja op alle vijf) |
| --- | --- |
| N1 | Hop (`agentd-hopos`): het park krijgt de twee fixes van applibhttp uit rc4: met alle werkers bezet wacht de acceptor op een bel in plaats van elke 5 ms (`BUSY_POLL` weg), en leegt hij intussen `back`, zodat een werker geen goede keep-alive-verbinding meer sluit. Hop blijft een eigen park houden (`freed(i)` voor de herstart, `none_free()`); het commentaar dat "geen werkernummer" als reden noemde, gaat recht. |
| N2 | `maybe_compact` en `high_water` uit de conntrack (`net/src/flows.rs`, de aanroepen in `nat.rs`): de functie compacteerde niets, het veld had geen lezer. |
| N3 | `net::Stats::host_tx_drops` weg: gedrukt, nooit opgehoogd. |
| N4 | De lus om DHCP heen weg: één client zonder termijn, die zelf opnieuw zendt (leandhcp: vaste rondes van 3 s, `ROUND_WINDOW`), tot er een lease is; de boot-guard van 120 s is het einde. |
| N5 | Gehouden: de keeper houdt na een verlopen lease het adres en blijft rebinden (`HOPOS_DHCP_RETRY`). Een DHCP-storing langer dan de lease mag niet elke node rebooten; een NAK of een ander adres blijft een reset. |

Gehouden: de drivers (de `dsb` per frame in tg3 bewust), applibhttp, de
conntrack zonder grens per flow.

### 15.4 Opslag

Stand: hopfs 1.294 regels code, de bestandscalls en de actor (`rpc.rs`)
953, de store-ops 726, `hopos/src/storage.rs` 165, blkdev 569, NVMe
1.537, virtio-blk 447.

| # | Besluit (Derek, 07-11: ja) |
| --- | --- |
| O1 | `LIFE_TICK` weg: een verbinding van een slot wacht op een stopbel per levensduur (`sync::Stop`, meerdere wachters) in plaats van elke 100 ms te kijken of haar levensduur nog leeft (`Watched` in `kern/src/system.rs`, `kern/src/store.rs`). |
| O2 | `Fs::list_n` weg: de toets gaat via het echte `list_into`. |

Gehouden: de commit van de boom elke 10 s naast `OP_SYNC` (zoals de
writeback van Linux), de store-ops als kopie en niet als sync.

### 15.5 Console

Af: één ring (`kern/src/conport.rs` 218, `hopos/src/conport.rs` 103) met
drie lezers (UART, tcp/5555, `NEXT_LOG` 0 van Hop), die een reset
overleeft. Geen besluiten.

### 15.6 Meten

| # | Besluit (Derek, 07-11: ja) |
| --- | --- |
| M1 | `waker(rounds seen kicks)` uit `HOPOS_TICK` en `WakerStats`: zonder schrijver, altijd 0. `rx` (de RX-kick op de M4) blijft. |
| M2 | `tools/loc.py` telt `testutil.rs` en `*_tests.rs` als toetsen (717 regels stonden bij productie); beide tellers blijven (de bronboom, en `tools/loc` per gelinkte smaak). |

Gehouden: `HOPOS_TICK` als de ene diagnoseregel van de kern (opt-in,
`hopos.tick=1`).

### 15.7 De SDK-rand (vóór de tag: erna is weghalen een breuk)

| # | Besluit (Derek, 07-11: ja) |
| --- | --- |
| S1 | Uit applib, zonder gebruiker in Hop, stulp, meet, haasbase en de andere repos: `App::net_ready` (met zijn setter), `Ctrl::rx_door`, `Env::from_bytes`, `Nic::deferring`. `Net::resolve6` blijft (stulp). |
| S2 | De lezers `le16`/`le32`/`le64` van abi twee keer in plaats van vier: één set voor systemapi en store (te kort is 0), één voor hopabi en codec (te kort is een fout van de aanroeper). |
| S3 | Achter `#[cfg(test)]`: `Queue::in_flight`, `Pump::is_holding`, `Switch::nat`, `Switch::is_attached`, `Nat::flow_count`, `HostPort::stack`. |

### 15.8 Daarna

Alles van 15.1 tot 15.7 in één ronde gebouwd, één gate en qemu-all, dan
rc6 op ijzer (per board een koude boot en één warme flip), dan v3.2.0.
