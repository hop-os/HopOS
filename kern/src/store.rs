//! De store-ops van een app (Go: `OLD/metal/kern/slots/storage.go`): een
//! kopie op afroep tussen de eigen map van de job in de object-store
//! (`apps/<cluster>/<job>/`) en het eigen hopfs-zicht van de taak.
//!
//! Bewust een kopie en geen synchronisatie (Go): een sync-daemon belooft
//! persistentie die er tussen twee uploads niet is. Pull bij de start, push
//! wanneer het bewaard moet zijn; persistentie is een daad van de app.
//!
//! # Wie wat doet
//!
//! De kern heeft geen S3, geen sleutels en geen TLS; Hop, de bevoorrechte
//! bewoner, heeft ze. In Go liep de hele transfer in de kern (HOP was de
//! kern); hier zijn het er twee:
//!
//! 1. De app roept `OP_STORE_*` over zijn system-verbinding. De kern toetst
//!    naam, pad en job (geen `..`, geen lege naam, een job met een naam),
//!    zet de call in de [`StoreQueue`] en laat de verbinding wachten op de
//!    bel van zijn plaats.
//! 2. Hop haalt de opdracht op met `NEXT_STORE` (een lange wacht op de bel
//!    van de rij), doet de S3-kant en verplaatst de bytes met
//!    `STORE_READ`/`STORE_WRITE`: die lezen en schrijven het bestand van
//!    HET SLOT VAN DE OPDRACHT, met diens generatie, door de hopfs-actor en
//!    dus door de mount-tabel van dat slot ([`crate::rpc::resolve`]). Hop
//!    kiest het pad niet: hij geeft het pad van de opdracht letterlijk terug
//!    en de kern vergelijkt.
//! 3. Hop meldt af met `STORE_DONE`; de kern zet de uitkomst op de plaats en
//!    luidt de bel, en de wachtende verbinding antwoordt de app.
//!
//! De prefix `apps/<cluster>/<job>/` bouwt Hop: alleen Hop kent de cluster.
//! De kern geeft de jobnaam van het slot mee (uit de lifecycle, niet van de
//! app), en de app kiest alleen een naam BINNEN zijn map.
//!
//! # Eigendom en annuleren
//!
//! De rij is een leesbare tabel in een `LocalCell` (handboek §1.1): elke
//! toegang is één synchrone lening, nooit over een `.await`. De verbinding
//! die een opdracht indiende is de eigenaar van haar plaats: alleen zij
//! geeft hem vrij, bij het antwoord of bij het einde van haar levensduur.
//! Een evict of stop annuleert zo vanzelf: de wachtende verbinding ziet op
//! haar tik ([`crate::system::LIFE_TICK`]) dat de generatie weg is en geeft
//! de plaats vrij; Hop's lopende `STORE_READ`/`STORE_WRITE` krijgt dan
//! `NO_ENT` (en de hopfs-actor weigert een oude generatie toch al), en zijn
//! `STORE_DONE` ook: hij gooit de opdracht weg. Valt Hop zelf weg met een
//! opdracht in handen, dan zet de wachtende verbinding hem terug in de rij.

use crate::cage::{Console, Timer};
use crate::rpc::{self, FsCall, FsInbox, PathBuf, clean_abs};
use crate::slots::{Reply, Servicers};
use crate::system::{LIFE_TICK, MAX_IO_CHUNK, Privilege};
use crate::{Error, Slot};
use abi::hopabi::{
    HDR_LEN, OP_READ, OP_STAT, OP_STORE_DROP, OP_STORE_LIST, OP_STORE_PULL, OP_STORE_PUSH,
    OP_TRUNCATE, OP_WRITE, STATUS_DENIED, STATUS_ERROR, STATUS_NO_ENT, STATUS_OK,
};
use abi::systemapi::PrivOp;
use abi::systemapi::store::{
    DoneHead, LOG_NEWS, MAX_STORE_JOB, MAX_STORE_LIST, MAX_STORE_PATH, MAX_WAIT_MS,
    NEXT_STORE_LOGS, StoreTask, decode_read_len,
};
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use sync::{Either, LocalCell, Signal, select};

/// Zoveel store-calls wachten tegelijk op Hop, over alle apps samen. Een
/// call houdt een verbindingstaak van de kern vast zolang hij wacht (de
/// pool heeft er `hopos::net::SYSTEM_WORKERS`), dus meer dan een handvol
/// heeft geen zin; vol is een luide fout voor de app, geen wachtrij erachter.
pub const STORE_DEPTH: usize = 8;
/// Zo lang mag een call in de rij staan voor Hop hem ophaalt. Hop pollt met
/// een lange wacht, dus een levende Hop pakt hem binnen een ronde; langer is
/// een Hop zonder store-taak (een oudere Hop), en dat hoort de app te horen
/// in plaats van een kwartier te wachten.
pub const PICKUP: Duration = Duration::from_secs(30);
/// Zo lang mag een call in totaal duren: net onder de store-timeout van de
/// client (`applib::sys::STORE_TIMEOUT`, 15 minuten, Go's `storeTimeout`),
/// zodat de app de reden van de kern leest en geen kale timeout.
pub const DEADLINE: Duration = Duration::from_secs(14 * 60);
/// De langste fouttekst die de kern aan een app of Hop geeft.
const MSG_MAX: usize = 160;
/// Zoveel geweigerde calls (vol, geen dienst) krijgen een eigen regel;
/// daarna tellen we (handboek §6).
const LOUD: u64 = 3;

/// Is `op` een store-call: een app-op (`OP_STORE_*`) of een bevoegde op van
/// Hop (`NEXT_STORE` tot en met `STORE_DONE`)?
#[must_use]
pub const fn is_store_call(op: u8) -> bool {
    matches!(
        op,
        OP_STORE_PULL | OP_STORE_PUSH | OP_STORE_LIST | OP_STORE_DROP
    ) || matches!(
        PrivOp::from_op(op),
        Some(PrivOp::NextStore | PrivOp::StoreRead | PrivOp::StoreWrite | PrivOp::StoreDone)
    )
}

/// Bytes in een vaste buffer: de rij alloceert niets.
#[derive(Copy, Clone)]
struct Bytes<const N: usize> {
    b: [u8; N],
    n: usize,
}

impl<const N: usize> Bytes<N> {
    const EMPTY: Self = Self { b: [0; N], n: 0 };

    fn as_bytes(&self) -> &[u8] {
        self.b.get(..self.n).unwrap_or(&[])
    }

    /// Zet `src`; te lang is een fout en laat de buffer leeg.
    fn set(&mut self, src: &[u8]) -> Result<(), Error> {
        self.n = 0;
        let d = self.b.get_mut(..src.len()).ok_or(Error::TooLarge {
            len: src.len(),
            max: N,
        })?;
        d.copy_from_slice(src);
        self.n = src.len();
        Ok(())
    }

    /// Zet zoveel van `src` als past (een fouttekst).
    fn set_cut(&mut self, src: &[u8]) {
        let n = src.len().min(N);
        if let (Some(d), Some(s)) = (self.b.get_mut(..n), src.get(..n)) {
            d.copy_from_slice(s);
        }
        self.n = n;
    }
}

/// Waar een plaats in de rij staat.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    /// Leeg.
    Free,
    /// Wacht op Hop, sinds `since` (ns van de timer).
    Queued { since: u64 },
    /// Hop heeft hem; `hop` is de generatie van Hop's slot op dat moment.
    Taken { hop: Option<u32> },
    /// Hop meldde af; de uitkomst staat klaar.
    Done,
}

/// Eén opdracht in de rij.
struct Entry {
    state: State,
    ticket: u64,
    slot: Slot,
    generation: u32,
    op: u8,
    /// Wanneer de app hem indiende (ns van de timer).
    submitted: u64,
    key: Bytes<MAX_STORE_PATH>,
    path: Bytes<MAX_STORE_PATH>,
    job: Bytes<MAX_STORE_JOB>,
    /// De uitkomst: status, maat, en de namen of de fouttekst.
    status: u16,
    size: u64,
    data: Bytes<MAX_STORE_LIST>,
}

impl Entry {
    const FREE: Entry = Entry {
        state: State::Free,
        ticket: 0,
        slot: Slot::FIRST,
        generation: 0,
        op: 0,
        submitted: 0,
        key: Bytes::EMPTY,
        path: Bytes::EMPTY,
        job: Bytes::EMPTY,
        status: 0,
        size: 0,
        data: Bytes::EMPTY,
    };

    fn is(&self, ticket: u64) -> bool {
        self.state != State::Free && self.ticket == ticket
    }
}

/// De tabel achter de rij: de plaatsen en het volgende ticket.
struct Table {
    e: [Entry; STORE_DEPTH],
    next: u64,
}

/// De rij van store-calls van één node: gevuld door de verbindingen van de
/// apps, geleegd door Hop.
///
/// Een `static` in de binary (hij is groot: per plaats een naam, een pad en
/// een lijst van 8 KiB), gekoppeld met `System::with_store`.
pub struct StoreQueue {
    t: LocalCell<Table>,
    /// De bel van Hop: er staat iets in de rij.
    work: Signal,
    /// Per plaats de bel van de wachtende verbinding.
    done: [Signal; STORE_DEPTH],
    /// Weigeringen omdat de rij vol was.
    full: AtomicU64,
    /// Calls die niemand ophaalde.
    orphans: AtomicU64,
}

impl StoreQueue {
    /// Een lege rij, voor in een `static`.
    #[must_use]
    #[expect(
        clippy::large_stack_arrays,
        reason = "alleen voor een static: const-evaluatie legt de tabel in .bss, nooit op een stack (acht plaatsen van 10,5 KiB)"
    )]
    pub const fn new() -> StoreQueue {
        StoreQueue {
            t: LocalCell::cell(Table {
                e: [const { Entry::FREE }; STORE_DEPTH],
                next: 1,
            }),
            work: Signal::new(),
            done: [const { Signal::new() }; STORE_DEPTH],
            full: AtomicU64::new(0),
            orphans: AtomicU64::new(0),
        }
    }

    /// Hoeveel plaatsen bezet zijn (diagnose en tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.t
            .borrow()
            .e
            .iter()
            .filter(|e| e.state != State::Free)
            .count()
    }

    /// Is de rij leeg?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Zet een getoetste call in een vrije plaats; geeft plaats en ticket,
    /// of `None` als de rij vol is.
    fn submit(&self, now: u64, who: (Slot, u32), op: u8, v: &Valid<'_>) -> Option<(usize, u64)> {
        let mut t = self.t.borrow_mut();
        let ticket = t.next;
        let (i, e) =
            t.e.iter_mut()
                .enumerate()
                .find(|(_, e)| e.state == State::Free)?;
        // De lengtes toetste `valid` al; een fout hier laat de plaats vrij.
        e.key.set(v.key.as_bytes()).ok()?;
        e.path.set(v.path).ok()?;
        e.job.set(v.job.as_bytes()).ok()?;
        e.state = State::Queued { since: now };
        e.ticket = ticket;
        (e.slot, e.generation) = who;
        e.op = op;
        e.submitted = now;
        e.status = 0;
        e.size = 0;
        e.data.n = 0;
        t.next = ticket.wrapping_add(1);
        let _ = self.done.get(i).map(Signal::take);
        drop(t);
        self.work.set();
        Some((i, ticket))
    }

    /// De toets van de wachtende verbinding op plaats `i`, na een bel of een
    /// tik. Klaar is de uitkomst (en de plaats is weer vrij); anders wacht
    /// ze verder. `alive` zegt of haar levensduur nog loopt, `hop` of de
    /// Hop die de opdracht heeft nog dezelfde is.
    fn check(
        &self,
        i: usize,
        ticket: u64,
        now: u64,
        alive: bool,
        hop: impl Fn(Option<u32>) -> bool,
        out: &mut [u8],
    ) -> Option<Outcome> {
        let mut t = self.t.borrow_mut();
        let e = t.e.get_mut(i).filter(|e| e.is(ticket))?;
        let age = now.saturating_sub(e.submitted);
        let outcome = match e.state {
            State::Done => {
                let data = e.data.as_bytes();
                let n = data.len().min(out.len());
                if let (Some(d), Some(s)) = (out.get_mut(..n), data.get(..n)) {
                    d.copy_from_slice(s);
                }
                Outcome::Done {
                    status: e.status,
                    size: e.size,
                    len: n,
                }
            }
            _ if !alive => Outcome::Evicted,
            State::Queued { since } if now.saturating_sub(since) >= nanos(PICKUP) => {
                Outcome::NoService
            }
            _ if age >= nanos(DEADLINE) => Outcome::Timeout,
            State::Taken { hop: h } if !hop(h) => {
                // Hop viel weg met de opdracht in handen: terug in de rij,
                // voor de volgende Hop.
                e.state = State::Queued { since: now };
                self.work.set();
                return None;
            }
            _ => return None,
        };
        e.state = State::Free;
        Some(outcome)
    }

    /// Geeft de oudste wachtende opdracht van een levende app aan Hop
    /// (generatie `hop`), geschreven als [`StoreTask`] in `out`; de lengte,
    /// of `None` als er niets klaarstaat.
    fn take(&self, svc: &Servicers, hop: Option<u32>, out: &mut [u8]) -> Option<usize> {
        let mut t = self.t.borrow_mut();
        let e =
            t.e.iter_mut()
                .filter(|e| matches!(e.state, State::Queued { .. }))
                .filter(|e| svc.current(e.slot) == Some(e.generation))
                .min_by_key(|e| e.ticket)?;
        let task = StoreTask {
            ticket: e.ticket,
            slot: u32::try_from(e.slot.get()).unwrap_or(u32::MAX),
            op: e.op,
            job: e.job.as_bytes(),
            key: e.key.as_bytes(),
            path: e.path.as_bytes(),
        };
        let n = task.encode(out).ok()?;
        e.state = State::Taken { hop };
        Some(n)
    }

    /// De opdracht achter `ticket` voor een lees of schrijf van Hop: het slot
    /// en de generatie, als de op en het pad kloppen.
    fn lookup(&self, ticket: u64, op: u8, path: &[u8]) -> Result<(Slot, u32), Fail> {
        let t = self.t.borrow();
        let e =
            t.e.iter()
                .find(|e| e.is(ticket) && matches!(e.state, State::Taken { .. }))
                .ok_or(Fail::Gone)?;
        if e.op != op {
            return Err(Fail::Refused(
                STATUS_ERROR,
                "this store call does not move bytes in that direction",
            ));
        }
        if e.path.as_bytes() != path {
            return Err(Fail::Refused(
                STATUS_DENIED,
                "the path is not the path of this store call",
            ));
        }
        Ok((e.slot, e.generation))
    }

    /// Zet de uitkomst van Hop op de opdracht en luidt de bel van de
    /// wachtende verbinding. Geeft slot, op en de naam voor de consoleregel.
    fn finish(&self, ticket: u64, status: u16, size: u64, data: &[u8]) -> Result<Finished, Fail> {
        let mut t = self.t.borrow_mut();
        let (i, e) =
            t.e.iter_mut()
                .enumerate()
                .find(|(_, e)| e.is(ticket) && matches!(e.state, State::Taken { .. }))
                .ok_or(Fail::Gone)?;
        let status = match status {
            STATUS_OK | STATUS_ERROR | STATUS_NO_ENT | STATUS_DENIED => status,
            _ => STATUS_ERROR,
        };
        if status == STATUS_OK && e.op == OP_STORE_LIST {
            if e.data.set(data).is_err() {
                // Afkappen leest als "dit is alles" terwijl het dat niet is
                // (Go): een luide fout, de app vraagt een smallere prefix.
                e.status = STATUS_ERROR;
                e.size = 0;
                e.data
                    .set_cut(b"list: too many objects for one answer; use a narrower prefix");
            } else {
                e.status = STATUS_OK;
                e.size = size;
            }
        } else if status == STATUS_OK {
            e.status = STATUS_OK;
            e.size = size;
            e.data.n = 0;
        } else {
            e.status = status;
            e.size = 0;
            e.data.set_cut(data.get(..MSG_MAX).unwrap_or(data));
        }
        e.state = State::Done;
        let f = Finished {
            slot: e.slot,
            op: e.op,
            status: e.status,
            size: e.size,
        };
        drop(t);
        if let Some(s) = self.done.get(i) {
            s.set();
        }
        Ok(f)
    }
}

impl Default for StoreQueue {
    fn default() -> Self {
        StoreQueue::new()
    }
}

/// Wat Hop net afrondde, voor de consoleregel.
struct Finished {
    slot: Slot,
    op: u8,
    status: u16,
    size: u64,
}

/// Hoe een wachtende call eindigde.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// Hop meldde af; de data staat vooraan in de antwoordbuffer.
    Done { status: u16, size: u64, len: usize },
    /// De levensduur van de app eindigde (stop, evict).
    Evicted,
    /// Niemand haalde hem op binnen [`PICKUP`].
    NoService,
    /// Over [`DEADLINE`].
    Timeout,
}

/// Een duur in nanoseconden van de timer.
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Waarom een store-call niet lukte.
#[derive(Debug)]
enum Fail {
    /// Een fout van de kern (hopfs, paden).
    Kern(Error),
    /// Een weigering met zijn status en zijn zin.
    Refused(u16, &'static str),
    /// De opdracht bestaat niet meer: de app is gestopt.
    Gone,
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Kern(e)
    }
}

impl Fail {
    fn status(&self) -> u16 {
        match self {
            Fail::Kern(Error::NoEnt) | Fail::Gone => STATUS_NO_ENT,
            Fail::Kern(Error::Denied | Error::Privilege { .. }) => STATUS_DENIED,
            Fail::Kern(_) => STATUS_ERROR,
            Fail::Refused(s, _) => *s,
        }
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fail::Kern(e) => write!(f, "store: {e}"),
            Fail::Refused(_, why) => write!(f, "store: {why}"),
            Fail::Gone => f.write_str("store: this store call is gone (the task was stopped)"),
        }
    }
}

/// De kop van een store-call zonder lening: getallen en de bereiken van pad
/// en data in de callbuffer (die gaat bij een lees of schrijf als waarde
/// naar de hopfs-actor).
#[derive(Clone, Debug)]
pub struct Head {
    /// De op.
    pub op: u8,
    /// Het volgnummer.
    pub seq: u32,
    /// `off` van de call.
    pub off: u64,
    /// `n` van de call.
    pub n: u64,
    /// Waar het pad staat.
    pub path: Range<usize>,
    /// Waar de data staat.
    pub data: Range<usize>,
}

impl Head {
    /// De kop van een call met een pad van `path_len` bytes in een payload
    /// van `total` bytes.
    #[must_use]
    pub fn new(op: u8, seq: u32, off: u64, n: u64, path_len: usize, total: usize) -> Head {
        let path = HDR_LEN..HDR_LEN + path_len;
        Head {
            op,
            seq,
            off,
            n,
            data: path.end..total.max(path.end),
            path,
        }
    }
}

/// Alles wat een store-call van de verbinding nodig heeft.
pub struct Ctx<'a, 'r, T, C> {
    /// De rij.
    pub queue: &'a StoreQueue,
    /// De hopfs-actor (voor de bytes van Hop); `None` op een board zonder
    /// schijf.
    pub fs: Option<&'a FsInbox<'r>>,
    /// De servicers: levensduren en jobnamen.
    pub svc: &'a Servicers,
    /// Het slot van de peer.
    pub slot: Slot,
    /// De levensduur waarvoor de verbinding toegelaten werd.
    pub generation: u32,
    /// Het bewijs, alleen als de peer het slot van Hop is.
    pub hop: Option<&'a Privilege>,
    /// Het slot van Hop (als er een is): een opdracht in handen van een Hop
    /// die wegviel, gaat terug in de rij.
    pub hop_slot: Option<Slot>,
    /// De antwoordplek van de verbinding bij de hopfs-actor.
    pub reply: &'r Reply,
    /// De tik van het wachten.
    pub timer: &'a T,
    /// De console.
    pub log: &'a C,
    /// De bellen van nieuwe logregels voor Hop: de console van de kern en
    /// de ringen van de apps.
    pub news: [Option<&'a Signal>; 2],
}

/// Dient één store-call; geeft de lengte van het antwoord in `out`.
pub async fn serve<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    h: Head,
    buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> usize {
    let r = match PrivOp::from_op(h.op) {
        None => return app_call(cx, &h, buf, out).await,
        Some(op) => match cx.hop {
            Some(proof) => hop_call(cx, proof, op, &h, buf, out).await,
            None => Err(Fail::Kern(Error::Privilege {
                slot: cx.slot.get(),
            })),
        },
    };
    match r {
        Ok((size, len)) => head(out, &h, STATUS_OK, size, len),
        Err(e) => fail(out, &h, &e),
    }
}

/// Een call van een app: toetsen, in de rij, wachten, antwoorden.
async fn app_call<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    h: &Head,
    buf: &[u8],
    out: &mut [u8],
) -> usize {
    let key = buf.get(h.path.clone()).unwrap_or(&[]);
    let local = buf.get(h.data.clone()).unwrap_or(&[]);
    let v = match valid(cx, h.op, key, local) {
        Ok(v) => v,
        Err(e) => return fail(out, h, &e),
    };
    let Some((i, ticket)) = cx
        .queue
        .submit(cx.timer.now(), (cx.slot, cx.generation), h.op, &v)
    else {
        let n = cx.queue.full.fetch_add(1, Relaxed).wrapping_add(1);
        if n <= LOUD {
            cx.log.log(format_args!(
                "store: slot {} refused, {STORE_DEPTH} store calls already waiting ({n} so far) HOPOS_STORE_FULL",
                cx.slot
            ));
        }
        return fail(
            out,
            h,
            &Fail::Refused(
                STATUS_ERROR,
                "queue full (8 store calls waiting); try again",
            ),
        );
    };
    let data = out.get_mut(HDR_LEN..).unwrap_or(&mut []);
    let outcome = wait(cx, i, ticket, data).await;
    match outcome {
        Outcome::Done { status, size, len } if status == STATUS_OK => {
            head(out, h, STATUS_OK, size, len)
        }
        Outcome::Done { status, len, .. } => head(out, h, status, 0, len),
        Outcome::Evicted => fail(out, h, &Fail::Gone),
        Outcome::NoService => {
            let n = cx.queue.orphans.fetch_add(1, Relaxed).wrapping_add(1);
            if n <= LOUD {
                cx.log.log(format_args!(
                    "store: slot {} op {}: nobody picked the call up within {} s; is Hop running its store task? ({n} so far) HOPOS_STORE_NO_SERVICE",
                    cx.slot,
                    h.op,
                    PICKUP.as_secs()
                ));
            }
            fail(
                out,
                h,
                &Fail::Refused(
                    STATUS_ERROR,
                    "no object store service picked the call up within 30 s",
                ),
            )
        }
        Outcome::Timeout => fail(
            out,
            h,
            &Fail::Refused(STATUS_ERROR, "the store call timed out after 14 minutes"),
        ),
    }
}

/// Wacht op de uitkomst van de opdracht op plaats `i`: op de bel, en elke
/// [`LIFE_TICK`] de toets van levensduur, ophaaltijd en termijn.
async fn wait<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    i: usize,
    ticket: u64,
    out: &mut [u8],
) -> Outcome {
    let Some(bell) = cx.queue.done.get(i) else {
        return Outcome::Evicted;
    };
    loop {
        let _ = select(bell.wait(), cx.timer.sleep(LIFE_TICK)).await;
        let alive = cx.svc.current(cx.slot) == Some(cx.generation);
        let hop = |taken: Option<u32>| match cx.hop_slot {
            Some(s) => taken.is_some() && cx.svc.current(s) == taken,
            None => true,
        };
        if let Some(o) = cx.queue.check(i, ticket, cx.timer.now(), alive, hop, out) {
            return o;
        }
    }
}

/// Een getoetste call: de genormaliseerde naam, het lokale pad en de job.
struct Valid<'b> {
    key: PathBuf,
    path: &'b [u8],
    job: Bytes<MAX_STORE_JOB>,
}

/// De toetsen van de kern (Go: `storeGate`): een schone naam binnen de eigen
/// map (geen `..`; pull, push en drop geen lege naam), een schoon lokaal
/// pad, en een slot met een job waarvan de naam een naamruimte kan zijn.
fn valid<'b, T, C>(
    cx: &Ctx<'_, '_, T, C>,
    op: u8,
    key: &'b [u8],
    local: &'b [u8],
) -> Result<Valid<'b>, Fail> {
    let mut v = Valid {
        key: PathBuf::new(),
        path: if local.is_empty() { key } else { local },
        job: Bytes::EMPTY,
    };
    if key.len() > MAX_STORE_PATH || v.path.len() > MAX_STORE_PATH {
        return Err(Error::TooLarge {
            len: key.len().max(v.path.len()),
            max: MAX_STORE_PATH,
        }
        .into());
    }
    clean_abs(key, &mut v.key)?;
    if op != OP_STORE_LIST {
        if v.key.as_bytes() == b"/" {
            return Err(Fail::Refused(STATUS_DENIED, "empty object name"));
        }
        // Alleen de toets op `..`: resolveren doet de hopfs-actor bij elke
        // lees en schrijf van Hop, met de mount-tabel van dat moment.
        clean_abs(v.path, &mut PathBuf::new())?;
    }
    let job = cx
        .svc
        .with_job(cx.slot, |j| {
            let mut b = Bytes::<MAX_STORE_JOB>::EMPTY;
            b.set(j).map(|()| b)
        })
        .ok_or(Fail::Gone)??;
    if job.n == 0 {
        return Err(Fail::Refused(
            STATUS_ERROR,
            "the task has no job identity; the object store is only for jobs",
        ));
    }
    if job.as_bytes().iter().any(|b| matches!(b, b'/' | b'\\')) || job.as_bytes() == b".." {
        return Err(Fail::Refused(
            STATUS_ERROR,
            "the job name cannot form a store namespace",
        ));
    }
    v.job = job;
    Ok(v)
}

/// De bevoegde store-ops van Hop. `&Privilege` is het bewijs.
async fn hop_call<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    _proof: &Privilege,
    op: PrivOp,
    h: &Head,
    buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(u64, usize), Fail> {
    match op {
        PrivOp::NextStore => next(cx, h, out).await,
        PrivOp::StoreRead => read(cx, h, buf, out).await,
        PrivOp::StoreWrite => write(cx, h, buf, out).await,
        PrivOp::StoreDone => done(cx, h, buf),
        _ => Err(Fail::Kern(Error::Kind)),
    }
}

/// `NEXT_STORE`: de oudste opdracht, of na hoogstens `n` ms niets; met
/// [`NEXT_STORE_LOGS`] ook [`LOG_NEWS`] zodra er nieuwe logregels zijn.
async fn next<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    h: &Head,
    out: &mut [u8],
) -> Result<(u64, usize), Fail> {
    let wait = Duration::from_millis(h.n.min(MAX_WAIT_MS));
    let until = cx.timer.now().saturating_add(nanos(wait));
    let hop = cx.hop_slot.and_then(|s| cx.svc.current(s));
    let data = out.get_mut(HDR_LEN..).unwrap_or(&mut []);
    let news = cx.news.map(|b| b.filter(|_| h.off & NEXT_STORE_LOGS != 0));
    let mut heard = false;
    loop {
        if let Some(n) = cx.queue.take(cx.svc, hop, data) {
            return Ok((1, n));
        }
        // Een opdracht gaat voor; dan de bellen, die elk wekken één keer.
        heard |= news.iter().flatten().fold(false, |any, b| b.take() | any);
        if heard {
            return Ok((LOG_NEWS, 0));
        }
        // Een Hop die wegging (een stop of een herstart door zijn
        // supervisor) wacht niet tot het einde: zijn verbinding houdt een
        // van zijn drie plaatsen vast ([`crate::system::MAX_HOP_CONNS`]), en
        // de nieuwe Hop kreeg er tot 22-10 tot 5 s lang maar twee (zijn
        // init-jobs en zijn klok "connection closed", HOPOS_SYSTEM_REFUSED).
        // Dezelfde tik als het toezicht op elke verbinding
        // ([`crate::system::LIFE_TICK`]).
        if cx.hop_slot.and_then(|s| cx.svc.current(s)) != hop {
            return Ok((0, 0));
        }
        let now = cx.timer.now();
        if now >= until {
            return Ok((0, 0));
        }
        let left = Duration::from_nanos(until - now).min(LIFE_TICK);
        let logs = select(rung(news[0]), rung(news[1]));
        let woke = select(select(cx.queue.work.wait(), logs), cx.timer.sleep(left)).await;
        heard = matches!(woke, Either::Left(Either::Right(_)));
    }
}

/// Wacht op bel `b`; zonder bel nooit.
async fn rung(b: Option<&Signal>) {
    match b {
        Some(b) => b.wait().await,
        None => core::future::pending().await,
    }
}

/// `STORE_READ`: de maat van het bestand van de opdracht, en een stuk vanaf
/// `n`, door de mount-tabel van het slot van de opdracht.
async fn read<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    h: &Head,
    buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(u64, usize), Fail> {
    let path = buf.get(h.path.clone()).unwrap_or(&[]);
    let who = cx.queue.lookup(h.off, OP_STORE_PUSH, path)?;
    let max = decode_read_len(buf.get(h.data.clone()).unwrap_or(&[]))
        .map_err(|_| Fail::Kern(Error::Corrupt { at: h.data.start }))?;
    let (size, _) = fs(cx, who, OP_STAT, 0, 0, h, 0..0, buf, out).await?;
    if max == 0 {
        return Ok((size, 0));
    }
    let n = max.min(MAX_IO_CHUNK as u64);
    let (_, len) = fs(cx, who, OP_READ, h.n, n, h, 0..0, buf, out).await?;
    Ok((size, len))
}

/// `STORE_WRITE`: een stuk in het bestand van de opdracht; op offset 0 eerst
/// op nul (vervangend, Go's `fsWriter`: een pull laat nooit de staart van
/// een oudere, langere versie staan).
async fn write<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    h: &Head,
    buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(u64, usize), Fail> {
    let path = buf.get(h.path.clone()).unwrap_or(&[]);
    let who = cx.queue.lookup(h.off, OP_STORE_PULL, path)?;
    if h.n == 0 {
        fs(cx, who, OP_TRUNCATE, 0, 0, h, 0..0, buf, out).await?;
    }
    let len = h.data.len();
    if len > 0 {
        fs(cx, who, OP_WRITE, h.n, 0, h, h.data.clone(), buf, out).await?;
    }
    Ok((len as u64, 0))
}

/// Eén bestandscall voor Hop, met het slot en de generatie van de opdracht:
/// de buffers gaan als waarde naar de hopfs-actor en komen terug (Go:
/// `fsReader`/`fsWriter` op `resolve`).
#[expect(
    clippy::too_many_arguments,
    reason = "de call brengt zijn buffers, zijn opdracht en zijn bereiken mee"
)]
async fn fs<T: Timer, C: Console>(
    cx: &Ctx<'_, '_, T, C>,
    who: (Slot, u32),
    op: u8,
    off: u64,
    n: u64,
    h: &Head,
    data: Range<usize>,
    buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(u64, usize), Fail> {
    let Some(inbox) = cx.fs else {
        return Err(Fail::Refused(STATUS_ERROR, "no storage layer on board"));
    };
    let c = FsCall {
        slot: who.0,
        generation: who.1,
        op,
        off,
        n,
        path: h.path.clone(),
        data,
        buf: core::mem::take(buf),
        out: core::mem::take(out),
    };
    let result = match rpc::call(inbox, cx.reply, c).await {
        Ok(d) => {
            (*buf, *out) = (d.buf, d.out);
            d.result
        }
        Err(c) => {
            (*buf, *out) = (c.buf, c.out);
            Err(Error::Busy)
        }
    };
    // Een oude generatie: de app is weg, en dan is de opdracht dat ook.
    result.map_err(|e| match e {
        Error::Denied if cx.svc.current(who.0) != Some(who.1) => Fail::Gone,
        e => Fail::Kern(e),
    })
}

/// `STORE_DONE`: de uitkomst naar de wachtende verbinding, met één regel.
fn done<T, C: Console>(cx: &Ctx<'_, '_, T, C>, h: &Head, buf: &[u8]) -> Result<(u64, usize), Fail> {
    let (d, rest) = DoneHead::decode(buf.get(h.data.clone()).unwrap_or(&[]))
        .map_err(|_| Fail::Kern(Error::Corrupt { at: h.data.start }))?;
    let f = cx.queue.finish(h.off, d.status, h.n, rest)?;
    let what = match f.op {
        OP_STORE_PULL => "pull",
        OP_STORE_PUSH => "push",
        OP_STORE_LIST => "list",
        _ => "drop",
    };
    if f.status == STATUS_OK {
        cx.log.log(format_args!(
            "store: slot {} {what} done (size {}) HOPOS_STORE_DONE",
            f.slot, f.size
        ));
    } else if f.status == STATUS_NO_ENT {
        // Een pull van een object dat er (nog) niet is, is een gewone vraag
        // (Go's store_demo begint ermee), geen fout van de dienst.
        cx.log.log(format_args!(
            "store: slot {} {what}: no such object HOPOS_STORE_MISS",
            f.slot
        ));
    } else {
        let why = core::str::from_utf8(rest.get(..MSG_MAX).unwrap_or(rest)).unwrap_or("?");
        cx.log.log(format_args!(
            "store: slot {} {what} failed (status {}): {why} HOPOS_STORE_FAIL",
            f.slot, f.status
        ));
    }
    Ok((0, 0))
}

/// Schrijft de kop van een antwoord met `len` databytes die al op
/// `out[HDR_LEN..]` staan; geeft de lengte.
fn head(out: &mut [u8], h: &Head, status: u16, size: u64, len: usize) -> usize {
    let r = abi::hopabi::Resp {
        op: h.op,
        status,
        seq: h.seq,
        size,
        data: &[],
    };
    abi::hopabi::encode_resp_head(out, &r, len).unwrap_or(0)
}

/// Een foutantwoord: de status uit de fout en de tekst in de data.
fn fail(out: &mut [u8], h: &Head, e: &Fail) -> usize {
    let data = out.get_mut(HDR_LEN..).unwrap_or(&mut []);
    let n = crate::fmt_into(
        data.get_mut(..MSG_MAX.min(data.len())).unwrap_or(&mut []),
        e,
    );
    head(out, h, e.status(), 0, n)
}

#[cfg(test)]
mod tests;
