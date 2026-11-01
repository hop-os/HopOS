//! De bestandscalls van de system-API en de hopfs-actor (Go:
//! `OLD/metal/kern/slots/rpc.go`).
//!
//! Paden van een app worden hier tegen de volume-tabel van DIE levensduur
//! geresolved: dat is de toegangsgrens op opslag. Zichtbaar is de eigen,
//! lege root (`/.tasks/slot<N>`, schoon bij elke start) plus uitsluitend de
//! expliciet gemounte gedeelde mappen ([`crate::slots::Mount`], `local ->
//! shared`). Een `..` is geen pad maar een weigering ([`Error::Denied`]).
//!
//! # Eigendom
//!
//! De [`FsActor`] is de enige eigenaar van de [`Fs`] (PORT.md §3, de rij
//! `hopfs.FS.mu`): één taak met een [`Mailbox`], en de boom is zijn
//! `&mut self`. Een verbindingstaak stuurt een [`FsCall`] en VERPLAATST
//! daarin haar eigen call- en antwoordbuffer (handboek §1.2); de actor leest
//! de data uit de ene, schrijft het antwoord in de andere en geeft beide
//! terug via de [`Reply`] van de verbinding. Er leeft dus geen lening over
//! een `.await`, en er wordt per call niets gealloceerd: een bestandscall is
//! een heet pad, en de heap is voor koude paden (handboek §6; zijn vrije
//! lijsten zijn begrensd, niet snel, en een vol plafond hoort geen call
//! halverwege te laten falen).
//!
//! De I/O loopt buiten de boom: een call wordt synchroon gepland (pad,
//! node, een verse run) en doet daarna zijn blok-I/O als future op de
//! wachtrij van het device (`blkdev::Queue`), met de boom alleen kort
//! geleend tussen twee stappen (PORT.md §3: "de metadata-lening loopt nooit
//! over de NVMe-await"). Zo staan er calls van meerdere apps tegelijk op de
//! schijf (tot [`FS_DEPTH`]); de regels die het contract dragen, staan bij
//! [`FsActor`]. Les van 30-09: tot dan wachtte de actor synchroon op het
//! device, en de periodieke commit (FLUSH, de boom, FLUSH) hield op een
//! trage schijf de hele OS-core tot 7 s stil: geen tik, Hop geen beurt, de
//! switch geen frame. GEMETEN 01-10 op de M4 (vitals `rand=20000`, elke
//! app één call tegelijk): met één call tegelijk in de actor (M26) haalden
//! één, twee en vier apps samen 7.600, 9.200 en 9.300 willekeurige 4 KiB-
//! lezingen per seconde; met de pool (M30, M31) 7.800, 14.400 en 25.000, en
//! acht apps 36.000, met de OS-core dan vol (`busy_ms=1000`).
//!
//! # De kern als lezer
//!
//! De kern leest zelf ook van het volume, zonder slot en zonder generatie
//! ([`FsMsg::KernRead`], [`kern_read`], [`read_file`]): de firmware van de
//! videocodec (`/firmware/<naam>.fwb`, zestien blobs van zo'n 300 KB) en de
//! teststream van het meetinstrument. Dezelfde actor, dezelfde brievenbus:
//! de lezing is een bericht als elk ander en loopt dus nooit door een
//! schrijf van een app heen. Het pad is een hopfs-pad, geen app-pad; de
//! eigen roots van de taken ([`TASKS_DIR`]) zijn ook voor de kern dicht,
//! want er is geen kern-lezing die daar iets te zoeken heeft.
//!
//! # Vastleggen
//!
//! [`committer`] legt de boom vast elke [`COMMIT_EVERY`] (Go:
//! `fsCommitEvery`) en meteen na de stop van een slot (de servicer-tabel
//! luidt zijn bel), zodat wat een app schreef een stroomuitval overleeft.
//! De data zelf staat er al; de commit is de boom die haar terugvindt.

use crate::cage::{Console, Timer};
use crate::hopfs::{self, BLOCK_SIZE, BatchRead, BlockIo, Fs, ReadStep, Tree};
use crate::slots::{Mount, Reply, Servicers, try_push};
use crate::system::MAX_IO_CHUNK;
use crate::{Error, Result, SLOT_CAP, Slot};
use abi::hopabi::{
    HDR_LEN, OP_LIST, OP_READ, OP_READ_MANY, OP_REMOVE, OP_STAT, OP_SYNC, OP_TRUNCATE, OP_WRITE,
    STATUS_ERROR, STATUS_OK, many,
};
use alloc::vec::Vec;
use bounded::BoundedVec;
use core::future::Future;
use core::ops::Range;
use core::pin::Pin;
use core::task::Poll;
use core::time::Duration;
use sync::mpsc::Mailbox;
use sync::{Futures, LocalCell, select};

/// De map onder hopfs waar de eigen roots van de taken wonen.
pub const TASKS_DIR: &[u8] = b"/.tasks";
/// Het langste pad na resolutie. De ABI laat 64 KiB toe (`path_len u16`);
/// een pad van meer dan een kilobyte is een fout van de app, en zo past de
/// resolutie in twee vaste buffers op de stack van de actor.
pub const MAX_PATH: usize = 1024;
/// Hoeveel volumes één levensduur hoogstens draagt (de flip-grens van
/// `kernflip::MAX_FLIP_MOUNTS`).
pub const MAX_MOUNTS: usize = crate::kernflip::MAX_FLIP_MOUNTS;
/// De diepte van de brievenbus van de actor: elke verbindingstaak heeft
/// hoogstens één call tegelijk uitstaan, plus de committer.
pub const FS_DEPTH: usize = 16;
/// Hoe vaak de boom vastgelegd wordt als hij veranderde (Go:
/// `fsCommitEvery`): de grens van wat een harde stroomuitval kost aan namen
/// en groottes.
pub const COMMIT_EVERY: Duration = Duration::from_secs(10);

/// Is `op` een bestandscall die deze module bedient?
#[must_use]
pub const fn is_fs_op(op: u8) -> bool {
    matches!(
        op,
        OP_STAT | OP_READ | OP_WRITE | OP_LIST | OP_REMOVE | OP_TRUNCATE | OP_SYNC | OP_READ_MANY
    )
}

// ---------------------------------------------------------------------------
// Paden: normaliseren, de volume-tabel en de resolutie.
// ---------------------------------------------------------------------------

/// Een pad in een vaste buffer: de resolutie alloceert niets.
pub struct PathBuf {
    b: [u8; MAX_PATH],
    n: usize,
}

impl PathBuf {
    /// Een leeg pad.
    #[must_use]
    pub const fn new() -> PathBuf {
        PathBuf {
            b: [0; MAX_PATH],
            n: 0,
        }
    }

    /// De bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.b.get(..self.n).unwrap_or(&[])
    }

    fn clear(&mut self) {
        self.n = 0;
    }

    fn push(&mut self, s: &[u8]) -> Result {
        let end = self.n + s.len();
        let d = self.b.get_mut(self.n..end).ok_or(Error::TooLarge {
            len: end,
            max: MAX_PATH,
        })?;
        d.copy_from_slice(s);
        self.n = end;
        Ok(())
    }

    fn set(&mut self, s: &[u8]) -> Result {
        self.clear();
        self.push(s)
    }
}

impl Default for PathBuf {
    fn default() -> Self {
        PathBuf::new()
    }
}

/// Normaliseert een app-pad naar de vorm `/a/b` in `out` (Go: `cleanAbs`):
/// lege en `.`-segmenten vallen weg, `..` is een weigering (de app heeft
/// buiten zijn zicht niets te zoeken). Een leeg pad is `/`.
pub fn clean_abs(p: &[u8], out: &mut PathBuf) -> Result {
    out.clear();
    for seg in p.split(|b| *b == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => return Err(Error::Denied),
            _ => {
                out.push(b"/")?;
                out.push(seg)?;
            }
        }
    }
    if out.n == 0 {
        out.push(b"/")?;
    }
    Ok(())
}

/// Ligt `p` (genormaliseerd) in of op `dir`?
fn under(p: &[u8], dir: &[u8]) -> bool {
    p == dir || (p.starts_with(dir) && p.get(dir.len()) == Some(&b'/'))
}

/// Normaliseert de volumes van een start tot een tabel, langste `local`
/// eerst (voor de prefix-resolutie). Go: `mountTable`, plus twee grenzen die
/// daar ontbraken: een volume is een gedeelde map, dus nooit `/` en nooit
/// iets onder [`TASKS_DIR`] (dan zag een app de root van een andere).
pub fn mount_table(mounts: &[Mount]) -> Result<Vec<Mount>> {
    if mounts.len() > MAX_MOUNTS {
        return Err(Error::Full { cap: MAX_MOUNTS });
    }
    let mut t: Vec<Mount> = Vec::new();
    t.try_reserve_exact(mounts.len())
        .map_err(|_| Error::OutOfMemory {
            bytes: core::mem::size_of_val(mounts),
        })?;
    let mut buf = PathBuf::new();
    for m in mounts {
        clean_abs(&m.local, &mut buf)?;
        let local = crate::slots::try_vec(buf.as_bytes())?;
        clean_abs(&m.shared, &mut buf)?;
        let shared = crate::slots::try_vec(buf.as_bytes())?;
        // De taak houdt haar eigen root; `/` overmounten zou hem verbergen.
        if local == b"/" || shared == b"/" || under(&shared, TASKS_DIR) {
            return Err(Error::Denied);
        }
        if t.iter().any(|x| x.local == local) {
            return Err(Error::BadPath); // Dubbel local-pad.
        }
        try_push(&mut t, Mount { local, shared })?;
    }
    // In-place en zonder allocatie: `sort_by` zou een hulpbuffer vragen.
    t.sort_unstable_by(|a, b| b.local.len().cmp(&a.local.len()));
    Ok(t)
}

/// Schrijft de eigen root van `slot` (`/.tasks/slot<N>`) in `out`.
pub fn push_root(slot: Slot, out: &mut PathBuf) -> Result {
    out.push(TASKS_DIR)?;
    out.push(b"/slot")?;
    let mut digits = [0u8; 3];
    let mut n = slot.get();
    let mut i = digits.len();
    loop {
        i -= 1;
        if let Some(d) = digits.get_mut(i) {
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 || i == 0 {
            break;
        }
    }
    out.push(digits.get(i..).unwrap_or(&[]))
}

/// Vertaalt een app-pad naar een hopfs-pad in `out` (Go: `resolve`): een
/// gemount prefix wordt de gedeelde map, de rest valt in de eigen root van
/// het slot. Geeft de index van het volume, of `None` voor de eigen root.
pub fn resolve(slot: Slot, mounts: &[Mount], p: &[u8], out: &mut PathBuf) -> Result<Option<usize>> {
    let mut cp = PathBuf::new();
    clean_abs(p, &mut cp)?;
    let cp = cp.as_bytes();
    for (i, m) in mounts.iter().enumerate() {
        if under(cp, &m.local) {
            out.set(&m.shared)?;
            out.push(cp.get(m.local.len()..).unwrap_or(&[]))?;
            return Ok(Some(i));
        }
    }
    out.clear();
    push_root(slot, out)?;
    if cp != b"/" {
        out.push(cp)?;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// De berichten.
// ---------------------------------------------------------------------------

/// Eén bestandscall, met de buffers van de verbinding als waarde.
///
/// `buf` is de callbuffer (pad en data staan erin op `path` en `data`),
/// `out` de antwoordbuffer: de data van een antwoord komt op
/// `out[HDR_LEN..]`, de kop schrijft de verbinding zelf.
#[derive(Debug)]
pub struct FsCall {
    /// Het slot van de peer.
    pub slot: Slot,
    /// De levensduur waarvoor de verbinding toegelaten werd.
    pub generation: u32,
    /// De operatie (`abi::hopabi::OP_*`).
    pub op: u8,
    /// De offset.
    pub off: u64,
    /// De lengte (read) of de maat (truncate).
    pub n: u64,
    /// Waar het pad in `buf` staat.
    pub path: Range<usize>,
    /// Waar de data in `buf` staat.
    pub data: Range<usize>,
    /// De callbuffer.
    pub buf: Vec<u8>,
    /// De antwoordbuffer.
    pub out: Vec<u8>,
}

/// Het antwoord van de actor: de buffers terug, en de uitkomst als
/// `(size, data_len)`.
#[derive(Debug)]
pub struct FsDone {
    /// De callbuffer, terug.
    pub buf: Vec<u8>,
    /// De antwoordbuffer, terug; de data staat op `out[HDR_LEN..]`.
    pub out: Vec<u8>,
    /// Het `size`-veld van het antwoord en het aantal databytes.
    pub result: Result<(u64, usize)>,
}

/// Waarom de boom vastgelegd wordt, voor de ene consoleregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommitWhy {
    /// De periodieke commit ([`COMMIT_EVERY`]).
    Periodic,
    /// Een slot stopte.
    Stopped(Slot),
}

/// Een bericht aan de actor.
#[derive(Debug)]
pub enum FsMsg {
    /// Een bestandscall; het antwoord gaat naar de [`Reply`].
    Call(FsCall),
    /// Leg de boom vast als hij veranderde.
    Commit(CommitWhy),
    /// De kern-flip: leg de boom vast en neem daarna geen call meer aan
    /// (elke call krijgt [`Error::Busy`], luid) tot [`FsMsg::Thaw`]. Het
    /// antwoord draagt de vastgelegde generatie als `size`.
    Freeze,
    /// De flip ging niet door: de actor neemt weer calls aan.
    Thaw,
    /// Een lezing voor de kern zelf ([`kern_read`]); het antwoord is een
    /// [`FsDone`] met het pad in `buf`, de data vooraan in `out` en als
    /// uitkomst `(bestandsmaat, gelezen bytes)`.
    KernRead(KernRead),
}

/// Een lezing voor de kern zelf: een hopfs-pad, zonder slot en generatie.
/// De buffers gaan als waarde heen en terug (handboek §1.2), zodat een
/// aanroeper die in stukken leest (de teststream) nooit per stuk alloceert:
/// een lus van stukken is een heet pad, en de heap is voor koude paden
/// (handboek §6).
#[derive(Debug)]
pub struct KernRead {
    /// Het absolute hopfs-pad (`/firmware/hevcdec.fwb`).
    pub path: Vec<u8>,
    /// De offset in het bestand.
    pub off: u64,
    /// De bestemming: hooguit `out.len()` bytes, vooraan. Leeg is een stat.
    pub out: Vec<u8>,
}

/// Een bericht met zijn antwoordplek.
pub struct FsEnvelope<'a> {
    msg: FsMsg,
    reply: Option<&'a Reply>,
}

/// De brievenbus van de hopfs-actor.
pub type FsInbox<'a> = Mailbox<FsEnvelope<'a>, FS_DEPTH>;

/// Stuurt een bestandscall naar de actor en wacht op het antwoord. Zit de
/// brievenbus vol, dan komt de call (met de buffers) meteen terug.
pub async fn call<'a>(
    inbox: &FsInbox<'a>,
    reply: &'a Reply,
    c: FsCall,
) -> core::result::Result<FsDone, FsCall> {
    match ask(inbox, reply, FsMsg::Call(c)).await {
        Ok(d) => Ok(d),
        Err(FsMsg::Call(c)) => Err(c),
        Err(other) => Ok(busy(other)),
    }
}

/// Bevriest de actor voor de kern-flip en geeft de generatie die hij net
/// vastlegde: wat daarna op de schijf staat, is precies wat de nieuwe kern
/// mount. Een volle brievenbus is [`Error::Busy`] (en dan geen flip).
pub async fn freeze<'a>(inbox: &FsInbox<'a>, reply: &'a Reply) -> Result<u64> {
    match ask(inbox, reply, FsMsg::Freeze).await {
        Ok(d) => d.result.map(|(generation, _)| generation),
        Err(_) => Err(Error::Busy),
    }
}

/// Leest voor de kern zelf: `r.out` vooraan gevuld vanaf `r.off`, of alleen
/// de maat als `r.out` leeg is. Geeft altijd beide buffers terug (`buf` is
/// het pad), ook bij een fout; een volle brievenbus is [`Error::Busy`].
/// Een termijn legt de aanroeper er zelf omheen (een `select` met zijn
/// timer), zoals bij [`freeze`].
pub async fn kern_read<'a>(inbox: &FsInbox<'a>, reply: &'a Reply, r: KernRead) -> FsDone {
    ask(inbox, reply, FsMsg::KernRead(r))
        .await
        .unwrap_or_else(busy)
}

/// Eén bericht aan de actor en het antwoord; een volle brievenbus geeft
/// het bericht terug.
async fn ask<'a>(
    inbox: &FsInbox<'a>,
    reply: &'a Reply,
    msg: FsMsg,
) -> core::result::Result<FsDone, FsMsg> {
    let env = FsEnvelope {
        msg,
        reply: Some(reply),
    };
    sync::oneshot::call(inbox, &reply.fs, env)
        .await
        .map_err(|sync::Full(env)| env.msg)
}

/// Het antwoord op een bericht dat de actor nooit zag: [`Error::Busy`],
/// met de buffers terug.
fn busy(msg: FsMsg) -> FsDone {
    let (buf, out) = match msg {
        FsMsg::Call(c) => (c.buf, c.out),
        FsMsg::KernRead(r) => (r.path, r.out),
        FsMsg::Commit(_) | FsMsg::Freeze | FsMsg::Thaw => (Vec::new(), Vec::new()),
    };
    FsDone {
        buf,
        out,
        result: Err(Error::Busy),
    }
}

/// Leest een heel bestand voor de kern (de firmware van de codec): eerst de
/// maat, dan precies zoveel bytes in één lezing. Groter dan `max` is
/// [`Error::TooLarge`] vóór er iets gealloceerd wordt; een bestand dat
/// tussen maat en lezing kromp, is [`Error::Corrupt`] in plaats van een
/// blob met een staart van nullen.
pub async fn read_file<'a>(
    inbox: &FsInbox<'a>,
    reply: &'a Reply,
    path: &[u8],
    max: usize,
) -> Result<Vec<u8>> {
    let r = KernRead {
        path: crate::slots::try_vec(path)?,
        off: 0,
        out: Vec::new(),
    };
    let d = kern_read(inbox, reply, r).await;
    let (size, _) = d.result?;
    let len = usize::try_from(size).unwrap_or(usize::MAX);
    if len > max {
        return Err(Error::TooLarge { len, max });
    }
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| Error::OutOfMemory { bytes: len })?;
    out.resize(len, 0);
    let r = KernRead {
        path: d.buf,
        off: 0,
        out,
    };
    let d = kern_read(inbox, reply, r).await;
    let (_, n) = d.result?;
    if n != len {
        return Err(Error::Corrupt { at: n });
    }
    Ok(d.out)
}

/// Ontdooit de actor na een flip die niet doorging. Vuur-en-vergeet: een
/// volle brievenbus is `false`, en dan zegt de aanroeper het luid.
#[must_use]
pub fn thaw(inbox: &FsInbox<'_>) -> bool {
    inbox
        .try_send(FsEnvelope {
            msg: FsMsg::Thaw,
            reply: None,
        })
        .is_ok()
}

// ---------------------------------------------------------------------------
// De actor.
// ---------------------------------------------------------------------------

/// De eigenaar van hopfs: boom, extents en vrije lijst ([`Tree`]), en de
/// calls die in de lucht zijn.
///
/// Meerdere calls tegelijk: wat binnenkomt, wordt synchroon gepland (pad,
/// generatie, node, een verse run) en loopt daarna als future in een vaste
/// [`Futures`] van [`FS_DEPTH`] plaatsen, elk met zijn eigen I/O op de
/// wachtrij (`blkdev::Queue`). De regels die het contract dragen
/// (docs/storage-sync.md), allemaal in `Desk::admit`:
///
/// - **Volgorde per app**: één call per slot tegelijk, in de volgorde van
///   de brievenbus. Een app wacht toch al op zijn antwoord; zo ziet een
///   tweede verbinding van dezelfde app ook nooit iets anders. Alleen
///   lezingen (`OP_READ`, `OP_READ_MANY`) mogen naast elkaar: een lees
///   verandert niets, dus twee bundels van één app over twee verbindingen
///   staan samen op het device. Een lees na een schrijf wacht nog steeds op
///   die schrijf, en een schrijf op de lezingen ervoor.
/// - **Synchroon blijft synchroon**: het antwoord komt pas als de I/O van
///   het device terug is (de future is dan klaar).
/// - **OP_SYNC is een barrière plus een echte Flush**: de eerdere calls van
///   dat slot zijn terug (de regel hierboven), dan legt hij de boom vast of
///   flusht hij; één vastlegging tegelijk.
/// - **Eén schrijver per bestand**, en remove, truncate, het klaarzetten
///   van een nieuwe levensduur en de flip-bevriezing alleen als er niets
///   anders loopt (ze geven blokken en nodes vrij).
///
/// Wat niet mag, wacht vooraan in de rij (de volgende in de brievenbus
/// wacht erachter): zo blijft de volgorde van binnenkomst de volgorde van
/// beginnen.
pub struct FsActor<'s, D, L> {
    tree: Tree<D>,
    desk: Desk<'s, L>,
}

/// Alles van de actor behalve de boom: de planning en de regels. Los van
/// de boom, omdat de calls in de lucht de boom lenen terwijl de actor
/// plant.
struct Desk<'s, L> {
    svc: &'s Servicers,
    log: L,
    /// Per slot de levensduur waarvoor root en volumes klaarstaan.
    prepared: [Option<u32>; SLOT_CAP + 1],
    /// Per slot de levensduur waarvan de eerste schrijf in een volume al
    /// een regel kreeg.
    saved: [Option<u32>; SLOT_CAP + 1],
    /// Mislukte commits: de eerste paar krijgen een regel, daarna tellen we.
    commit_fails: u64,
    /// Blokfouten onder een call; dezelfde regel.
    io_fails: u64,
    /// Bevroren voor de kern-flip ([`FsMsg::Freeze`]): calls worden
    /// geweigerd, commits overgeslagen. De boom op de schijf is dan die
    /// van de nieuwe kern.
    frozen: bool,
    /// Geweigerde calls tijdens de bevriezing.
    frozen_calls: u64,
    path: PathBuf,
    /// Per slot: wat hij in de lucht heeft.
    busy: [Busy; SLOT_CAP + 1],
    /// De nodes met een schrijf in de lucht.
    writing: [Option<usize>; FS_DEPTH],
    /// Een vastlegging (commit, sync, freeze) is in de lucht.
    committing: bool,
    /// Er loopt iets dat alleen mag lopen (truncate, freeze).
    alone: bool,
}

/// Hoeveel mislukte commits (en blokfouten) een eigen regel krijgen.
const LOUD_COMMIT_FAILS: u64 = 3;

/// Wat een slot in de lucht heeft: de volgorde per app ([`FsActor`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    /// Niets.
    Idle,
    /// Alleen lezingen, zoveel.
    Reads(usize),
    /// Eén call die geen lees is.
    Other,
}

/// Wat een call na het plannen nog moet doen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    /// Klaar zonder I/O, met dit antwoord.
    Done(u64, usize),
    /// Lezen uit node `n`.
    Read(usize),
    /// Een bundel lezingen uit node `n` ([`OP_READ_MANY`]).
    ReadMany(usize),
    /// Schrijven naar node `n`.
    Write(usize),
    /// Node `n` op maat `c.n`.
    Truncate(usize),
    /// De barrière.
    Sync,
}

/// Een bericht dat in de lucht is: zijn buffers en wat het doet.
enum Job<'a> {
    Call {
        c: FsCall,
        work: Work,
        /// In een volume (voor de regel bij de eerste schrijf).
        volume: bool,
        reply: Option<&'a Reply>,
    },
    Kern {
        r: KernRead,
        node: usize,
        size: u64,
        reply: Option<&'a Reply>,
    },
    Commit(CommitWhy),
    Freeze(Option<&'a Reply>),
}

/// Wat [`Desk::admit`] met een bericht doet.
enum Admit<'a> {
    /// Nog niet: het blijft vooraan in de rij.
    Wait(FsEnvelope<'a>),
    /// Afgehandeld (beantwoord of genegeerd).
    Done,
    /// In de lucht.
    Go(Job<'a>),
}

/// De I/O van een call, met alleen de boom kort geleend. Geeft de uitkomst
/// als `(size, data_len)`.
async fn run_call<D: BlockIo + Copy>(
    t: &Tree<D>,
    c: &mut FsCall,
    work: Work,
) -> Result<(u64, usize)> {
    match work {
        Work::Done(size, len) => Ok((size, len)),
        Work::Read(n) => {
            let room = c.out.len().saturating_sub(HDR_LEN).min(MAX_IO_CHUNK);
            let len = usize::try_from(c.n).unwrap_or(usize::MAX).min(room);
            let dst = c
                .out
                .get_mut(HDR_LEN..HDR_LEN + len)
                .ok_or(Error::TooLarge { len, max: room })?;
            let got = hopfs::read(t, n, c.off, dst).await?;
            Ok((got as u64, got))
        }
        Work::ReadMany(n) => read_many(t, n, c).await,
        Work::Write(n) => {
            let data = c.buf.get(c.data.clone()).ok_or(Error::Corrupt { at: 0 })?;
            hopfs::write(t, n, c.off, data).await?;
            Ok((data.len() as u64, 0))
        }
        Work::Truncate(n) => {
            hopfs::truncate(t, n, c.n).await?;
            Ok((c.n, 0))
        }
        Work::Sync => Ok((hopfs::sync(t).await?, 0)),
    }
}

/// Een gebundelde lees ([`OP_READ_MANY`], de draadvorm in [`many`]): alle
/// opdrachten samen op de wachtrij van het device, één antwoord.
///
/// Per ronde krijgt elke opdracht die nog bytes wacht haar volgende stap
/// (een gat is meteen nul), en gaan alle stappen in één batch naar het
/// device (`BlockIo::read_batch`: alles erop, één wachter): een lees van
/// 4 KiB is één ronde. Een stap leest rechtstreeks in het stuk van de
/// opdracht in `c.out`, een rand (een deel van een blok) in een blok
/// achteraan in `c.out`, één per opdracht. Zo draagt de future geen blok en
/// geen future per opdracht, en blijft de plaats in de pool van de actor zo
/// groot als die van een gewone lees. Daarna schuiven de bytes aaneen achter
/// de tabel met de uitkomsten. Een opdracht die faalt, krijgt een fout en
/// nul bytes, de andere blijven staan. [`Desk::plan`] toetste de lijst en
/// de ruimte.
async fn read_many<D: BlockIo + Copy>(
    t: &Tree<D>,
    n: usize,
    c: &mut FsCall,
) -> Result<(u64, usize)> {
    const M: usize = many::MAX_OPS;
    let FsCall { buf, out, data, .. } = c;
    let list = buf.get(data.clone()).ok_or(Error::Corrupt { at: 0 })?;
    let count = (list.len() / many::OP_LEN).min(M);
    let short = Error::TooLarge { len: count, max: M };
    let body = out.get_mut(HDR_LEN..).ok_or(short)?;
    let (table, rest) = body
        .split_at_mut_checked(count * many::RESULT_LEN)
        .ok_or(short)?;
    let edge = rest.len().checked_sub(count * BLOCK_SIZE).ok_or(short)?;
    let (room, edges) = rest.split_at_mut(edge);
    // Per opdracht: haar offset, waar haar stuk begint en hoe lang het is,
    // hoeveel bytes er komen en al zijn, en of ze faalde.
    let (mut off, mut at, mut len) = ([0u64; M], [0usize; M], [0usize; M]);
    let (mut want, mut done, mut failed) = ([0usize; M], [0usize; M], [false; M]);
    let mut pos = 0usize;
    for i in 0..count {
        let (o, l) = many::op(list, i).ok_or(short)?;
        (off[i], at[i], len[i]) = (o, pos, l as usize);
        pos += l as usize;
        match t.borrow().read_len(n, o, l as usize) {
            Ok(w) => want[i] = w,
            Err(_) => failed[i] = true,
        }
    }
    let mut disk = t.borrow().disk();
    loop {
        // De volgende stap per opdracht: (lba, lengte, de rand als die er is).
        let mut step = [None::<(u64, usize, Option<usize>)>; M];
        for i in 0..count {
            while !failed[i] && done[i] < want[i] && step[i].is_none() {
                let s = t
                    .borrow()
                    .read_step(n, off[i] + done[i] as u64, want[i] - done[i]);
                match s {
                    Ok(ReadStep::Hole { len: k }) => {
                        let from = at[i] + done[i];
                        if let Some(z) = room.get_mut(from..from + k) {
                            z.fill(0);
                        }
                        done[i] += k;
                    }
                    Ok(ReadStep::Whole { lba, len: k }) => step[i] = Some((lba, k, None)),
                    Ok(ReadStep::Part { lba, at: a, len: k }) => step[i] = Some((lba, k, Some(a))),
                    Err(_) => failed[i] = true,
                }
            }
        }
        if step.iter().all(Option::is_none) {
            break;
        }
        let mut ok = [false; M];
        {
            // De stukken van deze ronde, op volgorde uit `room` en `edges`
            // geknipt: elke lees haar eigen bytes.
            let mut batch: BoundedVec<BatchRead<'_>, M> = BoundedVec::new();
            let mut who = [0usize; M];
            let (mut r, mut e, mut base) = (&mut *room, &mut *edges, 0usize);
            for (i, s) in step.iter().enumerate().take(count) {
                let (block, rest_e) = core::mem::take(&mut e)
                    .split_at_mut_checked(BLOCK_SIZE)
                    .ok_or(short)?;
                e = rest_e;
                let Some((lba, k, part)) = *s else { continue };
                let from = at[i] + done[i];
                let (_, tail) = core::mem::take(&mut r)
                    .split_at_mut_checked(from - base)
                    .ok_or(short)?;
                let (dst, tail) = tail.split_at_mut_checked(k).ok_or(short)?;
                (r, base) = (tail, from + k);
                let into = if part.is_some() { block } else { dst };
                who[batch.len()] = i;
                batch.push(BatchRead::new(lba, into)).map_err(|_| short)?;
            }
            disk.read_batch(batch.as_mut_slice()).await;
            for (k, op) in batch.as_slice().iter().enumerate() {
                ok[who[k]] = op.result.is_ok();
            }
        }
        for (i, s) in step.iter().enumerate().take(count) {
            let Some((_, k, part)) = *s else { continue };
            if !ok[i] {
                failed[i] = true;
                continue;
            }
            if let Some(a) = part {
                let from = at[i] + done[i];
                let src = edges.get(i * BLOCK_SIZE + a..i * BLOCK_SIZE + a + k);
                if let (Some(d), Some(src)) = (room.get_mut(from..from + k), src) {
                    d.copy_from_slice(src);
                }
            }
            done[i] += k;
        }
    }
    let mut to = 0usize;
    for i in 0..count {
        let (k, status) = if failed[i] {
            (0, STATUS_ERROR)
        } else {
            (done[i].min(len[i]), STATUS_OK)
        };
        room.copy_within(at[i]..at[i] + k, to);
        many::put_result(table, i, k as u32, status);
        to += k;
    }
    Ok((to as u64, count * many::RESULT_LEN + to))
}

/// Een bericht in de lucht, tot het klaar is.
async fn work<'a, D: BlockIo + Copy>(
    t: &Tree<D>,
    mut job: Job<'a>,
) -> (Job<'a>, Result<(u64, usize)>) {
    let r = match &mut job {
        Job::Call { c, work, .. } => run_call(t, c, *work).await,
        Job::Kern { r, node, size, .. } => hopfs::read(t, *node, r.off, &mut r.out)
            .await
            .map(|n| (*size, n)),
        Job::Commit(_) => hopfs::commit(t).await.map(|g| (g.unwrap_or(0), 0)),
        Job::Freeze(_) => hopfs::commit(t).await.map(|_| (t.borrow().generation(), 0)),
    };
    (job, r)
}

impl<'s, D: BlockIo + Copy, L: Console> FsActor<'s, D, L> {
    /// Een actor over een gemounte `fs`, met de volume-tabellen uit `svc`.
    pub fn new(fs: Fs<D>, svc: &'s Servicers, log: L) -> FsActor<'s, D, L> {
        FsActor {
            tree: LocalCell::cell(fs),
            desk: Desk {
                svc,
                log,
                prepared: [None; SLOT_CAP + 1],
                saved: [None; SLOT_CAP + 1],
                commit_fails: 0,
                io_fails: 0,
                frozen: false,
                frozen_calls: 0,
                path: PathBuf::new(),
                busy: [Busy::Idle; SLOT_CAP + 1],
                writing: [None; FS_DEPTH],
                committing: false,
                alone: false,
            },
        }
    }

    /// De lus: plannen wat binnenkomt, tot [`FS_DEPTH`] calls in de lucht,
    /// elk antwoord terug naar zijn plek zodra zijn I/O terug is.
    pub async fn run(&mut self, inbox: &FsInbox<'_>) {
        let FsActor { tree, desk } = self;
        let tree = &*tree;
        let mut pool = core::pin::pin!(Futures::<_, FS_DEPTH>::new());
        let mut next: Option<FsEnvelope<'_>> = None;
        core::future::poll_fn(|cx| {
            loop {
                while let Poll::Ready(Some((job, r))) = pool.as_mut().poll_next(cx) {
                    desk.land(job, r);
                }
                let env = match next.take() {
                    Some(e) => e,
                    None => match Pin::new(&mut inbox.recv()).poll(cx) {
                        Poll::Ready(e) => e,
                        Poll::Pending => return Poll::Pending,
                    },
                };
                // Wat wacht, wacht op een call in de lucht: die wekt de taak
                // als hij klaar is.
                match desk.admit(tree, env, pool.is_empty(), pool.is_full()) {
                    Admit::Wait(e) => {
                        next = Some(e);
                        return Poll::Pending;
                    }
                    Admit::Done => {}
                    Admit::Go(job) => {
                        if pool.as_mut().push(work(tree, job)).is_err() {
                            // `admit` liet hem alleen toe met een vrije plaats.
                            desk.log.log(format_args!(
                                "hopfs: no room for an admitted call HOPOS_FS_FAIL"
                            ));
                        }
                    }
                }
            }
        })
        .await
    }

    /// Eén bestandscall, van begin tot eind, zonder brievenbus (de tests).
    /// Geeft het `size`-veld van het antwoord en het aantal databytes op
    /// `c.out[HDR_LEN..]`.
    #[cfg(test)]
    pub(crate) async fn handle(&mut self, c: &mut FsCall) -> Result<(u64, usize)> {
        let (work, volume) = self.desk.plan(&self.tree, c)?;
        let r = run_call(&self.tree, c, work).await;
        if r.is_ok() && volume && matches!(work, Work::Write(_)) {
            self.desk.first_save(c);
        }
        r
    }

    /// Eén lezing voor de kern ([`FsMsg::KernRead`]): de bestandsmaat en
    /// hoeveel bytes er vooraan in `out` kwamen. Een map is [`Error::Kind`],
    /// een pad onder [`TASKS_DIR`] [`Error::Denied`]. Zonder brievenbus (de
    /// tests).
    #[cfg(test)]
    pub(crate) async fn kern_read(
        &mut self,
        path: &[u8],
        off: u64,
        out: &mut [u8],
    ) -> Result<(u64, usize)> {
        let (node, size) = self.desk.plan_kern(&self.tree, path)?;
        if out.is_empty() {
            return Ok((size, 0));
        }
        let n = hopfs::read(&self.tree, node, off, out).await?;
        Ok((size, n))
    }

    /// Legt de boom vast als hij veranderde; één regel per nieuwe generatie.
    /// Zonder brievenbus (de tests).
    #[cfg(test)]
    pub(crate) async fn commit(&mut self, why: CommitWhy) {
        let r = hopfs::commit(&self.tree).await;
        self.desk.committed(why, r);
    }

    /// De bevriezing van de kern-flip: eerst vastleggen, dan pas dicht.
    /// Zonder brievenbus (de tests).
    #[cfg(test)]
    pub(crate) async fn freeze(&mut self) -> Result<(u64, usize)> {
        let r = hopfs::commit(&self.tree).await;
        self.desk.frozen_after(&self.tree, r)
    }

    /// Geeft de boom terug (een test die opnieuw mount).
    #[cfg(test)]
    pub(crate) fn into_fs(self) -> Fs<D> {
        self.tree.into_inner().into_inner()
    }

    /// De boom zelf (een test die kijkt).
    #[cfg(test)]
    pub(crate) fn fs(&mut self) -> &mut Fs<D> {
        self.tree.get_mut().get_mut()
    }

    /// De boom als gedeelde lening (een test die leest zoals een call).
    #[cfg(test)]
    pub(crate) fn tree(&self) -> &Tree<D> {
        &self.tree
    }
}

impl<L: Console> Desk<'_, L> {
    /// Mag `env` nu beginnen? Zo ja, dan plant hij het (synchroon) en geeft
    /// het als [`Job`]; wat zonder I/O kan, wordt meteen beantwoord.
    fn admit<'a, D: BlockIo + Copy>(
        &mut self,
        t: &Tree<D>,
        env: FsEnvelope<'a>,
        idle: bool,
        full: bool,
    ) -> Admit<'a> {
        if self.alone || full {
            return Admit::Wait(env);
        }
        let reply = env.reply;
        match env.msg {
            FsMsg::Call(c) => self.admit_call(t, c, reply, idle),
            FsMsg::Commit(_) if self.frozen => Admit::Done,
            FsMsg::Commit(why) if !self.committing => {
                self.committing = true;
                Admit::Go(Job::Commit(why))
            }
            FsMsg::Freeze if idle => {
                (self.committing, self.alone) = (true, true);
                Admit::Go(Job::Freeze(reply))
            }
            msg @ (FsMsg::Commit(_) | FsMsg::Freeze) => Admit::Wait(FsEnvelope { msg, reply }),
            FsMsg::KernRead(r) => self.admit_kern(t, r, reply),
            FsMsg::Thaw => {
                self.frozen = false;
                self.log.log(format_args!(
                    "hopfs: thawed, the flip did not go through ({} call(s) were refused) HOPOS_FS_THAWED",
                    self.frozen_calls
                ));
                self.frozen_calls = 0;
                Admit::Done
            }
        }
    }

    fn admit_call<'a, D: BlockIo + Copy>(
        &mut self,
        t: &Tree<D>,
        mut c: FsCall,
        reply: Option<&'a Reply>,
        idle: bool,
    ) -> Admit<'a> {
        if self.frozen {
            // Luid, de eerste paar keer: een call die hier strandt hoort de
            // aanroeper opnieuw te doen op de nieuwe kern.
            self.frozen_calls += 1;
            if self.frozen_calls <= LOUD_COMMIT_FAILS {
                self.log.log(format_args!(
                    "hopfs: slot {} op {} refused, frozen for the kernel flip HOPOS_FS_FROZEN_CALL",
                    c.slot, c.op
                ));
            }
            answer(reply, c, Err(Error::Busy));
            return Admit::Done;
        }
        let i = c.slot.get();
        let live = self.svc.current(c.slot) == Some(c.generation);
        let reads = matches!(c.op, OP_READ | OP_READ_MANY);
        let busy = match self.busy.get(i).copied().unwrap_or(Busy::Idle) {
            Busy::Idle => false,
            Busy::Reads(_) => !reads,
            Busy::Other => true,
        };
        // Een nieuwe levensduur wist zijn root en geeft daarmee nodes en
        // blokken vrij: dat wacht op een lege pool. Een geadopteerde (de
        // warme flip) houdt zijn root en geeft niets vrij.
        let fresh = live
            && self.prepared.get(i).copied().flatten() != Some(c.generation)
            && !self.svc.root_kept(c.slot, c.generation);
        let wait = busy
            || ((fresh || matches!(c.op, OP_REMOVE | OP_TRUNCATE)) && !idle)
            || (c.op == OP_SYNC && self.committing);
        if wait {
            return Admit::Wait(FsEnvelope {
                msg: FsMsg::Call(c),
                reply,
            });
        }
        let (work, volume) = match self.plan(t, &mut c) {
            Ok(p) => p,
            Err(e) => {
                answer(reply, c, Err(e));
                return Admit::Done;
            }
        };
        match work {
            Work::Done(size, len) => {
                answer(reply, c, Ok((size, len)));
                return Admit::Done;
            }
            Work::Write(n) if self.writing.contains(&Some(n)) => {
                // Een andere app schrijft in hetzelfde bestand (een gedeeld
                // volume): na hem. Het plannen was zonder gevolgen (het
                // bestand bestaat al), dus straks opnieuw.
                return Admit::Wait(FsEnvelope {
                    msg: FsMsg::Call(c),
                    reply,
                });
            }
            Work::Write(n) => {
                if let Some(s) = self.writing.iter_mut().find(|s| s.is_none()) {
                    *s = Some(n);
                }
            }
            Work::Truncate(_) => self.alone = true,
            Work::Sync => self.committing = true,
            Work::Read(_) | Work::ReadMany(_) => {}
        }
        if let Some(b) = self.busy.get_mut(i) {
            *b = match (*b, work) {
                (Busy::Reads(k), Work::Read(_) | Work::ReadMany(_)) => Busy::Reads(k + 1),
                (_, Work::Read(_) | Work::ReadMany(_)) => Busy::Reads(1),
                _ => Busy::Other,
            };
        }
        Admit::Go(Job::Call {
            c,
            work,
            volume,
            reply,
        })
    }

    fn admit_kern<'a, D: BlockIo + Copy>(
        &mut self,
        t: &Tree<D>,
        mut r: KernRead,
        reply: Option<&'a Reply>,
    ) -> Admit<'a> {
        // Bevroren weigert ook de kern: na de bevriezing hoort de schijf van
        // de volgende kern, en een lezing die daarna nog lukt, zou een
        // vergeten afhankelijkheid verstoppen.
        let plan = if self.frozen {
            Err(Error::Busy)
        } else {
            self.plan_kern(t, &r.path)
        };
        match plan {
            Ok((node, size)) if !r.out.is_empty() => Admit::Go(Job::Kern {
                r,
                node,
                size,
                reply,
            }),
            res => {
                let result = res.map(|(_, size)| (size, 0));
                if let Some(reply) = reply {
                    reply.fs.put(FsDone {
                        buf: core::mem::take(&mut r.path),
                        out: core::mem::take(&mut r.out),
                        result,
                    });
                }
                Admit::Done
            }
        }
    }

    /// Een call is terug van zijn I/O: de regels vrij, het antwoord naar
    /// zijn plek.
    fn land(&mut self, job: Job<'_>, r: Result<(u64, usize)>) {
        match job {
            Job::Call {
                c,
                work,
                volume,
                reply,
            } => {
                if let Some(b) = self.busy.get_mut(c.slot.get()) {
                    *b = match *b {
                        Busy::Reads(k) if k > 1 => Busy::Reads(k - 1),
                        _ => Busy::Idle,
                    };
                }
                match work {
                    Work::Write(n) => {
                        if let Some(s) = self.writing.iter_mut().find(|s| **s == Some(n)) {
                            *s = None;
                        }
                        if r.is_ok() && volume {
                            self.first_save(&c);
                        }
                    }
                    Work::Truncate(_) => self.alone = false,
                    Work::Sync => self.committing = false,
                    Work::ReadMany(_) => self.many_failed(&c, &r),
                    Work::Read(_) | Work::Done(..) => {}
                }
                self.io_failed(&c, &r);
                answer(reply, c, r);
            }
            Job::Kern { r: k, reply, .. } => {
                if let Some(reply) = reply {
                    reply.fs.put(FsDone {
                        buf: k.path,
                        out: k.out,
                        result: r,
                    });
                }
            }
            Job::Commit(why) => {
                self.committing = false;
                self.committed(why, r.map(|(g, _)| (g != 0).then_some(g)));
            }
            Job::Freeze(reply) => {
                (self.committing, self.alone) = (false, false);
                let result = match r {
                    Ok((g, _)) => Ok(self.freeze_done(g)),
                    Err(e) => Err(e),
                };
                if let Some(reply) = reply {
                    reply.fs.put(FsDone {
                        buf: Vec::new(),
                        out: Vec::new(),
                        result,
                    });
                }
            }
        }
    }

    /// Een blokfout is de schijf, niet de app: luid, de eerste paar keer
    /// (de driver zelf print niet).
    fn io_failed(&mut self, c: &FsCall, r: &Result<(u64, usize)>) {
        if let Err(Error::Io { lba }) = r {
            self.io_fails += 1;
            if self.io_fails <= LOUD_COMMIT_FAILS {
                self.log.log(format_args!(
                    "hopfs: slot {} op {}: block I/O failed at LBA {lba} ({} so far) HOPOS_FS_IO",
                    c.slot, c.op, self.io_fails
                ));
            }
        }
    }

    /// Een opdracht van een bundel die faalde, staat alleen in de tabel van
    /// het antwoord; dezelfde regel als [`Desk::io_failed`], met het aantal.
    fn many_failed(&mut self, c: &FsCall, r: &Result<(u64, usize)>) {
        if r.is_err() {
            return;
        }
        let count = c.data.len() / many::OP_LEN;
        let table = c.out.get(HDR_LEN..).unwrap_or(&[]);
        let bad = (0..count)
            .filter(|&i| many::result(table, i).is_some_and(|(_, s)| s != STATUS_OK))
            .count();
        if bad == 0 {
            return;
        }
        self.io_fails += 1;
        if self.io_fails <= LOUD_COMMIT_FAILS {
            self.log.log(format_args!(
                "hopfs: slot {} op {}: {bad} of {count} reads failed ({} so far) HOPOS_FS_IO",
                c.slot, c.op, self.io_fails
            ));
        }
    }

    /// De regel van een vastlegging: één per nieuwe generatie.
    fn committed(&mut self, why: CommitWhy, r: Result<Option<u64>>) {
        match r {
            Ok(Some(g)) => match why {
                CommitWhy::Periodic => self.log.log(format_args!(
                    "hopfs: tree committed as generation {g} (every {} s) HOPOS_FS_COMMIT",
                    COMMIT_EVERY.as_secs()
                )),
                CommitWhy::Stopped(s) => self.log.log(format_args!(
                    "hopfs: tree committed as generation {g} (slot {s} stopped) HOPOS_FS_COMMIT"
                )),
            },
            Ok(None) => {}
            Err(e) => {
                self.commit_fails += 1;
                if self.commit_fails <= LOUD_COMMIT_FAILS {
                    self.log.log(format_args!(
                        "hopfs: commit failed: {e} ({} so far) HOPOS_FS_COMMIT_FAIL",
                        self.commit_fails
                    ));
                }
            }
        }
    }

    /// Na de vastlegging van de bevriezing: dicht. Een commit die faalt
    /// bevriest niet: dan zou de nieuwe kern een oudere boom mounten dan de
    /// apps denken, en dat is geen flip maar verlies.
    #[cfg(test)]
    fn frozen_after<D: BlockIo>(
        &mut self,
        t: &Tree<D>,
        r: Result<Option<u64>>,
    ) -> Result<(u64, usize)> {
        r?;
        let g = t.borrow().generation();
        Ok(self.freeze_done(g))
    }

    fn freeze_done(&mut self, g: u64) -> (u64, usize) {
        self.frozen = true;
        self.frozen_calls = 0;
        self.log.log(format_args!(
            "hopfs: tree committed as generation {g} and frozen for the kernel flip HOPOS_FS_FROZEN generation={g}"
        ));
        (g, 0)
    }

    /// Zet root en volumes klaar voor een nieuwe levensduur (Go:
    /// `startImage`): de root van de vorige bewoner gaat weg, een verse lege
    /// komt ervoor in de plaats, en de gedeelde mappen bestaan.
    ///
    /// Een levensduur die bij een warme flip geadopteerd werd
    /// ([`Servicers::root_kept`]), is dezelfde app op een nieuwe kern: zijn
    /// root blijft zoals hij was, alleen het bestaan van root en volumes
    /// wordt verzekerd. De lifecycle-actor schreef dat gegeven bij de
    /// adoptie in de servicer-tabel; hier wordt het alleen gelezen.
    fn prepare<D: BlockIo>(&mut self, t: &Tree<D>, slot: Slot, generation: u32) -> Result {
        let i = slot.get();
        if self.prepared.get(i).copied().flatten() == Some(generation) {
            return Ok(());
        }
        let kept = self.svc.root_kept(slot, generation);
        let mut fs = t.borrow_mut();
        self.path.clear();
        push_root(slot, &mut self.path)?;
        if !kept {
            match fs.remove(self.path.as_bytes(), true) {
                Ok(()) | Err(Error::NoEnt) => {}
                Err(e) => return Err(e),
            }
        }
        fs.mkdir_all(self.path.as_bytes())?;
        let path = &mut self.path;
        self.svc
            .with_mounts(slot, |mounts| {
                for m in mounts {
                    path.set(&m.shared)?;
                    fs.mkdir_all(path.as_bytes())?;
                }
                Ok::<(), Error>(())
            })
            .ok_or(Error::Denied)??;
        if let Some(p) = self.prepared.get_mut(i) {
            *p = Some(generation);
        }
        if kept {
            self.log.log(format_args!(
                "hopfs: slot {slot} keeps its root over the kernel flip HOPOS_FS_ROOT_KEPT"
            ));
        }
        Ok(())
    }

    /// Resolveert het pad van `c` naar `self.path`; geeft of het in een
    /// volume ligt.
    fn resolve_call(&mut self, c: &FsCall) -> Result<bool> {
        let app_path = c.buf.get(c.path.clone()).ok_or(Error::Corrupt { at: 2 })?;
        let (slot, path) = (c.slot, &mut self.path);
        self.svc
            .with_mounts(slot, |m| {
                resolve(slot, m, app_path, path).map(|v| v.is_some())
            })
            .ok_or(Error::Denied)?
    }

    /// Het synchrone deel van een call (Go: `handleWithLimit`, zonder store
    /// en codec): generatie, klaarzetten, pad, en wat zonder I/O kan. Geeft
    /// wat er nog moet gebeuren en of het pad in een volume ligt.
    fn plan<D: BlockIo>(&mut self, t: &Tree<D>, c: &mut FsCall) -> Result<(Work, bool)> {
        if !is_fs_op(c.op) {
            return Err(Error::Kind);
        }
        // Een verbinding van een vorige levensduur krijgt niets: de
        // generatie moet de levende zijn.
        if self.svc.current(c.slot) != Some(c.generation) {
            return Err(Error::Denied);
        }
        self.prepare(t, c.slot, c.generation)?;
        let volume = self.resolve_call(c)?;
        let p = self.path.as_bytes();
        let mut fs = t.borrow_mut();
        let work = match c.op {
            OP_STAT => {
                let (size, _dir) = fs.stat(p)?;
                Work::Done(size, 0)
            }
            OP_READ => {
                let n = fs.find(p)?;
                fs.read_len(n, 0, 0)?; // Een map is geen bestand.
                Work::Read(n)
            }
            OP_READ_MANY => {
                // De hele lijst vóór er één opdracht naar het device gaat:
                // de vorm, de grenzen, en of het antwoord met een blok per
                // opdracht voor de randen in de buffer past.
                let list = c.buf.get(c.data.clone()).ok_or(Error::Corrupt { at: 0 })?;
                let sum = many::check(list, c.n).map_err(|e| match e {
                    many::Invalid::Shape => Error::Corrupt { at: 0 },
                    many::Invalid::TooMany(k) => Error::TooLarge {
                        len: k,
                        max: many::MAX_OPS,
                    },
                    many::Invalid::TooLarge(b) => Error::TooLarge {
                        len: b,
                        max: many::MAX_BYTES,
                    },
                })?;
                let count = list.len() / many::OP_LEN;
                let need = HDR_LEN + count * (many::RESULT_LEN + BLOCK_SIZE) + sum;
                if need > c.out.len() {
                    return Err(Error::TooLarge {
                        len: need,
                        max: c.out.len(),
                    });
                }
                let n = fs.find(p)?;
                fs.read_len(n, 0, 0)?;
                Work::ReadMany(n)
            }
            OP_WRITE => {
                let data = c.buf.get(c.data.clone()).ok_or(Error::Corrupt { at: 0 })?;
                if data.len() > MAX_IO_CHUNK {
                    return Err(Error::TooLarge {
                        len: data.len(),
                        max: MAX_IO_CHUNK,
                    });
                }
                Work::Write(fs.write_open(p, c.off, data.len())?)
            }
            OP_LIST => {
                let room = c.out.len().saturating_sub(HDR_LEN).min(MAX_IO_CHUNK);
                let dst = c.out.get_mut(HDR_LEN..HDR_LEN + room).unwrap_or(&mut []);
                let (count, len) = fs.list_into(p, dst)?;
                Work::Done(count as u64, len)
            }
            OP_REMOVE => {
                // De eigen root en een volume zelf zijn het zicht, geen
                // bestand: die blijven.
                let app_path = c.buf.get(c.path.clone()).unwrap_or(&[]);
                let mut cp = PathBuf::new();
                clean_abs(app_path, &mut cp)?;
                let is_mount = self
                    .svc
                    .with_mounts(c.slot, |m| m.iter().any(|m| m.local == cp.as_bytes()))
                    .unwrap_or(false);
                if cp.as_bytes() == b"/" || is_mount {
                    return Err(Error::Denied);
                }
                fs.remove(p, false)?;
                Work::Done(0, 0)
            }
            OP_SYNC => {
                if c.off != 0 || c.n != 0 || !c.data.is_empty() {
                    return Err(Error::Kind);
                }
                // De gebruikelijke generatie- en mountresolutie geldt ook
                // voor een barrière. Na remove sync't de app de oudermap.
                // De volgorde (docs/storage-sync.md): een slot heeft één
                // call tegelijk, dus elke eerdere schrijf van deze app is
                // van het device terug vóór deze flush en commit beginnen.
                fs.stat(p)?;
                Work::Sync
            }
            OP_TRUNCATE => Work::Truncate(fs.truncate_open(p, c.n)?),
            _ => return Err(Error::Kind),
        };
        Ok((work, volume))
    }

    /// Het synchrone deel van een lezing voor de kern: de node en de maat.
    fn plan_kern<D: BlockIo>(&mut self, t: &Tree<D>, path: &[u8]) -> Result<(usize, u64)> {
        clean_abs(path, &mut self.path)?;
        let p = self.path.as_bytes();
        if under(p, TASKS_DIR) {
            return Err(Error::Denied);
        }
        let mut fs = t.borrow_mut();
        let (size, dir) = fs.stat(p)?;
        if dir {
            return Err(Error::Kind);
        }
        Ok((fs.find(p)?, size))
    }

    /// Eén regel per levensduur bij de eerste geslaagde schrijf in een
    /// volume: het bewijs dat een bewoner zijn staat buiten zijn eigen root
    /// bewaart (Hop: `/hop/agent-state.json`).
    fn first_save(&mut self, c: &FsCall) {
        let i = c.slot.get();
        if self.saved.get(i).copied().flatten() == Some(c.generation) {
            return;
        }
        if let Some(s) = self.saved.get_mut(i) {
            *s = Some(c.generation);
        }
        // Het pad van deze call opnieuw: intussen planden anderen.
        if self.resolve_call(c).is_err() {
            return;
        }
        let app = c.buf.get(c.path.clone()).unwrap_or(&[]);
        let app = core::str::from_utf8(app).unwrap_or("<not utf-8>");
        let to = core::str::from_utf8(self.path.as_bytes()).unwrap_or("<not utf-8>");
        self.log.log(format_args!(
            "hopfs: slot {} saved {app} as {to} ({} bytes at {}) HOPOS_FS_SAVED",
            c.slot,
            c.data.len(),
            c.off
        ));
    }
}

/// Het antwoord van een call naar zijn plek, met de buffers terug.
fn answer(reply: Option<&Reply>, c: FsCall, result: Result<(u64, usize)>) {
    if let Some(reply) = reply {
        reply.fs.put(FsDone {
            buf: c.buf,
            out: c.out,
            result,
        });
    }
}

/// Legt de boom vast: elke [`COMMIT_EVERY`], en meteen als een slot stopte
/// (de servicer-tabel verliest zijn generatie en luidt
/// [`Servicers::changed`]). Vuur-en-vergeet: een volle brievenbus is de
/// volgende ronde opnieuw.
pub async fn committer<T: Timer>(
    svc: &Servicers,
    inbox: &FsInbox<'_>,
    timer: &T,
    max_slots: usize,
) {
    let mut live = [false; SLOT_CAP + 1];
    let every = u64::try_from(COMMIT_EVERY.as_nanos()).unwrap_or(u64::MAX);
    let mut next = timer.now().saturating_add(every);
    loop {
        let left = Duration::from_nanos(next.saturating_sub(timer.now()));
        let _ = select(svc.changed(), timer.sleep(left)).await;
        let at = timer.now();
        let mut why = None;
        for i in 1..=max_slots.min(SLOT_CAP) {
            let Some(slot) = Slot::new(i) else { continue };
            let now = svc.current(slot).is_some();
            if let Some(was) = live.get_mut(i) {
                if *was && !now {
                    why = Some(CommitWhy::Stopped(slot));
                }
                *was = now;
            }
        }
        if why.is_none() && at >= next {
            why = Some(CommitWhy::Periodic);
        }
        if let Some(w) = why {
            let _ = inbox.try_send(FsEnvelope {
                msg: FsMsg::Commit(w),
                reply: None,
            });
            next = at.saturating_add(every);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
