//! Het geheugenplan van HopOS, in twee lagen.
//!
//! - **De slot-ABI**: wat een app ziet. Dat is één regio, zijn eigen
//!   partitie. Onderin zijn RAM (`RamStart`/`RamSize`, door de kern in élk
//!   image gepatcht), bovenin een staart van [`ABI_TAIL`] bytes met zijn
//!   control-page, zijn outbox-ring en zijn frame-ringen. Een app rekent
//!   alles uit twee waarden die al in zijn image staan ([`Tail`]) en kent
//!   geen enkel absoluut adres. Deze indeling wijzigen is de app-ABI breken;
//!   daarom staat er een versie op ([`crate::ABI_VERSION`]).
//! - **Het PA-plan** ([`Plan`]): waar de kern zijn éígen structuren fysiek
//!   legt: de partitie-pool waar slots uit gesneden worden, de
//!   stage-2-tabellen met hun ctx-blokken en park-mailboxen, de
//!   boot-scratch, en de control-pages van de eigen node-cores (die hebben
//!   geen partitie).
//!
//! Het canonieke linkadres ([`LINK_BASE`]) hoort bij de MAP-helft van de
//! kooi: die legt elke partitie op hetzelfde adres, zodat één artifact in
//! elk slot draait. Op ARM doet de stage-2-tabel dat (en begrenst meteen);
//! op RISC-V doet een aparte tabel in de staart het ([`ABI_MAP_OFF`]) naast
//! de PMP-whitelist die begrenst.
//!
//! Wat hier NIET staat: de control-page-velden (die zijn van [`crate::hopabi`])
//! en de ringkop (die is van [`crate::ring`]). Deze module bezit adressen en
//! maten, geen protocol.
//!
//! Vervallen uit Go (slot-ABI 7): de inbox-ring (`InboxOff`, HOP naar app)
//! bestond alleen voor de antwoorden van de mailbox-RPC (recordtypes 3 en 4),
//! en die RPC is sinds Go-ABI 6 (03-09) vervangen door de system-API over het
//! slot-LAN. De plek blijft gereserveerd binnen [`RING_STRIDE`], zodat
//! [`ABI_STUB_OFF`] en alles erboven op hun Go-adres blijven.

mod plan;

pub use crate::Region;
pub use plan::{POOL_MAX, Plan, PlanSpec, Pool, carve_pool, coalesce, pool_of};

use core::fmt;
use core::mem::{offset_of, size_of};
use dev::Pa;

// ---------------------------------------------------------------------------
// Het thuis van de kern en de maten van zijn DMA-regio's.
// ---------------------------------------------------------------------------

/// Het begin van het RAM van de kern op QEMU `-M virt` (het DRAM begint daar
/// op 0x4000_0000). Een board met een ander thuisadres zet
/// [`PlanSpec::ram_base`].
pub const HOP_RAM_START: u64 = 0x4000_0000;
/// De maat van de NIC-DMA-regio: 8 MB.
pub const NET_DMA_SIZE: u64 = 0x0080_0000;
/// De maat van de xHCI-DMA-regio ([`PlanSpec::usb_dma_pa`]).
///
/// 2 MB is ruim: de vaste structuren zijn ~16 KB, elk slot kost 20 KB en de
/// scratchpad is een handvol pagina's. De maat is 2 MB omdat de pool op die
/// korrel gesneden wordt; kleiner zou alsnog 2 MB kosten.
pub const USB_DMA_SIZE: u64 = 0x0020_0000;

// ---------------------------------------------------------------------------
// Het canonieke adresbeeld van een app (IPA).
// ---------------------------------------------------------------------------

/// De canonieke IPA-basis van slot 1. Elk image is op dit bereik gelinkt;
/// de stage-2 legt dat venster op de fysieke partitie uit de pool.
pub const SLOTS_BASE: u64 = 0x5000_0000;
/// Het IPA-venster per slot: 512 MB. Een IPA-vorm, geen fysieke
/// reservering; de fysieke capaciteit is de pool.
pub const SLOT_STRIDE: u64 = 0x2000_0000;
/// De compile-time bovengrens op het aantal slots.
///
/// 128 dekt de Ampere Altra (127 app-cores). De per-slot plan-regio's
/// (control-pages van node-cores, kooi-blokken) worden voor
/// [`PlanSpec::max_slots`] gereserveerd; een board zet die lager.
pub const SLOT_CAP: usize = 128;
/// De kooi-capaciteit die een board standaard neemt: kooien tellen niet
/// mee als cores (meerdere kooien delen één core, Go: `MaxSlots` los van
/// `NumAppCores`), dus een board met één app-core heeft toch plaats voor
/// 32 apps. Elke kooi kost een servicer-taak, een control-page en een
/// kooi-blok; een board met een kleine staart (LicheeRV, Radxa) zet het
/// lager, een board met meer app-cores dan dit neemt die plus één.
pub const SLOTS_DEFAULT: usize = 32;
/// De basis waartegen elk app-image gelinkt is: het venster van slot 1.
/// [`crate::place::build`] toetst segmenten tegen `[LINK_BASE, LINK_BASE +
/// app_ram)`.
pub const LINK_BASE: u64 = SLOTS_BASE;
/// Waar de tekst van een image binnen het linkvenster begint. De eerste
/// 64 KB draagt de stage-1-tabellen van de app-runtime.
pub const LINK_TEXT_OFF: u64 = 0x10000;

// Het venster van slot 128 moet binnen de 39-bit IPA-ruimte van de
// ARM-stage-2 vallen, anders is SLOT_CAP een leugen.
const _: () = assert!(SLOTS_BASE + SLOT_CAP as u64 * SLOT_STRIDE <= 1 << 39);
const _: () = assert!(SLOT_CAP <= u8::MAX as usize);

/// De IPA-basis van de oude control-regio. Wat ervan over is, is de
/// boot-scratch: het énige IPA-venster buiten de partitie dat een slot nog
/// ziet, en read-only.
pub const CTRL_BASE: u64 = 0xB000_0000;
/// De maat van één control-page: 4 KB.
pub const CTRL_STRIDE: u64 = 0x1000;
/// De boot-scratch (IPA): buiten alle RAM-declaraties, dus door elke MMU als
/// device gemapt en coherent zonder cache-onderhoud. Uitsluitend
/// gealigneerde 64-bit toegang. `cpuinit` schrijft er vóór de EL-drop het
/// boot-EL op +0. Fysiek: [`PlanSpec::boot_scratch_pa`].
pub const BOOT_SCRATCH: u64 = CTRL_BASE;
/// De offset van de feitenpagina van de kern-flip op de boot-scratch (O6,
/// `docs/flip.md`): twee woorden, [`FACTS_HANDOFF`] en [`FACTS_FIRMWARE`].
/// Elke ingang krijgt bij een sprong precies dit adres in x0; bij stroom aan
/// geeft de firmware er iets anders, en bouwt de kern de pagina opnieuw.
///
/// Waarom 0x80 en niet laag: hier stond 0x10, en Apple's `cpuinit` legt
/// daar HCR_EL2 en CNTHCTL_EL2 neer. Gemeten 01-09 op de M4 bij de eerste
/// flip op ijzer: de nieuwe kern zag `stray handoff pointer
/// (0x480000000/0xc03)`, liep het koude pad en wiste de app-core-regio onder
/// twee levende bewoners. 0x80 ligt boven alles wat enig board hier
/// gebruikt (Apple komt tot +0x50), onder het parameterblok (+0x100), en in
/// een eigen cacheline: 0x40..0x7F dragen de woorden die de stubs met de MMU
/// uit schrijven.
pub const FACTS_OFF: u64 = 0x80;
/// Het woord van de feitenpagina met het adres van de handoff, of 0: een
/// boot zonder handoff is een koude boot.
pub const FACTS_HANDOFF: u64 = 0;
/// Het woord van de feitenpagina met wat de firmware bij stroom aan in x0
/// gaf (de DTB, de boot_args van Apple; op UEFI 0).
pub const FACTS_FIRMWARE: u64 = 8;
/// Hoeveel van de boot-scratch het plan reserveert: tot en met de
/// feitenpagina.
pub const BOOT_SCRATCH_LEN: u64 = FACTS_OFF + 16;

/// De feitenpagina van een board met de boot-scratch op `boot_scratch`.
#[must_use]
pub const fn facts_pa(boot_scratch: u64) -> u64 {
    boot_scratch + FACTS_OFF
}

/// De maat van het handoff-blob van de kern-flip
/// (`kern::kernflip::HANDOFF_TAIL`, de binary toetst het).
pub const FLIP_HANDOFF_LEN: u64 = 0x4_0000;

/// Waar het handoff-blob van de kern-flip staat: de [`FLIP_HANDOFF_LEN`]
/// bytes direct onder het staging-maatwoord `stage_hdr`, op elk board.
#[must_use]
pub const fn flip_handoff_pa(stage_hdr: u64) -> u64 {
    stage_hdr - FLIP_HANDOFF_LEN
}

const _: () = assert!(FACTS_OFF / dev::LINE != 0x40 / dev::LINE);
const _: () = assert!(FACTS_OFF / dev::LINE == (FACTS_OFF + FACTS_FIRMWARE) / dev::LINE);
const _: () = assert!(BOOT_SCRATCH_LEN <= 0x100);

/// Het framebuffer-grant-venster (IPA): GB0 is vrij in het canonieke beeld
/// en de fysieke framebuffer mag boven de 4 GB liggen, dus identity kan
/// niet. Alleen de display-houder heeft dit gigabyte.
pub const FB_IPA: u64 = 0x2000_0000;

const _: () = assert!(FB_IPA + (1 << 30) <= SLOTS_BASE + SLOT_STRIDE);

// ---------------------------------------------------------------------------
// De slot-ABI: de staart van de eigen partitie.
// ---------------------------------------------------------------------------

/// De staart per slot: 2 MB, uit de partitie gesneden boven het app-RAM.
pub const ABI_TAIL: u64 = 0x20_0000;
/// De control-page in de staart ([`CTRL_STRIDE`] groot).
pub const ABI_CTRL_OFF: u64 = 0x0;
/// De ring-regio in de staart ([`RING_STRIDE`] groot).
pub const ABI_RING_OFF: u64 = 0x1000;
/// De scratch van de kooi-stub (RISC-V): voortgang en readbacks van de stub
/// die de PMP-kooi zet. Niet de control-page (die is van de app) en niet de
/// plan-regio (daar mag het hart na het locken van de kooi niet meer bij).
pub const ABI_STUB_OFF: u64 = 0x11000;
/// De map-tabel van de kooi (RISC-V): het canonieke linkadres naar de echte
/// partitie. In de partitie, want de walker is zelf aan de kooi
/// onderworpen; dat de app erbij kan is geen gat, hij bereikt er nooit iets
/// buiten zijn partitie mee.
pub const ABI_MAP_OFF: u64 = 0x12000;
/// Hoeveel pagina's de map-tabel draagt: wortel plus één niveau, genoeg
/// zolang een partitie binnen één gigabyte valt.
pub const ABI_MAP_PAGES: u64 = 2;
/// De frame-ringen in de staart: TX onderin, RX erboven.
pub const ABI_NET_OFF: u64 = 0x20000;

/// De ring-regio: 64 KB, met de outbox onderin. De bovenste helft was de
/// inbox van de mailbox-RPC en is gereserveerd.
pub const RING_STRIDE: u64 = 0x10000;
/// De outbox (app naar kern: bootstrap- en crashlog) binnen de ring-regio.
pub const OUTBOX_OFF: u64 = 0x0;
/// De datacapaciteit van de outbox: de halve ring-regio min een pagina voor
/// de ringkop. 28 KB, een 8-voud.
pub const RING_DATA_CAP: u64 = RING_STRIDE / 2 - 0x1000;

/// Wat er in de staart boven [`ABI_NET_OFF`] overblijft, eerlijk in twee
/// richtingen. Afgeleid en niet met de hand uitgerekend: dan is "past het?"
/// een eigenschap van de indeling en geen hoop. 960 KB per richting.
pub const NET_RING_HALF: u64 = (ABI_TAIL - ABI_NET_OFF) / 2;
/// De TX-ring (app naar switch) binnen de net-regio.
pub const NET_TX_OFF: u64 = 0x0;
/// De RX-ring (switch naar app) binnen de net-regio.
pub const NET_RX_OFF: u64 = NET_RING_HALF;
/// De datacapaciteit van één frame-ring: de helft min de ringkop.
pub const NET_RING_DATA_CAP: u64 = NET_RING_HALF - 0x1000;
/// De MTU van het slot-LAN. Geen draad, geen bitfouten, dus zo groot als
/// IPv4 toelaat: één MiB is dan 16 segmenten in plaats van 700, met 8 ACK's
/// in plaats van 350 (04-09). Naar buiten klemt de stack op 1500.
pub const NET_MTU: usize = 65535;

// De staart is een aaneengesloten reeks regio's zonder overlap. Een misstap
// hier is stil: de app leest dan op het adres van zijn buurregio.
const _: () = assert!(ABI_CTRL_OFF + CTRL_STRIDE <= ABI_RING_OFF);
const _: () = assert!(ABI_RING_OFF + RING_STRIDE <= ABI_STUB_OFF);
const _: () = assert!(ABI_STUB_OFF + 0x1000 <= ABI_MAP_OFF);
const _: () = assert!(ABI_MAP_OFF + ABI_MAP_PAGES * 0x1000 <= ABI_NET_OFF);
const _: () = assert!(ABI_NET_OFF + 2 * NET_RING_HALF <= ABI_TAIL);
const _: () = assert!(NET_RING_HALF == 0xF_0000);
const _: () = assert!(RING_DATA_CAP == 0x7000 && RING_DATA_CAP.is_multiple_of(8));
const _: () = assert!(NET_RING_DATA_CAP.is_multiple_of(8));
// Een ring (kop plus data) blijft binnen zijn helft.
const _: () = assert!(crate::ring::DATA_OFF + RING_DATA_CAP <= RING_STRIDE / 2);
const _: () = assert!(crate::ring::DATA_OFF + NET_RING_DATA_CAP <= NET_RING_HALF);
// De frame-ringen zijn één 2 MB-blok voor de kooi; de staart ook.
const _: () = assert!(ABI_TAIL == 2 << 20);

/// De ABI-staart van één slot, uitgerekend uit zijn RAM-declaratie.
///
/// De app geeft `RamStart`/`RamSize` uit zijn eigen image (het canonieke
/// linkadres); de kern geeft de fysieke partitiebasis met dezelfde
/// app-RAM-maat. Dezelfde rekensom, een andere basis; de compiler bewaakt
/// dat beide kanten hem delen.
///
/// # Invariants
///
/// `base` is 4 KB-gealigneerd en `base + ABI_TAIL` loopt niet om.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Tail {
    base: Pa,
}

impl Tail {
    /// De staart boven `ram_size` bytes app-RAM vanaf `ram`, of `None` als
    /// dat omloopt of de staart niet op een pagina begint.
    #[must_use]
    pub const fn new(ram: u64, ram_size: u64) -> Option<Tail> {
        let Some(base) = ram.checked_add(ram_size) else {
            return None;
        };
        if base.checked_add(ABI_TAIL).is_none() || !base.is_multiple_of(CTRL_STRIDE) {
            return None;
        }
        // INVARIANT: zojuist getoetst.
        Some(Tail { base: Pa(base) })
    }

    /// De basis van de staart: net boven het app-RAM.
    #[must_use]
    pub const fn base(self) -> Pa {
        self.base
    }

    /// De control-page van het slot.
    #[must_use]
    pub const fn ctrl_page(self) -> Pa {
        self.base.add(ABI_CTRL_OFF)
    }

    /// De outbox-ring (app naar kern).
    #[must_use]
    pub const fn outbox(self) -> Pa {
        self.base.add(ABI_RING_OFF + OUTBOX_OFF)
    }

    /// De scratch van de kooi-stub.
    #[must_use]
    pub const fn stub(self) -> Pa {
        self.base.add(ABI_STUB_OFF)
    }

    /// De map-tabel van de kooi.
    #[must_use]
    pub const fn map(self) -> Pa {
        self.base.add(ABI_MAP_OFF)
    }

    /// De TX-frame-ring (app naar switch).
    #[must_use]
    pub const fn net_tx(self) -> Pa {
        self.base.add(ABI_NET_OFF + NET_TX_OFF)
    }

    /// De RX-frame-ring (switch naar app).
    #[must_use]
    pub const fn net_rx(self) -> Pa {
        self.base.add(ABI_NET_OFF + NET_RX_OFF)
    }
}

/// Het staging-contract tussen kern en apploader: het image landt
/// 8-uitgelijnd tegen de bovenkant van het app-RAM.
///
/// Geeft `(adres, gestagede maat)`, of `None` als het image niet past of de
/// maat onzin is. Beide kanten rekenen met deze functie, de app in IPA en de
/// kern in PA, zodat de compiler de pariteit bewaakt die eerst een
/// commentaar moest bewaken.
#[must_use]
pub const fn stage_addr(ram_base: u64, ram_size: u64, img_size: u64) -> Option<(u64, u64)> {
    let Some(padded) = img_size.checked_add(7) else {
        return None;
    };
    let staged = padded & !7;
    if img_size == 0 || staged >= ram_size || ram_base.checked_add(ram_size).is_none() {
        return None;
    }
    Some((ram_base + ram_size - staged, staged))
}

// ---------------------------------------------------------------------------
// Slots en cores.
// ---------------------------------------------------------------------------

/// Een app-slot: 1 tot en met [`SLOT_CAP`]. Slot 0 bestaat niet als app; het
/// is de kern zelf, en die heeft eigen namen ([`HOST_IP4`], [`HOST_MAC`]).
///
/// # Invariants
///
/// `1 <= self.0 <= SLOT_CAP`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Slot(u8);

impl Slot {
    /// Slot 1: het linkvenster van elk image.
    pub const FIRST: Slot = Slot(1);

    /// Slot `i`, of `None` buiten `1..=SLOT_CAP`.
    #[must_use]
    pub const fn new(i: usize) -> Option<Slot> {
        if i == 0 || i > SLOT_CAP {
            return None;
        }
        // INVARIANT: zojuist begrensd; SLOT_CAP past in een u8.
        Some(Slot(i as u8))
    }

    /// Het slotnummer, 1-based.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// De canonieke IPA-basis van dit slot.
    #[must_use]
    pub const fn base(self) -> u64 {
        SLOTS_BASE + (self.0 as u64 - 1) * SLOT_STRIDE
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Een fysieke core-index in de plan-tabellen: 0 tot en met [`SLOT_CAP`]
/// (het plan reserveert `max_slots + 1` blokken).
///
/// # Invariants
///
/// `self.0 <= SLOT_CAP`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Core(u8);

impl Core {
    /// Core `i`, of `None` boven [`SLOT_CAP`].
    #[must_use]
    pub const fn new(i: usize) -> Option<Core> {
        if i > SLOT_CAP {
            return None;
        }
        // INVARIANT: zojuist begrensd.
        Some(Core(i as u8))
    }

    /// De index.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// De context-id van deze core als secundaire van een SMP-app: 129 tot
    /// en met 255, zodat hij in een bewonersbyte ([`SchedBlock::list`])
    /// naast de kooi-id's 1..=128 past. `None` voor core 0 en 1: die zijn
    /// nooit een secundaire.
    #[must_use]
    pub const fn smp_context_id(self) -> Option<u8> {
        if self.0 < 2 {
            return None;
        }
        Some((SLOT_CAP as u8 - 1) + self.0)
    }
}

impl fmt::Display for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// De kooi-regio ([`Plan`]): per slot een blok van CAGE_STRIDE.
// ---------------------------------------------------------------------------

/// De maat van één kooi-blok. Blok 0 draagt de gedeelde EL2-vectoren van de
/// app-cores (+0, 2 KB), de parkeerlus, de sched-blokken en de
/// switch-code-kopie; blok i draagt de stage-2-tabellen van slot i (+0 tot
/// [`CTX_OFF`]), zijn ctx-blok en het ctx-blok van secundaire core i.
pub const CAGE_STRIDE: u64 = 0x10000;
/// De parkeerlus in blok 0. Een gestopte app-core gaat niet terug naar de
/// firmware (PSCI CPU_OFF is op de Pi 5-stockfirmware een eenrichtingsdeur,
/// gemeten 10-07) maar parkeert op EL2 in een WFE-lus op zijn mailbox.
pub const PARK_CODE_OFF: u64 = 0x1000;
/// Het eerste sched-blok in blok 0; `SLOT_CAP + 1` blokken van
/// [`PARK_MBOX_LEN`], één per core.
pub const PARK_MBOX_OFF: u64 = 0x1100;
/// De maat van een sched-blok: de park-mailbox plus de staat van de
/// coöperatieve core-deling ([`SchedBlock`]).
pub const PARK_MBOX_LEN: u64 = 256;
/// De switch-code-kopie in blok 0: de EL2-blobs die een app-core uitvoert,
/// zodat hij nooit meer kern-image-bytes uitvoert. Dat is de randvoorwaarde
/// om bij een kern-flip het oude kern-venster te kunnen verlaten.
pub const SWITCH_CODE_OFF: u64 = 0xA000;
/// De harde bovengrens van de switch-code-kopie: de rest van blok 0. Liever
/// hard bij boot dan stil in het tabelblok van slot 1 schrijven.
pub const SWITCH_CODE_MAX: u64 = CAGE_STRIDE - SWITCH_CODE_OFF;
/// Het ctx-blok van slot i binnen kooi-blok i.
pub const CTX_OFF: u64 = 0x6000;
/// Het ctx-blok van secundaire core i binnen kooi-blok i: los van de
/// kooi-contexten, zonder de administratie te vergroten.
pub const SMP_CTX_OFF: u64 = 0x6800;

const _: () = assert!(PARK_CODE_OFF >= 0x800 && PARK_CODE_OFF + 0x100 <= PARK_MBOX_OFF);
const _: () = assert!(PARK_MBOX_OFF + (SLOT_CAP as u64 + 1) * PARK_MBOX_LEN == 0x9200);
const _: () = assert!(0x9200 <= SWITCH_CODE_OFF);
const _: () = assert!(CTX_OFF + CTX_LEN <= SMP_CTX_OFF);
const _: () = assert!(SMP_CTX_OFF + CTX_LEN <= CAGE_STRIDE);

// ---------------------------------------------------------------------------
// Het sched-blok: één per core, 256 bytes.
// ---------------------------------------------------------------------------

/// Park-mailbox woord 0: de toestand of het startschot.
pub const SCHED_MBOX_CTX: u64 = 0;
/// Park-mailbox woord 1: de doel-PC.
pub const SCHED_MBOX_PC: u64 = 8;
/// Vier werkregisters van de getrapte app (ARM x0..x3 in de vector-thunks;
/// RISC-V x5..x7).
pub const SCHED_SCRATCH: u64 = 16;
/// Welke context er nu draait (een slot, of een SMP-context boven
/// [`SLOT_CAP`]; 0 = geen). Op beide architecturen: de dispatch en de
/// OS-core schrijven het, de switcher leest er bij een yield de bewoner
/// uit (op ARM niet uit het VMID, dat een secundaire core met zijn
/// primaire deelt, 03-09), en de kern leest er de vasthouder van een core
/// uit (`hopos/src/kooi.rs` `holder`).
pub const SCHED_CURRENT: u64 = 48;
/// De laatst gedispatchte lijst-index. Alleen de RISC-V-switcher leest
/// hem; de dispatch zet hem op beide architecturen op 0 bij een start. Een
/// eigen veld en niet [`SCHED_CURSOR`], want dat ligt in de regels van de
/// kern.
pub const SCHED_ROTOR: u64 = 56;
/// De periode van de kill-tick in timebase-tikken (0 = geen tick). Geen
/// preemptie: een tick hervat dezelfde bewoner en kijkt alleen of de kern
/// hem dood wil ([`CTX_REVOKE`]).
pub const SCHED_TICK_TICKS: u64 = 64;
/// De bel naar de OS-core zoals DIT hart hem adresseert (RISC-V): de PA
/// waar de kick van een bewoner (`ecall` met a7 = 2) een 1 schrijft, op
/// QEMU virt `msip` van hart 0. 0 = geen bel: de kick is dan een no-op en
/// de kern hoort het frame op zijn failsafe.
pub const SCHED_OS_BELL: u64 = 72;
/// De laatst geplande lijst-index (ARM).
pub const SCHED_CURSOR: u64 = 80;
/// De lijstlengte (monotoon; 0-bytes zijn gaten).
pub const SCHED_COUNT: u64 = 88;
/// De bewonerslijst: [`SLOT_CAP`] bytes met context-id's.
pub const SCHED_LIST: u64 = 96;
/// De fysieke basis van de kooi-regio, zodat de switcher ctx, VTTBR en
/// park afleidt.
pub const SCHED_S2_PA: u64 = 224;
/// De PA van `mtimecmp` van dit hart (0 = geen comparator: spinnen).
pub const SCHED_CLINT_PA: u64 = 232;
/// De maximale slaapduur in tikken (0 = niet slapen, wel tikken). Een derde
/// stand, want op de SG2002 bleek "mtimecmp is bruikbaar" iets anders dan
/// "een wfi erop wordt betrouwbaar gewekt" (01-08, boots 6 en 7).
pub const SCHED_SLEEP_CAP: u64 = 240;
/// De PA van `msip` van dit hart: het wek-IPI van de kern.
pub const SCHED_MSIP_PA: u64 = 248;

/// Park-mailbox woord 0: nooit geparkeerd.
pub const PARK_COLD: u64 = 0;
/// Park-mailbox woord 0: geparkeerd in de WFE-lus. Elke grotere waarde is
/// het startschot (de x0 van de trampoline).
pub const PARK_PARKED: u64 = 1;

/// De indeling van een sched-blok, als type: de offsets hierboven zijn de
/// velden van deze struct, en de asserties eronder houden ze byte voor
/// byte gelijk.
///
/// De indeling is verdeeld naar SCHRIJVER, niet naar onderwerp, en de grens
/// loopt op cachelines: regel 0 (0..63) is alleen van de arch-laag op de
/// core zelf, regel 1..3 alleen van de kern. Op RISC-V zijn de harten niet
/// coherent (gemeten 30-07) en schrijft de switcher cachebaar; twee
/// schrijvers in één regel is dan dataverlies. De park-mailbox hoort bij de
/// kern maar bestaat alleen op ARM, waar dit blok device-gemapt is.
#[repr(C)]
#[derive(Debug)]
pub struct SchedBlock {
    /// Park-mailbox woord 0.
    pub mbox_ctx: u64,
    /// Park-mailbox woord 1.
    pub mbox_pc: u64,
    /// Werkregisters.
    pub scratch: [u64; 4],
    /// De lopende context ([`SCHED_CURRENT`]).
    pub current: u64,
    /// De rotor ([`SCHED_ROTOR`]).
    pub rotor: u64,
    /// De kill-tick-periode.
    pub tick_ticks: u64,
    /// De bel naar de OS-core (RISC-V).
    pub os_bell: u64,
    /// De cursor (ARM).
    pub cursor: u64,
    /// De lijstlengte.
    pub count: u64,
    /// De bewonerslijst.
    pub list: [u8; SLOT_CAP],
    /// De kooi-regio-PA.
    pub s2_pa: u64,
    /// De `mtimecmp`-PA.
    pub clint_pa: u64,
    /// De slaapgrens.
    pub sleep_cap: u64,
    /// De `msip`-PA.
    pub msip_pa: u64,
}

const _: () = assert!(size_of::<SchedBlock>() as u64 == PARK_MBOX_LEN);
const _: () = assert!(offset_of!(SchedBlock, mbox_ctx) as u64 == SCHED_MBOX_CTX);
const _: () = assert!(offset_of!(SchedBlock, mbox_pc) as u64 == SCHED_MBOX_PC);
const _: () = assert!(offset_of!(SchedBlock, scratch) as u64 == SCHED_SCRATCH);
const _: () = assert!(offset_of!(SchedBlock, current) as u64 == SCHED_CURRENT);
const _: () = assert!(offset_of!(SchedBlock, rotor) as u64 == SCHED_ROTOR);
const _: () = assert!(offset_of!(SchedBlock, tick_ticks) as u64 == SCHED_TICK_TICKS);
const _: () = assert!(offset_of!(SchedBlock, os_bell) as u64 == SCHED_OS_BELL);
const _: () = assert!(offset_of!(SchedBlock, cursor) as u64 == SCHED_CURSOR);
const _: () = assert!(offset_of!(SchedBlock, count) as u64 == SCHED_COUNT);
const _: () = assert!(offset_of!(SchedBlock, list) as u64 == SCHED_LIST);
const _: () = assert!(offset_of!(SchedBlock, s2_pa) as u64 == SCHED_S2_PA);
const _: () = assert!(offset_of!(SchedBlock, clint_pa) as u64 == SCHED_CLINT_PA);
const _: () = assert!(offset_of!(SchedBlock, sleep_cap) as u64 == SCHED_SLEEP_CAP);
const _: () = assert!(offset_of!(SchedBlock, msip_pa) as u64 == SCHED_MSIP_PA);
// De schrijversgrens: regel 0 is van de arch-laag, de rest van de kern.
const _: () = assert!(SCHED_ROTOR + 8 <= dev::LINE && SCHED_TICK_TICKS >= dev::LINE);

// ---------------------------------------------------------------------------
// Het ctx-blok: de staat van een geyielde bewoner, per slot.
// ---------------------------------------------------------------------------

/// De toestand van het slot ([`CtxState`]); tevens het levensteken dat de
/// kern leest.
pub const CTX_STATE: u64 = 0;
/// De control-page-PA van de bewoner. De kern zet hem bij elke start; de
/// trampoline krijgt hem als x0 en het fault-rapport vindt er de page.
pub const CTX_CTRL_PA: u64 = 8;
/// De trampoline- of vector-PA voor een koude boot.
pub const CTX_BOOT_PC: u64 = 16;
/// 31 registers: ARM x0..x30, RISC-V x1..x31.
pub const CTX_GPRS: u64 = 24;
/// ARM `sp_el0`/`sp_el1`; RISC-V ongebruikt.
pub const CTX_SP: u64 = 272;
/// De hervat-PC plus status: ARM `elr_el2`/`spsr_el2`, RISC-V
/// `sepc`/`sstatus`.
pub const CTX_RESUME: u64 = 288;
/// Het vertaal- en kooi-regime: ARM 19 EL1-sysregs, RISC-V `satp`, `stvec`,
/// `sscratch`, `pmpcfg0`, `pmpaddr0..7`.
pub const CTX_REGIME: u64 = 304;
/// Het aantal regime-woorden op ARM (einde blok 456).
pub const CTX_REGIME_ARM_WORDS: u64 = 19;
/// Het aantal regime-woorden op RISC-V (einde blok 400).
pub const CTX_REGIME_RV_WORDS: u64 = 12;
/// De wektijd die de bewoner bij zijn yield meegaf (0 = nu). Eén schrijver:
/// de switcher.
pub const CTX_WAKE: u64 = 464;
/// Bit 63 in [`CTX_WAKE`]: wek alleen op de wektijd of een kick, niet op de
/// doorbell. Zonder dit bit keerde de slaap van een semafoor-wachter meteen
/// terug zolang er RX lag die niemand draineerde (QEMU 03-09).
pub const CTX_WAKE_NO_PEEK: u64 = 1 << 63;
/// Hoe vaak de switcher met deze bewoner als laatste ging slapen: tientallen
/// per seconde is slaap, miljoenen is spin.
pub const CTX_SLEEPS: u64 = 472;
/// Het affiniteitswoord van de core die als laatste met deze bewoner
/// yieldde: het adres waarop een sibling hem wekt (HVC #4).
pub const CTX_KICK_TARGET: u64 = 480;
/// Wat er in [`CTX_KICK_TARGET`] staat zolang er nog niet geyield is. Niet
/// nul, want nul is fysieke core 0: de O6N-hang van 21-09, waar een dode
/// ketenschakel élke wek naar de primaire opving.
pub const CTX_KICK_NONE: u64 = 1 << 63;
/// Hoe vaak een sibling deze bewoner via HVC #4 wekte.
pub const CTX_WAKES: u64 = 488;
/// Het slot van de eenheid waar deze bewoner bij hoort (zijn eigen, of dat
/// van de primaire). Door de kern gezet vóór de dispatch; buiten elke kooi,
/// dus niet te vervalsen.
pub const CTX_UNIT_SLOT: u64 = 496;
/// De intrekking door de kern (RISC-V): niet-nul is "beëindig bij de eerste
/// gelegenheid". Eén schrijver (de kern) en een eigen cacheline, want de
/// intrekking komt terwijl de bewoner leeft en de switcher in regel 0
/// schrijft; twee niet-coherente schrijvers in één regel verliezen er één.
pub const CTX_REVOKE: u64 = 512;
/// De PA van het head-woord van de RX-frame-ring van dit slot, voor de
/// doorbell-peek (0 = geen peek). In de regel van [`CTX_REVOKE`]: ook hier is
/// de kern de enige schrijver.
pub const CTX_RING_HEAD_PA: u64 = 520;
/// Een sibling-wek die kwam terwijl deze core nog draaide of yieldde. Zonder
/// dit woord overschreef de yield de wek en sliep een lock-wachter
/// seconden met alles runnable (2-core-app, 04-09).
pub const CTX_KICK_PENDING: u64 = 536;
/// Het entry-argument van een node-core, los van de control-page.
pub const CTX_BOOT_ARG: u64 = 544;
/// De vertrouwde, circulaire link naar het ctx-blok van de volgende sibling;
/// nooit door de app te schrijven.
pub const CTX_NEXT_PA: u64 = 576;
/// De vertrouwde SMP-handoff: control-page-offsets onder 256 bytes, alleen
/// door de kern geschreven vóór de secundaire gedispatcht wordt.
pub const CTX_SMP: u64 = 768;
/// De maat van het hele ctx-blok. FP staat er bewust niet in: de laag die
/// de kern bezit draait met de MMU uit en een SIMD-store naar Device faultt.
pub const CTX_LEN: u64 = 1024;
/// De FP-registers van een bewoner, in de kier achter het ctx-blok (vóór
/// [`SMP_CTX_OFF`]). Alleen voor kooi-contexten: een secundair ctx-blok
/// heeft deze kier niet.
///
/// riscv64: f0..f31 en `fcsr`, 33 woorden. De riscv-switcher en de
/// OS-core bewaren ze bij elke trap en zetten ze terug bij het hervatten;
/// een gedeeld hart draagt sinds 02-10 meer dan één bewoner.
///
/// arm64: q0..q31 (elk twee woorden, laag dan hoog), FPCR en FPSR,
/// [`CTX_FPRS_ARM_WORDS`] woorden, met daarachter [`CTX_FP_LIVE`]. Alleen
/// de OS-core gebruikt ze (`cpu::el2::oscore`, die onderbreekt), via
/// GP-registers, want ook deze kier is op sommige borden Device. De
/// switcher van de app-cores wisselt alleen op een yield en bewaart geen FP.
pub const CTX_FPRS: u64 = CTX_LEN;
const _: () = assert!(CTX_OFF + CTX_FPRS + 33 * 8 <= SMP_CTX_OFF);
/// Het aantal FP-woorden op arm64: 32 keer 16 bytes plus FPCR en FPSR.
pub const CTX_FPRS_ARM_WORDS: u64 = 66;
/// arm64: niet-nul = [`CTX_FPRS`] draagt de FP-staat van de bewoner en de
/// OS-core zet hem terug bij de volgende beurt. De OS-core schrijft het bij
/// elke terugkeer (1 na een onderbreking, 0 na een yield); een verse
/// bewoner krijgt 1 met nullen, zodat hij niets van een voorganger ziet.
pub const CTX_FP_LIVE: u64 = CTX_FPRS + 8 * CTX_FPRS_ARM_WORDS;
/// Het einde van wat een ctx-blok van een kooi-context inclusief zijn
/// FP-kier beslaat, voor een kladblok buiten het plan (de zelftests).
pub const CTX_FP_END: u64 = CTX_FP_LIVE + 8;
const _: () = assert!(CTX_OFF + CTX_FP_END <= SMP_CTX_OFF);

/// De toestand van een ctx-blok ([`CTX_STATE`]). De kern schrijft `Empty`,
/// `BootPending` en `Running`; de switcher `Running`, `Saved` en `Dead`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum CtxState {
    /// Geen bewoner (vers of vrijgegeven).
    Empty = 0,
    /// De kern zette boot-ctx en PC klaar; koude boot bij de rotatie.
    BootPending = 1,
    /// Context geldig: geyield, hervatbaar.
    Saved = 2,
    /// Draait nu op zijn core.
    Running = 3,
    /// Geëindigd (exit, fault of revoke); de switcher slaat hem over.
    Dead = 4,
}

impl CtxState {
    /// De toestand van een rauw woord, of `None` voor een onbekende waarde.
    #[must_use]
    pub const fn from_raw(v: u64) -> Option<CtxState> {
        Some(match v {
            0 => Self::Empty,
            1 => Self::BootPending,
            2 => Self::Saved,
            3 => Self::Running,
            4 => Self::Dead,
            _ => return None,
        })
    }

    /// Het rauwe woord.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self as u64
    }
}

/// De indeling van een ctx-blok, als type; zie [`SchedBlock`] voor het
/// waarom.
#[repr(C)]
#[derive(Debug)]
pub struct CtxBlock {
    /// [`CTX_STATE`].
    pub state: u64,
    /// [`CTX_CTRL_PA`].
    pub ctrl_pa: u64,
    /// [`CTX_BOOT_PC`].
    pub boot_pc: u64,
    /// [`CTX_GPRS`].
    pub gprs: [u64; 31],
    /// [`CTX_SP`].
    pub sp: [u64; 2],
    /// [`CTX_RESUME`].
    pub resume: [u64; 2],
    /// [`CTX_REGIME`], op zijn ARM-maat.
    pub regime: [u64; CTX_REGIME_ARM_WORDS as usize],
    _pad0: u64,
    /// [`CTX_WAKE`].
    pub wake: u64,
    /// [`CTX_SLEEPS`].
    pub sleeps: u64,
    /// [`CTX_KICK_TARGET`].
    pub kick_target: u64,
    /// [`CTX_WAKES`].
    pub wakes: u64,
    /// [`CTX_UNIT_SLOT`].
    pub unit_slot: u64,
    _pad1: u64,
    /// [`CTX_REVOKE`].
    pub revoke: u64,
    /// [`CTX_RING_HEAD_PA`].
    pub ring_head_pa: u64,
    _pad2: u64,
    /// [`CTX_KICK_PENDING`].
    pub kick_pending: u64,
    /// [`CTX_BOOT_ARG`].
    pub boot_arg: u64,
    _pad3: [u64; 3],
    /// [`CTX_NEXT_PA`].
    pub next_pa: u64,
    _pad4: [u64; 23],
    /// [`CTX_SMP`].
    pub smp: [u64; 32],
}

const _: () = assert!(size_of::<CtxBlock>() as u64 == CTX_LEN);
const _: () = assert!(offset_of!(CtxBlock, state) as u64 == CTX_STATE);
const _: () = assert!(offset_of!(CtxBlock, ctrl_pa) as u64 == CTX_CTRL_PA);
const _: () = assert!(offset_of!(CtxBlock, boot_pc) as u64 == CTX_BOOT_PC);
const _: () = assert!(offset_of!(CtxBlock, gprs) as u64 == CTX_GPRS);
const _: () = assert!(offset_of!(CtxBlock, sp) as u64 == CTX_SP);
const _: () = assert!(offset_of!(CtxBlock, resume) as u64 == CTX_RESUME);
const _: () = assert!(offset_of!(CtxBlock, regime) as u64 == CTX_REGIME);
const _: () = assert!(offset_of!(CtxBlock, wake) as u64 == CTX_WAKE);
const _: () = assert!(offset_of!(CtxBlock, sleeps) as u64 == CTX_SLEEPS);
const _: () = assert!(offset_of!(CtxBlock, kick_target) as u64 == CTX_KICK_TARGET);
const _: () = assert!(offset_of!(CtxBlock, wakes) as u64 == CTX_WAKES);
const _: () = assert!(offset_of!(CtxBlock, unit_slot) as u64 == CTX_UNIT_SLOT);
const _: () = assert!(offset_of!(CtxBlock, revoke) as u64 == CTX_REVOKE);
const _: () = assert!(offset_of!(CtxBlock, ring_head_pa) as u64 == CTX_RING_HEAD_PA);
const _: () = assert!(offset_of!(CtxBlock, kick_pending) as u64 == CTX_KICK_PENDING);
const _: () = assert!(offset_of!(CtxBlock, boot_arg) as u64 == CTX_BOOT_ARG);
const _: () = assert!(offset_of!(CtxBlock, next_pa) as u64 == CTX_NEXT_PA);
const _: () = assert!(offset_of!(CtxBlock, smp) as u64 == CTX_SMP);
// Het RISC-V-regime past ook vóór CTX_WAKE.
const _: () = assert!(CTX_REGIME + CTX_REGIME_RV_WORDS * 8 <= CTX_WAKE);
// De SMP-handoff draagt de control-page-velden onder 256 bytes.
const _: () = assert!(CTX_LEN - CTX_SMP == 256);

// ---------------------------------------------------------------------------
// Het interne net: deterministisch, geen tabellen die leren.
// ---------------------------------------------------------------------------

/// De prefixlengte van het slot-LAN: 10.100.0.0/24.
pub const NET_PREFIX: u32 = 24;
const NET_A: u32 = 10;
const NET_B: u32 = 100;

/// Het interne IPv4 van de kern (.1): de gateway van elke app.
pub const HOST_IP4: u32 = NET_A << 24 | NET_B << 16 | 1;
/// De MAC van de kern: `02:00:00:00:00:00`.
pub const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0];

/// Het interne IPv4 van een slot, big-endian als getal: slot i is .(i+1).
/// Eén bron van waarheid, zodat de switch en de app-stack nooit
/// uiteenlopen; niemand hoeft het op de control-page te schrijven.
#[must_use]
pub const fn slot_ip4(slot: Slot) -> u32 {
    port_ip4(slot.get())
}

/// De MAC van een slot: `02:00:00:00:00:<slot>`.
#[must_use]
pub const fn slot_mac(slot: Slot) -> [u8; 6] {
    port_mac(slot.get())
}

/// Het interne IPv4 van poort `i` van het slot-LAN: de kern is poort 0
/// (.1), slot i is poort i (.(i+1)). De vorm van [`slot_ip4`] voor wie de
/// kern als 0 telt: de switch (zijn poorten zijn 0..=`SLOT_CAP`) en een app
/// die zijn slot als getal kent. De afkapping op één byte is die van het
/// plan (`SLOT_CAP` past erin).
#[must_use]
pub const fn port_ip4(i: usize) -> u32 {
    HOST_IP4 + (i & 0xff) as u32
}

/// De MAC van poort `i`: `02:00:00:00:00:<i>` (de kern is 0, [`HOST_MAC`]).
#[must_use]
pub const fn port_mac(i: usize) -> [u8; 6] {
    [0x02, 0, 0, 0, 0, (i & 0xff) as u8]
}

/// De inverse van [`port_ip4`]: de poort achter een adres op het slot-LAN
/// (0 is de kern), of `None` buiten het subnet en voor .0. Wie een app
/// zoekt, toetst zelf `1..=max_slots`.
#[must_use]
pub const fn ip4_port(ip: u32) -> Option<usize> {
    if ip >> (32 - NET_PREFIX) != HOST_IP4 >> (32 - NET_PREFIX) {
        return None;
    }
    match (ip & 0xff) as usize {
        0 => None,
        h => Some(h - 1),
    }
}

/// Een IPv4-adres uit het net-plan, met `Display` als dotted-quad.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Ip4(pub u32);

impl fmt::Display for Ip4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0.to_be_bytes();
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

#[cfg(test)]
mod tests;
