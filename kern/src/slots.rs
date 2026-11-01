//! De slot-lifecycle: één actor die start, stop, status, SMP en adoptie
//! serialiseert, plus per slot een servicer-taak.
//!
//! In Go was dit `lifecycleMu` (één Start/Stop tegelijk), `svcMu` (de
//! servicer-tabel), `partMu`/`poolMu` (partities en cores) en `diagMu`. Hier
//! is serialisatie de vorm: de [`Lifecycle`]-actor is één taak met een
//! [`Mailbox`], en hij bezit de partitie-pool, de core-plaatsing en de
//! bewoners als gewone `&mut self`. De servicer-tabel is een
//! [`LocalCell`] die alleen de actor schrijft en de system-listener kort
//! leest (handboek §1.1).
//!
//! LEES VOORDAT JE HIER MACHINERIE BIJBOUWT: `OLD/docs/v1/
//! slot-lifecycle-grenzen.md`. Hop serialiseert de opdrachten al, en dit
//! ontwerp reconcilieert geen in-memory staat: het faalt luid en de watchdog
//! reset de node.
//!
//! # Een start in twee stappen
//!
//! De download van een image duurt minuten en mag de actor niet vasthouden.
//! Daarom (zoals `startStream` in Go, dat het venster twee keer kort pakte):
//!
//! 1. [`Request::Claim`]: plaatsen, partitie reserveren, wissen (E1, E3).
//!    Het antwoord is een [`ImageGrant`]: het recht om in die ene partitie te
//!    schrijven. Grote data loopt niet door de kern; de grant wel.
//! 2. [`Request::Arm`]: de grant komt terug, de kooi wordt gebouwd (E5) en
//!    gedispatcht. Faalt het startschot, dan is de uitkomst onbekend: de
//!    kern kan de isolatie niet meer bevestigen, en dat is `fatal`
//!    (docs/sequence-hopos.md §13.5; tot 01-11 een quarantaine).
//!
//! [`Request::Abort`] geeft een grant terug waarin nooit iets draaide: de
//! gewone rollback.
//!
//! # De device-grants
//!
//! De actor bezit ook de grant-aanbieder ([`Grants`], kaal [`NoGrants`]) en
//! roept hem op de vier plekken uit `kern::grants`: [`Request::Env`] tussen
//! de claim en de env op de control-page, `arm` na de kooibouw en vóór de
//! dispatch, `adopt` bij de adoptie na een flip, en `release` na een
//! bevestigde stop en bij elke abort. Eén eigenaar voor partitie én glas:
//! wie de grant vrijgeeft, weet dat de houder niet meer draait.
//!
//! # De device-reservering
//!
//! Een apparaat dat met DMA in gewoon DRAM werkt (de videocodec van de O6N:
//! page tables, firmware, referentieframes) krijgt een blok BUITEN de
//! partities: [`Request::ReserveDevice`] (Go: `slots.ReserveDevice`). Ook
//! dat is een bericht aan de eigenaar van de pool, geen tweede pool: de
//! kern vraagt het één keer bij boot (`hopos::codec`), het blok telt af van
//! de capaciteit, en de regel `HOPOS_POOL_DEVICE` zegt waar het ligt. Komt
//! het ijzer niet op, dan gaat het terug ([`Request::ReleaseDevice`]).

use crate::cage::{Cage, Console, Cores, PortError, Power, Status, Timer};
use crate::grants::{Grants, NoGrants};
use crate::kernflip::{MAX_FLIP_JOB, MAX_FLIP_MOUNTS, MAX_FLIP_PATH, MAX_FLIP_PORTS};
use crate::partmem::{Owned, Partition, PartitionPool, Stopped};
use crate::pool::{GroupName, Placement, Places};
use crate::{Core, Error, GRAIN, Region, Result, Slot, SlotVec};
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::time::Duration;
use sync::mpsc::Mailbox;
use sync::{Either, LocalCell, Oneshot, Signal, oneshot, select, yield_now};

/// De brokmaat van de scrub. Een ononderbroken veeg over een partitie
/// verhongert de netstack (96 MB x 127 loaders is ongeveer 12 s, gemeten in
/// de Go-kern); tussen twee brokken yieldt de actor.
pub const SCRUB_CHUNK: u64 = 4 << 20;
/// Het wachtpatroon van stop: kijk elke 10 ms.
pub const STOP_POLL: Duration = Duration::from_millis(10);
/// Hoe lang een ingetrokken slot krijgt om te sterven. De intrekking raakt
/// een gesavede bewoner pas bij zijn volgende hervatting (een paar
/// yield-tikken).
pub const REVOKE_GRACE: Duration = Duration::from_secs(1);
/// Hoe lang een nieuwe bewoner van een gedeelde app-core op zijn eerste
/// beurt wacht voordat de vasthouder geofferd wordt, en daarna nog eens
/// (share.go `bootPendingDispatch`, 2 s).
pub const RECLAIM_WAIT: Duration = Duration::from_secs(2);
/// De start-gratie van een servicer: bij de start is de ctx heel even nog
/// leeg. Zonder gratie stierf de servicer meteen en verdronk niemand meer de
/// logs van de echte app (gemeten 30-07: de stervensreden van welcome bleef
/// in de ring staan). Eén termijn, geen tik: daarna wacht hij alleen op zijn
/// bel ([`Servicers::ring`]) en zijn stop.
pub const IDLE_GRACE: Duration = Duration::from_secs(2);
/// Hoeveel records de servicer achter elkaar leest voordat hij de executor
/// een ronde geeft. Les van 30-09 (de soak): een app die zijn outbox volschrijft,
/// hield de servicer in één poll tot de ring leeg was, en daarmee de hele
/// OS-core (de tik, Hop, de switch). Zestien logregels is een burst; meer
/// wacht een ronde.
pub const SERVICER_BATCH: usize = 16;
/// De ringsoort van een logregel (`ring.TypeLog`, [`abi::ring::Kind::LOG`]);
/// niet de framesoort van de system-API ([`abi::systemapi::Kind::Log`], 3).
pub const RING_LOG: u8 = 1;
const _: () = assert!(RING_LOG as u32 == abi::ring::Kind::LOG.raw());

/// Pusht met een faalbare reservering (handboek §6: `Vec::push` breekt bij
/// OOM het programma af).
pub(crate) fn try_push<T>(v: &mut Vec<T>, x: T) -> Result {
    v.try_reserve(1).map_err(|_| Error::OutOfMemory {
        bytes: core::mem::size_of::<T>(),
    })?;
    v.push(x);
    Ok(())
}

/// Kopieert een slice naar een verse `Vec`, faalbaar.
pub(crate) fn try_vec<T: Clone>(s: &[T]) -> Result<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(s.len())
        .map_err(|_| Error::OutOfMemory {
            bytes: core::mem::size_of_val(s),
        })?;
    v.extend_from_slice(s);
    Ok(v)
}

/// Een lijst poorten voor een logregel: `:80 :8081`, zonder allocatie.
struct PortList<'a>(&'a [u16]);

impl core::fmt::Display for PortList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, ":{p}")?;
        }
        Ok(())
    }
}

/// De volumes van een start als één regel: `/data -> /volumes/demo`, met
/// een komma ertussen; een pad dat geen UTF-8 is, staat er als `?`.
struct MountList<'a>(&'a [Mount]);

impl core::fmt::Display for MountList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, m) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            let local = core::str::from_utf8(&m.local).unwrap_or("?");
            let shared = core::str::from_utf8(&m.shared).unwrap_or("?");
            write!(f, "{local} -> {shared}")?;
        }
        Ok(())
    }
}

/// Eén volume: `{local, shared}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mount {
    /// Het pad zoals de app het ziet.
    pub local: Vec<u8>,
    /// Het gedeelde pad in hopfs.
    pub shared: Vec<u8>,
}

/// Wat een start vraagt.
#[derive(Clone, Debug)]
pub struct StartSpec {
    /// Het slot, of `None`: de actor kiest het laagste vrije, zoals een fd
    /// (geen maximum, alleen "past het").
    pub slot: Option<Slot>,
    /// Een slot dat de actor bij `slot: None` overslaat: dat van Hop, dat
    /// tijdens zijn herstart even leeg staat.
    pub avoid: Option<Slot>,
    /// De zichtbare partitie in bytes (`memory_limit`, inclusief ABI-staart).
    pub mem_limit: u64,
    /// De core-vraag.
    pub placement: Placement,
    /// De job-naam (de store-naamruimte; leeg = geen).
    pub job: Vec<u8>,
    /// De volumes.
    pub mounts: Vec<Mount>,
    /// De gepubliceerde poorten.
    pub ports: Vec<u16>,
}

impl StartSpec {
    /// Een spec voor `slot`, zonder job, volumes of poorten.
    #[must_use]
    pub fn new(slot: Slot, mem_limit: u64, placement: Placement) -> StartSpec {
        StartSpec {
            slot: Some(slot),
            ..StartSpec::free(mem_limit, placement, None)
        }
    }

    /// Een spec waarvoor de actor het slot kiest: het laagste vrije, behalve
    /// `avoid`.
    #[must_use]
    pub fn free(mem_limit: u64, placement: Placement, avoid: Option<Slot>) -> StartSpec {
        StartSpec {
            slot: None,
            avoid,
            mem_limit,
            placement,
            job: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
        }
    }
}

/// Het recht om de image van één start in zijn partitie te schrijven.
///
/// Uitgegeven door [`Request::Claim`], terug via [`Request::Arm`] of
/// [`Request::Abort`]. Zolang de grant buiten is, draait er niets in de
/// partitie (hij draagt een `Partition<Free>`).
#[must_use = "een grant die niet terugkomt houdt de partitie geclaimd"]
#[derive(Debug)]
pub struct ImageGrant {
    part: Partition<crate::partmem::Free>,
    generation: u32,
}

impl ImageGrant {
    /// Het slot.
    #[must_use]
    pub fn slot(&self) -> Slot {
        self.part.slot()
    }

    /// De partitie.
    #[must_use]
    pub fn region(&self) -> Region {
        self.part.region()
    }

    /// Schrijft `bytes` op offset `off` in de partitie; buiten de partitie
    /// is een fout, nooit een schrijf.
    pub fn write(&mut self, mem: &mut impl crate::cage::PhysMem, off: u64, bytes: &[u8]) -> Result {
        let r = self.region();
        let len = bytes.len() as u64;
        match off.checked_add(len) {
            Some(end) if end <= r.size => {
                mem.copy_in(r.base + off, bytes);
                Ok(())
            }
            _ => Err(Error::Range {
                base: r.base.wrapping_add(off),
                size: len,
            }),
        }
    }
}

/// Een verzoek aan de lifecycle-actor.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "een claim draagt zijn spec door de brievenbus; een Box zou een onfaalbare allocatie zijn"
)]
pub enum Request {
    /// Plaats, reserveer en wis; het antwoord is een [`ImageGrant`].
    Claim(StartSpec),
    /// Bouw de kooi en dispatch; `entry` is het startadres van de image.
    Arm {
        /// De grant uit de claim.
        grant: ImageGrant,
        /// Het startadres.
        entry: u64,
    },
    /// Geef een grant terug waarin nooit iets draaide.
    Abort(ImageGrant),
    /// De env van een lopende start (`key=val\n`, zoals hij op de
    /// control-page gaat): de grant-aanbieder mag regels toevoegen. Alleen
    /// voor een slot in de stroom, tussen de claim en [`Request::Arm`]; het
    /// antwoord is [`Response::Env`] met de complete blob.
    Env {
        /// Het slot van de stroom.
        slot: Slot,
        /// De env zoals de start hem vroeg.
        env: Vec<u8>,
    },
    /// Stop een slot; `timeout` is de coöperatieve kans.
    Stop {
        /// Het slot.
        slot: Slot,
        /// Hoe lang de app krijgt om zelf te stoppen.
        timeout: Duration,
    },
    /// De status van een slot.
    Status(Slot),
    /// De bezette slots (een bewoner, of een stroom) boven `after`,
    /// oplopend en hoogstens `max`; het antwoord is [`Response::Occupied`].
    Occupied {
        /// Het slot waarna de lijst begint (0 = vanaf het begin).
        after: usize,
        /// Hoeveel er hoogstens terugkomen.
        max: usize,
    },
    /// De servicer zag een SMP-verzoek (vuur-en-vergeet).
    Smp(Slot),
    /// Beschrijf elke levende bewoner voor de kern-flip
    /// ([`Lifecycle::snapshot`]); het antwoord is [`Response::Snapshot`].
    Snapshot,
    /// Een blok van `size` bytes buiten de partitie-pool voor een apparaat
    /// met DMA (de arena van de videocodec); het antwoord is
    /// [`Response::Device`]. Alleen de kern stuurt dit, bij boot.
    ReserveDevice {
        /// De maat in bytes; naar boven op de korrel.
        size: u64,
        /// Voor wie, voor de ene regel `HOPOS_POOL_DEVICE`.
        what: &'static str,
    },
    /// Geef een blok van [`Request::ReserveDevice`] terug (het ijzer kwam
    /// niet op).
    ReleaseDevice {
        /// Het blok zoals het werd gegeven.
        region: Region,
        /// Voor wie.
        what: &'static str,
    },
}

/// Het antwoord van de actor.
#[derive(Debug)]
pub enum Response {
    /// De claim lukte.
    Granted(ImageGrant),
    /// Klaar.
    Done,
    /// De status.
    Status(SlotStatus),
    /// De slots van [`Request::Occupied`].
    Occupied(Vec<Slot>),
    /// De bewoners voor het handoff-blob van de kern-flip.
    Snapshot(Vec<SlotState>),
    /// De env van de start, met wat de grant-aanbieder erbij zette.
    Env(Vec<u8>),
    /// Het blok van een [`Request::ReserveDevice`].
    Device(Region),
    /// Het lukte niet.
    Failed(Error),
}

/// De toestand van een slot in het grootboek.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// Geen eigenaar.
    Empty,
    /// Geclaimd; de image stroomt (de grant is buiten).
    Streaming,
    /// Gedispatcht.
    Running,
}

/// Wat HOP over een slot ziet.
#[derive(Copy, Clone, Debug)]
pub struct SlotStatus {
    /// De toestand in het grootboek.
    pub occupancy: Occupancy,
    /// De primaire core en de span.
    pub core: Option<(Core, usize)>,
    /// De partitie.
    pub partition: Option<Region>,
    /// Wat de kooi meldt.
    pub cage: Status,
}

/// De antwoordplekken van één aanroeper. Een aanroeper uit een vaste pool
/// (een system-verbinding, de boot-code) heeft er één.
///
/// Twee plaatsen, één per actor: [`Response`] van de lifecycle en
/// [`FsDone`](crate::rpc::FsDone) van de hopfs-actor ([`crate::rpc`]) en de
/// apparaat-actor ([`crate::deviceabi`]). Een aanroeper doet één verzoek
/// tegelijk.
pub struct Reply {
    val: Oneshot<Response>,
    pub(crate) fs: Oneshot<crate::rpc::FsDone>,
}

impl Reply {
    /// Een lege antwoordplek.
    #[must_use]
    pub const fn new() -> Reply {
        Reply {
            val: Oneshot::new(),
            fs: Oneshot::new(),
        }
    }
}

impl Default for Reply {
    fn default() -> Self {
        Reply::new()
    }
}

/// Een verzoek met zijn antwoordplek.
pub struct Envelope<'a> {
    req: Request,
    reply: Option<&'a Reply>,
}

/// Stuurt `req` naar de actor en wacht op het antwoord.
pub async fn call<'a, const N: usize>(
    inbox: &Mailbox<Envelope<'a>, N>,
    reply: &'a Reply,
    req: Request,
) -> Result<Response> {
    let env = Envelope {
        req,
        reply: Some(reply),
    };
    oneshot::call(inbox, &reply.val, env)
        .await
        .map_err(|_| Error::Busy)
}

/// Eén levensduur van een slot, van de registratie tot de evict: de
/// besturing van zijn servicer-taak en wat de system-API en de hopfs-actor
/// erover lezen. De lifecycle-actor maakt hem bij de plaatsing
/// ([`Servicers::open`]) en haalt hem bij de stop uit de tabel
/// ([`Servicers::close`]); hij is weg als de laatste houder hem loslaat (de
/// servicer, een open system-verbinding). Geen tabel van een vast aantal:
/// een slot bestaat van de plaatsing tot de stop.
///
/// `stop` is een [`Signal`]: de servicer is de enige wachter.
pub struct ServicerCtl {
    life: Life,
    stop: Signal,
    gone: Signal,
    /// De bel van de servicer: werk in de outbox, een SMP-verzoek of een
    /// gevallen context, gezien bij de kick van de app-core of na een beurt
    /// op de OS-core ([`Servicers::ring`], docs/sequence-hopos.md §13.3).
    bell: Signal,
    /// De open system-verbindingen van deze levensduur. Die van een vorige
    /// levensduur tellen niet: zij houden hun eigen besturing vast. Tot
    /// 24-10 deelde het slot één teller over zijn levensduren, en een nieuwe
    /// Hop die binnen milliseconden na zijn start belde (de ORION, na POST
    /// /restart), vond de plaatsen van de oude nog bezet: zijn kloktaak
    /// werd geweigerd, en zonder klok weigerde zijn TLS elke download.
    conns: Cell<u32>,
    part: Region,
    /// De volume-tabel (langste `local` eerst); `None` is "geen zicht", en
    /// dan weigert elke bestandscall.
    mounts: Option<Vec<Mount>>,
    /// De jobnaam (de naamruimte van de store-ops); `None` is geen job, en
    /// dan weigert elke store-op.
    job: Option<Vec<u8>>,
}

impl ServicerCtl {
    /// Probeert een system-verbinding van deze levensduur te claimen
    /// (hooguit `max`).
    pub fn try_conn(&self, max: u8) -> bool {
        let n = self.conns.get();
        if n >= u32::from(max) {
            return false;
        }
        self.conns.set(n + 1);
        true
    }

    /// Geeft een system-verbinding van deze levensduur terug.
    pub fn drop_conn(&self) {
        self.conns.set(self.conns.get().saturating_sub(1));
    }

    /// Het aantal open system-verbindingen van deze levensduur.
    #[must_use]
    pub fn conns(&self) -> u32 {
        self.conns.get()
    }

    /// De generatie van deze levensduur.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.life.generation
    }

    /// Meldt dat de servicer weg is (de servicer zelf; een toets die geen
    /// servicer draait).
    pub fn set_gone(&self) {
        self.gone.set();
    }
}

/// Eén regel van de servicer-tabel: de levende levensduur van een slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Life {
    /// De generatie die de lifecycle-actor hem gaf.
    generation: u32,
    /// Geadopteerd bij een warme kern-flip: dezelfde app draait door, alleen
    /// de kern is nieuw. Zijn root (`/.tasks/slot<N>`) is dan van hem en
    /// blijft staan; alleen een echte start begint leeg. Tot 24-10 zag de
    /// hopfs-actor van kern B elke geadopteerde generatie als een nieuwe
    /// levensduur, en de eerste bestandscall na de flip wiste de root van
    /// een app die gewoon doordraaide.
    adopted: bool,
}

/// De servicers van de levende slots: per slot de besturing van zijn
/// levensduur ([`ServicerCtl`]), op slotnummer ([`SlotVec`]).
///
/// De actor schrijft, de servicers, de system-listener en de hopfs-actor
/// lezen kort (handboek §1.1); de eigen root van een slot is
/// `/.tasks/slot<N>` en volgt uit het slot.
///
/// Elke wissel in de tabel luidt [`Servicers::changed`], en een stop zet
/// het slot klaar voor [`Servicers::take_stopped`]: de committer hoort een
/// stop zo, en hoeft niet rond te kijken.
pub struct Servicers {
    table: LocalCell<SlotVec<Rc<ServicerCtl>>>,
    stopped: LocalCell<Option<Slot>>,
    changed: Signal,
}

impl Servicers {
    /// Een lege tabel, voor in een `static`.
    #[must_use]
    pub const fn new() -> Servicers {
        Servicers {
            table: LocalCell::cell(SlotVec::new()),
            stopped: LocalCell::cell(None),
            changed: Signal::new(),
        }
    }

    /// Luidt de servicer van `slot`: zijn outbox heeft werk, er staat een
    /// SMP-verzoek, of zijn context viel. De kern luidt hem bij de kick van
    /// een app-core en na een beurt op de OS-core; de servicer zelf pollt
    /// niet (O10).
    pub fn ring(&self, slot: Slot) {
        if let Some(c) = self.table.borrow().get(slot) {
            c.bell.set();
        }
    }

    /// Wacht tot een slot een levensduur begon of beëindigde. Eén wachter
    /// (de committer); tien wissels vóór de wachter kijkt zijn één wek.
    pub fn changed(&self) -> sync::signal::Wait<'_> {
        self.changed.wait()
    }

    /// Het slot dat als laatste stopte sinds de vorige vraag (de committer
    /// legt dan de boom vast).
    pub fn take_stopped(&self) -> Option<Slot> {
        self.stopped.borrow_mut().take()
    }

    /// Doet `f` op de jobnaam van de levende bewoner van `slot` (leeg = geen
    /// job): de naamruimte van de store-ops ([`crate::store`]), binnen één
    /// lening. `None` zonder levende servicer.
    pub fn with_job<R>(&self, slot: Slot, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let t = self.table.borrow();
        let c = t.get(slot)?;
        Some(f(c.job.as_deref().unwrap_or(&[])))
    }

    /// Doet `f` op de volume-tabel van `slot` (langste `local` eerst),
    /// binnen één lening. `None` als het slot geen zicht heeft.
    pub fn with_mounts<R>(&self, slot: Slot, f: impl FnOnce(&[Mount]) -> R) -> Option<R> {
        let t = self.table.borrow();
        t.get(slot)?.mounts.as_deref().map(f)
    }

    /// De besturing van de levende levensduur van `slot`.
    #[must_use]
    pub fn ctl(&self, slot: Slot) -> Option<Rc<ServicerCtl>> {
        self.table.borrow().get(slot).cloned()
    }

    /// De generatie van de levende servicer van `slot`, of `None`. Eén
    /// lening, geen `.await`.
    #[must_use]
    pub fn current(&self, slot: Slot) -> Option<u32> {
        self.table.borrow().get(slot).map(|c| c.life.generation)
    }

    /// Houdt levensduur `generation` van `slot` zijn root? Waar alleen als
    /// hij levend is en bij een warme flip geadopteerd werd: dan draait
    /// dezelfde app door en is wat in `/.tasks/slot<N>` staat van hem. Een
    /// gewone start (ook een herstart na de flip) begint leeg.
    #[must_use]
    pub fn root_kept(&self, slot: Slot, generation: u32) -> bool {
        self.table
            .borrow()
            .get(slot)
            .is_some_and(|c| c.life.adopted && c.life.generation == generation)
    }

    /// De partitie van de levende bewoner van `slot`. `None` zonder levende
    /// servicer (gestopt, of nog niet gestart): een codec-grant op een slot
    /// dat vrijkomt, weigert dan dicht. De actor haalt de levensduur weg
    /// vóór er iets vrijkomt (`evict`); wie hier een partitie krijgt, krijgt
    /// die van de levensduur die [`Servicers::current`] op dat moment zag.
    #[must_use]
    pub fn partition(&self, slot: Slot) -> Option<Region> {
        self.table.borrow().get(slot).map(|c| c.part)
    }

    /// Publiceert een nieuwe levensduur van `slot`; geeft zijn besturing
    /// voor de servicer-taak. Faalt alleen als de tabel niet kan groeien.
    fn open(
        &self,
        slot: Slot,
        life: Life,
        part: Region,
        mounts: Option<Vec<Mount>>,
        job: Option<Vec<u8>>,
    ) -> Result<Rc<ServicerCtl>> {
        // `Rc::new` kent op stable Rust geen faalbare vorm; de besturing is
        // een handvol woorden, en een heap die dat niet meer kan geven, is
        // een kern die luid valt.
        let ctl = Rc::new(ServicerCtl {
            life,
            stop: Signal::new(),
            gone: Signal::new(),
            bell: Signal::new(),
            conns: Cell::new(0),
            part,
            mounts,
            job,
        });
        self.table.borrow_mut().insert(slot, Rc::clone(&ctl))?;
        self.changed.set();
        Ok(ctl)
    }

    /// Haalt de levensduur van `slot` uit de tabel en geeft zijn besturing.
    fn close(&self, slot: Slot) -> Option<Rc<ServicerCtl>> {
        let old = self.table.borrow_mut().remove(slot)?;
        *self.stopped.borrow_mut() = Some(slot);
        self.changed.set();
        Some(old)
    }
}

impl Default for Servicers {
    fn default() -> Self {
        Servicers::new()
    }
}

/// De outbox van één levensduur, zoals de servicer hem leest.
pub trait Outbox {
    /// Het volgende record in `buf`: soort en lengte.
    fn read_into(&mut self, buf: &mut [u8]) -> Option<(u8, usize)>;
    /// Is de ring corrupt?
    fn corrupt(&self) -> bool;
    /// Leeft de context van het slot nog?
    fn live(&self) -> bool;
    /// Staat er een onbeantwoord SMP-verzoek?
    fn smp_pending(&self) -> bool;
}

/// De servicer-taak van één levensduur van `slot`: dient hem en meldt
/// `gone`. De kooi spawnt er één per plaatsing ([`Cage::serve`]), met de
/// besturing uit [`Servicers::open`].
///
/// `open` opent de outbox voor de partitie van de levensduur.
pub async fn servicer_task<'a, O, T, L, const N: usize>(
    slot: Slot,
    ctl: Rc<ServicerCtl>,
    open: impl FnOnce(Region) -> O,
    timer: &T,
    log: &L,
    inbox: &Mailbox<Envelope<'a>, N>,
    buf: &mut [u8],
) where
    O: Outbox,
    T: Timer,
    L: Console,
{
    serve(slot, &ctl, &mut open(ctl.part), timer, log, inbox, buf).await;
    ctl.gone.set();
}

/// Eén levensduur: outbox lezen, logs doorzetten, SMP-verzoeken melden.
/// Stopt bij evict, een corrupte ring of een weggevallen context. Wacht op
/// zijn bel of zijn stop; alleen vóór de context de eerste keer leeft, met
/// de termijn van [`IDLE_GRACE`] erbij.
async fn serve<'a, O: Outbox, T: Timer, L: Console, const N: usize>(
    slot: Slot,
    ctl: &ServicerCtl,
    out: &mut O,
    timer: &T,
    log: &L,
    inbox: &Mailbox<Envelope<'a>, N>,
    buf: &mut [u8],
) {
    let mut saw_live = false;
    let mut idle_since: Option<u64> = None;
    let mut batch = 0usize;
    let mut stopping = false;
    loop {
        // Een stop (de evict) gaat pas door als de outbox leeg is: het
        // afscheid van de app staat erin vlak vóór hij parkeert, en de
        // evict komt binnen 10 ms daarna, vaak vóór deze tik (QEMU, 05-10:
        // HOPOS_APP_STOP en HOPOS_APP_SHUTDOWN haalden de console niet).
        stopping |= ctl.stop.take();
        // De app kan geparkeerde cores niet zelf starten (de mailboxen liggen
        // buiten elke stage-2-map); de actor dispatcht namens hem.
        if out.smp_pending() {
            let _ = inbox.try_send(Envelope {
                req: Request::Smp(slot),
                reply: None,
            });
        }
        if let Some((kind, n)) = out.read_into(buf) {
            let line = buf.get(..n).unwrap_or(&[]);
            if kind == RING_LOG {
                log.app_line(slot, line);
            } else {
                log.log(format_args!(
                    "slot {slot}: stray record {kind} HOPOS_RING_STRAY"
                ));
            }
            batch += 1;
            if batch >= SERVICER_BATCH {
                batch = 0;
                yield_now().await;
            }
            continue;
        }
        batch = 0;
        if stopping {
            return;
        }
        if out.corrupt() {
            log.log(format_args!(
                "slot {slot}: outbox corrupt HOPOS_SERVICER_RING"
            ));
            return;
        }
        let wake = select(ctl.stop.wait(), ctl.bell.wait());
        let woke = if out.live() {
            saw_live = true;
            wake.await
        } else if saw_live {
            return;
        } else {
            let grace = IDLE_GRACE.as_nanos() as u64;
            let since = *idle_since.get_or_insert(timer.now());
            let left = grace.saturating_sub(timer.now().saturating_sub(since));
            if left == 0 {
                return;
            }
            match select(wake, timer.sleep(Duration::from_nanos(left))).await {
                Either::Left(w) => w,
                Either::Right(()) => Either::Right(()),
            }
        };
        if let Either::Left(()) = woke {
            stopping = true;
        }
    }
}

/// Eén regel over de val van een app, als hij viel: de fault die EL2 ving
/// (een stage-2-fault, een getrapte SMC), of de exception die de
/// vectortabel van applib op EL1 ving (exitcode `EXIT_APP_FAULT`).
fn report_fault<L: Console>(log: &L, slot: Slot, st: &crate::cage::Status) {
    use crate::cage::esr_class;
    if st.fault_vec != 0 {
        log.log(format_args!(
            "slot {slot}: fault vec={} esr={:#x} ({}) far={:#x} HOPOS_SLOT_FAULT",
            st.fault_vec - 1,
            st.fault_esr,
            esr_class(st.fault_esr),
            st.fault_far
        ));
    } else if st.app_fault_vec != 0 && st.exit_code == abi::hopabi::EXIT_APP_FAULT {
        log.log(format_args!(
            "slot {slot}: fault at EL1 vec={} esr={:#x} ({}) elr={:#x} far={:#x} HOPOS_SLOT_FAULT",
            st.app_fault_vec - 1,
            st.app_fault_esr,
            esr_class(st.app_fault_esr),
            st.app_fault_elr,
            st.app_fault_far
        ));
    }
}

enum Held {
    Streaming,
    Running(Partition<Owned>),
}

struct Resident {
    held: Held,
    core: Core,
    span: usize,
    generation: u32,
    job: Vec<u8>,
    mounts: Vec<Mount>,
    ports: Vec<u16>,
}

/// Alles wat een volgende kern over één levende bewoner moet weten (de
/// flip). Puur data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotState {
    /// De sharegroup (leeg = dedicated).
    pub share_group: Vec<u8>,
    /// De volledige pool van de groep, ook tijdelijk lege cores.
    pub group_cores: Vec<usize>,
    /// Het slot.
    pub slot: usize,
    /// De basis van de zichtbare partitie.
    pub part_base: u64,
    /// De maat van de zichtbare partitie.
    pub part_size: u64,
    /// De primaire core.
    pub core: usize,
    /// De span.
    pub cores: usize,
    /// De job-naam.
    pub job: Vec<u8>,
    /// De gepubliceerde poorten.
    pub ports: Vec<u16>,
    /// De volumes.
    pub mounts: Vec<Mount>,
}

/// De lifecycle-actor. Eén taak; hij bezit de pool, de plaatsing, de
/// bewoners, de kooi en de grant-aanbieder (`G`, kaal [`NoGrants`]).
pub struct Lifecycle<'s, C, K, T, L, G = NoGrants> {
    cage: C,
    cores: K,
    timer: T,
    log: L,
    parts: PartitionPool,
    places: Places,
    residents: SlotVec<Resident>,
    svc: &'s Servicers,
    generation: u32,
    grants: G,
}

impl<'s, C: Cage, K: Cores, T: Timer, L: Console, G: Grants> Lifecycle<'s, C, K, T, L, G> {
    /// Een actor over deze kooi, cores en pools, met `grants` als
    /// grant-aanbieder (de gui-smaak geeft zijn framebuffer-grant, kaal
    /// [`NoGrants`]).
    #[expect(
        clippy::too_many_arguments,
        reason = "elk deel is een eigenaar die als waarde de actor in gaat; een bouwer zou alleen de volgorde verstoppen"
    )]
    pub fn new(
        cage: C,
        cores: K,
        timer: T,
        log: L,
        parts: PartitionPool,
        places: Places,
        svc: &'s Servicers,
        grants: G,
    ) -> Self {
        Lifecycle {
            cage,
            cores,
            timer,
            log,
            parts,
            places,
            residents: SlotVec::new(),
            svc,
            generation: 0,
            grants,
        }
    }

    /// De lus van de actor: verzoek lezen, afhandelen, antwoorden.
    pub async fn run<const N: usize>(&mut self, inbox: &Mailbox<Envelope<'_>, N>) {
        loop {
            let env = inbox.recv().await;
            let r = self.handle(env.req).await;
            if let Some(reply) = env.reply {
                reply.val.put(r);
            }
        }
    }

    /// Handelt één verzoek af.
    pub async fn handle(&mut self, req: Request) -> Response {
        let r = match req {
            Request::Claim(spec) => self.claim(spec).await.map(Response::Granted),
            Request::Arm { grant, entry } => self.arm(grant, entry).await.map(|()| Response::Done),
            Request::Abort(grant) => {
                self.abort(grant);
                Ok(Response::Done)
            }
            Request::Env { slot, env } => self.env(slot, env).map(Response::Env),
            Request::Stop { slot, timeout } => {
                self.stop(slot, timeout).await.map(|()| Response::Done)
            }
            Request::Status(slot) => Ok(Response::Status(self.status(slot))),
            Request::Occupied { after, max } => self.occupied(after, max).map(Response::Occupied),
            Request::Smp(slot) => {
                self.smp(slot);
                Ok(Response::Done)
            }
            Request::Snapshot => self.snapshot().map(Response::Snapshot),
            Request::ReserveDevice { size, what } => {
                self.reserve_device(size, what).map(Response::Device)
            }
            Request::ReleaseDevice { region, what } => {
                self.release_device(region, what).map(|()| Response::Done)
            }
        };
        r.unwrap_or_else(Response::Failed)
    }

    /// Een blok buiten de partitie-pool (Go: `ReserveDevice`): best-fit en
    /// de hoogste basis, zoals een partitie, zodat het lage DRAM voor de
    /// vensters en de DMA onder 4 GB blijft. Eén regel, ook bij falen: een
    /// node zonder arena moet zeggen hoeveel er nog was.
    fn reserve_device(&mut self, size: u64, what: &str) -> Result<Region> {
        let size = crate::align_grain(size)
            .filter(|s| *s != 0)
            .ok_or(Error::PartitionSize { size })?;
        match self.parts.reserve_device(size) {
            Ok(base) => {
                self.log.log(format_args!(
                    "pool: {} MB for {what} at {base:#x} outside the partition pool, {} MB left for slots (largest {} MB) HOPOS_POOL_DEVICE base={base:#x} mb={}",
                    size >> 20,
                    self.parts.capacity() >> 20,
                    self.parts.largest() >> 20,
                    size >> 20
                ));
                Ok(Region::new(base, size))
            }
            Err(e) => {
                self.log.log(format_args!(
                    "pool: no {} MB for {what}: {e} ({} MB free, largest {} MB) HOPOS_POOL_DEVICE_FAIL",
                    size >> 20,
                    self.parts.capacity() >> 20,
                    self.parts.largest() >> 20
                ));
                Err(e)
            }
        }
    }

    /// Geeft een apparaatblok terug aan de pool.
    fn release_device(&mut self, region: Region, what: &str) -> Result {
        self.parts.release_device(region.base, region.size)?;
        self.log.log(format_args!(
            "pool: {} MB of {what} at {:#x} back in the partition pool, {} MB for slots HOPOS_POOL_DEVICE_RELEASE",
            region.size >> 20,
            region.base,
            self.parts.capacity() >> 20
        ));
        Ok(())
    }

    fn resident(&self, slot: Slot) -> Option<&Resident> {
        self.residents.get(slot)
    }

    fn resident_mut(&mut self, slot: Slot) -> Option<&mut Resident> {
        self.residents.get_mut(slot)
    }

    /// Stap 1 van een start: plaats, reserveer en wis (E1, E3). De claim
    /// komt pas na de toetsen; een geweigerde claim laat niets achter.
    async fn claim(&mut self, spec: StartSpec) -> Result<ImageGrant> {
        // De volumes eerst: een spec die buiten zijn zicht wil, claimt niets.
        crate::rpc::mount_table(&spec.mounts)?;
        // Geen maximum: het laagste nummer zonder bewoner en zonder
        // partitie. Of het past, zegt de plaatsing hieronder.
        let slot = match spec.slot {
            Some(s) => s,
            None => self
                .residents
                .lowest_free(|s| Some(s) != spec.avoid && self.parts.partition_of(s).is_none())
                .ok_or(Error::Full {
                    cap: abi::layout::SLOT_MAX,
                })?,
        };
        if self.resident(slot).is_some() || self.parts.partition_of(slot).is_some() {
            return Err(Error::StillOwned { slot: slot.get() });
        }
        let (core, system) = self
            .places
            .place_anywhere(&self.cores, slot, &spec.placement)?;
        if system {
            self.log.log(format_args!(
                "slot {slot}: no free app core, joining the system group on the OS core HOPOS_PLACE_SYSTEM"
            ));
        }
        let span = spec.placement.cores.max(1);
        // Dedicated: de cores moeten geparkeerd of koud zijn. Een gedeelde
        // core draait meestal juist (zijn buren). Zonder groep komt de run
        // uit `place_dedicated` en ligt hij helemaal in de app-cores
        // (`run_free`), dus laat `Core::new` er niets uit vallen; de
        // OS-core komt alleen met een groep.
        if self.places.group_of(slot).is_none()
            && let Some(busy) = (core.get()..core.get() + span)
                .filter_map(Core::new)
                .find(|c| self.cores.power(*c) != Power::Off)
        {
            self.places.release(slot);
            return Err(Error::CoreBusy { core: busy.get() });
        }
        let part = match self.parts.alloc(slot, spec.mem_limit) {
            Ok(p) => p,
            Err(e) => {
                self.places.release(slot);
                return Err(e);
            }
        };
        // E3: een nieuwe eigenaar krijgt gewist geheugen, in brokken.
        let r = part.region();
        let mut off = 0;
        while off < r.size {
            let n = SCRUB_CHUNK.min(r.size - off);
            self.cage.clear(r.base + off, n);
            off += n;
            sync::yield_now().await;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let resident = Resident {
            held: Held::Streaming,
            core,
            span,
            generation,
            job: spec.job,
            mounts: spec.mounts,
            ports: spec.ports,
        };
        if let Err(e) = self.residents.insert(slot, resident) {
            part.abandon(&mut self.parts);
            self.places.release(slot);
            return Err(e);
        }
        Ok(ImageGrant { part, generation })
    }

    fn streaming(&self, grant: &ImageGrant) -> bool {
        self.resident(grant.slot())
            .is_some_and(|r| matches!(r.held, Held::Streaming) && r.generation == grant.generation)
    }

    /// Stap 2: zet de poorten door, bouw de kooi (E5), registreer de
    /// servicer, dispatch.
    ///
    /// De poorten gaan vóór de bouw en vóór het startschot open, zoals in
    /// Go's `armSlot`: een poort die al van een ander slot is, laat de start
    /// dan falen terwijl er nog niets draait en de grant gewoon terug kan.
    /// Na een geslaagde dispatch komt er geen faalbare stap meer.
    async fn arm(&mut self, grant: ImageGrant, entry: u64) -> Result {
        let slot = grant.slot();
        if !self.streaming(&grant) {
            // Een grant uit een andere levensduur: niets aanraken. Het token
            // droppen laat een eventuele claim staan (fail-closed).
            return Err(Error::NotOwned { slot: slot.get() });
        }
        let (core, span, generation) = match self.resident(slot) {
            Some(r) => (r.core, r.span, r.generation),
            None => return Err(Error::NotOwned { slot: slot.get() }),
        };
        let region = grant.region();
        let ports = match self.resident(slot) {
            Some(r) => try_vec(&r.ports)?,
            None => Vec::new(),
        };
        if let Err(e) = self.publish(slot, &ports).await {
            self.abort(grant);
            return Err(e);
        }
        if let Err(e) = self.cage.build(slot, region, entry, core, span) {
            // Nooit gedispatcht: bevestigde afwezigheid van uitvoering. De
            // poorten gingen al open; die horen bij deze start en gaan dicht.
            if !ports.is_empty() {
                self.cage.unpublish(slot);
            }
            self.abort(grant);
            return Err(Error::Cage {
                slot: slot.get(),
                code: e.code,
            });
        }
        // De grant-haak na de kooibouw en vóór het startschot (Go:
        // `grantArm` in `armSlot`): het venster van de houder de kooi in.
        // Faalt hij, dan is dat een startfout zoals een mislukte bouw: er
        // draaide nog niets, dus poorten dicht en de grant terug (de abort
        // geeft ook de device-grant vrij).
        if let Err(e) = self.grants.arm(slot) {
            self.log.log(format_args!(
                "slot {slot}: device grant not armed: {e}, start refused HOPOS_GRANT_ARM_FAIL"
            ));
            if !ports.is_empty() {
                self.cage.unpublish(slot);
            }
            self.cage.detach(slot).await;
            self.abort(grant);
            return Err(e);
        }
        let life = Life {
            generation,
            adopted: false,
        };
        // Een servicer die er niet bij kan, is een weigering zoals een
        // mislukte bouw: er draait nog niets.
        if let Err(e) = self.register(slot, life, region) {
            self.log.log(format_args!(
                "slot {slot}: no servicer: {e}, start refused HOPOS_SLOT_SPAWN"
            ));
            if !ports.is_empty() {
                self.cage.unpublish(slot);
            }
            self.grants.release(slot);
            self.cage.detach(slot).await;
            self.abort(grant);
            return Err(e);
        }
        // Een startschot dat faalt of niet aankomt, heeft een onbekende
        // uitkomst (ook een geweigerde CPU_ON: de koude boot geeft de core
        // schoon terug, zonder een tweede boekhouding van "nooit gelopen").
        if let Err(e) = self.cage.dispatch(slot, core) {
            self.log.fatal(format_args!(
                "slot {slot}: the start on core {core} failed (cage code {:#x}), execution unconfirmed",
                e.code
            ));
        }
        if !self.boots(slot, core).await {
            self.log.fatal(format_args!(
                "slot {slot}: core {core} never picked up the start, execution unconfirmed"
            ));
        }
        if let Some(r) = self.resident_mut(slot) {
            r.held = Held::Running(grant.part.dispatched());
        }
        Ok(())
    }

    /// Zet de poorten van een start door (via de kooi, die het slot-LAN
    /// bezit), met één regel per start. Een weigering trekt in wat er al
    /// open stond: alles of niets.
    async fn publish(&mut self, slot: Slot, ports: &[u16]) -> Result {
        if ports.is_empty() {
            return Ok(());
        }
        match self.cage.publish(slot, ports).await {
            Ok(()) => {
                self.log.log(format_args!(
                    "slot {slot}: {} port(s) published tcp+udp on the uplink: {} HOPOS_SLOT_PUBLISH",
                    ports.len(),
                    PortList(ports)
                ));
                Ok(())
            }
            Err(e) => {
                self.cage.unpublish(slot);
                let err = match e {
                    PortError::Taken { port, owner } => Error::PortTaken {
                        slot: slot.get(),
                        port,
                        owner,
                    },
                    PortError::Refused { port } => Error::PortRefused {
                        slot: slot.get(),
                        port,
                    },
                };
                self.log.log(format_args!("{err} HOPOS_SLOT_PUBLISH_FAIL"));
                Err(err)
            }
        }
    }

    /// Publiceert levensduur `life` van `slot` in de servicer-tabel, met
    /// zijn volumes, jobnaam en partitie, en geeft hem zijn servicer
    /// ([`Cage::serve`]). Faalt als de tabel of de servicer er niet bij kan:
    /// dan is er nog niets gedispatcht, en is het een weigering ("past
    /// niet").
    fn register(&mut self, slot: Slot, life: Life, region: Region) -> Result {
        // De volumes van deze levensduur, genormaliseerd. De claim toetste
        // ze al; faalt het hier toch (geheugen, of een geadopteerde spec),
        // dan krijgt het slot geen zicht en weigert elke bestandscall luid,
        // in plaats van dat een volume stil in de eigen root belandt.
        let mounts = match self
            .resident(slot)
            .map(|r| crate::rpc::mount_table(&r.mounts))
        {
            Some(Ok(m)) => Some(m),
            Some(Err(e)) => {
                self.log.log(format_args!(
                    "slot {slot}: volumes refused: {e}, file calls denied HOPOS_FS_MOUNTS"
                ));
                None
            }
            None => None,
        };
        // Eén regel per levensduur met volumes: het bewijs dat de jobspec tot
        // in de tabel van de hopfs-actor kwam (een start zonder volumes
        // zwijgt, zoals een start zonder poorten).
        if let Some(m) = mounts.as_deref().filter(|m| !m.is_empty()) {
            self.log.log(format_args!(
                "slot {slot}: {} volume(s) mounted: {} HOPOS_SLOT_MOUNTS",
                m.len(),
                MountList(m)
            ));
        }
        // Een geheugenweigering hier laat het slot zonder naamruimte: elke
        // store-op wordt dan luid geweigerd, nooit in een verkeerde map.
        let job = self
            .resident(slot)
            .filter(|r| !r.job.is_empty())
            .and_then(|r| try_vec(&r.job).ok());
        let ctl = self.svc.open(slot, life, region, mounts, job)?;
        if let Err(e) = self.cage.serve(slot, ctl) {
            self.svc.close(slot);
            return Err(Error::Cage {
                slot: slot.get(),
                code: e.code,
            });
        }
        Ok(())
    }

    /// De gewone rollback: de grant komt terug en er draaide nooit iets.
    /// Een device-grant van deze start gaat mee terug (Go: `rollback` van
    /// `startGrant`): wie nooit draaide, houdt geen glas vast.
    fn abort(&mut self, grant: ImageGrant) {
        if !self.streaming(&grant) {
            return;
        }
        let slot = grant.slot();
        self.grants.release(slot);
        grant.part.abandon(&mut self.parts);
        self.places.release(slot);
        self.residents.remove(slot);
    }

    /// De env-haak van een lopende start (Go: `prepareGrantedEnv`): de
    /// grant-aanbieder mag regels achter de env van `slot` zetten, en de
    /// complete blob gaat terug naar wie de stroom houdt, die hem op de
    /// control-page schrijft.
    ///
    /// Alleen voor een slot in de stroom: de grant die hier ontstaat, gaat
    /// terug bij de abort of na de bevestigde stop, en een slot zonder
    /// stroom heeft geen van beide voor zich. Past het geheel niet in de
    /// control-page ([`abi::hopabi::CTRL_ENV_MAX`]), dan is dat een
    /// weigering van de grant, geen fout van de start: de grant gaat terug
    /// en de app draait zonder, met één regel.
    fn env(&mut self, slot: Slot, mut env: Vec<u8>) -> Result<Vec<u8>> {
        if !self
            .resident(slot)
            .is_some_and(|r| matches!(r.held, Held::Streaming))
        {
            return Err(Error::NotOwned { slot: slot.get() });
        }
        let mut extra = Vec::new();
        self.grants.env(slot, &env, &mut extra);
        if extra.is_empty() {
            return Ok(env);
        }
        // Een env zonder slotregel krijgt er een: anders plakt de eerste
        // regel van de aanbieder aan de laatste waarde van de start.
        let sep = usize::from(env.last().is_some_and(|&b| b != b'\n'));
        let max = abi::hopabi::CTRL_ENV_MAX as usize;
        let len = env.len() + sep + extra.len();
        let refused = if len > max {
            Some(Error::TooLarge { len, max })
        } else {
            env.try_reserve(sep + extra.len())
                .err()
                .map(|_| Error::OutOfMemory { bytes: len })
        };
        if let Some(e) = refused {
            self.grants.release(slot);
            self.log.log(format_args!(
                "slot {slot}: device grant refused: env {e}, the app runs without it HOPOS_GRANT_ENV"
            ));
            return Ok(env);
        }
        if sep == 1 {
            env.push(b'\n');
        }
        env.extend_from_slice(&extra);
        Ok(env)
    }

    /// Stopt de servicer van `slot` en wacht tot hij weg is.
    async fn evict(&mut self, slot: Slot) {
        if let Some(ctl) = self.svc.close(slot) {
            ctl.stop.set();
            ctl.gone.wait().await;
        }
    }

    async fn wait_quiet(&self, slot: Slot, core: Core, span: usize, timeout: Duration) -> bool {
        let deadline = self.timer.now().saturating_add(timeout.as_nanos() as u64);
        loop {
            let quiet = core.run(span).all(|c| self.cage.quiet(slot, c));
            if quiet {
                return true;
            }
            if self.timer.now() >= deadline {
                return false;
            }
            self.timer.sleep(STOP_POLL).await;
        }
    }

    /// Wacht tot een nieuwe bewoner van een gedeelde app-core zijn eerste
    /// beurt kreeg (share.go `bootPendingDispatch`). Op arm64 wisselt de
    /// core alleen bij een vrijwillige yield, dus een buur die rekent houdt
    /// hem vast tot iemand ingrijpt. GEMETEN 14/15-08 (Go, LicheeRV): uren
    /// gijzeling en elke plaatsing dood. Na [`RECLAIM_WAIT`] dus één
    /// escalatie: de vasthouder offeren (zijn kooi intrekken, de rotatie
    /// leeft door), en nog één ronde. `false`: nog steeds niet opgepikt.
    ///
    /// Nooit op de OS-core (die heeft de rotatie van de kern), en nooit Hop
    /// zelf: zonder Hop herstart niemand het slachtoffer.
    async fn boots(&mut self, slot: Slot, core: Core) -> bool {
        if core == Core::OS || self.boot_wait(slot).await {
            return true;
        }
        let hog = self.cage.holder(core).filter(|h| {
            *h != slot
                && self.resident(*h).is_some_and(|r| r.core == core)
                && self
                    .places
                    .group_of(*h)
                    .is_none_or(|(g, _)| g.as_slice() != crate::pool::hop_group().as_slice())
        });
        let Some(hog) = hog else {
            self.log.log(format_args!(
                "slot {slot}: core {core} holds no resident to reclaim HOPOS_CORE_RECLAIM_FAILED"
            ));
            return self.boot_wait(slot).await;
        };
        self.log.log(format_args!(
            "slot {slot}: core {core} never yielded in {} s, sacrificing resident slot {hog} HOPOS_CORE_RECLAIM",
            RECLAIM_WAIT.as_secs()
        ));
        self.cage.revoke(hog);
        self.cores.kick(core);
        self.boot_wait(slot).await
    }

    /// Hooguit [`RECLAIM_WAIT`] wachten tot `slot` niet meer boot-pending is.
    async fn boot_wait(&self, slot: Slot) -> bool {
        let deadline = self
            .timer
            .now()
            .saturating_add(RECLAIM_WAIT.as_nanos() as u64);
        while self.cage.pending(slot) {
            if self.timer.now() >= deadline {
                return false;
            }
            self.timer.sleep(STOP_POLL).await;
        }
        true
    }

    /// Stop: de coöperatieve kans (met de termijn op de vlag), dan de
    /// intrekking, dan pas de servicer en de ringen weg. Alleen een
    /// bevestigde beëindiging geeft de partitie en de cores terug (E2, E9);
    /// wordt het slot ook na de intrekking niet stil, dan kan de kern de
    /// isolatie niet meer bevestigen, en dat is `fatal` (§13.5).
    async fn stop(&mut self, slot: Slot, timeout: Duration) -> Result {
        let Some(r) = self.resident(slot) else {
            return Ok(()); // Een leeg kooinummer zegt niets over core i.
        };
        if matches!(r.held, Held::Streaming) {
            // De grant is buiten; Hop serialiseert Stop tegen een lopende
            // stream (slot-lifecycle-grenzen.md). Eerst Abort.
            return Err(Error::StillOwned { slot: slot.get() });
        }
        let (core, span) = (r.core, r.span);
        let published = !r.ports.is_empty();
        // De kill-vlag draagt de termijn: dat is de gratie van de app
        // (SIGTERM, dan SIGKILL). Tot 05-10 ging eerst de servicer weg en
        // gingen de ringen van de switch, en pas dan de vlag: de app had dan
        // geen net, geen system-API (geen generatie) en geen outbox-lezer
        // meer, dus niets om nog netjes mee af te sluiten. Nu houdt hij die
        // drie tot hij stil is of ingetrokken wordt.
        self.cage.request_exit(slot, timeout);
        // De deuren dicht zodra de app gevraagd is te stoppen: een nieuwe
        // verbinding naar een app die weggaat, bereikt niemand meer, en de
        // poort is dan vrij voor de volgende start (Go: `UnpublishSlot` in
        // de stop).
        self.cage.unpublish(slot);
        if published {
            self.log.log(format_args!(
                "slot {slot}: ports withdrawn from the uplink HOPOS_SLOT_UNPUBLISH"
            ));
        }
        if self.wait_quiet(slot, core, span, timeout).await {
            // De val van de app, één regel, wie de stop ook vroeg: tot 01-11
            // drukte alleen de plaatsing van de boot hem (O7), en een fout
            // van een app van Hop stond nergens op de console.
            report_fault(&self.log, slot, &self.cage.status(slot));
        } else {
            // Eén intrekking velt alle cores van het slot (gedeelde tabel en
            // VMID); de kick laat ook slapers de intrekking zien.
            self.cage.revoke(slot);
            for c in core.run(span) {
                self.cores.kick(c);
            }
            if !self.wait_quiet(slot, core, span, REVOKE_GRACE).await {
                self.log.fatal(format_args!(
                    "slot {slot}: not stopped after revocation, execution unconfirmed"
                ));
            }
        }
        // Nu pas de servicer weg (hij las de laatste regels van de app al
        // door) en de ringen van de switch af (bevestigd): na dit punt
        // schrijft niemand meer in een partitie die vrijkomt.
        self.evict(slot).await;
        self.cage.detach(slot).await;
        let Some(r) = self.residents.remove(slot) else {
            return Ok(());
        };
        // Pas na de bevestigde stop gaat de device-grant terug (Go:
        // `grantRelease` in `releaseSlot`).
        self.grants.release(slot);
        if let Held::Running(p) = r.held {
            p.release(&mut self.parts, Stopped::confirmed(slot))?;
        }
        self.places.release(slot);
        // Eén regel per bevestigde stop, wie hem ook vroeg (Hop, de boot,
        // een flip): tot 04-10 schreef alleen de boot-plaatsing hem, en een
        // stop door Hop was op de console onzichtbaar (O6N, een BURN-job in
        // `system`: geen marker, wel gestopt).
        self.log.log(format_args!(
            "slot {slot}: stopped, partition and core released HOPOS_SLOT_STOPPED"
        ));
        Ok(())
    }

    /// De status van `slot`.
    #[must_use]
    pub fn status(&self, slot: Slot) -> SlotStatus {
        let r = self.resident(slot);
        SlotStatus {
            occupancy: match r.map(|r| &r.held) {
                None => Occupancy::Empty,
                Some(Held::Streaming) => Occupancy::Streaming,
                Some(Held::Running(_)) => Occupancy::Running,
            },
            core: r.map(|r| (r.core, r.span)),
            partition: self.parts.partition_of(slot),
            cage: if r.is_some() {
                self.cage.status(slot)
            } else {
                Status::default()
            },
        }
    }

    /// De bezette slots boven `after`, oplopend, hoogstens `max`.
    fn occupied(&self, after: usize, max: usize) -> Result<Vec<Slot>> {
        let mut out = Vec::new();
        for (slot, _) in self.residents.iter().filter(|(s, _)| s.get() > after) {
            if out.len() >= max {
                break;
            }
            try_push(&mut out, slot)?;
        }
        Ok(out)
    }

    /// Dispatcht een extra SMP-core namens de app. Alleen de breedte uit
    /// HOP's eigen boekhouding telt, NOOIT de app-schrijfbare page: een
    /// opgehoogde telling zou anders buurcores in de kooi trekken.
    fn smp(&mut self, slot: Slot) {
        let requested = self.cage.smp_request(slot);
        if requested == 0 {
            return;
        }
        let Some(r) = self.resident(slot) else {
            self.cage.clear_smp_request(slot);
            return;
        };
        if !matches!(r.held, Held::Running(_)) {
            return; // Nog aan het stromen: er draait niets dat vraagt.
        }
        let (core, span) = (r.core, r.span);
        // De app vraagt virtuele CPU's relatief aan zijn kooinummer.
        let offset = requested.wrapping_sub(slot.get() as u64);
        if offset < 1 || offset >= span as u64 {
            self.log.log(format_args!(
                "HOPOS_SMP_REJECT slot {slot}: core {requested} outside [{},{}] (trusted width {span})",
                slot.get() + 1,
                slot.get() + span - 1
            ));
            self.cage.clear_smp_request(slot);
            return;
        }
        let Some(c) = Core::new(core.get() + offset as usize) else {
            self.cage.clear_smp_request(slot);
            return;
        };
        if self.cores.power(c) == Power::On && !self.cage.quiet(slot, c) {
            // Dit pad was stil, en dat is een van de twee verklaringen als
            // een SMP-app hangt (22-09, O6N: vijf cores gevraagd, vier
            // gekregen, geen foutregel).
            self.log.log(format_args!(
                "slot {slot}: SMP core {c} already live — request cleared"
            ));
            self.cage.clear_smp_request(slot);
            return;
        }
        match self.cage.dispatch_secondary(slot, c) {
            Ok(()) => self.log.log(format_args!(
                "slot {slot}: SMP core {c} dispatched HOPOS_SMP_DISPATCH_OK"
            )),
            // Een startschot met onbekende uitkomst, net als in `arm`.
            Err(e) => self.log.fatal(format_args!(
                "slot {slot}: the SMP start on core {c} failed (cage code {:#x}), execution unconfirmed",
                e.code
            )),
        }
        self.cage.clear_smp_request(slot);
    }

    /// Beschrijft elke levende bewoner voor de flip, en weigert als er iets
    /// bij zit dat niet over te dragen is (een lopende stream, een dode
    /// context). Weigeren is de veilige kant: de flip gaat niet door.
    pub fn snapshot(&self) -> Result<Vec<SlotState>> {
        let mut out = Vec::new();
        for (slot, region) in self.parts.owners() {
            let r = self.resident(slot);
            let live = matches!(r.map(|r| &r.held), Some(Held::Running(_)));
            if !live || !self.cage.live(slot) {
                return Err(Error::NotLive { slot: slot.get() });
            }
            let Some(r) = r else { continue };
            let mut st = SlotState {
                slot: slot.get(),
                part_base: region.base,
                part_size: region.size,
                core: r.core.get(),
                cores: r.span,
                job: try_vec(&r.job)?,
                ports: try_vec(&r.ports)?,
                mounts: try_vec(&r.mounts)?,
                ..SlotState::default()
            };
            if let Some((name, members)) = self.places.group_of(slot) {
                st.share_group = try_vec(name.as_slice())?;
                for c in members {
                    try_push(&mut st.group_cores, c.get())?;
                }
            }
            if st.ports.len() > MAX_FLIP_PORTS
                || st.job.len() > MAX_FLIP_JOB
                || st.share_group.len() > MAX_FLIP_JOB
                || st.mounts.len() > MAX_FLIP_MOUNTS
                || st
                    .mounts
                    .iter()
                    .any(|m| m.local.len() > MAX_FLIP_PATH || m.shared.len() > MAX_FLIP_PATH)
            {
                return Err(Error::TooLarge {
                    len: st.job.len().max(st.ports.len()),
                    max: MAX_FLIP_JOB,
                });
            }
            try_push(&mut out, st)?;
        }
        validate_adoption(&out, &self.cores, &self.places, &self.parts)?;
        Ok(out)
    }

    /// Herstelt EERST alle eigendomsclaims, daarna de diensten (E8). Een
    /// onbruikbare overdracht is een fout vóór er iets geclaimd is.
    pub fn adopt(&mut self, states: &[SlotState]) -> Result<usize> {
        validate_adoption(states, &self.cores, &self.places, &self.parts)?;
        for st in states {
            let slot = Slot::new(st.slot).ok_or(Error::SlotRange {
                slot: st.slot,
                max: abi::layout::SLOT_MAX,
            })?;
            let core = adopted_core(st.core).ok_or(Error::CoreRange {
                core: st.core,
                max: self.cores.app_cores(),
            })?;
            let part = self.parts.adopt(slot, st.part_base, st.part_size)?;
            let mut group_cores: Vec<Core> = Vec::new();
            for c in &st.group_cores {
                try_push(
                    &mut group_cores,
                    adopted_core(*c).ok_or(Error::CoreRange { core: *c, max: 0 })?,
                )?;
            }
            let mut name = GroupName::new();
            for b in &st.share_group {
                name.push(*b).map_err(|_| Error::TooLarge {
                    len: st.share_group.len(),
                    max: crate::pool::MAX_GROUP_NAME,
                })?;
            }
            let group = (!st.share_group.is_empty()).then_some((&name, group_cores.as_slice()));
            self.places.adopt(slot, core, st.cores, group)?;
            self.generation = self.generation.wrapping_add(1);
            let generation = self.generation;
            self.residents.insert(
                slot,
                Resident {
                    held: Held::Running(part),
                    core,
                    span: st.cores,
                    generation,
                    job: try_vec(&st.job)?,
                    mounts: try_vec(&st.mounts)?,
                    ports: try_vec(&st.ports)?,
                },
            )?;
        }
        // Dan de device-grants, uit de geërfde kooien en vóór er één bewoner
        // opnieuw verbindt (Go: `grant.Adopt` in `adopt.go`, dat daar
        // panikeerde). Een fout laat de claims staan en start geen dienst:
        // fail-closed, de watchdog en Hop ruimen op.
        for st in states {
            if let Some(slot) = Slot::new(st.slot)
                && let Err(e) = self.grants.adopt(slot)
            {
                self.log.log(format_args!(
                    "slot {slot}: device grant not restored: {e} HOPOS_GRANT_ADOPT_FAIL"
                ));
                return Err(e);
            }
        }
        // Pas nu de diensten: geen servicer vóór ALLE oude claims terug zijn.
        for st in states {
            if let Some(slot) = Slot::new(st.slot)
                && let Some(generation) = self.resident(slot).map(|r| r.generation)
            {
                // Geadopteerd: dezelfde app draait door, dus zijn root blijft
                // (de hopfs-actor leest het bij zijn eerste bestandscall).
                let life = Life {
                    generation,
                    adopted: true,
                };
                self.register(slot, life, Region::new(st.part_base, st.part_size))?;
                self.log.log(format_args!(
                    "slot {slot}: adopted: partition {} MB @ {:#x} on core {} ({} core(s)), ownership restored",
                    st.part_size >> 20,
                    st.part_base,
                    st.core,
                    st.cores
                ));
            }
        }
        Ok(states.len())
    }

    /// Zet de poorten van elke geadopteerde bewoner door: ze horen bij de
    /// slots, en de NAT van deze kern begon leeg. Na [`Lifecycle::adopt`],
    /// als het net luistert (de bootreeks: lease, uplink, stack, listener,
    /// slots, Hop). Een weigering is een regel (`publish`), de bewoner
    /// draait door.
    pub async fn republish(&mut self) {
        let mut todo: Vec<(Slot, Vec<u16>)> = Vec::new();
        for (slot, r) in self.residents.iter() {
            if let Ok(p) = try_vec(&r.ports) {
                let _ = try_push(&mut todo, (slot, p));
            }
        }
        for (slot, ports) in todo {
            let _ = self.publish(slot, &ports).await;
        }
    }

    /// De kooi (voor de tests).
    #[cfg(test)]
    pub(crate) fn cage(&mut self) -> &mut C {
        &mut self.cage
    }

    /// De grant-aanbieder (voor de tests).
    #[cfg(test)]
    pub(crate) fn grants(&mut self) -> &mut G {
        &mut self.grants
    }
}

/// Toetst het complete eigendomsbeeld, vóór de sprong én vóór het herstel.
/// Er zijn weinig kooien: paarsgewijze toetsen houden dit boot-pad simpel.
pub fn validate_adoption(
    states: &[SlotState],
    cores: &impl Cores,
    places: &Places,
    parts: &PartitionPool,
) -> Result {
    let app = cores.app_cores();
    let is_app = |c: usize| (1..=app).contains(&c);
    // De OS-core (logische core 0, PORT.md beslissing 2): alleen een bewoner
    // van een groep die hem van deze kern mag delen (Hop's groep `hop`, en
    // wat de config vertrouwt), met één core en de OS-core als hele pool.
    // Zo neemt de nieuwe kern na een flip Hop op de OS-core over, en nooit
    // een gewone app die daar volgens een blob zou staan.
    let on_os = |s: &SlotState| {
        s.core == Core::OS.get()
            && s.cores == 1
            && places.os_group(&s.share_group)
            && s.group_cores.as_slice() == [Core::OS.get()]
    };
    for (j, s) in states.iter().enumerate() {
        let bad = Error::Range {
            base: s.part_base,
            size: s.part_size,
        };
        if Slot::new(s.slot).is_none()
            || s.part_size == 0
            || s.part_base.checked_add(s.part_size).is_none()
            || s.part_base % GRAIN != 0
            || s.part_size % GRAIN != 0
        {
            return Err(bad);
        }
        let claim = s
            .part_size
            .checked_add(parts.reserve_of(s.part_size))
            .ok_or(bad)?;
        s.part_base.checked_add(claim).ok_or(bad)?;
        if s.cores < 1 || !(is_app(s.core) || on_os(s)) || s.cores > app + 1 - s.core {
            return Err(Error::CoreRange {
                core: s.core,
                max: app,
            });
        }
        if s.share_group.is_empty() {
            if !s.group_cores.is_empty() {
                return Err(Error::PoolSize {
                    have: s.group_cores.len(),
                    want: 0,
                });
            }
        } else {
            if s.share_group.len() > MAX_FLIP_JOB
                || s.cores != 1
                || !s.group_cores.contains(&s.core)
            {
                return Err(Error::PoolSize {
                    have: s.group_cores.len(),
                    want: s.cores,
                });
            }
            for (k, c) in s.group_cores.iter().enumerate() {
                if !(is_app(*c) || on_os(s))
                    || s.group_cores.get(..k).is_some_and(|p| p.contains(c))
                {
                    return Err(Error::CoreRange { core: *c, max: app });
                }
            }
        }
        for p in states.get(..j).unwrap_or(&[]) {
            let p_claim = p.part_size.saturating_add(parts.reserve_of(p.part_size));
            if s.slot == p.slot
                || (s.part_base < p.part_base.saturating_add(p_claim)
                    && p.part_base < s.part_base.saturating_add(claim))
            {
                return Err(Error::NotFree {
                    base: s.part_base,
                    size: s.part_size,
                });
            }
            if !s.share_group.is_empty() && s.share_group == p.share_group {
                if s.group_cores != p.group_cores {
                    return Err(Error::PoolSize {
                        have: p.group_cores.len(),
                        want: s.group_cores.len(),
                    });
                }
                continue;
            }
            let owns = |v: &SlotState, c: usize| {
                (c >= v.core && c < v.core + v.cores) || v.group_cores.contains(&c)
            };
            if let Some(c) = (1..=app).find(|c| owns(s, *c) && owns(p, *c)) {
                return Err(Error::CoreBusy { core: c });
            }
        }
    }
    Ok(())
}

/// De core uit een handoff-blob: 0 is de OS-core ([`Core::OS`]), de rest
/// een app-core. Of hij daar mag staan, toetste [`validate_adoption`].
fn adopted_core(c: usize) -> Option<Core> {
    if c == Core::OS.get() {
        Some(Core::OS)
    } else {
        Core::new(c)
    }
}

#[cfg(test)]
mod smp_tests;

#[cfg(test)]
mod grant_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cage::CageError;
    use crate::partmem::Geometry;
    use crate::pool::tests::FakeCores;
    use crate::testutil::{FakeTimer, block_on, join2};
    use std::cell::{Cell, RefCell};
    use std::string::String;
    use std::vec;

    pub(crate) const MIB: u64 = 1 << 20;

    /// Hoe de nep-app op een stop reageert.
    #[derive(Copy, Clone, PartialEq)]
    pub(crate) enum Obey {
        /// Stopt op de kill-vlag.
        Exit,
        /// Stopt pas na de intrekking.
        Revoke,
        /// Stopt nooit (een core die niet parkeert).
        Never,
    }

    /// Hoeveel slots de nep-kooi bijhoudt: genoeg voor de toets voorbij de
    /// oude grens van 128.
    pub(crate) const FAKE_SLOTS: usize = 512;

    pub(crate) struct FakeCage {
        pub(crate) obey: Obey,
        pub(crate) exit_asked: [bool; FAKE_SLOTS],
        pub(crate) revoked: [bool; FAKE_SLOTS],
        pub(crate) cleared: Vec<(u64, u64)>,
        pub(crate) built: Vec<(usize, Region, usize, usize)>,
        pub(crate) dispatched: Vec<(usize, usize)>,
        pub(crate) secondaries: Vec<(usize, usize)>,
        pub(crate) fail_dispatch: bool,
        /// Het startschot van een secundaire faalt (onbekende uitkomst).
        pub(crate) fail_secondary: bool,
        pub(crate) fail_build: bool,
        pub(crate) smp_req: [u64; FAKE_SLOTS],
        pub(crate) calls: Cell<u32>,
        /// Wat er doorgezet werd: slot en poorten, in volgorde.
        pub(crate) published: Vec<(usize, Vec<u16>)>,
        /// Welke slots hun publicaties terugtrokken, in volgorde.
        pub(crate) unpublished: Vec<usize>,
        /// Welke slots van de switch gingen, en of hun kill-vlag toen al
        /// stond (hij hoort er pas na te komen).
        pub(crate) detached: Vec<(usize, bool)>,
        /// Een poort die al van dit slot is (de switch weigert hem).
        pub(crate) taken: Option<(u16, usize)>,
        /// Cores die nooit stil worden, wat de app ook doet (een secundaire
        /// die de intrekking niet bevestigt).
        pub(crate) stuck: [bool; FAKE_SLOTS],
        /// Welke (slot, core)-paren de stop naar stilte vroeg.
        pub(crate) asked_quiet: RefCell<Vec<(usize, usize)>>,
        /// De termijn in ms die de stop per slot op de vlag zette.
        pub(crate) exit_grace_ms: [u64; FAKE_SLOTS],
        /// Slots die boot-pending blijven tot `hog` ingetrokken is (een buur
        /// die nooit yieldt; zonder `hog` voor altijd).
        pub(crate) boot_pending: [bool; FAKE_SLOTS],
        /// Wat `SCHED_CURRENT` zegt: de vasthouder van de core.
        pub(crate) hog: Option<usize>,
        /// Draait op het moment van het startschot, met het slot: de eerste
        /// instructie van de bewoner. De toets van de system-API kijkt
        /// daar of een call van die bewoner al toegelaten wordt.
        pub(crate) on_dispatch: Option<std::boxed::Box<dyn Fn(usize)>>,
    }

    impl FakeCage {
        pub(crate) fn new(obey: Obey) -> FakeCage {
            FakeCage {
                obey,
                exit_asked: [false; FAKE_SLOTS],
                revoked: [false; FAKE_SLOTS],
                cleared: Vec::new(),
                built: Vec::new(),
                dispatched: Vec::new(),
                secondaries: Vec::new(),
                fail_dispatch: false,
                fail_secondary: false,
                fail_build: false,
                smp_req: [0; FAKE_SLOTS],
                calls: Cell::new(0),
                published: Vec::new(),
                unpublished: Vec::new(),
                detached: Vec::new(),
                taken: None,
                stuck: [false; FAKE_SLOTS],
                asked_quiet: RefCell::new(Vec::new()),
                exit_grace_ms: [0; FAKE_SLOTS],
                boot_pending: [false; FAKE_SLOTS],
                hog: None,
                on_dispatch: None,
            }
        }
    }

    impl Cage for FakeCage {
        fn clear(&mut self, base: u64, len: u64) {
            self.cleared.push((base, len));
        }
        fn build(
            &mut self,
            slot: Slot,
            part: Region,
            _: u64,
            first: Core,
            cores: usize,
        ) -> core::result::Result<(), CageError> {
            if self.fail_build {
                return Err(CageError { code: 7 });
            }
            self.built.push((slot.get(), part, first.get(), cores));
            Ok(())
        }
        fn dispatch(&mut self, slot: Slot, core: Core) -> core::result::Result<(), CageError> {
            self.dispatched.push((slot.get(), core.get()));
            if let Some(f) = &self.on_dispatch {
                f(slot.get());
            }
            if self.fail_dispatch {
                return Err(CageError { code: 1 });
            }
            Ok(())
        }
        fn dispatch_secondary(
            &mut self,
            slot: Slot,
            core: Core,
        ) -> core::result::Result<(), CageError> {
            self.secondaries.push((slot.get(), core.get()));
            if self.fail_secondary {
                return Err(CageError { code: 7 });
            }
            Ok(())
        }
        fn request_exit(&mut self, slot: Slot, grace: Duration) {
            self.calls.set(self.calls.get() + 1);
            self.exit_asked[slot.get()] = true;
            self.exit_grace_ms[slot.get()] = grace.as_millis() as u64;
        }
        fn quiet(&self, slot: Slot, core: Core) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.asked_quiet.borrow_mut().push((slot.get(), core.get()));
            if self.stuck[core.get()] {
                return false;
            }
            let i = slot.get();
            match self.obey {
                Obey::Exit => self.exit_asked[i] || self.revoked[i],
                Obey::Revoke => self.revoked[i],
                Obey::Never => false,
            }
        }
        fn live(&self, _: Slot) -> bool {
            true
        }
        fn revoke(&mut self, slot: Slot) {
            self.calls.set(self.calls.get() + 1);
            self.revoked[slot.get()] = true;
        }
        fn pending(&self, slot: Slot) -> bool {
            self.boot_pending[slot.get()] && !self.hog.is_some_and(|h| self.revoked[h])
        }
        fn holder(&self, _: Core) -> Option<Slot> {
            self.hog.and_then(Slot::new)
        }
        fn smp_request(&self, slot: Slot) -> u64 {
            self.smp_req[slot.get()]
        }
        fn clear_smp_request(&mut self, slot: Slot) {
            self.smp_req[slot.get()] = 0;
        }
        fn status(&self, _: Slot) -> Status {
            Status::default()
        }
        fn publish(
            &mut self,
            slot: Slot,
            ports: &[u16],
        ) -> impl core::future::Future<Output = core::result::Result<(), PortError>> {
            let r = match self.taken {
                Some((port, owner)) if ports.contains(&port) => {
                    Err(PortError::Taken { port, owner })
                }
                _ => {
                    self.published.push((slot.get(), ports.to_vec()));
                    Ok(())
                }
            };
            core::future::ready(r)
        }
        fn unpublish(&mut self, slot: Slot) {
            self.unpublished.push(slot.get());
        }
        fn detach(&mut self, slot: Slot) -> impl core::future::Future<Output = ()> {
            self.detached
                .push((slot.get(), self.exit_asked[slot.get()]));
            core::future::ready(())
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeConsole {
        pub(crate) lines: RefCell<Vec<String>>,
        pub(crate) app: RefCell<Vec<(usize, Vec<u8>)>>,
    }

    impl Console for &FakeConsole {
        fn log(&self, args: core::fmt::Arguments<'_>) {
            self.lines.borrow_mut().push(std::format!("{args}"));
        }
        fn app_line(&self, slot: Slot, line: &[u8]) {
            self.app.borrow_mut().push((slot.get(), line.to_vec()));
        }
    }

    impl FakeConsole {
        pub(crate) fn saw(&self, marker: &str) -> bool {
            self.lines.borrow().iter().any(|l| l.contains(marker))
        }
    }

    pub(crate) type Actor<'s, G = NoGrants> =
        Lifecycle<'s, FakeCage, FakeCores, FakeTimer, &'s FakeConsole, G>;

    pub(crate) fn actor<'s>(
        svc: &'s Servicers,
        con: &'s FakeConsole,
        obey: Obey,
        pool_mib: u64,
        cores: usize,
    ) -> Actor<'s> {
        actor_with(svc, con, obey, pool_mib, cores, NoGrants)
    }

    /// Een actor met een eigen grant-aanbieder.
    pub(crate) fn actor_with<'s, G: Grants>(
        svc: &'s Servicers,
        con: &'s FakeConsole,
        obey: Obey,
        pool_mib: u64,
        cores: usize,
        grants: G,
    ) -> Actor<'s, G> {
        let parts = PartitionPool::new(
            &[Region::new(0x8000_0000, pool_mib * MIB)],
            Region::default(),
            Geometry::FLAT,
        )
        .unwrap();
        Lifecycle::new(
            FakeCage::new(obey),
            FakeCores::new(cores),
            FakeTimer::default(),
            con,
            parts,
            Places::new(),
            svc,
            grants,
        )
    }

    pub(crate) fn s(i: usize) -> Slot {
        Slot::new(i).unwrap()
    }

    pub(crate) fn ded(cores: usize) -> Placement {
        Placement {
            group: None,
            pool_cores: 1,
            cores,
            class: None,
            prefer: None,
        }
    }

    /// Claim + arm, zoals Hop een start doet, met een servicer die zijn
    /// levensduur al uitdiende (de tests draaien geen servicer-taak).
    pub(crate) fn start<G: Grants>(
        a: &mut Actor<'_, G>,
        slot: usize,
        mib: u64,
        cores: usize,
    ) -> Result {
        start_live(a, slot, mib, cores)?;
        a.svc.ctl(s(slot)).unwrap().gone.set();
        Ok(())
    }

    /// Claim + arm; de servicer-taak draait de test zelf.
    fn start_live<G: Grants>(a: &mut Actor<'_, G>, slot: usize, mib: u64, cores: usize) -> Result {
        let g = block_on(a.claim(StartSpec::new(s(slot), mib * MIB, ded(cores))))?;
        block_on(a.arm(g, 0x4001_0000))
    }

    /// Eén core die iedereen deelt: zo past elk aantal slots op de cores.
    fn shared() -> Placement {
        Placement {
            group: Some(crate::pool::group_name(b"many").unwrap()),
            ..ded(1)
        }
    }

    /// Een start waarvoor de actor het slot kiest; geeft het slot.
    fn start_free<G: Grants>(a: &mut Actor<'_, G>, mib: u64) -> Result<Slot> {
        let g = block_on(a.claim(StartSpec::free(mib * MIB, shared(), None)))?;
        let slot = g.slot();
        block_on(a.arm(g, 0x4001_0000))?;
        a.svc.ctl(slot).unwrap().gone.set();
        Ok(slot)
    }

    // Er is geen maximum aantal slots (Derek, 01-11): de kern deelt het
    // laagste vrije nummer uit, ver voorbij de oude grens van 128 (en de
    // 32 van een board), zolang het geheugen het draagt; een vrijgekomen
    // nummer komt als eerste terug, zoals een fd.
    #[test]
    fn far_more_slots_than_the_old_ceiling_take_the_lowest_free_number() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 2048, 4);
        for want in 1..=300 {
            assert_eq!(start_free(&mut a, 4).unwrap(), s(want));
        }
        assert_eq!(
            svc.current(s(300)),
            a.resident(s(300)).map(|r| r.generation)
        );
        stop(&mut a, 7).unwrap();
        stop(&mut a, 200).unwrap();
        assert_eq!(
            start_free(&mut a, 4).unwrap(),
            s(7),
            "the lowest free number"
        );
        assert_eq!(start_free(&mut a, 4).unwrap(), s(200));
        assert_eq!(start_free(&mut a, 4).unwrap(), s(301));
        // De lijst van de bezette slots, in stukken, zoals SLOTS ze geeft.
        let mut page = |after, max| match block_on(a.handle(Request::Occupied { after, max })) {
            Response::Occupied(v) => v.iter().map(|s| s.get()).collect::<Vec<_>>(),
            r => panic!("{r:?}"),
        };
        assert_eq!(page(0, 3), [1, 2, 3]);
        assert_eq!(page(299, 10), [300, 301]);
        assert!(page(301, 10).is_empty());
    }

    // Wat niet past, is dezelfde weigering als altijd: de partitie past niet,
    // en er blijft niets geclaimd; het nummer is daarna nog vrij.
    #[test]
    fn a_slot_that_does_not_fit_is_refused_and_claims_nothing() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 16, 4);
        for want in 1..=4 {
            assert_eq!(start_free(&mut a, 4).unwrap(), s(want));
        }
        let r = block_on(a.claim(StartSpec::free(4 * MIB, shared(), None)));
        assert!(
            matches!(r, Err(Error::NoPartition { .. })),
            "a fifth 4 MiB in a 16 MiB pool: {r:?}"
        );
        assert!(a.resident(s(5)).is_none() && a.parts.partition_of(s(5)).is_none());
        assert_eq!(a.places.placement_of(s(5)), None, "the core went back");
        stop(&mut a, 2).unwrap();
        assert_eq!(start_free(&mut a, 4).unwrap(), s(2));
    }

    pub(crate) fn stop<G: Grants>(a: &mut Actor<'_, G>, slot: usize) -> Result {
        block_on(a.stop(s(slot), Duration::from_millis(50)))
    }

    /// De adoptie van kern B, met het vertrek van elke servicer klaar zoals
    /// bij [`start`]: de servicer-taken draait de toets niet.
    pub(crate) fn adopt<G: Grants>(a: &mut Actor<'_, G>, states: &[SlotState]) -> Result<usize> {
        let n = a.adopt(states)?;
        for st in states {
            a.svc.ctl(s(st.slot)).unwrap().gone.set();
        }
        Ok(n)
    }

    /// Een start met volumes, zoals de kern Hop plaatst.
    pub(crate) fn start_with_mounts(
        a: &mut Actor<'_>,
        slot: usize,
        mib: u64,
        cores: usize,
        mounts: Vec<Mount>,
    ) -> Result {
        let mut spec = StartSpec::new(s(slot), mib * MIB, ded(cores));
        spec.mounts = mounts;
        let g = block_on(a.claim(spec))?;
        block_on(a.arm(g, 0x4001_0000))?;
        a.svc.ctl(s(slot)).unwrap().gone.set();
        Ok(())
    }

    /// Een start met een jobnaam en volumes, zoals Hop een jobspec start.
    pub(crate) fn start_job(
        a: &mut Actor<'_>,
        slot: usize,
        job: &[u8],
        mounts: Vec<Mount>,
    ) -> Result {
        let mut spec = StartSpec::new(s(slot), 8 * MIB, ded(1));
        spec.job = job.to_vec();
        spec.mounts = mounts;
        let g = block_on(a.claim(spec))?;
        block_on(a.arm(g, 0x4001_0000))?;
        a.svc.ctl(s(slot)).unwrap().gone.set();
        Ok(())
    }

    /// Een start met gepubliceerde poorten, zoals Hop een jobspec start.
    fn start_with_ports(a: &mut Actor<'_>, slot: usize, ports: &[u16]) -> Result {
        let mut spec = StartSpec::new(s(slot), 16 * MIB, ded(1));
        spec.ports = ports.to_vec();
        let g = block_on(a.claim(spec))?;
        block_on(a.arm(g, 0x4001_0000))?;
        a.svc.ctl(s(slot)).unwrap().gone.set();
        Ok(())
    }

    #[test]
    fn ports_open_before_the_start_and_close_at_the_stop() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start_with_ports(&mut a, 2, &[80, 8443]).unwrap();
        assert_eq!(a.cage.published, [(2, vec![80, 8443])]);
        // Vóór het startschot: de publicatie kwam vóór de bouw.
        assert_eq!(a.cage.built.len(), 1);
        assert!(con.saw("slot 2: 2 port(s) published tcp+udp on the uplink: :80 :8443"));
        assert!(a.cage.unpublished.is_empty());
        stop(&mut a, 2).unwrap();
        assert_eq!(a.cage.unpublished, [2]);
        assert!(con.saw("HOPOS_SLOT_UNPUBLISH"));
        // Zonder poorten wordt er niets doorgezet, en een stop zegt niets.
        start(&mut a, 3, 16, 1).unwrap();
        assert_eq!(a.cage.published.len(), 1);
        let lines = con.lines.borrow().len();
        stop(&mut a, 3).unwrap();
        assert!(
            !con.lines.borrow()[lines..]
                .iter()
                .any(|l| l.contains("UNPUBLISH"))
        );
    }

    #[test]
    fn a_taken_port_refuses_the_start_and_leaves_nothing() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        a.cage.taken = Some((80, 1));
        let e = start_with_ports(&mut a, 2, &[8080, 80]).unwrap_err();
        assert_eq!(
            e,
            Error::PortTaken {
                slot: 2,
                port: 80,
                owner: 1
            }
        );
        assert!(con.saw("slot 2: port 80 is taken by slot 1 HOPOS_SLOT_PUBLISH_FAIL"));
        // Niets gebouwd, niets gedispatcht, alles ingetrokken en terug.
        assert!(a.cage.built.is_empty() && a.cage.dispatched.is_empty());
        assert_eq!(a.cage.unpublished, [2]);
        assert_eq!(a.status(s(2)).occupancy, Occupancy::Empty);
        assert!(a.parts.partition_of(s(2)).is_none());
        // Het slot is daarna gewoon weer te starten.
        a.cage.taken = None;
        start_with_ports(&mut a, 2, &[8080]).unwrap();
    }

    #[test]
    fn a_failed_build_withdraws_the_ports() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        a.cage.fail_build = true;
        assert!(start_with_ports(&mut a, 2, &[80]).is_err());
        assert_eq!(a.cage.published, [(2, vec![80])]);
        assert_eq!(a.cage.unpublished, [2]);
        assert_eq!(a.status(s(2)).occupancy, Occupancy::Empty);
    }

    #[test]
    fn start_places_scrubs_builds_and_dispatches() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start(&mut a, 1, 16, 1).unwrap();
        let st = a.status(s(1));
        assert_eq!(st.occupancy, Occupancy::Running);
        assert_eq!(a.cage.built.len(), 1);
        assert_eq!(a.cage.dispatched, vec![(1, 1)]);
        assert_eq!(svc.current(s(1)), Some(1), "servicer not registered");
    }

    // E3: een nieuwe eigenaar krijgt gewist geheugen, in brokken.
    #[test]
    fn new_owner_memory_is_clean() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        let g = block_on(a.claim(StartSpec::new(s(2), 10 * MIB, ded(1)))).unwrap();
        let r = g.region();
        let covered: u64 = a.cage.cleared.iter().map(|c| c.1).sum();
        assert_eq!(covered, r.size);
        assert_eq!(a.cage.cleared.first().unwrap().0, r.base);
        assert!(a.cage.cleared.iter().all(|c| c.1 <= SCRUB_CHUNK));
        a.abort(g);
    }

    #[test]
    fn abort_returns_memory_that_never_ran() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 32, 4);
        let g = block_on(a.claim(StartSpec::new(s(1), 32 * MIB, ded(1)))).unwrap();
        a.abort(g);
        assert_eq!(a.status(s(1)).occupancy, Occupancy::Empty);
        start(&mut a, 2, 32, 1).unwrap();
    }

    #[test]
    fn duplicate_start_claim_failure_keeps_live_owner() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start(&mut a, 1, 16, 1).unwrap();
        let before = a.parts.partition_of(s(1));
        assert_eq!(start(&mut a, 1, 16, 1), Err(Error::StillOwned { slot: 1 }));
        assert_eq!(a.parts.partition_of(s(1)), before);
        assert_eq!(a.status(s(1)).occupancy, Occupancy::Running);
    }

    #[test]
    fn build_failure_is_an_ordinary_rollback() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 32, 4);
        a.cage.fail_build = true;
        assert!(matches!(
            start(&mut a, 1, 32, 1),
            Err(Error::Cage { slot: 1, code: 7 })
        ));
        assert!(a.cage.dispatched.is_empty());
        assert_eq!(a.status(s(1)).occupancy, Occupancy::Empty);
        a.cage.fail_build = false;
        start(&mut a, 1, 32, 1).unwrap();
    }

    // Een startschot met een onbekende uitkomst: de kern kan de isolatie
    // niet bevestigen, dus `fatal` (§13.5; tot 01-11 een quarantaine).
    #[test]
    #[should_panic(expected = "HOPOS_FATAL slot 1: the start on core 1 failed")]
    fn a_failed_dispatch_is_fatal() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Never, 32, 2);
        a.cage.fail_dispatch = true;
        let _ = start(&mut a, 1, 32, 1);
    }

    // Een stop die ook na de intrekking niet stil wordt: `fatal`, na de
    // intrekking (eerst de harde kill).
    #[test]
    #[should_panic(expected = "HOPOS_FATAL slot 1: not stopped after revocation")]
    fn an_unconfirmed_stop_is_fatal() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Never, 32, 2);
        start(&mut a, 1, 32, 1).unwrap();
        let _ = stop(&mut a, 1);
    }

    #[test]
    fn stop_confirms_before_release_and_frees_everything() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 32, 1);
        start(&mut a, 1, 32, 1).unwrap();
        assert!(a.cage.detached.is_empty());
        stop(&mut a, 1).unwrap();
        // De app had zijn net tot hij stil was, en de vlag droeg de termijn.
        assert_eq!(
            a.cage.detached,
            [(1, true)],
            "off the switch only after the kill flag"
        );
        assert_eq!(a.cage.exit_grace_ms[1], 50, "the grace rides on the flag");
        assert!(!a.cage.revoked[1], "cooperative exit needed no revocation");
        assert_eq!(a.status(s(1)).occupancy, Occupancy::Empty);
        assert_eq!(svc.current(s(1)), None);
        start(&mut a, 3, 32, 1).unwrap();
    }

    #[test]
    fn empty_stop_does_not_touch_another_cage_on_same_numbered_core() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start(&mut a, 2, 16, 1).unwrap(); // Landt op core 1.
        let before = a.cage.calls.get();
        stop(&mut a, 1).unwrap();
        assert_eq!(a.cage.calls.get(), before, "empty stop touched the cage");
        assert!(!a.cage.exit_asked[2]);
    }

    #[test]
    fn stop_wake_uses_cage_context_and_physical_core() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Revoke, 64, 6);
        start(&mut a, 5, 16, 1).unwrap(); // Core 1.
        start(&mut a, 1, 16, 2).unwrap(); // Cores 2 en 3.
        stop(&mut a, 1).unwrap();
        assert_eq!(
            &a.cores.kicks[1..5],
            &[0, 1, 1, 0],
            "kick missed the physical span"
        );
        assert!(a.cage.revoked[1] && !a.cage.revoked[5]);
    }

    #[test]
    fn smp_request_translates_virtual_cpu_to_assigned_core() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 6);
        start(&mut a, 9, 8, 1).unwrap(); // Core 1.
        start(&mut a, 4, 8, 3).unwrap(); // Cores 2..4.
        // De app vraagt virtuele CPU 5 (= slot 4 + 1): dat is core 3.
        a.cage.smp_req[4] = 5;
        a.smp(s(4));
        assert_eq!(a.cage.secondaries, vec![(4, 3)]);
        assert_eq!(a.cage.smp_req[4], 0);
        // Buiten de vertrouwde breedte: geweigerd en beantwoord.
        a.cage.smp_req[4] = 7;
        a.smp(s(4));
        assert_eq!(a.cage.secondaries.len(), 1);
        assert_eq!(a.cage.smp_req[4], 0);
        assert!(con.saw("HOPOS_SMP_REJECT"));
    }

    #[test]
    fn ownership_stop_serializes_smp_dispatch() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start(&mut a, 1, 8, 2).unwrap();
        a.cage.smp_req[1] = 2;
        stop(&mut a, 1).unwrap();
        // Het verzoek komt pas na de stop bij de actor aan: geen dispatch.
        block_on(a.handle(Request::Smp(s(1))));
        assert!(
            a.cage.secondaries.is_empty(),
            "stopped owner got a secondary"
        );
    }

    // De actor-lus met een brievenbus en een antwoordplek.
    #[test]
    fn lifecycle_actor_answers_over_the_mailbox() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 4> = Mailbox::new();
        let client = async {
            let spec = StartSpec::new(s(1), 8 * MIB, ded(1));
            let Response::Granted(g) = call(&inbox, &reply, Request::Claim(spec)).await.unwrap()
            else {
                panic!("no grant");
            };
            let r = call(&inbox, &reply, Request::Arm { grant: g, entry: 0 })
                .await
                .unwrap();
            assert!(matches!(r, Response::Done));
            let Response::Status(st) = call(&inbox, &reply, Request::Status(s(1))).await.unwrap()
            else {
                panic!("no status");
            };
            st.occupancy
        };
        let mut actor_loop = core::pin::pin!(a.run(&inbox));
        let mut client = core::pin::pin!(client);
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        let got = loop {
            let _ = actor_loop.as_mut().poll(&mut cx);
            if let core::task::Poll::Ready(v) = client.as_mut().poll(&mut cx) {
                break v;
            }
        };
        assert_eq!(got, Occupancy::Running);
    }

    struct FakeOutbox<'c> {
        lines: Vec<Vec<u8>>,
        live: &'c Cell<bool>,
    }

    impl Outbox for FakeOutbox<'_> {
        fn read_into(&mut self, buf: &mut [u8]) -> Option<(u8, usize)> {
            let l = self.lines.pop()?;
            buf[..l.len()].copy_from_slice(&l);
            Some((RING_LOG, l.len()))
        }
        fn corrupt(&self) -> bool {
            false
        }
        fn live(&self) -> bool {
            self.live.get()
        }
        fn smp_pending(&self) -> bool {
            false
        }
    }

    /// Een outbox die de test na de start nog vult.
    struct SharedOutbox<'c> {
        lines: &'c RefCell<Vec<Vec<u8>>>,
        live: &'c Cell<bool>,
    }

    impl Outbox for SharedOutbox<'_> {
        fn read_into(&mut self, buf: &mut [u8]) -> Option<(u8, usize)> {
            let l = self.lines.borrow_mut().pop()?;
            buf[..l.len()].copy_from_slice(&l);
            Some((RING_LOG, l.len()))
        }
        fn corrupt(&self) -> bool {
            false
        }
        fn live(&self) -> bool {
            self.live.get()
        }
        fn smp_pending(&self) -> bool {
            false
        }
    }

    // O10: de servicer pollt niet. Een regel die zonder bel in de outbox
    // komt, blijft liggen; de bel (de kick van de app-core) laat hem lezen,
    // en na de val van de context gaat hij op de volgende bel weg.
    #[test]
    fn the_servicer_waits_on_its_bell_not_on_a_tick() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let live = Cell::new(true);
        let lines = RefCell::new(Vec::new());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start_live(&mut a, 1, 8, 1).unwrap();
        let inbox: Mailbox<Envelope<'_>, 4> = Mailbox::new();
        let timer = FakeTimer::default();
        let conr = &con;
        let mut buf = [0u8; 64];
        let servicer = servicer_task(
            s(1),
            svc.ctl(s(1)).unwrap(),
            |_| SharedOutbox {
                lines: &lines,
                live: &live,
            },
            &timer,
            &conr,
            &inbox,
            &mut buf,
        );
        let mut servicer = core::pin::pin!(servicer);
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        let ctl = svc.ctl(s(1)).unwrap();
        assert!(servicer.as_mut().poll(&mut cx).is_pending());
        lines.borrow_mut().push(b"hi".to_vec());
        for _ in 0..4 {
            assert!(servicer.as_mut().poll(&mut cx).is_pending());
        }
        assert!(con.app.borrow().is_empty(), "no bell, no read");
        svc.ring(s(1));
        assert!(servicer.as_mut().poll(&mut cx).is_pending());
        assert_eq!(con.app.borrow().as_slice(), &[(1, b"hi".to_vec())]);
        live.set(false);
        assert!(servicer.as_mut().poll(&mut cx).is_pending());
        assert!(!ctl.gone.is_set(), "the fall is seen on the bell");
        svc.ring(s(1));
        assert!(
            servicer.as_mut().poll(&mut cx).is_ready(),
            "one lifetime, one task"
        );
        assert!(ctl.gone.is_set(), "the servicer left after the fall");
    }

    // De stop (de evict) komt vóór de servicer zijn beurt had: de laatste
    // regels van de app (zijn afscheid) komen toch door, dan pas gone.
    #[test]
    fn servicer_drains_the_outbox_before_it_leaves_on_a_stop() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let live = Cell::new(false);
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start_live(&mut a, 1, 8, 1).unwrap();
        let inbox: Mailbox<Envelope<'_>, 4> = Mailbox::new();
        let timer = FakeTimer::default();
        let conr = &con;
        let mut buf = [0u8; 64];
        let servicer = servicer_task(
            s(1),
            svc.ctl(s(1)).unwrap(),
            |_| FakeOutbox {
                lines: vec![b"bye".to_vec(), b"stopping".to_vec()],
                live: &live,
            },
            &timer,
            &conr,
            &inbox,
            &mut buf,
        );
        let mut servicer = core::pin::pin!(servicer);
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        let ctl = svc.ctl(s(1)).unwrap();
        ctl.stop.set();
        for _ in 0..4 {
            if servicer.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
        assert_eq!(
            con.app.borrow().as_slice(),
            &[(1, b"stopping".to_vec()), (1, b"bye".to_vec())]
        );
        assert!(ctl.gone.is_set(), "the servicer left after the drain");
    }

    // Een outbox vol logregels: de servicer geeft na elke SERVICER_BATCH
    // records een ronde af, in plaats van de core te houden tot de ring leeg
    // is (30-09).
    #[test]
    fn servicer_yields_after_a_batch_of_records() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let live = Cell::new(true);
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start_live(&mut a, 1, 8, 1).unwrap();
        let inbox: Mailbox<Envelope<'_>, 4> = Mailbox::new();
        let timer = FakeTimer::default();
        let conr = &con;
        let mut buf = [0u8; 64];
        let lines: Vec<Vec<u8>> = (0..3 * SERVICER_BATCH + 5)
            .map(|i| std::format!("line {i}").into_bytes())
            .collect();
        let servicer = servicer_task(
            s(1),
            svc.ctl(s(1)).unwrap(),
            |_| FakeOutbox {
                lines: lines.clone(),
                live: &live,
            },
            &timer,
            &conr,
            &inbox,
            &mut buf,
        );
        let mut servicer = core::pin::pin!(servicer);
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        assert!(servicer.as_mut().poll(&mut cx).is_pending());
        assert_eq!(con.app.borrow().len(), SERVICER_BATCH);
        assert!(servicer.as_mut().poll(&mut cx).is_pending());
        assert_eq!(con.app.borrow().len(), 2 * SERVICER_BATCH);
        for _ in 0..4 {
            let _ = servicer.as_mut().poll(&mut cx);
        }
        assert_eq!(con.app.borrow().len(), lines.len());
    }

    // Evict = stop.set(); gone.wait(): de stop wacht tot de servicer weg is,
    // en de logregels van de app komen door.
    #[test]
    fn stop_evicts_servicer_and_waits_for_gone() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let live = Cell::new(true);
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        start_live(&mut a, 1, 8, 1).unwrap();
        let inbox: Mailbox<Envelope<'_>, 4> = Mailbox::new();
        let timer = FakeTimer::default();
        let conr = &con;
        let mut buf = [0u8; 64];
        let servicer = servicer_task(
            s(1),
            svc.ctl(s(1)).unwrap(),
            |_| FakeOutbox {
                lines: vec![b"hello".to_vec()],
                live: &live,
            },
            &timer,
            &conr,
            &inbox,
            &mut buf,
        );
        let mut servicer = core::pin::pin!(servicer);
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        for _ in 0..4 {
            let _ = servicer.as_mut().poll(&mut cx);
        }
        assert_eq!(con.app.borrow().as_slice(), &[(1, b"hello".to_vec())]);
        let ((), r) = join2(
            async {
                let mut done = false;
                for _ in 0..64 {
                    core::future::poll_fn(|cx| {
                        if !done {
                            done = servicer.as_mut().poll(cx).is_ready();
                        }
                        core::task::Poll::Ready(())
                    })
                    .await;
                    sync::yield_now().await;
                }
            },
            a.stop(s(1), Duration::from_millis(20)),
        );
        r.unwrap();
        assert_eq!(svc.current(s(1)), None);
    }

    fn st(slot: usize, core: usize, cores: usize, base: u64) -> SlotState {
        SlotState {
            slot,
            core,
            cores,
            part_base: base,
            part_size: 32 * MIB,
            ..SlotState::default()
        }
    }

    fn pool() -> PartitionPool {
        PartitionPool::new(
            &[Region::new(0x8000_0000, 256 * MIB)],
            Region::default(),
            Geometry::FLAT,
        )
        .unwrap()
    }

    #[test]
    fn adoption_rejects_conflicting_owners() {
        let b = FakeCores::new(4);
        let p = pool();
        let first = st(1, 1, 1, 0x8000_0000);
        let second = st(2, 2, 1, 0x8400_0000);
        validate_adoption(&[first.clone(), second.clone()], &b, &Places::new(), &p).unwrap();
        let changes: [fn(&mut SlotState); 6] = [
            |s| s.slot = 1,
            |s| s.part_base = 0x8000_0000,
            |s| s.part_base = u64::MAX - 0x1f_ffff,
            |s| s.core = 1,
            |s| s.core = 0,
            |s| s.cores = 4,
        ];
        for change in changes {
            let mut s2 = second.clone();
            change(&mut s2);
            assert!(validate_adoption(&[first.clone(), s2], &b, &Places::new(), &p).is_err());
        }
        let mut grouped = first;
        grouped.share_group = b"trusted".to_vec();
        grouped.group_cores = vec![1, 2];
        assert!(
            validate_adoption(&[grouped, second], &b, &Places::new(), &p).is_err(),
            "empty group core overlapped dedicated owner"
        );
    }

    // PORT.md beslissing 2: na een flip neemt de nieuwe kern Hop op de
    // OS-core over, want zijn groep mag die core delen; een gewone app die
    // volgens het blob op de OS-core staat, weigert hij.
    #[test]
    fn adoption_takes_hop_on_the_os_core_and_nobody_else() {
        let b = FakeCores::new(1);
        let p = pool();
        let mut places = Places::new();
        places.share_os_core(crate::pool::HOP_GROUP).unwrap();
        let mut hop = st(1, 0, 1, 0x8000_0000);
        hop.share_group = crate::pool::HOP_GROUP.to_vec();
        hop.group_cores = vec![0];
        let app = st(2, 1, 1, 0x8400_0000);
        validate_adoption(&[hop.clone(), app.clone()], &b, &places, &p).unwrap();
        // Dezelfde Hop bij een kern die de OS-core niet deelt: geweigerd.
        assert!(validate_adoption(&[hop.clone()], &b, &Places::new(), &p).is_err());
        // Een andere groep of een dedicated app op core 0: geweigerd.
        let mut other = hop.clone();
        other.share_group = b"web".to_vec();
        assert!(validate_adoption(&[other], &b, &places, &p).is_err());
        assert!(validate_adoption(&[st(3, 0, 1, 0x8800_0000)], &b, &places, &p).is_err());
        // Twee cores op de OS-core: geweigerd.
        let mut wide = hop.clone();
        wide.cores = 2;
        assert!(validate_adoption(&[wide], &b, &places, &p).is_err());
        // En de actor plaatst hem terug op Core::OS.
        assert_eq!(adopted_core(0), Some(Core::OS));
        assert_eq!(adopted_core(1), Core::new(1));
    }

    #[test]
    fn adoption_rejects_physical_smp_overlap() {
        let b = FakeCores::new(4);
        let p = pool();
        let first = st(6, 2, 2, 0x8000_0000);
        for core in [2, 3] {
            let second = st(1, core, 1, 0x8200_0000);
            assert!(
                validate_adoption(&[first.clone(), second.clone()], &b, &Places::new(), &p)
                    .is_err()
            );
            assert!(validate_adoption(&[second, first.clone()], &b, &Places::new(), &p).is_err());
        }
    }

    #[test]
    fn adoption_separates_cages_from_smp_cores() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 256, 4);
        let mut states = [st(1, 3, 2, 0x8000_0000), st(2, 1, 1, 0x8200_0000)];
        states[1].ports = vec![80];
        assert_eq!(a.adopt(&states).unwrap(), 2);
        // De NAT van de nieuwe kern is leeg: de poorten gaan open als het
        // net luistert, alleen slot 2 had er.
        assert!(a.cage.published.is_empty());
        block_on(a.republish());
        assert_eq!(a.cage.published, [(2, vec![80])]);
        assert_eq!(
            a.places.placement_of(s(1)).map(|p| (p.0.get(), p.1)),
            Some((3, 2))
        );
        assert_eq!(
            a.places.placement_of(s(2)).map(|p| (p.0.get(), p.1)),
            Some((1, 1))
        );
        let b = FakeCores::new(4);
        assert!(!a.places.run_free(&b, 3, 1, None) && !a.places.run_free(&b, 4, 1, None));
        assert!(
            a.places.run_free(&b, 2, 1, None),
            "cage 2 reserved physical core 2"
        );
        // De bewoners zijn weer van iemand: hun geheugen is niet vrij.
        assert!(a.parts.partition_of(s(1)).is_some());
        assert_eq!(
            svc.current(s(2)),
            Some(2),
            "adopted resident has no servicer"
        );
        assert!(con.saw("adopted"));
    }

    #[test]
    fn flip_preserves_whole_group_pool() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 256, 3);
        let mut spec = StartSpec::new(s(4), 32 * MIB, ded(1));
        spec.placement.group = Some({
            let mut g = GroupName::new();
            for b in b"trusted" {
                g.push(*b).unwrap();
            }
            g
        });
        spec.placement.pool_cores = 2;
        let g = block_on(a.claim(spec.clone())).unwrap();
        block_on(a.arm(g, 0)).unwrap();
        let states = a.snapshot().unwrap();
        assert_eq!(states[0].group_cores, vec![1, 2]);

        // De nieuwe kern.
        let (svc2, con2) = (Servicers::new(), FakeConsole::default());
        let mut b = actor(&svc2, &con2, Obey::Exit, 256, 3);
        b.adopt(&states).unwrap();
        assert_eq!(start(&mut b, 3, 8, 1), Ok(()));
        assert_eq!(b.places.placement_of(s(3)).unwrap().0.get(), 3);
        assert!(
            start(&mut b, 6, 8, 1).is_err(),
            "empty group core was given away"
        );
        let mut join = spec;
        join.slot = Some(s(5));
        let g = block_on(b.claim(join)).unwrap();
        assert_eq!(b.places.placement_of(s(5)).unwrap().0.get(), 2);
        block_on(b.arm(g, 0)).unwrap();
    }

    /// De volumes van een bewoner gaan mee over de flip: de snapshot draagt
    /// ze, en de nieuwe kern zet ze bij de adoptie terug in de tabel van de
    /// hopfs-actor, met dezelfde regel als bij een start.
    #[test]
    fn volumes_travel_through_the_flip() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 256, 3);
        let vol = Mount {
            local: b"/data".to_vec(),
            shared: b"/volumes/demo".to_vec(),
        };
        start_with_mounts(&mut a, 2, 8, 1, vec![vol.clone()]).unwrap();
        assert!(con.saw("slot 2: 1 volume(s) mounted: /data -> /volumes/demo HOPOS_SLOT_MOUNTS"));
        let states = a.snapshot().unwrap();
        assert_eq!(states[0].mounts, core::slice::from_ref(&vol));

        let (svc2, con2) = (Servicers::new(), FakeConsole::default());
        let mut b = actor(&svc2, &con2, Obey::Exit, 256, 3);
        b.adopt(&states).unwrap();
        assert_eq!(svc2.with_mounts(s(2), <[Mount]>::to_vec), Some(vec![vol]));
        assert!(con2.saw("slot 2: 1 volume(s) mounted: /data -> /volumes/demo HOPOS_SLOT_MOUNTS"));
    }

    /// De arena van de videocodec (docs/media.md, haak 2): een blok buiten
    /// de partities, via de eigenaar van de pool. Geen partitie kan het
    /// daarna krijgen; terug is de capaciteit weer heel.
    #[test]
    fn a_device_block_leaves_the_pool_and_comes_back() {
        let svc = Servicers::new();
        let con = FakeConsole::default();
        let mut a = actor(&svc, &con, Obey::Exit, 256, 2);
        let before = a.parts.capacity();
        let r = block_on(a.handle(Request::ReserveDevice {
            size: 64 * MIB - 5,
            what: "the codec arena",
        }));
        let Response::Device(arena) = r else {
            panic!("{r:?}");
        };
        assert_eq!(arena.size, 64 * MIB, "naar boven op de korrel");
        assert_eq!(a.parts.capacity(), before - 64 * MIB);
        assert!(con.saw("pool: 64 MB for the codec arena at"));
        assert!(con.saw("HOPOS_POOL_DEVICE base="));
        // Een partitie komt er nooit in.
        start(&mut a, 1, 128, 1).unwrap();
        let part = a.parts.partition_of(s(1)).unwrap();
        assert!(!part.overlaps(arena), "{part:?} in {arena:?}");
        // Meer dan er is: een weigering met de getallen, geen blok.
        let r = block_on(a.handle(Request::ReserveDevice {
            size: 128 * MIB,
            what: "the codec arena",
        }));
        assert!(
            matches!(r, Response::Failed(Error::NoPartition { .. })),
            "{r:?}"
        );
        assert!(con.saw("pool: no 128 MB for the codec arena"));
        assert!(con.saw("HOPOS_POOL_DEVICE_FAIL"));
        // Het ijzer kwam niet op: terug.
        let r = block_on(a.handle(Request::ReleaseDevice {
            region: arena,
            what: "the codec arena",
        }));
        assert!(matches!(r, Response::Done), "{r:?}");
        assert!(con.saw("HOPOS_POOL_DEVICE_RELEASE"));
        stop(&mut a, 1).unwrap();
        assert_eq!(a.parts.capacity(), before);
    }
}
