//! De control-page van een slot en de payload van een system-call.
//!
//! **De control-page** is de eerste pagina van de staart
//! ([`crate::layout::Tail::ctrl_page`]): 64-bit woorden in de kop, een
//! env-blob in het midden, en woorden die later kwamen bovenaan de page,
//! naar beneden groeiend. Elk veld heeft één schrijver; die staat bij het
//! veld. De offsets zijn byte voor byte die van Go-ABI 10 (Go telde door tot
//! 10; deze crate begint opnieuw bij 1, zie [`crate::ABI_VERSION`]).
//!
//! **De call-payload** ([`Req`], [`Resp`]) is wat een `Call`- of
//! `Result`-frame van [`crate::systemapi`] draagt: een kop van 24 bytes
//! (little-endian) plus pad en data.
//!
//! ```text
//! req:  ver u8 | op u8 | path_len u16 | seq u32 | off u64 | n u64 | path | data
//! resp: ver u8 | op u8 | status u16   | seq u32 | size u64 | _ u64 | data
//! ```
//!
//! Stateless (paden, geen fd's): een app-crash laat bij de kern niets
//! achter.
//!
//! De codec-payloads (Go `codec.go`) staan in `codec`: aanwijzingen naar
//! bytes die al in de partitie van de app liggen, geen bytes. De
//! device-payloads (`device.go`) horen bij de optische drive. Beide alleen
//! met de feature `media`; hun opcodes staan er altijd, zodat de nummers
//! bezet blijven.
//! De lijst van het gebundelde lezen ([`OP_READ_MANY`]) staat in [`many`].

use crate::{Error, Result};

// Het media-vlak (docs/media.md): de codec-payloads en de SCSI-payloads van
// de optische drive, alleen met de feature `media`.
#[cfg(feature = "media")]
pub mod codec;
#[cfg(feature = "media")]
pub mod device;
pub mod many;

// ---------------------------------------------------------------------------
// De control-page.
// ---------------------------------------------------------------------------

/// App: de status ([`AppStatus`]).
pub const CTRL_STATUS: u64 = 0x00;
/// App: de exitcode, gezet bij exit.
pub const CTRL_EXIT_CODE: u64 = 0x08;
/// Kern naar app: niet 0 is "stop jezelf" (coöperatief). Sinds 05-10 is de
/// waarde de termijn in ms die de app krijgt vóór de intrekking
/// ([`KILL_STOP`] is de oude vorm zonder termijn, en de ondergrens).
pub const CTRL_KILL: u64 = 0x10;
/// App: een oplopende teller, voor hang-detectie.
pub const CTRL_HEARTBEAT: u64 = 0x18;
/// App: de eigen RAM-maat, als bewijs dat de patch aankwam.
pub const CTRL_RAM_SIZE: u64 = 0x20;
/// Kern naar app: de lengte van de env-blob in bytes.
pub const CTRL_ENV_LEN: u64 = 0x28;
/// Kern naar trampoline: de app-entry (EL1) voor de ERET.
pub const CTRL_ENTRY: u64 = 0x30;
/// Kern naar trampoline: het fysieke adres van de stage-2-L1-tabel.
pub const CTRL_S2_TABLE: u64 = 0x38;
/// Kern naar app: de klok-offset (wall-ns bij tellerstand 0, `i64` als
/// bits; 0 = geen klok). De teller is gedeeld over alle cores, dus de
/// offset van de kern geldt exact voor elke app.
pub const CTRL_WALL_OFF: u64 = 0x40;
/// App naar kern: geaccumuleerde idle-TIJD in timer-tikken. Sinds 18-07
/// tijd in plaats van rondes: rondes bleken op ijzer door SEV-ruis
/// opgeblazen. Stond op 0xD8 en botste daar met [`CTRL_SMP_TCR`]; de
/// veldtabel onder de offsets bewaakt dat voortaan.
pub const CTRL_IDLE: u64 = 0x48;
/// Kern naar trampoline: de fysieke basis van de EL2-vectoren.
pub const CTRL_VEC_PA: u64 = 0x50;
/// Vector: ESR_EL2 van de fault die het slot deed vallen.
pub const CTRL_FAULT_ESR: u64 = 0x58;
/// Vector: FAR_EL2, het faultadres.
pub const CTRL_FAULT_FAR: u64 = 0x60;
/// Vector: vectorindex plus 1 (0 = geen fault gezien); zie [`FAULT_SYNC`].
pub const CTRL_FAULT_VEC: u64 = 0x68;
/// Kern naar app: het aantal cores (1 = geen SMP).
pub const CTRL_CORES: u64 = 0x70;
/// De actieve VBAR_EL1 van de dispatchende primaire, voor een secundaire.
pub const CTRL_SMP_VBAR: u64 = 0x78;
/// App naar secundaire: de stacktop (IPA).
pub const CTRL_SMP_SP: u64 = 0x88;
/// App naar secundaire: eerste runtime-argument (Go: `*m`).
pub const CTRL_SMP_MP: u64 = 0x90;
/// App naar secundaire: tweede runtime-argument (Go: `g0`).
pub const CTRL_SMP_G0: u64 = 0x98;
/// App naar secundaire: de entry (IPA).
pub const CTRL_SMP_FN: u64 = 0xA0;
/// App naar trampoline: de EL1-stub waar de secundaire heen ERET't.
pub const CTRL_SMP_STUB: u64 = 0xA8;
/// App naar secundaire: de stage-1-L1-tabel, zodat de stub geen geheugen
/// leest vóór zijn MMU aan staat (een pre-MMU-lees kan stale zijn).
pub const CTRL_SMP_TTBR0: u64 = 0xB0;
/// Kern naar trampoline: het slotnummer (= VMID).
pub const CTRL_SLOT: u64 = 0xB8;
/// App naar kern: de core-index die de runtime als extra SMP-core wil
/// (0 = geen verzoek). De kern valideert tegen [`CTRL_CORES`].
pub const CTRL_SMP_REQ: u64 = 0xC0;
/// Kern naar trampoline: de park-mailbox van déze core.
pub const CTRL_MBOX_PA: u64 = 0xC8;
/// Kern naar trampoline: de park-mailbox van de secundaire.
pub const CTRL_SMP_MBOX: u64 = 0xD0;
/// De actieve TCR_EL1 van de primaire, voor een secundaire (gelezen van de
/// levende registers, geen afgeleide kopie: de 39-bit-standaard kon de
/// Altra-UART op 16 TB niet vertalen, gemeten 17-07).
pub const CTRL_SMP_TCR: u64 = 0xD8;
/// Kern naar app: deelt dit slot het hart van de kern ([`HART_MAGIC`]
/// bovenin, [`HART_KERN`] onderin; [`shares_kern_hart`] leest hem)? Dan
/// draaien app en kern in één cache, en kan de app zijn frame-ringen
/// zonder onderhoud beloven ([`crate::ring::Coherence::Hardware`]), ook op
/// een architectuur waar de harts onderling niet coherent zijn (de C906 van
/// de LicheeRV, 03-10: per rondreis tussen twee bewoners van de OS-core
/// ~270 payload-regels plus de indexen, aan beide kanten). Een oude kern
/// liet dit woord 0 (in Go droeg het de maat van het gestagede image, en
/// geen v3-kern schreef het); zonder de magic belooft de app niets.
pub const CTRL_HART: u64 = 0xE0;
/// App naar kern: de werkelijke geheugen-draw van de runtime (0 = nog niet
/// gerapporteerd).
pub const CTRL_MEM_SYS: u64 = 0xE8;
/// De actieve MAIR_EL1 van de primaire, voor een secundaire.
pub const CTRL_SMP_MAIR: u64 = 0xF8;
/// Kern naar app: 1 als dit slot zijn core deelt; de idle-governor yieldt
/// dan naar de switcher in plaats van WFE te slapen.
pub const CTRL_SHARED: u64 = 0x100;
/// App naar kern: het aantal idle-rondes, van alle cores van de app (zoals
/// `CTRL_IDLE`). Bewust ongelezen (besluit Derek 06-08) tot een
/// onverklaarbaar hoog cpu-percentage erom vraagt.
pub const CTRL_WAKES: u64 = 0x108;
/// App naar switcher: de wek-drempel van de doorbell (head | bit 63). Alleen
/// wie de ring draint mag hem wapenen: anders maakt elke ARP-flood een app
/// zonder netstack permanent "due".
pub const CTRL_RX_DOOR: u64 = 0x110;
/// App naar kern: 1 is "maak van mijn doorbell een vFIQ". Alleen voor een
/// app met één core: HCR_EL2 is per core.
pub const CTRL_DOOR_IRQ: u64 = 0x118;
/// De env-blob: `key=val\n`-bytes die de kern schrijft en de app bij start
/// inleest.
pub const CTRL_ENV_DATA: u64 = 0x120;
/// Kern naar app: hoe de cores van dit slot idlen ([`IDLE_YIELD`]; 0 = wat
/// de architectuur zelf doet). Bovenaan de page: woorden die ná de env
/// kwamen, groeien naar beneden, zodat de env niet meer verschuift.
pub const CTRL_IDLE_MODE: u64 = 0xFF8;
/// Kern naar app: de heetste die-temperatuur van de node in milligraden
/// (`i64` als bits; 0 = geen meting), elke seconde gezet door de
/// telemetrie van de kern. Voor de heartbeat van Hop (`hop agents`, zoals
/// `Temp: board.TempMilliC` in de Go-agent). Onder [`CTRL_IDLE_MODE`],
/// naar beneden groeiend (29-09).
pub const CTRL_TEMP: u64 = 0xFF0;
/// Kern naar app: de timebase van de teller van de app in tikken per
/// seconde, gezet bij de bouw van de kooi (0 = de app neemt wat de
/// architectuur zelf zegt). Op arm64 zegt CNTFRQ_EL0 het al; RISC-V heeft
/// geen register waaruit hij volgt (de TIME-CSR telt 10 MHz op QEMU virt en
/// 25 MHz op de LicheeRV), dus daar is dit woord de enige bron. Onder
/// [`CTRL_TEMP`], naar beneden groeiend (29-09).
pub const CTRL_TIMEBASE_HZ: u64 = 0xFE8;
/// App naar kern: de vectorindex plus 1 van een exception die de app op
/// EL1 zelf ving (0 = geen), gezet door de vectortabel van applib vlak
/// vóór hij de app met [`EXIT_APP_FAULT`] laat eindigen. Onder
/// [`CTRL_TIMEBASE_HZ`], naar beneden groeiend, met de drie woorden eronder
/// (30-09).
///
/// Waarom een eigen blok naast [`CTRL_FAULT_VEC`]: dat rapport is van EL2
/// en ziet alleen wat naar EL2 trapt. Een fault die op EL1 blijft (een
/// alignment-fault, een ongedefinieerde instructie) sprong tot 30-09 naar
/// een lege VBAR_EL1, en EL2 zag dan alleen de tweede fault: de
/// instructie-abort op `VBAR + 0x200` (de eerste Pi 5-boot: `esr=0x82000005
/// far=0x200`). Hier staat de échte.
pub const CTRL_APP_FAULT_VEC: u64 = 0xFE0;
/// App naar kern: ESR_EL1 van die exception.
pub const CTRL_APP_FAULT_ESR: u64 = 0xFD8;
/// App naar kern: ELR_EL1, de PC waar hij viel.
pub const CTRL_APP_FAULT_ELR: u64 = 0xFD0;
/// App naar kern: FAR_EL1, het adres dat hij raakte (alleen zinvol bij een
/// abort).
pub const CTRL_APP_FAULT_FAR: u64 = 0xFC8;
/// Kern naar app: 32 bytes zaad uit de DRBG van de kern (`cpu::drbg`),
/// vier woorden van 0xFA8 tot 0xFC8, onder [`CTRL_APP_FAULT_FAR`], naar
/// beneden groeiend (30-09). De kern zet het bij de bouw van de kooi, na
/// een flip-adoptie en daarna elke seconde vers vanuit de telemetrie-tik;
/// elk slot krijgt eigen bytes. Geldig alleen onder het protocol van
/// [`CTRL_RNG_GEN`]; de bron staat in [`CTRL_RNG_SOURCE`].
///
/// Zaad, geen sleutel: de app haalt het door zijn eigen DRBG
/// (`applib::rand`), samen met eigen jitter, en gebruikt het nooit rauw.
/// Waarom op de page en niet als system-op: een woord op de page kost geen
/// verbinding (de TLS van Hop heeft zijn zaad nodig vóór er een
/// system-verbinding is), het is de vorm van [`CTRL_TEMP`] en
/// [`CTRL_WALL_OFF`], en een oude app leest het simpelweg niet.
pub const CTRL_RNG_SEED: u64 = 0xFA8;
/// De lengte van [`CTRL_RNG_SEED`] in bytes.
pub const CTRL_RNG_SEED_LEN: usize = 32;
/// Kern naar app: de generatie van [`CTRL_RNG_SEED`], een seqlock met één
/// schrijver (de kern op zijn OS-core). 0 = geen zaad (een oude kern, of
/// een kern zonder geseede DRBG); oneven = de kern schrijft net; even en
/// niet 0 = geldig. De kern schrijft oneven, dan het zaad en de bron, dan
/// de volgende even waarde, met een barrière en een cache-clean na elke
/// stap. De lezer leest de generatie, het zaad en de generatie opnieuw, en
/// neemt het zaad alleen als beide gelijk, even en niet 0 zijn. Een nieuwe
/// generatie is vers zaad: de app mengt het bij (herzaaien). Per page
/// monotoon, ook over een flip: de kern telt door vanaf wat er staat.
pub const CTRL_RNG_GEN: u64 = 0xFA0;
/// Kern naar app: de bron van het zaad in [`CTRL_RNG_SEED`]:
/// [`RNG_MAGIC`] in de bovenste 32 bits en een `RNG_SRC_*` in de onderste
/// byte ([`rng_source`] leest hem). Een woord zonder de magic is geen bron:
/// op een oude kern kan hier een lange env-blob staan (tot 0xEA8 bytes, zie
/// [`CTRL_ENV_LEGACY_MAX`]), en tekst vormt nooit de magic (zijn bytes
/// liggen boven 0x7F).
pub const CTRL_RNG_SOURCE: u64 = 0xF98;
/// De ruimte voor de env-blob. 0xEA8 tot het RNG-blok (30-09) er 40 van
/// nam.
pub const CTRL_ENV_MAX: u64 = CTRL_RNG_SOURCE - CTRL_ENV_DATA;
/// De grootste env die een kern ooit schreef: 0xEA8 bytes, vóór het
/// RNG-blok. Een lezer die ook op een oude kern moet draaien, accepteert
/// een [`CTRL_ENV_LEN`] tot hier zolang [`CTRL_RNG_SOURCE`] geen
/// [`RNG_MAGIC`] draagt (een oude kern); een nieuwe kern schrijft nooit
/// meer dan [`CTRL_ENV_MAX`].
pub const CTRL_ENV_LEGACY_MAX: u64 = CTRL_APP_FAULT_FAR - CTRL_ENV_DATA;

/// De bovenste 32 bits van [`CTRL_RNG_SOURCE`]. Elke byte ligt boven 0x7F,
/// dus een env van tekst die op een oude kern over het woord loopt, is
/// nooit een bron.
pub const RNG_MAGIC: u32 = 0xC0DE_5EED;
/// [`CTRL_RNG_SOURCE`]: de DRBG van de kern is uit timing-jitter gezaaid,
/// niet uit hardware (QEMU virt, een board zonder TRNG).
pub const RNG_SRC_JITTER: u8 = 1;
/// [`CTRL_RNG_SOURCE`]: hardware, RNDR (FEAT_RNG; de O6N).
pub const RNG_SRC_RNDR: u8 = 2;
/// [`CTRL_RNG_SOURCE`]: hardware, de SMCCC TRNG van de firmware (DEN 0098;
/// de Altra).
pub const RNG_SRC_SMCCC: u8 = 3;
/// [`CTRL_RNG_SOURCE`]: hardware, een TRNG-blok van de SoC (de RNG200 van
/// de Pi's, de RKRNG van de Radxa).
pub const RNG_SRC_SOC: u8 = 4;

/// De bovenste 32 bits van [`CTRL_HART`]; elke byte boven 0x7F, zoals
/// [`RNG_MAGIC`], zodat env-tekst van een oude kern hem nooit vormt.
pub const HART_MAGIC: u32 = 0xC0DE_4A87;
/// [`CTRL_HART`]: de app draait op het hart van de kern (de OS-core).
pub const HART_KERN: u64 = 1;

/// Het woord voor [`CTRL_HART`]: `kern` = op het hart van de kern.
#[must_use]
pub const fn hart_word(kern: bool) -> u64 {
    ((HART_MAGIC as u64) << 32) | if kern { HART_KERN } else { 0 }
}

/// Zegt het [`CTRL_HART`]-woord `word` dat de app op het hart van de kern
/// draait? Zonder de magic nooit.
#[must_use]
pub const fn shares_kern_hart(word: u64) -> bool {
    (word >> 32) as u32 == HART_MAGIC && word & HART_KERN != 0
}

/// Het woord voor [`CTRL_RNG_SOURCE`] bij bron `src` (een `RNG_SRC_*`).
#[must_use]
pub const fn rng_source_word(src: u8) -> u64 {
    ((RNG_MAGIC as u64) << 32) | src as u64
}

/// De `RNG_SRC_*` uit een [`CTRL_RNG_SOURCE`]-woord, of `None` als de magic
/// ontbreekt of de bron onbekend is (een oude kern, een env eroverheen).
#[must_use]
pub const fn rng_source(word: u64) -> Option<u8> {
    if (word >> 32) as u32 != RNG_MAGIC || word & 0xFFFF_FF00 != 0 {
        return None;
    }
    match word as u8 {
        s @ (RNG_SRC_JITTER | RNG_SRC_RNDR | RNG_SRC_SMCCC | RNG_SRC_SOC) => Some(s),
        _ => None,
    }
}

/// [`CTRL_EXIT_CODE`] van een app die op EL1 een exception ving: het
/// rapport staat in [`CTRL_APP_FAULT_VEC`] en de drie woorden eronder.
/// Geen gewone exitcode (0 klaar, 1 een spawn, 2 een paniek), en leesbaar
/// in een hexdump.
pub const EXIT_APP_FAULT: u64 = 0xFA17;

// De exitcodes van een fatale val, per soort (H9 van 01-11): de kern laat
// elke ongevraagde exit liggen en de watchdog reset de node, dus voor de
// kern zijn ze gelijk; de soort is er voor de diagnose (`HOPOS_HOP_EXIT`,
// [`CTRL_EXIT_CODE`] in `SLOT_STATUS`). Hop eerst (zijn `fail`), elke app
// mag ze gebruiken. 0 klaar, 1 een spawn en 2 een paniek zijn van applib.

/// De kern antwoordde niet, of weigerde wat hij nooit weigert (Hop:
/// `HOP_KERNEL_GONE`, zijn `EXIT_KERNEL`).
pub const EXIT_KERNEL: u64 = 3;
/// De config klopt niet (H2: `fail(config)`).
pub const EXIT_CONFIG: u64 = 4;
/// Een interne invariant brak: een taak die niet spawnt, een poort die niet
/// bindt, een staat die niet te lezen is.
pub const EXIT_INTERNAL: u64 = 5;

/// De bit in [`CTRL_RX_DOOR`] die de drempel wapent; een byte-index haalt
/// dat bit nooit.
pub const RX_DOOR_ARMED: u64 = 1 << 63;

/// [`CTRL_IDLE_MODE`]: idle is een yield naar EL2 (HVC #1), ook met een
/// eigen core. Zo idlet Apple silicon: op de M4 slaapt een app-core op EL1
/// niet (gemeten 02-09). De kern zet dit niet op een SMP-slot.
pub const IDLE_YIELD: u64 = 1;

/// [`CTRL_IDLE_MODE`]: de kern hoort de kick van deze app (RISC-V: `ecall`
/// met a7 = 2; de switcher belt de OS-core en hervat de app meteen, de
/// rotatie van de OS-core neemt hem als een yield naar nu). Zonder dit bit
/// kickt de app niet: voor een switcher van vóór 02-10 is elke a7 anders dan
/// 0 een exit.
pub const IDLE_KICK: u64 = 2;

/// [`CTRL_FAULT_VEC`]: geen fault gezien sinds de laatste start.
pub const FAULT_NONE: u64 = 0;
/// [`CTRL_FAULT_VEC`]: synchroon vanuit EL1 (index 8), een stage-2-fault;
/// ESR en FAR zijn geldig. Een kooi-overtreding en een hard-kill landen
/// allebei hier.
pub const FAULT_SYNC: u64 = 9;

/// [`CTRL_KILL`]: stop jezelf, zonder termijn. Een kern van vóór 05-10
/// schrijft dit; een nieuwe schrijft de termijn in ms, nooit lager dan dit
/// (0 zou geen verzoek zijn). Een app die alleen op "niet 0" kijkt (de
/// Go-SDK, applib tot 05-10) stopt bij allebei meteen.
pub const KILL_STOP: u64 = 1;

/// De status van een app op de control-page ([`CTRL_STATUS`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum AppStatus {
    /// De kern heeft de page geveegd.
    Empty = 0,
    /// De kern heeft de core gestart; de runtime is nog niet klaar.
    Booting = 1,
    /// De runtime draait (gezet door de app).
    Ready = 2,
    /// De app is gestopt; de exitcode staat in [`CTRL_EXIT_CODE`].
    Exited = 3,
    /// De apploader heeft het echte image gestaged en geparkeerd.
    Staged = 4,
}

impl AppStatus {
    /// De status van een rauw woord, of `None` voor een onbekende waarde.
    #[must_use]
    pub const fn from_raw(v: u64) -> Option<AppStatus> {
        Some(match v {
            0 => Self::Empty,
            1 => Self::Booting,
            2 => Self::Ready,
            3 => Self::Exited,
            4 => Self::Staged,
            _ => return None,
        })
    }

    /// Het rauwe woord.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self as u64
    }
}

/// Elk veld van de control-page als (offset, lengte), oplopend. De toets
/// eronder maakt een botsing zoals `CtrlSMPTcr` en `CtrlIdle` op 0xD8
/// (18-07) een compilefout: elk veld is 8-uitgelijnd, begint niet vóór het
/// einde van het vorige, en het laatste sluit de page af. Vrij: 0x80 en 0xF0
/// droegen in Go de SMP-trampoline en de zelfplaatsing; niemand leest ze
/// nog. 0xE0 (in Go de maat van het gestagede image) is sinds 03-10
/// [`CTRL_HART`].
const FIELDS: &[(u64, u64)] = &[
    (CTRL_STATUS, 8),
    (CTRL_EXIT_CODE, 8),
    (CTRL_KILL, 8),
    (CTRL_HEARTBEAT, 8),
    (CTRL_RAM_SIZE, 8),
    (CTRL_ENV_LEN, 8),
    (CTRL_ENTRY, 8),
    (CTRL_S2_TABLE, 8),
    (CTRL_WALL_OFF, 8),
    (CTRL_IDLE, 8),
    (CTRL_VEC_PA, 8),
    (CTRL_FAULT_ESR, 8),
    (CTRL_FAULT_FAR, 8),
    (CTRL_FAULT_VEC, 8),
    (CTRL_CORES, 8),
    (CTRL_SMP_VBAR, 8),
    (CTRL_SMP_SP, 8),
    (CTRL_SMP_MP, 8),
    (CTRL_SMP_G0, 8),
    (CTRL_SMP_FN, 8),
    (CTRL_SMP_STUB, 8),
    (CTRL_SMP_TTBR0, 8),
    (CTRL_SLOT, 8),
    (CTRL_SMP_REQ, 8),
    (CTRL_MBOX_PA, 8),
    (CTRL_SMP_MBOX, 8),
    (CTRL_SMP_TCR, 8),
    (CTRL_HART, 8),
    (CTRL_MEM_SYS, 8),
    (CTRL_SMP_MAIR, 8),
    (CTRL_SHARED, 8),
    (CTRL_WAKES, 8),
    (CTRL_RX_DOOR, 8),
    (CTRL_DOOR_IRQ, 8),
    (CTRL_ENV_DATA, CTRL_ENV_MAX),
    (CTRL_RNG_SOURCE, 8),
    (CTRL_RNG_GEN, 8),
    (CTRL_RNG_SEED, CTRL_RNG_SEED_LEN as u64),
    (CTRL_APP_FAULT_FAR, 8),
    (CTRL_APP_FAULT_ELR, 8),
    (CTRL_APP_FAULT_ESR, 8),
    (CTRL_APP_FAULT_VEC, 8),
    (CTRL_TIMEBASE_HZ, 8),
    (CTRL_TEMP, 8),
    (CTRL_IDLE_MODE, 8),
];

const _: () = {
    let mut end = 0;
    let mut i = 0;
    while i < FIELDS.len() {
        let (off, len) = FIELDS[i];
        assert!(off % 8 == 0 && off >= end);
        end = off + len;
        i += 1;
    }
    assert!(end == crate::layout::CTRL_STRIDE);
};
// Het zaad sluit precies aan op het fault-rapport erboven.
const _: () = assert!(CTRL_RNG_SEED + CTRL_RNG_SEED_LEN as u64 == CTRL_APP_FAULT_FAR);
// De SMP-handoff in het ctx-blok draagt de control-velden onder 256 bytes.
const _: () = assert!(CTRL_SMP_MAIR + 8 <= crate::layout::CTX_LEN - crate::layout::CTX_SMP);

// ---------------------------------------------------------------------------
// De call-payload.
// ---------------------------------------------------------------------------

/// De versie van het payload-formaat.
pub const VERSION: u8 = 1;
/// De lengte van de kop van een request of response.
pub const HDR_LEN: usize = 24;

/// `stat(path)`: de maat (een map: 0, status OK).
pub const OP_STAT: u8 = 1;
/// `read(path, off, n)`: de data.
pub const OP_READ: u8 = 2;
/// `write(path, off, data)`: maakt bestand en ouder-mappen.
pub const OP_WRITE: u8 = 3;
/// `list(path)`: namen, `\n`-gescheiden (`naam/` is een map).
pub const OP_LIST: u8 = 4;
/// `remove(path)`: een bestand of lege map.
pub const OP_REMOVE: u8 = 5;
// 6 was OpFetch: de kern downloadde een app-opgegeven URL met zijn volle
// rechten, een SSRF-pad naar alles wat de node bereikt. Gesloopt; het nummer
// blijft leeg, zodat een oud image een nette "onbekende op" krijgt.
/// `truncate(path, n)`: maakt bestand en ouder-mappen.
pub const OP_TRUNCATE: u8 = 7;
/// Object naar eigen pad (vervangend); de maat. `path` is de objectnaam
/// binnen de eigen map, `data` leeg (het lokale pad is dezelfde naam, zoals
/// in Go) of een ander lokaal pad (sinds alpha.12, additief).
pub const OP_STORE_PULL: u8 = 8;
/// Eigen pad naar object (vervangend); de maat. `path` en `data` zoals bij
/// [`OP_STORE_PULL`]: de objectnaam, en eventueel een ander lokaal pad.
pub const OP_STORE_PUSH: u8 = 9;
/// Keys onder de eigen map plus pad-prefix, `\n`-gescheiden en relatief
/// aan de eigen map (voer voor [`OP_STORE_PULL`]); `size` het aantal. Een
/// lijst die niet in één antwoord past, is een fout, nooit afgekapt.
pub const OP_STORE_LIST: u8 = 10;
/// Object weg (idempotent).
pub const OP_STORE_DROP: u8 = 11;
// 12 en 13 waren OpSurfGrant/OpSurfRevoke (gesloopt 06-08, dezelfde dag als
// gebouwd); de nummers blijven leeg.
/// Codec openen; het handvat in `size`. Niet idempotent, zoals alle
/// codec-ops: twee keer dezelfde feed is twee happen bitstream.
pub const OP_CODEC_OPEN: u8 = 14;
/// Bitstream erin (een buffer in de eigen partitie: `off`, `n`).
pub const OP_CODEC_FEED: u8 = 15;
/// Een lege beeldbuffer erin.
pub const OP_CODEC_OFFER: u8 = 16;
/// Nul of meer events. Met `n` > 0 wacht de kern tot er een is, hoogstens
/// `n` ms (en een seconde); met `n` = 0 antwoordt hij meteen.
pub const OP_CODEC_POLL: u8 = 17;
/// Codec sluiten.
pub const OP_CODEC_CLOSE: u8 = 18;
/// Eén SCSI-uitwisseling; niet herhaalbaar.
pub const OP_DEVICE_COMMAND: u8 = 19;
/// Bevestigde opslagbarrière voor een zichtbaar bestaand bestand of directory.
/// `off`, `n` en data zijn nul/leeg. Geeft de vastgelegde boomgeneratie in size.
/// Na verwijderen gebruikt de app het ouderpad; een vluchtige FS weigert.
pub const OP_SYNC: u8 = 20;
/// Gebundeld lezen: tot [`many::MAX_OPS`] lezingen uit één bestand in één
/// call, samen op het device, één antwoord met per opdracht de uitkomst
/// (de draadvorm in [`many`]). Idempotent, zoals [`OP_READ`]. Additief: een
/// oude kern geeft een nette "onbekende op".
pub const OP_READ_MANY: u8 = 21;
/// Het hoogste opnummer van deze module; de bevoegde operaties van
/// [`crate::systemapi`] liggen erboven.
pub const OP_MAX: u8 = OP_READ_MANY;

/// Een call-status: gelukt.
pub const STATUS_OK: u16 = 0;
/// Een call-status: algemene fout (tekst in de data).
pub const STATUS_ERROR: u16 = 1;
/// Een call-status: het pad bestaat niet.
pub const STATUS_NO_ENT: u16 = 2;
/// Een call-status: buiten de mounts of de eigen root.
pub const STATUS_DENIED: u16 = 3;

/// Een request.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Req<'a> {
    /// De operatie.
    pub op: u8,
    /// Het volgnummer, terug in de response.
    pub seq: u32,
    /// De offset.
    pub off: u64,
    /// De lengte of het getal-argument.
    pub n: u64,
    /// Het pad.
    pub path: &'a [u8],
    /// De data.
    pub data: &'a [u8],
}

/// Een response.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Resp<'a> {
    /// De operatie.
    pub op: u8,
    /// De status (`STATUS_*`); bij een fout draagt `data` de tekst.
    pub status: u16,
    /// Het volgnummer van de request.
    pub seq: u32,
    /// De maat of het handvat.
    pub size: u64,
    /// De data.
    pub data: &'a [u8],
}

/// Een little-endian `u16` op `b[i..]`; de aanroeper toetste de lengte.
fn le16(b: &[u8], i: usize) -> u16 {
    let mut w = [0u8; 2];
    w.copy_from_slice(&b[i..i + 2]);
    u16::from_le_bytes(w)
}

/// Een little-endian `u32` op `b[i..]`.
fn le32(b: &[u8], i: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[i..i + 4]);
    u32::from_le_bytes(w)
}

/// Een little-endian `u64` op `b[i..]`.
fn le64(b: &[u8], i: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(w)
}

/// Schrijft de kop van 24 bytes; `dst` is minstens [`HDR_LEN`] lang.
fn put_head(dst: &mut [u8], op: u8, w16: u16, seq: u32, a: u64, b: u64) {
    dst[0] = VERSION;
    dst[1] = op;
    dst[2..4].copy_from_slice(&w16.to_le_bytes());
    dst[4..8].copy_from_slice(&seq.to_le_bytes());
    dst[8..16].copy_from_slice(&a.to_le_bytes());
    dst[16..24].copy_from_slice(&b.to_le_bytes());
}

/// Toetst lengte en versie van een binnengekomen payload.
fn check_head(b: &[u8]) -> Result {
    if b.len() < HDR_LEN {
        return Err(Error::Short {
            len: b.len(),
            need: HDR_LEN,
        });
    }
    if b[0] != VERSION {
        return Err(Error::BadVersion {
            got: b[0],
            want: VERSION,
        });
    }
    Ok(())
}

/// Serialiseert een request in `dst`; geeft de lengte.
pub fn encode_req(dst: &mut [u8], r: &Req<'_>) -> Result<usize> {
    let total = HDR_LEN + r.path.len() + r.data.len();
    if dst.len() < total {
        return Err(Error::Short {
            len: dst.len(),
            need: total,
        });
    }
    encode_req_head(dst, r)?;
    let (path, data) = dst[HDR_LEN..total].split_at_mut(r.path.len());
    path.copy_from_slice(r.path);
    data.copy_from_slice(r.data);
    Ok(total)
}

/// Schrijft alleen de kop van `r` in `dst[..HDR_LEN]`, met de padlengte van
/// `r.path`; pad en data schrijft de aanroeper zelf erachter (applib zet ze
/// rechtstreeks op de draad). Geeft [`HDR_LEN`]: zelfde draadvorm als
/// [`encode_req`], zonder de kopie.
pub fn encode_req_head(dst: &mut [u8], r: &Req<'_>) -> Result<usize> {
    let plen = u16::try_from(r.path.len()).map_err(|_| Error::PayloadTooLarge {
        len: r.path.len(),
        max: u16::MAX as usize,
    })?;
    if dst.len() < HDR_LEN {
        return Err(Error::Short {
            len: dst.len(),
            need: HDR_LEN,
        });
    }
    put_head(dst, r.op, plen, r.seq, r.off, r.n);
    Ok(HDR_LEN)
}

/// Parseert een request; pad en data lenen uit `b`.
pub fn decode_req(b: &[u8]) -> Result<Req<'_>> {
    check_head(b)?;
    let plen = usize::from(le16(b, 2));
    let (path, data) = b[HDR_LEN..].split_at_checked(plen).ok_or(Error::Short {
        len: b.len(),
        need: HDR_LEN + plen,
    })?;
    Ok(Req {
        op: b[1],
        seq: le32(b, 4),
        off: le64(b, 8),
        n: le64(b, 16),
        path,
        data,
    })
}

/// Serialiseert een response in `dst`; geeft de lengte.
pub fn encode_resp(dst: &mut [u8], r: &Resp<'_>) -> Result<usize> {
    let total = HDR_LEN + r.data.len();
    if dst.len() < total {
        return Err(Error::Short {
            len: dst.len(),
            need: total,
        });
    }
    dst[HDR_LEN..total].copy_from_slice(r.data);
    encode_resp_head(dst, r, r.data.len())
}

/// Schrijft alleen de kop van `r` in `dst[..HDR_LEN]`; de `n` databytes
/// staan er al op `dst[HDR_LEN..]` (de aanroeper liet de opslag er direct in
/// lezen). `r.data` wordt genegeerd. Geeft de lengte: zelfde draadvorm als
/// [`encode_resp`], zonder de kopie.
pub fn encode_resp_head(dst: &mut [u8], r: &Resp<'_>, n: usize) -> Result<usize> {
    let total = HDR_LEN.saturating_add(n);
    if dst.len() < total {
        return Err(Error::Short {
            len: dst.len(),
            need: total,
        });
    }
    put_head(dst, r.op, r.status, r.seq, r.size, 0);
    Ok(total)
}

/// Parseert een response; de data leent uit `b`.
pub fn decode_resp(b: &[u8]) -> Result<Resp<'_>> {
    check_head(b)?;
    Ok(Resp {
        op: b[1],
        status: le16(b, 2),
        seq: le32(b, 4),
        size: le64(b, 8),
        data: &b[HDR_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Het bronwoord van het RNG-blok: alleen met de magic, alleen een
    /// bekende bron, en tekst (een env op een oude kern) is nooit een bron.
    #[test]
    fn rng_source_needs_the_magic() {
        for s in [RNG_SRC_JITTER, RNG_SRC_RNDR, RNG_SRC_SMCCC, RNG_SRC_SOC] {
            assert_eq!(rng_source(rng_source_word(s)), Some(s));
        }
        assert_eq!(rng_source(0), None);
        assert_eq!(rng_source(rng_source_word(0)), None);
        assert_eq!(rng_source(rng_source_word(9)), None);
        assert_eq!(rng_source(rng_source_word(RNG_SRC_SOC) | 0x100), None);
        assert_eq!(rng_source(u64::from_le_bytes(*b"HOP_X=1\n")), None);
        assert_eq!(CTRL_ENV_LEGACY_MAX, 0xEA8);
        assert_eq!(CTRL_ENV_MAX, 0xE78);
        assert!(shares_kern_hart(hart_word(true)));
        assert!(!shares_kern_hart(hart_word(false)));
        assert!(!shares_kern_hart(u64::from_le_bytes(*b"HOP_X=1\n")));
        assert!(!shares_kern_hart(HART_KERN));
    }

    #[test]
    fn req_roundtrip() {
        let r = Req {
            op: OP_WRITE,
            seq: 7,
            off: 1 << 40,
            n: 3,
            path: b"/data/x",
            data: b"abc",
        };
        let mut buf = [0u8; 64];
        let n = encode_req(&mut buf, &r).unwrap();
        assert_eq!(n, HDR_LEN + 7 + 3);
        assert_eq!(decode_req(&buf[..n]).unwrap(), r);
        let mut head = [0u8; HDR_LEN];
        assert_eq!(encode_req_head(&mut head, &r), Ok(HDR_LEN));
        assert_eq!(head[..], buf[..HDR_LEN]);
        assert!(matches!(
            decode_req(&buf[..HDR_LEN + 3]),
            Err(Error::Short { .. })
        ));
        buf[0] = 2;
        assert!(matches!(
            decode_req(&buf[..n]),
            Err(Error::BadVersion { got: 2, want: 1 })
        ));
    }

    #[test]
    fn resp_roundtrip_en_kop_alleen() {
        let r = Resp {
            op: OP_READ,
            status: STATUS_NO_ENT,
            seq: 9,
            size: 42,
            data: b"weg",
        };
        let mut buf = [0u8; 64];
        let n = encode_resp(&mut buf, &r).unwrap();
        assert_eq!(decode_resp(&buf[..n]).unwrap(), r);
        let mut other = [0u8; 64];
        other[HDR_LEN..HDR_LEN + 3].copy_from_slice(b"weg");
        let m = encode_resp_head(&mut other, &Resp { data: &[], ..r }, 3).unwrap();
        assert_eq!(&other[..m], &buf[..n]);
        assert!(decode_resp(&buf[..HDR_LEN - 1]).is_err());
    }

    #[test]
    fn app_status_roundtrip() {
        for s in [
            AppStatus::Empty,
            AppStatus::Booting,
            AppStatus::Ready,
            AppStatus::Exited,
            AppStatus::Staged,
        ] {
            assert_eq!(AppStatus::from_raw(s.raw()), Some(s));
        }
        assert_eq!(AppStatus::from_raw(5), None);
    }
}
