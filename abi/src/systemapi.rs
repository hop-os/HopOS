//! Het ene app-naar-kern-callcontract: frames over een blijvende
//! TCP-verbinding naar 10.100.0.1:10100 op het geïsoleerde slot-LAN.
//!
//! ARM64 en RISC-V spreken exact deze bytes; alleen hun ring-doorbell onder
//! het netwerk verschilt. Een frame is een kop van 12 bytes plus payload:
//!
//! ```text
//! magic u32 ("HOPS", little-endian) | version u8 | kind u8 | 0 u16 | len u32
//! ```
//!
//! Een `Call` draagt een [`crate::hopabi::Req`], een `Result` een
//! [`crate::hopabi::Resp`], een `Log` een logregel.
//!
//! Hier staan ook de **bevoegde operaties** ([`PrivOp`], PORT.md §6
//! beslissing 1): Hop, de eerste bewoner, bestuurt er de lifecycle mee. Wie
//! ze mag, bepaalt het token dat de kern bij boot aan het slot van Hop geeft
//! (`kern::system::Privilege`), niet een slotnummer in deze crate; de kern
//! weigert ze van elk ander slot met [`crate::hopabi::STATUS_DENIED`].
//!
//! # De levensloop van een slot
//!
//! ```text
//! START_SLOT(spec, image_size) -> slot        de kern kiest het slot
//! STREAM_IMAGE(slot, 0, brok)  -> More
//! STREAM_IMAGE(slot, n, brok)  -> More
//! STREAM_IMAGE(slot, m, brok)  -> Placed      laatste byte: plaatsen + starten
//!                              |  Failed(tekst)
//! SLOT_STATUS(slot)            -> SlotInfo    staat, core, heartbeat, exit (slot 0: de kern)
//! NEXT_LOG(slot, max)          -> regel | leeg (slot 0: de kern zelf)
//! STOP_SLOT(slot, timeout_ms)  -> Ok (de kern geeft vrij) | fout (quarantaine)
//! ```
//!
//! # Hop zelf herstarten
//!
//! ```text
//! STATE_PUT(blob)                      bij elke wijziging: de staat bij de kern
//! START_SLOT(HOP_IMAGE_JOB), STREAM    alleen voor een nieuw image
//! HOP_RESTART(slot | 0, sha, termijn)  -> Ok; Hop vertrekt zelf, dan de nieuwe
//! STATE_GET(0)                         de nieuwe Hop: zijn staat terug, adopteren
//! ```
//!
//! # De store-ops van een app
//!
//! Een app kopieert op afroep tussen zijn eigen map in de object-store
//! (`apps/<cluster>/<job>/`) en zijn hopfs-zicht (`OP_STORE_*` van
//! [`crate::hopabi`]). De kern heeft geen S3, geen sleutels en geen TLS;
//! Hop wel. Dus wacht de call van de app in een rij van de kern, en haalt
//! Hop hem op ([`store`]):
//!
//! ```text
//! app:  OP_STORE_PUSH("state.json")            wacht
//! Hop:  NEXT_STORE(wacht)      -> StoreTask     ticket, slot, job, key, pad
//! Hop:  STORE_READ(ticket, 0)  -> bytes         door de mounts van dat slot
//! Hop:  PUT apps/<cluster>/<job>/state.json     op S3
//! Hop:  STORE_DONE(ticket, OK, maat)
//! app:                         <- maat
//! ```
//!
//! Na de laatste byte plaatst de kern het image zelf: ELF lezen, het plan
//! van [`crate::place::build`], de RAM-declaratie patchen, BSS nullen, de
//! env op de control-page, en dan de kooi bouwen en dispatchen. Faalt dat,
//! dan ruimt de kern zijn reserveringen zelf op; de aanroeper ruimt alleen
//! zijn boekhouding op. Een STOP_SLOT op een half gestroomd slot breekt de
//! stroom af. Niets blokkeert: elke call antwoordt meteen.
//!
//! Wat hier NIET staat: de socket zelf. De kop wordt hier geschreven en
//! gelezen ([`encode_header`], [`HeaderReader`]); de bytes verplaatsen doet
//! de netstack van wie de verbinding bezit. Een payload van een MiB landt zo
//! rechtstreeks in de buffer van de lezer, zonder tussenkopie (04-09: elke
//! verse MiB per call was GC-werk aan beide kanten).

use crate::layout::HOST_IP4;
use crate::{Error, Result};

/// De versie van het frame-formaat.
pub const VERSION: u8 = 1;
/// De TCP-poort van de kern op het slot-LAN.
pub const PORT: u16 = 10100;
/// Het adres van de kern op het slot-LAN: 10.100.0.1.
pub const ADDRESS: [u8; 4] = HOST_IP4.to_be_bytes();

const _: () = assert!(u32::from_be_bytes(ADDRESS) == HOST_IP4);
const _: () = assert!(ADDRESS[0] == 10 && ADDRESS[1] == 100 && ADDRESS[3] == 1);

/// De grootste I/O-hap: amortiseert protocol- en opslagkosten zonder ooit
/// een heel bestand in kerngeheugen te houden.
pub const MAX_IO_CHUNK: usize = 1 << 20;
/// De grootste payload: een hap plus ruimte voor kop en pad.
pub const MAX_PAYLOAD: usize = MAX_IO_CHUNK + (64 << 10);
/// De lengte van de framekop.
pub const HEADER_LEN: usize = 12;
/// Het magische getal: "HOPS" little-endian op de draad.
pub const MAGIC: u32 = 0x5350_4f48;

const _: () = assert!(MAX_PAYLOAD <= u32::MAX as usize);
const _: () = assert!(u32::from_le_bytes(*b"HOPS") == MAGIC);

pub mod store;

mod mounts;
pub use mounts::{MAX_MOUNT_BYTES, MAX_MOUNT_PATH, MAX_START_MOUNTS, MountRef, Mounts, mount_blob};

/// De soort van een frame.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// App naar kern: een request.
    Call = 1,
    /// Kern naar app: de response.
    Result = 2,
    /// App naar kern: een logregel.
    Log = 3,
}

impl Kind {
    /// De soort van een rauw getal.
    pub const fn from_raw(v: u8) -> Result<Kind> {
        match v {
            1 => Ok(Self::Call),
            2 => Ok(Self::Result),
            3 => Ok(Self::Log),
            _ => Err(Error::BadKind(v)),
        }
    }
}

/// Een gelezen framekop.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Header {
    /// De soort.
    pub kind: Kind,
    /// De payloadlengte, hoogstens [`MAX_PAYLOAD`].
    pub len: usize,
}

/// Toetst een payloadlengte tegen [`MAX_PAYLOAD`].
fn check_len(len: usize) -> Result {
    if len > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge {
            len,
            max: MAX_PAYLOAD,
        });
    }
    Ok(())
}

/// Schrijft de kop van een begrensd, zelfbeschrijvend frame. De lengte
/// wordt vóór het schrijven getoetst: een te groot frame gaat nooit de
/// draad op.
pub fn encode_header(kind: Kind, len: usize) -> Result<[u8; HEADER_LEN]> {
    check_len(len)?;
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4] = VERSION;
    h[5] = kind as u8;
    // Geen omloop: check_len begrensde op MAX_PAYLOAD < u32::MAX.
    h[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(h)
}

/// Leest en toetst een framekop: magie, versie, soort en maat worden
/// geweigerd vóór er één payloadbyte gelezen is.
pub fn decode_header(h: &[u8; HEADER_LEN]) -> Result<Header> {
    let [m0, m1, m2, m3, ver, kind, _, _, l0, l1, l2, l3] = *h;
    let magic = u32::from_le_bytes([m0, m1, m2, m3]);
    if magic != MAGIC {
        return Err(Error::BadMagic(magic));
    }
    if ver != VERSION {
        return Err(Error::BadVersion {
            got: ver,
            want: VERSION,
        });
    }
    let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
    check_len(len)?;
    Ok(Header {
        kind: Kind::from_raw(kind)?,
        len,
    })
}

/// Verzamelt een framekop uit stukken zoals TCP ze levert.
///
/// Een verbinding levert bytes in willekeurige happen, tot één byte per
/// keer; de lezer voert ze hier in tot de kop compleet is en leest daarna
/// de payload waar hij hem hebben wil.
#[derive(Clone, Debug, Default)]
pub struct HeaderReader {
    buf: [u8; HEADER_LEN],
    have: usize,
}

impl HeaderReader {
    /// Een lege lezer.
    #[must_use]
    pub const fn new() -> HeaderReader {
        HeaderReader {
            buf: [0; HEADER_LEN],
            have: 0,
        }
    }

    /// Neemt bytes uit `src` tot de kop compleet is; geeft hoeveel er
    /// genomen zijn. De rest van `src` is payload.
    pub fn fill(&mut self, src: &[u8]) -> usize {
        let want = HEADER_LEN - self.have;
        let n = want.min(src.len());
        self.buf[self.have..self.have + n].copy_from_slice(&src[..n]);
        self.have += n;
        n
    }

    /// Is de kop compleet?
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.have == HEADER_LEN
    }

    /// De getoetste kop, of `None` zolang hij niet compleet is. Zet de
    /// lezer terug voor het volgende frame.
    pub fn take(&mut self) -> Option<Result<Header>> {
        if !self.is_complete() {
            return None;
        }
        self.have = 0;
        Some(decode_header(&self.buf))
    }
}

/// De bevoegde operaties: het opnummer van een [`crate::hopabi::Req`] in
/// een `Call`-frame, boven de gewone ops ([`crate::hopabi::OP_MAX`]).
///
/// Per op staat welke velden van de request wat dragen en wat de response
/// zegt; de helpers hieronder bouwen en lezen ze zonder allocatie. Een
/// fout is altijd status [`crate::hopabi::STATUS_ERROR`] (of
/// [`crate::hopabi::STATUS_DENIED`]) met de reden als tekst in `data`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PrivOp {
    /// Reserveer een slot: `path` de jobnaam, `data` een [`StartHead`] plus
    /// de sharegroup-naam, de env-blob, de poorten en de volumes
    /// ([`StartReq`]). Het antwoord: `size`
    /// is het slot dat de kern koos. Niet idempotent.
    StartSlot = 0x40,
    /// Stroom een brok image: `off` het slot, `n` de offset van de eerste
    /// byte in het image (moet gelijk zijn aan wat de kern al ontving), `data`
    /// de bytes, hoogstens [`MAX_IO_CHUNK`]. Het antwoord ([`StreamResp`]):
    /// `size` de ontvangen bytes, `data[0]` een [`StreamState`], bij
    /// [`StreamState::Failed`] gevolgd door de reden. Een fout-status is een
    /// geweigerde brok; ook dan is de stroom afgebroken.
    StreamImage = 0x41,
    /// Stop een slot: `off` het slot, `n` de coöperatieve termijn in ms.
    /// Breekt ook een half gestroomd slot af. `STATUS_OK`: de kern neemt de
    /// vrijgave op zich; een fout: niet bevestigd, het slot blijft in
    /// quarantaine.
    StopSlot = 0x42,
    /// De status van een slot: `off` het slot. Het antwoord: `data` is een
    /// [`SlotInfo`].
    SlotStatus = 0x43,
    /// De volgende logregel van een slot: `off` het slot, `n` de grootste
    /// lengte. Het antwoord: `size` 1 en de regel in `data` (afgekapt op
    /// `n`), of `size` 0 als er niets klaarstaat. De kern bewaart per slot
    /// een korte ring.
    ///
    /// Slot 0 is de kern zelf, zoals bij [`PrivOp::SlotStatus`]: de
    /// volgende regel van zijn console, dezelfde bytes als de UART en
    /// tcp/5555, dus ook de regels van de apps (`slot N: ...`); een regel
    /// langer dan `n` komt in stukken. Eén ring van 32 KiB
    /// (`kern::conport`) die een flip en een reset overleeft, met de
    /// positie van Hop erin: na een flip leest Hop verder, na een reset
    /// krijgt de nieuwe Hop eerst wat de vorige nog niet las.
    NextLog = 0x44,
    /// Zet de klok: `n` de Unix-tijd in nanoseconden.
    SetClock = 0x45,
    /// Flip naar een nieuwe kern: `off` het gereserveerde slot waarin de
    /// bundel gestroomd is (START_SLOT met de jobnaam
    /// [`FLIP_BUNDLE_JOB`], dan STREAM_IMAGE: rauw, zonder
    /// plaatsing), `path` de verwachte SHA-256 (32 bytes, of 64 hex-tekens).
    /// `STATUS_OK`: de bundel is getoetst en klaargelegd, de sprong volgt
    /// een halve seconde later; het slot is dan al terug in de pool.
    ///
    /// `n` draagt de vlaggen van de flip ([`FLIP_COLD`]); een Hop van vóór
    /// de vlag stuurt 0, en dat is de warme flip van altijd.
    Flip = 0x46,
    /// De volgende store-opdracht van een app ([`store`]): `n` de langste
    /// wachttijd in ms (0 = niet wachten, de kern kapt af op
    /// [`store::MAX_WAIT_MS`]). Het antwoord: `size` 1 en een
    /// [`store::StoreTask`] in `data`, of `size` 0 als er niets klaarstaat.
    NextStore = 0x47,
    /// Lees een stuk van het bestand van een opdracht (push): `off` het
    /// ticket, `n` de offset in het bestand, `path` het lokale pad van de
    /// opdracht (letterlijk zoals [`store::StoreTask::path`]), `data` de
    /// grootste lengte als `u64` ([`store::read_len`]). Het antwoord:
    /// `size` de maat van het hele bestand, `data` de bytes vanaf `n`.
    StoreRead = 0x48,
    /// Schrijf een stuk van het bestand van een opdracht (pull): `off` het
    /// ticket, `n` de offset, `path` het lokale pad, `data` de bytes. Een
    /// schrijf op offset 0 kort het bestand eerst in tot 0 (vervangend),
    /// ook met nul bytes: zo bestaat een leeg object ook lokaal.
    StoreWrite = 0x49,
    /// Rond een opdracht af: `off` het ticket, `n` de maat (pull en push:
    /// bytes; list: het aantal namen), `data` een [`store::DoneHead`] plus
    /// de namen (list, `\n`-gescheiden, relatief aan de eigen map) of de
    /// fouttekst. `STATUS_NO_ENT` als antwoord: de opdracht bestaat niet
    /// meer (de app is gestopt); Hop gooit hem dan weg.
    StoreDone = 0x4A,
    /// Leg de staat van Hop bij de kern neer: `data` het hele blob,
    /// hoogstens [`HOP_STATE_MAX`] bytes, vervangend en atomair (een lezer
    /// ziet het oude of het nieuwe blob, nooit een mengsel); nul bytes wist
    /// het. Het blob leeft in het geheugen van de kern: het overleeft een
    /// herstart van Hop ([`PrivOp::HopRestart`]) en sterft met de boot (ook
    /// met de reset van de watchdog na een falende Hop). Zo kan een staat van een vorige
    /// boot nooit een bewoner van deze boot voor de zijne houden (het spook
    /// van 02-10 kwam uit een bestand op hopfs). De kern leest het niet.
    StatePut = 0x4B,
    /// Lees de staat van Hop: `n` de offset in het blob. Het antwoord:
    /// `size` de maat van het hele blob (0: er ligt niets, ook dan
    /// `STATUS_OK`), `data` de bytes vanaf `n`, zoveel als in het antwoord
    /// past en nooit meer dan [`HOP_STATE_MAX`]. Een `n` voorbij het einde
    /// is een fout.
    StateGet = 0x4C,
    /// Herstart Hop in zijn eigen slot, met zijn eigen image of een nieuw:
    /// `off` 0 voor hetzelfde image, anders het slot waarin het nieuwe image
    /// rauw gestroomd is (START_SLOT met de jobnaam [`HOP_IMAGE_JOB`], dan
    /// STREAM_IMAGE), `path` dan de verwachte SHA-256 (32 bytes, of 64
    /// hex-tekens; bij `off` 0 leeg, een som zonder image weigert de kern),
    /// `n` in de lage 32 bits de termijn in ms waarbinnen Hop zelf vertrekt
    /// (0: [`HOP_RESTART_GRACE_MS`], hoogstens [`HOP_RESTART_GRACE_MAX_MS`]);
    /// de hoge 32 bits zijn vlaggen, nu geen enkele: een gezette weigert de
    /// kern, liever "nee" dan iets anders dan gevraagd. `STATUS_OK`: het
    /// image is getoetst (som en ELF) en overgenomen, het slot is terug in
    /// de pool. Dan is Hop aan zet, zoals een systemd-dienst die om zijn
    /// eigen herstart vraagt: hij schrijft dit antwoord en zijn staat weg en
    /// keert terug uit `main` (een exit). De kern wacht daar hoogstens de
    /// termijn op; vertrok Hop dan nog niet, dan volgt de stop-bel (de
    /// kill-vlag) met een korte termijn als vangnet. Daarna plaatst de kern
    /// de nieuwe Hop in hetzelfde slot, met dezelfde env, poorten en
    /// bevoegdheid.
    ///
    /// De belofte van die start, het seintje dat Hop nodig heeft: de kern
    /// plaatst de nieuwe Hop pas als de oude bevestigd weg is, en zijn
    /// system-verbindingen worden vanaf zijn eerste instructie toegelaten.
    /// De generatie van het slot staat vóór het startschot, en de plaatsen
    /// van de vorige levensduur tellen niet tegen de nieuwe (de teller per
    /// levensduur, `kern::slots::ServicerCtl::try_conn`). Hop hoeft dus
    /// niet te wachten of opnieuw te proberen: een weigering of een
    /// mislukte call naar de kern bij zijn start is een fout, en dan stopt
    /// Hop hard (de kern laat hem liggen, `HOPOS_HOP_DOWN`, en de watchdog
    /// reset de node).
    ///
    /// De andere bewoners merken niets; de staat van Hop ligt
    /// in [`PrivOp::StatePut`]. Haalt een nieuw image het niet (het plaatst
    /// niet, of de nieuwe Hop faalt of vertrekt ongevraagd), dan is er geen
    /// Hop meer: een falende Hop is een falende node. De kern zet geen vorig
    /// image terug; de node-watchdog ziet Hop's hartslag en de canary
    /// stoppen, reset de node, en de koude boot start de Hop van het
    /// bootmedium. Een fout:
    /// er ligt al een herstart klaar, of er is geen image om "hetzelfde" uit
    /// te plaatsen (na een warme kern-flip die over de staging ging). Viel
    /// de weigering bij de toets van het image (som, ELF), dan is het slot
    /// al terug in de pool en is een STOP_SLOT erop een no-op; viel hij
    /// ervóór (velden, een onvolledige stroom), dan staat de stroom nog en
    /// breekt STOP_SLOT hem af.
    HopRestart = 0x4D,
}

/// De grootste staat van Hop ([`PrivOp::StatePut`]): één hap, dus één call.
pub const HOP_STATE_MAX: usize = MAX_IO_CHUNK;

/// De termijn van [`PrivOp::HopRestart`] als `n` 0 zegt: zo lang krijgt
/// Hop om zelf te vertrekken, Hop's eigen 3 s van de nette stop.
pub const HOP_RESTART_GRACE_MS: u64 = 3000;

/// De langste termijn van [`PrivOp::HopRestart`]: de lease-TTL en de
/// agent-timeout van de leader zijn 30 s, en een herstart die langer
/// duurt dan dat, is voor het cluster een verloren node.
pub const HOP_RESTART_GRACE_MAX_MS: u64 = 30_000;

/// De jobnaam waarmee Hop een slot reserveert voor een flip-bundel: de
/// bytes van STREAM_IMAGE gaan dan rauw de partitie in (vanaf offset 0),
/// zonder plaatsing en zonder start, tot [`PrivOp::Flip`] ze opeist. Een
/// naam en geen vlag in de wire: Hop sprak de ABI van zijn getagde HopOS
/// (alpha.4), en een naam paste daarin zonder nieuw veld. Tot 3.0.16 stond
/// hij in `kern::system` (dat hem nog her-exporteert) en hield Hop een
/// tekstkopie.
pub const FLIP_BUNDLE_JOB: &[u8] = b"hopos.flip-bundle";

/// De jobnaam waarmee Hop een slot reserveert voor een nieuw image van
/// zichzelf: de bytes van STREAM_IMAGE gaan rauw de partitie in, zonder
/// plaatsing en zonder start, tot [`PrivOp::HopRestart`] ze opeist (de vorm
/// van [`FLIP_BUNDLE_JOB`]).
pub const HOP_IMAGE_JOB: &[u8] = b"hopos.hop-image";

/// Vlag in `n` van [`PrivOp::Flip`]: de KOUDE flip. De kern stopt elke
/// bewoner naast Hop, zet de app-cores uit en springt zonder bewoners over
/// te dragen; de nieuwe kern installeert zijn eigen switch-code en plaatst
/// Hop koud uit de staging (`hopos/src/flip.rs`, `docs/flip.md`). De weg
/// voor een bundel met een andere switch-code, die de warme flip weigert.
/// Additief (29-09): de andere bits van `n` zijn nul en blijven dat.
pub const FLIP_COLD: u64 = 1;

/// Het laagste bevoegde opnummer.
pub const PRIV_OP_FIRST: u8 = PrivOp::StartSlot as u8;
/// Het hoogste bevoegde opnummer.
pub const PRIV_OP_LAST: u8 = PrivOp::HopRestart as u8;

const _: () = assert!(PRIV_OP_FIRST > crate::hopabi::OP_MAX);

impl PrivOp {
    /// Alle bevoegde operaties, in opnummer-volgorde.
    pub const ALL: [PrivOp; 14] = [
        Self::StartSlot,
        Self::StreamImage,
        Self::StopSlot,
        Self::SlotStatus,
        Self::NextLog,
        Self::SetClock,
        Self::Flip,
        Self::NextStore,
        Self::StoreRead,
        Self::StoreWrite,
        Self::StoreDone,
        Self::StatePut,
        Self::StateGet,
        Self::HopRestart,
    ];

    /// De bevoegde operatie van een opnummer, of `None` voor een gewone of
    /// onbekende op.
    #[must_use]
    pub const fn from_op(op: u8) -> Option<PrivOp> {
        Some(match op {
            0x40 => Self::StartSlot,
            0x41 => Self::StreamImage,
            0x42 => Self::StopSlot,
            0x43 => Self::SlotStatus,
            0x44 => Self::NextLog,
            0x45 => Self::SetClock,
            0x46 => Self::Flip,
            0x47 => Self::NextStore,
            0x48 => Self::StoreRead,
            0x49 => Self::StoreWrite,
            0x4A => Self::StoreDone,
            0x4B => Self::StatePut,
            0x4C => Self::StateGet,
            0x4D => Self::HopRestart,
            _ => return None,
        })
    }

    /// Het opnummer.
    #[must_use]
    pub const fn op(self) -> u8 {
        self as u8
    }
}

/// De core-klasse die een start vraagt (PORT.md §6 beslissing 3).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum CoreClass {
    /// Geen voorkeur.
    #[default]
    Any = 0,
    /// Een kleine (zuinige) core.
    Small = 1,
    /// Een middelgrote core.
    Mid = 2,
    /// Een grote core.
    Big = 3,
}

impl CoreClass {
    /// De klasse van een rauw getal.
    pub const fn from_raw(v: u8) -> Result<CoreClass> {
        match v {
            0 => Ok(Self::Any),
            1 => Ok(Self::Small),
            2 => Ok(Self::Mid),
            3 => Ok(Self::Big),
            _ => Err(Error::BadKind(v)),
        }
    }
}

/// Het vaste deel van een [`PrivOp::StartSlot`]-request, vooraan in `data`
/// (little-endian). Daarachter: `group_len` bytes sharegroup-naam, dan
/// `env_len` bytes env-blob (`key=val\n`), dan `port_count` poorten van elk
/// [`PORT_LEN`] bytes (`u16` little-endian), dan `mounts_len` bytes volumes
/// ([`mount_blob`]).
///
/// `port_count` staat op de plek die tot alpha.7 `reserved` heette en 0
/// was: een Hop van vóór de poorten stuurt dus nul poorten, en dezelfde
/// bytes betekenen hetzelfde. `mounts_len` staat sinds 30-09 (alpha.11) op
/// het tweede gereserveerde woord, met dezelfde afspraak: nul volumes zijn
/// de bytes van alpha.10. Een kern van vóór een veld weigert een start die
/// het gebruikt luid (de lengte van `data` klopt dan niet), nooit stil
/// zonder de poorten of zonder het volume.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct StartHead {
    /// De zichtbare partitie in bytes (de `memory_limit` van de job,
    /// inclusief de ABI-staart).
    pub memory_limit: u64,
    /// De maat van het image in bytes; de kern plaatst na precies zoveel.
    pub image_size: u64,
    /// De SMP-cores van de app (1 of meer).
    pub cores: u16,
    /// De poolgrootte in cores (alleen met een sharegroup).
    pub pool_cores: u16,
    /// De [`CoreClass`] als getal.
    pub core_class: u8,
    /// De lengte van de sharegroup-naam (0 = eigen cores).
    pub group_len: u8,
    /// Het aantal gepubliceerde poorten achter de env (de `ports` van de
    /// jobspec), hoogstens [`MAX_START_PORTS`].
    pub port_count: u16,
    /// De lengte van de env-blob.
    pub env_len: u32,
    /// De lengte van de volume-blob achter de poorten, hoogstens
    /// [`MAX_MOUNT_BYTES`]; tot alpha.10 gereserveerd en 0.
    pub mounts_len: u32,
}

/// De lengte van [`StartHead`] op de draad.
pub const START_HEAD_LEN: usize = 32;

/// De bytes van één poort achter de env: een `u16`, little-endian.
pub const PORT_LEN: usize = 2;

/// Zoveel poorten mag één start publiceren. Elke poort kost de NAT twee
/// publicaties (tcp en udp, zoals Go) uit een tabel van 512 voor de hele
/// node; zestien per app is ruim voor een dienst en houdt één jobspec uit
/// de buurt van dat plafond.
pub const MAX_START_PORTS: usize = 16;

macro_rules! field {
    ($t:ty, $f:ident, $off:expr) => {
        const _: () = assert!(core::mem::offset_of!($t, $f) == $off);
    };
}

const _: () = assert!(core::mem::size_of::<StartHead>() == START_HEAD_LEN);
field!(StartHead, memory_limit, 0);
field!(StartHead, image_size, 8);
field!(StartHead, cores, 16);
field!(StartHead, pool_cores, 18);
field!(StartHead, core_class, 20);
field!(StartHead, group_len, 21);
field!(StartHead, port_count, 22);
field!(StartHead, env_len, 24);
field!(StartHead, mounts_len, 28);

/// Een [`PrivOp::StartSlot`]-request, met de variabele delen geleend.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct StartReq<'a> {
    /// De zichtbare partitie in bytes.
    pub memory_limit: u64,
    /// De maat van het image in bytes (niet 0).
    pub image_size: u64,
    /// De SMP-cores van de app.
    pub cores: u16,
    /// De poolgrootte (sharegroup).
    pub pool_cores: u16,
    /// De gevraagde klasse.
    pub core_class: CoreClass,
    /// De sharegroup-naam; leeg = eigen cores.
    pub group: &'a [u8],
    /// De env-blob (`key=val\n`), voor de control-page.
    pub env: &'a [u8],
    /// De gepubliceerde poorten zoals ze op de draad staan: [`PORT_LEN`]
    /// bytes per poort, `u16` little-endian ([`StartReq::ports`] leest ze,
    /// [`port_blob`] schrijft ze). De kern zet elke poort van de uplink door
    /// naar dezelfde poort in het slot, tcp en udp.
    pub ports: &'a [u8],
    /// De volumes zoals ze op de draad staan ([`mount_blob`] schrijft ze,
    /// [`Mounts`] leest ze); leeg is het formaat van alpha.10. De kern
    /// normaliseert en toetst de paden: dat is de toegangsgrens.
    pub mounts: &'a [u8],
    /// De jobnaam (de store-naamruimte).
    pub job: &'a [u8],
}

/// Schrijft `ports` in de draadvorm van [`StartReq::ports`] in `dst`; geeft
/// de lengte. Meer dan [`MAX_START_PORTS`] of een poort 0 wordt geweigerd,
/// zoals de kern dat bij het lezen ook doet.
pub fn port_blob(ports: &[u16], dst: &mut [u8]) -> Result<usize> {
    if ports.len() > MAX_START_PORTS {
        return Err(Error::TooMany {
            what: "start ports",
            cap: MAX_START_PORTS,
        });
    }
    if ports.contains(&0) {
        return Err(Error::Missing("port number"));
    }
    let need = ports.len() * PORT_LEN;
    let len = dst.len();
    let out = dst.get_mut(..need).ok_or(short(len, need))?;
    for (d, p) in out.chunks_exact_mut(PORT_LEN).zip(ports) {
        d.copy_from_slice(&p.to_le_bytes());
    }
    Ok(need)
}

/// Een little-endian `u16` op `b[i..]`, of 0 als `b` te kort is.
fn le16(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map_or(0, u16::from_le_bytes)
}

/// Een little-endian `u32` op `b[i..]`, of 0.
fn le32(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map_or(0, u32::from_le_bytes)
}

/// Een little-endian `u64` op `b[i..]`, of 0.
fn le64(b: &[u8], i: usize) -> u64 {
    b.get(i..i + 8)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
        .map_or(0, u64::from_le_bytes)
}

/// Een te kort buffer.
fn short(len: usize, need: usize) -> Error {
    Error::Short { len, need }
}

impl<'a> StartReq<'a> {
    /// Schrijft de hele call-payload (kop, jobnaam, [`StartHead`], groep,
    /// env) in `dst`; geeft de lengte.
    pub fn encode(&self, dst: &mut [u8], seq: u32) -> Result<usize> {
        let group_len = u8::try_from(self.group.len()).map_err(|_| Error::PayloadTooLarge {
            len: self.group.len(),
            max: u8::MAX as usize,
        })?;
        let env_len = u32::try_from(self.env.len()).map_err(|_| Error::PayloadTooLarge {
            len: self.env.len(),
            max: u32::MAX as usize,
        })?;
        let port_count = port_count(self.ports)?;
        mounts::validate(self.mounts)?;
        let req = crate::hopabi::Req {
            op: PrivOp::StartSlot.op(),
            seq,
            path: self.job,
            ..Default::default()
        };
        let at = crate::hopabi::encode_req(dst, &req)?;
        let need = at
            + START_HEAD_LEN
            + self.group.len()
            + self.env.len()
            + self.ports.len()
            + self.mounts.len();
        let len = dst.len();
        let out = dst.get_mut(at..need).ok_or(short(len, need))?;
        let (head, rest) = out.split_at_mut(START_HEAD_LEN);
        head[0..8].copy_from_slice(&self.memory_limit.to_le_bytes());
        head[8..16].copy_from_slice(&self.image_size.to_le_bytes());
        head[16..18].copy_from_slice(&self.cores.to_le_bytes());
        head[18..20].copy_from_slice(&self.pool_cores.to_le_bytes());
        head[20] = self.core_class as u8;
        head[21] = group_len;
        head[22..24].copy_from_slice(&port_count.to_le_bytes());
        head[24..28].copy_from_slice(&env_len.to_le_bytes());
        // Past: `validate` begrensde de blob op MAX_MOUNT_BYTES (< u32::MAX).
        head[28..32].copy_from_slice(&(self.mounts.len() as u32).to_le_bytes());
        let (group, rest) = rest.split_at_mut(self.group.len());
        let (env, rest) = rest.split_at_mut(self.env.len());
        let (ports, mounts) = rest.split_at_mut(self.ports.len());
        group.copy_from_slice(self.group);
        env.copy_from_slice(self.env);
        ports.copy_from_slice(self.ports);
        mounts.copy_from_slice(self.mounts);
        Ok(need)
    }

    /// Leest een start uit een gedecodeerde request; toetst op, maten en
    /// klasse.
    pub fn decode(r: &crate::hopabi::Req<'a>) -> Result<StartReq<'a>> {
        if r.op != PrivOp::StartSlot.op() {
            return Err(Error::BadKind(r.op));
        }
        let d = r.data;
        let head = d
            .get(..START_HEAD_LEN)
            .ok_or(short(d.len(), START_HEAD_LEN))?;
        let group_len = usize::from(head[21]);
        let ports_len = usize::from(le16(head, 22)).saturating_mul(PORT_LEN);
        let env_len = le32(head, 24) as usize;
        let mounts_len = le32(head, 28) as usize;
        let need = START_HEAD_LEN
            .saturating_add(group_len)
            .saturating_add(env_len)
            .saturating_add(ports_len)
            .saturating_add(mounts_len);
        if d.len() != need {
            return Err(short(d.len(), need));
        }
        let (group, rest) = d[START_HEAD_LEN..].split_at(group_len);
        let (env, rest) = rest.split_at(env_len);
        let (ports, mounts) = rest.split_at(ports_len);
        port_count(ports)?;
        mounts::validate(mounts)?;
        Ok(StartReq {
            memory_limit: le64(head, 0),
            image_size: le64(head, 8),
            cores: le16(head, 16),
            pool_cores: le16(head, 18),
            core_class: CoreClass::from_raw(head[20])?,
            group,
            env,
            ports,
            mounts,
            job: r.path,
        })
    }

    /// De gepubliceerde poorten, in de volgorde van de draad.
    pub fn ports(&self) -> impl Iterator<Item = u16> + 'a {
        self.ports
            .chunks_exact(PORT_LEN)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
    }
}

/// Het aantal poorten in een draadblob, na de toetsen: een hele poort per
/// [`PORT_LEN`] bytes, hoogstens [`MAX_START_PORTS`], geen poort 0.
fn port_count(ports: &[u8]) -> Result<u16> {
    if !ports.len().is_multiple_of(PORT_LEN) {
        return Err(short(ports.len(), ports.len() + 1));
    }
    let n = ports.len() / PORT_LEN;
    if n > MAX_START_PORTS {
        return Err(Error::TooMany {
            what: "start ports",
            cap: MAX_START_PORTS,
        });
    }
    if ports.chunks_exact(PORT_LEN).any(|p| p == [0, 0]) {
        return Err(Error::Missing("port number"));
    }
    // Past: MAX_START_PORTS is ver onder u16::MAX (de assertie hieronder).
    Ok(n as u16)
}

const _: () = assert!(MAX_START_PORTS <= u16::MAX as usize);

/// Een [`PrivOp::StreamImage`]-request.
#[must_use]
pub fn stream_req(seq: u32, slot: u64, offset: u64, chunk: &[u8]) -> crate::hopabi::Req<'_> {
    crate::hopabi::Req {
        op: PrivOp::StreamImage.op(),
        seq,
        off: slot,
        n: offset,
        path: &[],
        data: chunk,
    }
}

/// Een request met alleen getallen: stop, status, log, klok.
#[must_use]
pub fn plain_req(op: PrivOp, seq: u32, slot: u64, n: u64) -> crate::hopabi::Req<'static> {
    crate::hopabi::Req {
        op: op.op(),
        seq,
        off: slot,
        n,
        path: &[],
        data: &[],
    }
}

/// Waar een stroom staat na een brok.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum StreamState {
    /// Er mogen meer bytes komen.
    More = 0,
    /// De laatste byte is binnen; het image is geplaatst en de app gestart.
    Placed = 1,
    /// De laatste byte is binnen, maar plaatsen of starten faalde; de reden
    /// staat erachter. De kern heeft zijn reserveringen opgeruimd (of, bij
    /// een onbekende dispatch-uitkomst, het slot in quarantaine gezet).
    Failed = 2,
}

/// Het antwoord op een [`PrivOp::StreamImage`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct StreamResp<'a> {
    /// Hoeveel bytes de kern nu heeft.
    pub received: u64,
    /// De stand.
    pub state: StreamState,
    /// De reden bij [`StreamState::Failed`]; anders leeg.
    pub why: &'a [u8],
}

impl<'a> StreamResp<'a> {
    /// Schrijft de data van het antwoord (stand plus reden) in `dst`; geeft
    /// de lengte. `received` gaat in `Resp.size`.
    pub fn encode_data(&self, dst: &mut [u8]) -> Result<usize> {
        let need = 1 + self.why.len();
        let len = dst.len();
        let out = dst.get_mut(..need).ok_or(short(len, need))?;
        out[0] = self.state as u8;
        out[1..].copy_from_slice(self.why);
        Ok(need)
    }

    /// Leest het antwoord uit een gedecodeerde response met status OK.
    pub fn decode(r: &crate::hopabi::Resp<'a>) -> Result<StreamResp<'a>> {
        let (&state, why) = r.data.split_first().ok_or(short(0, 1))?;
        let state = match state {
            0 => StreamState::More,
            1 => StreamState::Placed,
            2 => StreamState::Failed,
            v => return Err(Error::BadKind(v)),
        };
        Ok(StreamResp {
            received: r.size,
            state,
            why,
        })
    }
}

/// De toestand van een slot in het grootboek van de kern.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum SlotState {
    /// Geen eigenaar.
    #[default]
    Empty = 0,
    /// Gereserveerd; het image stroomt.
    Streaming = 1,
    /// Gedispatcht.
    Running = 2,
    /// Beëindiging onbevestigd; het slot wordt niet hergebruikt.
    Quarantined = 3,
}

/// Het antwoord op een [`PrivOp::SlotStatus`], in `data` (little-endian).
///
/// Slot 0 is de kern: `Running`, `core_on` 1, de OS-core met span 1, `app`
/// [`crate::hopabi::AppStatus::Ready`], `heartbeat` het tiknummer,
/// `ram_size` de kern-RAM, `mem_sys` de heap in gebruik, `idle_ns` en
/// `wakes` de slaap van zijn executor (cumulatief, op dezelfde klok als
/// `at_ns`), `cores` 1, de rest 0. Een lezer rekent er cpu en geheugen van
/// de kern mee uit zoals van elke app. Elke andere bevoegde op weigert
/// slot 0.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct SlotInfo {
    /// De [`SlotState`] als getal.
    pub state: u8,
    /// 1 als de primaire core van het slot draait.
    pub core_on: u8,
    /// Gereserveerd, 0.
    pub reserved: u16,
    /// De primaire core van het slot, het logische nummer van de kern (0 is
    /// de OS-core; zonder core is `span` 0).
    pub core: u16,
    /// Het aantal cores van het slot.
    pub span: u16,
    /// De app-status van de control-page ([`crate::hopabi::AppStatus`]).
    pub app: u64,
    /// De exitcode.
    pub exit_code: u64,
    /// De heartbeat-teller van de app.
    pub heartbeat: u64,
    /// De RAM-maat die de app zelf meldt.
    pub ram_size: u64,
    /// Vector + 1 van een fault (0 = geen).
    pub fault_vec: u64,
    /// ESR van die fault.
    pub fault_esr: u64,
    /// FAR van die fault.
    pub fault_far: u64,
    /// De partitiemaat in bytes (0 = geen partitie).
    pub partition: u64,
    /// Tijdens een stroom: de ontvangen bytes.
    pub received: u64,
    /// Tijdens een stroom: de aangekondigde image-maat.
    pub image_size: u64,
    /// Het geheugen dat de app zelf in gebruik meldt (`CTRL_MEM_SYS`).
    pub mem_sys: u64,
    /// De meetlat (docs/apps.md), rauw: idle-nanoseconden van de app (alle
    /// cores bij elkaar), wekken, cores, en de kernklok bij het lezen. Hop
    /// vergelijkt twee standen voor cpu-procent en wek-tempo.
    pub idle_ns: u64,
    /// Het aantal wekken van de slaper van de app.
    pub wakes: u64,
    /// De cores van de app, zoals hij ze zelf telt.
    pub cores: u64,
    /// De klok van de kern (ns) bij het lezen van deze stand.
    pub at_ns: u64,
}

/// De lengte van [`SlotInfo`] op de draad.
pub const SLOT_INFO_LEN: usize = 128;
/// De lengte tot en met Hop v3.0.3 (vóór d827e5b, 02-10: tot en met
/// `image_size`); de velden erna zijn het gebruik (`mem_sys`, `idle_ns`,
/// `wakes`, `cores`, `at_ns`). Een lezer die geen maat vraagt krijgt dit
/// voorvoegsel, zodat een nieuwe kern een oudere Hop niet breekt (de Hop
/// van de kaart overleeft een kern-flip; gemeten 03-10: zijn client
/// verbreekt de verbinding op een langer antwoord en elke nieuwe
/// plaatsing kwam in een herstartlus).
pub const SLOT_INFO_LEN_V1: usize = 88;
const _: () = assert!(SLOT_INFO_LEN_V1 == 88 && SLOT_INFO_LEN_V1 < SLOT_INFO_LEN);

const _: () = assert!(core::mem::size_of::<SlotInfo>() == SLOT_INFO_LEN);
field!(SlotInfo, state, 0);
field!(SlotInfo, core_on, 1);
field!(SlotInfo, reserved, 2);
field!(SlotInfo, core, 4);
field!(SlotInfo, span, 6);
field!(SlotInfo, app, 8);
field!(SlotInfo, exit_code, 16);
field!(SlotInfo, heartbeat, 24);
field!(SlotInfo, ram_size, 32);
field!(SlotInfo, fault_vec, 40);
field!(SlotInfo, fault_esr, 48);
field!(SlotInfo, fault_far, 56);
field!(SlotInfo, partition, 64);
field!(SlotInfo, received, 72);
field!(SlotInfo, image_size, 80);

impl SlotInfo {
    /// De bytes op de draad.
    #[must_use]
    pub fn encode(&self) -> [u8; SLOT_INFO_LEN] {
        let mut b = [0u8; SLOT_INFO_LEN];
        b[0] = self.state;
        b[1] = self.core_on;
        b[2..4].copy_from_slice(&self.reserved.to_le_bytes());
        b[4..6].copy_from_slice(&self.core.to_le_bytes());
        b[6..8].copy_from_slice(&self.span.to_le_bytes());
        let words = [
            self.app,
            self.exit_code,
            self.heartbeat,
            self.ram_size,
            self.fault_vec,
            self.fault_esr,
            self.fault_far,
            self.partition,
            self.received,
            self.image_size,
            self.mem_sys,
            self.idle_ns,
            self.wakes,
            self.cores,
            self.at_ns,
        ];
        for (w, o) in words.iter().zip(b[8..].chunks_exact_mut(8)) {
            o.copy_from_slice(&w.to_le_bytes());
        }
        b
    }

    /// Leest de bytes van de draad.
    pub fn decode(b: &[u8]) -> Result<SlotInfo> {
        if b.len() < SLOT_INFO_LEN {
            return Err(short(b.len(), SLOT_INFO_LEN));
        }
        Ok(SlotInfo {
            state: b[0],
            core_on: b[1],
            reserved: le16(b, 2),
            core: le16(b, 4),
            span: le16(b, 6),
            app: le64(b, 8),
            exit_code: le64(b, 16),
            heartbeat: le64(b, 24),
            ram_size: le64(b, 32),
            fault_vec: le64(b, 40),
            fault_esr: le64(b, 48),
            fault_far: le64(b, 56),
            partition: le64(b, 64),
            received: le64(b, 72),
            image_size: le64(b, 80),
            mem_sys: le64(b, 88),
            idle_ns: le64(b, 96),
            wakes: le64(b, 104),
            cores: le64(b, 112),
            at_ns: le64(b, 120),
        })
    }

    /// De [`SlotState`], of `None` voor een onbekende waarde.
    #[must_use]
    pub const fn slot_state(&self) -> Option<SlotState> {
        Some(match self.state {
            0 => SlotState::Empty,
            1 => SlotState::Streaming,
            2 => SlotState::Running,
            3 => SlotState::Quarantined,
            _ => return None,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_en_fragmentatie() {
        let p = vec![0xa5u8; MAX_IO_CHUNK];
        let mut wire = encode_header(Kind::Call, p.len()).unwrap().to_vec();
        wire.extend_from_slice(&p);
        // Eén byte per keer, zoals de oneByteReader van de Go-test.
        let mut r = HeaderReader::new();
        let mut pos = 0;
        while !r.is_complete() {
            pos += r.fill(&wire[pos..pos + 1]);
        }
        let h = r.take().unwrap().unwrap();
        assert_eq!(
            h,
            Header {
                kind: Kind::Call,
                len: MAX_IO_CHUNK
            }
        );
        assert_eq!(&wire[pos..pos + h.len], &p[..]);
        assert!(
            !r.is_complete(),
            "lezer staat klaar voor het volgende frame"
        );
    }

    #[test]
    fn rejects_oversize_before_write() {
        assert!(matches!(
            encode_header(Kind::Call, MAX_PAYLOAD + 1),
            Err(Error::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn kop_weigert_magie_versie_soort_en_maat() {
        let good = encode_header(Kind::Log, 5).unwrap();
        let mut bad = good;
        bad[0] ^= 1;
        assert!(matches!(decode_header(&bad), Err(Error::BadMagic(_))));
        let mut bad = good;
        bad[4] = 2;
        assert!(matches!(decode_header(&bad), Err(Error::BadVersion { .. })));
        let mut bad = good;
        bad[5] = 9;
        assert_eq!(decode_header(&bad), Err(Error::BadKind(9)));
        let mut bad = good;
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_header(&bad),
            Err(Error::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn bevoegde_ops_botsen_niet() {
        let mut seen = 0;
        for op in 0..=u8::MAX {
            if let Some(p) = PrivOp::from_op(op) {
                assert_eq!(p.op(), op);
                assert!(op > crate::hopabi::OP_MAX);
                assert!((PRIV_OP_FIRST..=PRIV_OP_LAST).contains(&op));
                seen += 1;
            }
        }
        assert_eq!(seen, PrivOp::ALL.len());
        for (i, p) in PrivOp::ALL.iter().enumerate() {
            assert_eq!(usize::from(p.op() - PRIV_OP_FIRST), i, "{p:?}");
        }
    }

    #[test]
    fn start_roundtrip_over_de_call_payload() {
        let s = StartReq {
            memory_limit: 64 << 20,
            image_size: 3 << 20,
            cores: 2,
            pool_cores: 4,
            core_class: CoreClass::Big,
            group: b"web",
            env: b"A=1\nB=2\n",
            ports: &[80, 0, 0x90, 0x1f],
            job: b"demo",
            mounts: &[],
        };
        let mut buf = [0u8; 256];
        let n = s.encode(&mut buf, 7).unwrap();
        let req = crate::hopabi::decode_req(&buf[..n]).unwrap();
        assert_eq!((req.op, req.seq), (PrivOp::StartSlot.op(), 7));
        assert_eq!(StartReq::decode(&req).unwrap(), s);
        // Een afgekapte payload of een onbekende klasse wordt geweigerd.
        let short = crate::hopabi::decode_req(&buf[..n - 1]).unwrap();
        assert!(matches!(StartReq::decode(&short), Err(Error::Short { .. })));
        let at = crate::hopabi::HDR_LEN + 4 + 20;
        buf[at] = 9;
        let bad = crate::hopabi::decode_req(&buf[..n]).unwrap();
        assert_eq!(StartReq::decode(&bad), Err(Error::BadKind(9)));
        let mut tiny = [0u8; 40];
        assert!(s.encode(&mut tiny, 1).is_err());
    }

    #[test]
    fn start_draagt_de_poorten_achter_de_env() {
        let mut blob = [0u8; MAX_START_PORTS * PORT_LEN + 2];
        let n = port_blob(&[80, 8081], &mut blob).unwrap();
        let s = StartReq {
            memory_limit: 32 << 20,
            image_size: 1 << 20,
            cores: 1,
            env: b"ER_PORT_HTTP=80\n",
            ports: &blob[..n],
            job: b"welcome",
            ..Default::default()
        };
        let mut buf = [0u8; 256];
        let len = s.encode(&mut buf, 1).unwrap();
        let req = crate::hopabi::decode_req(&buf[..len]).unwrap();
        let back = StartReq::decode(&req).unwrap();
        assert_eq!(back.ports().collect::<Vec<_>>(), [80, 8081]);
        assert_eq!(back.env, b"ER_PORT_HTTP=80\n");
        // Het aantal staat op de oude plek van `reserved`, offset 22 van de kop.
        let head = crate::hopabi::HDR_LEN + s.job.len();
        assert_eq!(&buf[head + 22..head + 24], &[2, 0]);

        // Een start zonder poorten is byte voor byte die van alpha.7.
        let old = StartReq { ports: &[], ..s };
        let len = old.encode(&mut buf, 1).unwrap();
        assert_eq!(&buf[head + 22..head + 24], &[0, 0]);
        let req = crate::hopabi::decode_req(&buf[..len]).unwrap();
        assert_eq!(StartReq::decode(&req).unwrap().ports().count(), 0);

        // Poort 0, te veel poorten en een halve poort worden geweigerd.
        let mut spare = [0u8; MAX_START_PORTS * PORT_LEN + 2];
        assert!(port_blob(&[80, 0], &mut spare).is_err());
        assert!(port_blob(&[1; MAX_START_PORTS + 1], &mut spare).is_err());
        assert!(port_blob(&[80], &mut [0u8; 1]).is_err());
        let zero = StartReq {
            ports: &[0, 0],
            ..s
        };
        let len = zero.encode(&mut buf, 1);
        assert!(len.is_err());
        let half = StartReq { ports: &[80], ..s };
        assert!(half.encode(&mut buf, 1).is_err());
    }

    #[test]
    fn stream_antwoord_roundtrip() {
        for (state, why) in [
            (StreamState::More, &b""[..]),
            (StreamState::Placed, b""),
            (StreamState::Failed, b"no PT_LOAD segments"),
        ] {
            let r = StreamResp {
                received: 42,
                state,
                why,
            };
            let mut d = [0u8; 64];
            let n = r.encode_data(&mut d).unwrap();
            let resp = crate::hopabi::Resp {
                op: PrivOp::StreamImage.op(),
                size: 42,
                data: &d[..n],
                ..Default::default()
            };
            assert_eq!(StreamResp::decode(&resp).unwrap(), r);
        }
        let empty = crate::hopabi::Resp::default();
        assert!(StreamResp::decode(&empty).is_err());
        let req = stream_req(3, 5, 1024, b"abc");
        assert_eq!((req.off, req.n, req.data), (5, 1024, &b"abc"[..]));
    }

    #[test]
    fn slot_info_roundtrip() {
        let i = SlotInfo {
            state: SlotState::Running as u8,
            core_on: 1,
            reserved: 0,
            core: 3,
            span: 2,
            app: 2,
            exit_code: 0,
            heartbeat: 99,
            ram_size: 62 << 20,
            fault_vec: 0,
            fault_esr: 0,
            fault_far: 0,
            partition: 64 << 20,
            received: 0,
            image_size: 0,
            mem_sys: 4096,
            idle_ns: 7,
            wakes: 8,
            cores: 1,
            at_ns: 9,
        };
        let b = i.encode();
        assert_eq!(SlotInfo::decode(&b).unwrap(), i);
        assert_eq!(i.slot_state(), Some(SlotState::Running));
        assert!(SlotInfo::decode(&b[..SLOT_INFO_LEN - 1]).is_err());
    }
}
