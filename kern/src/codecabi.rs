//! De system-functies waarmee een taak het codec-blok gebruikt, en de
//! sessietabel per levensduur (Go: `OLD/metal/kern/slots/codec.go` en
//! `codecabi.go`). Alleen met de feature `media`.
//!
//! De vorm van deze naad wordt door één getal bepaald: een 4K-beeld in P010
//! is 24 MB, bij 24 fps 597 MB/s, en het slot-LAN piekt op 550 MB/s. Die
//! beelden KUNNEN niet over de system-calls, en ze horen er ook niet: ze
//! staan al in het geheugen van de app (handboek §6, een beeld is een
//! grant). Deze laag draagt dus aanwijzingen: de app noemt een stuk van zijn
//! EIGEN partitie, [`codec_grant`] toetst het en rekent het om naar fysiek,
//! en de driver hangt het in de page tables van de codec. Dat is de grant én
//! de isolatie in één stap: het ijzer kan per constructie niets aanraken wat
//! niet van deze taak is.
//!
//! De kern doet er het cache-onderhoud bij ([`CacheMaint`]): de VPU van de O6N
//! is niet coherent (`_CCA = 0`), de partitie van een app wel gecached.
//!
//! # Eigendom
//!
//! De [`CodecService`] bezit de engine (het ijzer, boot-staat) en per
//! levensduur de tabel met sessies ([`Life`]). Een sessie is een
//! `driver_codec::Session`-handvat: wie de [`Life`] laat vallen, laat de
//! handvatten vallen, en de engine sluit ze bij zijn volgende beurt. Zo is
//! "bij evict alles sluiten" een `Drop`, en bestaan Go's `codecMu`,
//! `codecOwner`, `codecHandles.mu` en `ReleaseCodecs` niet meer (PORT.md §3).
//!
//! De tabel is herroepbaar (handboek §2.1, Revocable): elke toegang geeft
//! de levensduur mee waarvoor de verbinding werd toegelaten, en een tabel
//! van een andere levensduur geeft `None`. Een verzoek van de vorige huurder
//! dat nog in de lucht is als het slot opnieuw wordt uitgegeven, landt dus
//! nooit in de tabel van de opvolger, wiens handvat 1 toevallig hetzelfde
//! getal is.
//!
//! De driverbeurten zijn non-blocking (het contract van `driver-codec`).
//! Vóór open kan de verbindingstaak ontbrekende firmware async bijlezen;
//! daarbij houdt zij geen lening van de dienst vast. De eigenlijke
//! dienst is één synchrone beurt per call: de verbindingstaak leent
//! hem via [`serve_with_firmware`] ([`Port::serve`]) voor precies die
//! beurt, zonder `.await` ertussen (handboek §1.1). Op de OS-core draait er tussen twee `.await`-punten
//! niemand anders, dus die lening is de beurt van de eigenaar; een slot is
//! er niet.

use crate::cage::{Console, Timer};
use crate::{Region, Slot};
use abi::hopabi::codec::{
    BufArgs, EVENT_LEN, Event as WireEvent, FeedArgs, Kind as WireKind, OpenArgs,
};
use abi::hopabi::{
    HDR_LEN, OP_CODEC_CLOSE, OP_CODEC_FEED, OP_CODEC_OFFER, OP_CODEC_OPEN, OP_CODEC_POLL, Req,
    STATUS_DENIED, STATUS_ERROR, STATUS_NO_ENT, STATUS_OK,
};
use alloc::vec::Vec;
use bounded::BoundedVec;
use core::cell::RefCell;
use core::fmt;
use core::time::Duration;
use driver_codec::{
    Buffer, Codec, Config, Direction, Engine, Error as CodecError, Event, Flags, Kind, Pixel,
    Session,
};
use sync::{Local, LocalCell};

/// De paginamaat van de codec-MMU. Vier kilobyte op de Linlon V8, en elk
/// ander blok onder `driver-codec` zal niet grover zijn dan de CPU zelf.
pub const CODEC_PAGE: u64 = 4096;
/// Hoeveel levensduren tegelijk een codec-sessie hebben: niet meer dan het
/// ijzer sessies heeft (de Linlon V8 telt er hoogstens zestien).
pub const MAX_LIVES: usize = 16;
/// Hoeveel sessies één levensduur tegelijk open heeft.
pub const MAX_SESSIONS: usize = 8;
/// Hoeveel buffers één levensduur tegelijk bij het ijzer heeft liggen.
pub const MAX_HELD: usize = 64;
/// Hoeveel events één poll hoogstens oplevert. Eén call levert alles wat er
/// ligt: een round-trip per beeld is precies de kost die deze naad moest
/// vermijden.
pub const MAX_POLL: usize = 32;
/// De langste wachtende poll, wat de app ook vraagt: zolang is haar
/// verbinding van die ene call.
pub const POLL_WAIT_MAX: Duration = Duration::from_secs(1);
/// Hoe vaak de kern de engine pompt tijdens een wachtende poll: de driver
/// heeft geen interrupt (`media/mve/src/pump.rs`), dus de eigenaar van het
/// ijzer kijkt, niet de app over de draad. Een poll met een reden:
/// docs/description.md §4.11.
pub const POLL_PUMP: Duration = Duration::from_millis(1);

/// Is `op` een codec-call?
#[must_use]
pub const fn is_codec_op(op: u8) -> bool {
    matches!(
        op,
        OP_CODEC_OPEN | OP_CODEC_FEED | OP_CODEC_OFFER | OP_CODEC_POLL | OP_CODEC_CLOSE
    )
}

/// Wie er leeft, en in welke partitie: de servicer-tabel van de kern.
pub trait Lives {
    /// De generatie van de levende servicer van `slot`, of `None`.
    fn current(&self, slot: Slot) -> Option<u32>;
    /// De partitie van de levende bewoner van `slot`, of `None`.
    fn partition(&self, slot: Slot) -> Option<Region>;
}

/// Het cache-onderhoud rond een buffer die het niet-coherente ijzer leest of
/// schrijft. Op ARM `dc cvac` en `dc civac` via `dev`.
pub trait CacheMaint {
    /// Schrijft vuile regels uit (de app schreef, het ijzer leest DRAM).
    fn clean(&mut self, pa: u64, len: u64);
    /// Schrijft uit en gooit weg (het ijzer schrijft, de app leest straks).
    fn clean_inv(&mut self, pa: u64, len: u64);
}

/// Waarom een grant geweigerd werd; de getallen gaan mee naar de app.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GrantError {
    /// Nul bytes.
    Zero,
    /// Buiten de partitie, of `off + n` loopt om.
    Outside {
        /// De afstand.
        off: u64,
        /// De lengte.
        n: u64,
        /// De maat van de partitie.
        size: u64,
    },
    /// Geen hele pagina's.
    Pages {
        /// De afstand.
        off: u64,
        /// De lengte.
        n: u64,
    },
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            GrantError::Zero => f.write_str("buffer of zero bytes"),
            GrantError::Outside { off, n, size } => write!(
                f,
                "buffer {off:#x}+{n} falls outside the {} MB partition",
                size >> 20
            ),
            GrantError::Pages { off, n } => write!(
                f,
                "buffer {off:#x}+{n} is not a whole number of {CODEC_PAGE}-byte pages"
            ),
        }
    }
}

/// Rekent een stuk van een partitie om naar een fysieke buffer die het ijzer
/// mag zien.
///
/// De enige plek waar een adres van buiten de kern binnenkomt, dus hier
/// hoort de argwaan. Drie toetsen, alle drie om dezelfde reden: wat hier
/// doorheen komt wordt straks door een DMA-motor beschreven, en die vraagt
/// niet nog eens of het mocht.
///
/// 1. Binnen de partitie.
/// 2. Geen omloop: met `off = 2^64 - 1` wordt `off + n` weer klein en haalt
///    het de grenstoets.
/// 3. Pagina-precies: de codec-MMU kent niets fijners, en een buffer die
///    halverwege een pagina begint neemt de buren mee.
pub fn codec_grant(part: Region, off: u64, n: u64) -> Result<Buffer, GrantError> {
    if n == 0 {
        return Err(GrantError::Zero);
    }
    let outside = GrantError::Outside {
        off,
        n,
        size: part.size,
    };
    let end = off.checked_add(n).ok_or(outside)?;
    if end > part.size {
        return Err(outside);
    }
    if !off.is_multiple_of(CODEC_PAGE) || !n.is_multiple_of(CODEC_PAGE) {
        return Err(GrantError::Pages { off, n });
    }
    let pa = part.base.checked_add(off).ok_or(outside)?;
    Ok(Buffer { pa, size: n })
}

/// Eén open sessie in een levensduur.
#[derive(Debug)]
struct Entry {
    handle: u32,
    session: Session,
}

/// Eén buffer die bij het ijzer ligt: bij welke sessie, en of hij als
/// INVOER kwam. Bij Consumed hoeft er niets geveegd (de app schreef, het
/// ijzer las); bij Produced juist wel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct HeldBuf {
    pa: u64,
    handle: u32,
    input: bool,
}

/// De sessietabel van één levensduur van een slot. Handvatten beginnen bij
/// 1: nul is "geen sessie".
///
/// Geen eigen `Drop`: de handvatten erin zijn de vrijgave. Valt de tabel,
/// dan vallen zij, en sluit de engine hun sessies bij zijn volgende beurt.
#[derive(Debug)]
pub struct Life {
    slot: Slot,
    generation: u32,
    next: u32,
    sessions: BoundedVec<Entry, MAX_SESSIONS>,
    held: BoundedVec<HeldBuf, MAX_HELD>,
}

impl Life {
    fn new(slot: Slot, generation: u32) -> Life {
        Life {
            slot,
            generation,
            next: 0,
            sessions: BoundedVec::new(),
            held: BoundedVec::new(),
        }
    }

    fn session(&self, h: u32) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|e| e.handle == h)
            .map(|e| &e.session)
    }

    fn mark(&mut self, pa: u64, handle: u32, input: bool) {
        mark(&mut self.held, pa, handle, input);
    }
}

/// Onthoudt van welke kant een buffer kwam. Vol: de buffer wordt dan niet
/// geveegd bij zijn terugkeer; met 64 plaatsen en een ijzer dat er hoogstens
/// een paar dozijn vasthoudt, is dat een app die dubbel aanbiedt.
fn mark(held: &mut BoundedVec<HeldBuf, MAX_HELD>, pa: u64, handle: u32, input: bool) {
    forget(held, pa);
    let _ = held.push(HeldBuf { pa, handle, input });
}

fn forget(held: &mut BoundedVec<HeldBuf, MAX_HELD>, pa: u64) {
    held.retain(|b| b.pa != pa);
}

/// Was deze buffer invoer? Haalt hem uit de boekhouding.
fn was_input(held: &mut BoundedVec<HeldBuf, MAX_HELD>, pa: u64) -> bool {
    let i = held.iter().find(|b| b.pa == pa).is_some_and(|b| b.input);
    forget(held, pa);
    i
}

/// Wat een call teruggeeft: status, het `size`-veld en het aantal
/// databytes op `out[HDR_LEN..]`.
type Answer = Result<(u64, usize), (u16, Why)>;

/// De tekst bij een foutstatus, zonder allocatie.
#[derive(Copy, Clone, Debug)]
enum Why {
    NoCodec,
    NoSession,
    Released,
    Full,
    Bad(abi::Error),
    Grant(GrantError),
    Codec(CodecError),
    FirmwareRead {
        name: &'static str,
        error: crate::Error,
    },
    Filled {
        filled: u64,
        size: u64,
    },
    Unknown(u8),
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::NoCodec => f.write_str("this node has no codec hardware"),
            Why::NoSession => f.write_str("no such codec session"),
            Why::Released => f.write_str("slot is being released"),
            Why::Full => write!(f, "{MAX_SESSIONS} codec sessions per task at most"),
            Why::Bad(e) => write!(f, "bad codec args: {e}"),
            Why::Grant(e) => write!(f, "{e}"),
            Why::Codec(e) => write!(f, "{e}"),
            Why::FirmwareRead { name, error } => write!(f, "codec firmware {name}: {error}"),
            Why::Filled { filled, size } => {
                write!(f, "filled {filled} exceeds the {size}-byte buffer")
            }
            Why::Unknown(op) => write!(f, "unknown codec op {op}"),
        }
    }
}

/// De codec-dienst: de engine, de levensduren en het cache-onderhoud.
///
/// Leeft in een `static` van de kern-binary (const-gebouwd, dus zonder
/// grote kopie op de bootstack); de engine komt erin bij `codec_up`.
pub struct CodecService<E, L, C> {
    engine: Option<E>,
    lives: L,
    cache: C,
    tables: BoundedVec<Life, MAX_LIVES>,
    /// Onvertaalbare events: de eerste paar krijgen een regel.
    strays: u32,
}

impl<E: Engine, L: Lives, C: CacheMaint> CodecService<E, L, C> {
    /// Een dienst zonder ijzer: elke call antwoordt "no codec hardware".
    pub const fn new(lives: L, cache: C) -> Self {
        CodecService {
            engine: None,
            lives,
            cache,
            tables: BoundedVec::new(),
            strays: 0,
        }
    }

    /// Zet het ijzer erin (één keer, bij `codec_up`).
    pub fn install(&mut self, engine: E) {
        self.engine = Some(engine);
    }

    /// De engine, voor diagnose en het meetinstrument van de kern.
    pub fn engine(&mut self) -> Option<&mut E> {
        self.engine.as_mut()
    }

    /// Laat elke tabel vallen waarvan de levensduur voorbij is, en laat de
    /// engine de gevallen sessies sluiten. De evict van de kern: een app die
    /// omvalt met een open decoder houdt geen hardware-sessie vast tot de
    /// volgende kern-flip (de zombievorm die we bij stulp zagen).
    pub fn reap(&mut self) {
        let lives = &self.lives;
        self.tables
            .retain(|t| lives.current(t.slot) == Some(t.generation));
        if let Some(e) = self.engine.as_mut() {
            e.reap();
        }
    }

    /// De tabel van `slot` in levensduur `generation`, of `None` als die
    /// levensduur voorbij is (Revocable: `try_access`). `create` maakt hem
    /// aan als hij er nog niet is.
    fn life(&mut self, slot: Slot, generation: u32, create: bool) -> Option<&mut Life> {
        self.reap();
        if self.lives.current(slot) != Some(generation) {
            return None;
        }
        let at = self
            .tables
            .iter()
            .position(|t| t.slot == slot && t.generation == generation);
        let i = match at {
            Some(i) => i,
            None if create => {
                if self.tables.is_full() {
                    // Een lege tabel houdt alleen de teller van zijn
                    // handvatten vast; bij ruimtegebrek mag hij wijken.
                    self.tables.retain(|t| !t.sessions.is_empty());
                }
                self.tables.push(Life::new(slot, generation)).ok()?;
                self.tables.len() - 1
            }
            None => return None,
        };
        self.tables.as_mut_slice().get_mut(i)
    }

    /// Bedient één codec-call namens `slot` in levensduur `generation` en
    /// schrijft het antwoord in `out`; geeft de lengte.
    pub fn serve(
        &mut self,
        slot: Slot,
        generation: u32,
        req: &Req<'_>,
        out: &mut [u8],
        log: &impl Console,
    ) -> usize {
        let r = if self.engine.is_none() {
            Err((STATUS_ERROR, Why::NoCodec))
        } else {
            self.call(slot, generation, req, out, log)
        };
        match r {
            Ok((size, n)) => answer(out, req, STATUS_OK, size, n),
            Err((status, why)) => {
                let n = crate::fmt_into(out.get_mut(HDR_LEN..).unwrap_or(&mut []), &why);
                answer(out, req, status, 0, n)
            }
        }
    }

    fn call(
        &mut self,
        slot: Slot,
        gen_: u32,
        req: &Req<'_>,
        out: &mut [u8],
        log: &impl Console,
    ) -> Answer {
        let bad = |e| (STATUS_ERROR, Why::Bad(e));
        match req.op {
            OP_CODEC_OPEN => self.open(slot, gen_, OpenArgs::decode(req.data).map_err(bad)?),
            OP_CODEC_FEED => {
                let a = FeedArgs::decode(req.data).map_err(bad)?;
                self.feed(slot, gen_, req, a)
            }
            OP_CODEC_OFFER => {
                let a = BufArgs::decode(req.data).map_err(bad)?;
                self.offer(slot, gen_, req, a.handle)
            }
            OP_CODEC_POLL => {
                let a = BufArgs::decode(req.data).map_err(bad)?;
                self.poll(slot, gen_, a.handle, out, log)
            }
            OP_CODEC_CLOSE => {
                let a = BufArgs::decode(req.data).map_err(bad)?;
                self.close(slot, gen_, a.handle)
            }
            op => Err((STATUS_ERROR, Why::Unknown(op))),
        }
    }

    fn firmware_needed(
        &mut self,
        slot: Slot,
        generation: u32,
        req: &Req<'_>,
    ) -> Option<&'static str> {
        if req.op != OP_CODEC_OPEN || self.lives.current(slot) != Some(generation) {
            return None;
        }
        let a = OpenArgs::decode(req.data).ok()?;
        self.engine.as_mut()?.firmware_needed(&config(a))
    }

    fn open(&mut self, slot: Slot, gen_: u32, a: OpenArgs) -> Answer {
        let cfg = config(a);
        // Eerst de tabel: een levensduur die voorbij is, krijgt geen sessie
        // (in Go kon een Open die firmware laadde een evict inhalen; hier is
        // de beurt ondeelbaar, en de toets staat vóór het ijzer).
        let room = match self.life(slot, gen_, true) {
            None => return Err((STATUS_ERROR, Why::Released)),
            Some(t) => !t.sessions.is_full(),
        };
        if !room {
            return Err((STATUS_ERROR, Why::Full));
        }
        let engine = self.engine.as_mut().ok_or((STATUS_ERROR, Why::NoCodec))?;
        let session = engine
            .open(&cfg)
            .map_err(|e| (STATUS_ERROR, Why::Codec(e)))?;
        let t = self
            .life(slot, gen_, true)
            .ok_or((STATUS_ERROR, Why::Released))?;
        t.next = t.next.wrapping_add(1).max(1);
        let handle = t.next;
        // Vol kan hier niet meer (net getoetst, en er zat geen beurt
        // tussen); lukt het toch niet, dan valt de sessie en sluit ze.
        t.sessions
            .push(Entry { handle, session })
            .map_err(|_| (STATUS_ERROR, Why::Full))?;
        Ok((u64::from(handle), 0))
    }

    /// De partitie, de tabel en het handvat van een call op een buffer.
    fn grant(
        &mut self,
        slot: Slot,
        gen_: u32,
        req: &Req<'_>,
        h: u32,
    ) -> Result<Buffer, (u16, Why)> {
        let t = self
            .life(slot, gen_, false)
            .ok_or((STATUS_NO_ENT, Why::NoSession))?;
        t.session(h).ok_or((STATUS_NO_ENT, Why::NoSession))?;
        let part = self
            .lives
            .partition(slot)
            .ok_or((STATUS_DENIED, Why::Released))?;
        codec_grant(part, req.off, req.n).map_err(|e| (STATUS_DENIED, Why::Grant(e)))
    }

    fn feed(&mut self, slot: Slot, gen_: u32, req: &Req<'_>, a: FeedArgs) -> Answer {
        let b = self.grant(slot, gen_, req, a.handle)?;
        if a.filled > b.size {
            return Err((
                STATUS_ERROR,
                Why::Filled {
                    filled: a.filled,
                    size: b.size,
                },
            ));
        }
        // De app schreef met haar cache aan; het ijzer leest DRAM. Eerst
        // uitschrijven, dan pas aanbieden.
        self.cache.clean(b.pa, b.size);
        let (engine, t) = self.parts(slot, gen_)?;
        let s = t.session(a.handle).ok_or((STATUS_NO_ENT, Why::NoSession))?;
        engine
            .feed(s, b, a.filled, Flags(a.flags), a.tag)
            .map_err(|e| (STATUS_ERROR, Why::Codec(e)))?;
        t.mark(b.pa, a.handle, true);
        Ok((a.filled, 0))
    }

    fn offer(&mut self, slot: Slot, gen_: u32, req: &Req<'_>, h: u32) -> Answer {
        let b = self.grant(slot, gen_, req, h)?;
        // Het ijzer gaat hierin SCHRIJVEN. Wat de app er nog vuil van in haar
        // cache heeft, moet nu weg: een regel die later uitgezet wordt,
        // schrijft over een beeld heen dat er al lag.
        self.cache.clean_inv(b.pa, b.size);
        let (engine, t) = self.parts(slot, gen_)?;
        let s = t.session(h).ok_or((STATUS_NO_ENT, Why::NoSession))?;
        engine
            .offer(s, b)
            .map_err(|e| (STATUS_ERROR, Why::Codec(e)))?;
        t.mark(b.pa, h, false);
        Ok((b.size, 0))
    }

    fn close(&mut self, slot: Slot, gen_: u32, h: u32) -> Answer {
        let (engine, t) = self.parts(slot, gen_)?;
        let i = t
            .sessions
            .iter()
            .position(|e| e.handle == h)
            .ok_or((STATUS_NO_ENT, Why::NoSession))?;
        let e = t
            .sessions
            .swap_remove(i)
            .ok_or((STATUS_NO_ENT, Why::NoSession))?;
        // Na close zijn de buffers weer van de app en komen ze nooit meer als
        // event terug; hun boekhouding gaat mee.
        t.held.retain(|b| b.handle != h);
        engine.close(e.session);
        Ok((0, 0))
    }

    fn poll(
        &mut self,
        slot: Slot,
        gen_: u32,
        h: u32,
        out: &mut [u8],
        log: &impl Console,
    ) -> Answer {
        let part = self.lives.partition(slot);
        self.life(slot, gen_, false)
            .ok_or((STATUS_NO_ENT, Why::NoSession))?;
        let (engine, t) = split(&mut self.engine, &mut self.tables, slot, gen_)?;
        let cache = &mut self.cache;
        let Life { sessions, held, .. } = t;
        let s = sessions
            .iter()
            .find(|e| e.handle == h)
            .map(|e| &e.session)
            .ok_or((STATUS_NO_ENT, Why::NoSession))?;
        let base = part.map_or(u64::MAX, |p| p.base);
        let data = out.get_mut(HDR_LEN..).unwrap_or(&mut []);
        let max = (data.len() / EVENT_LEN).min(MAX_POLL);
        let mut n = 0;
        let mut strays = 0;
        while n < max {
            let Some(ev) = engine.next_event(s) else {
                break;
            };
            let w = match wire_event(&ev, base) {
                Some(w) => w,
                None => {
                    // Het event is al uit de sessie en kan niet terug. Een fout
                    // voor de hele poll liet het en alles ervóór verdwijnen:
                    // buffers die de app nooit terugziet. Dus gaat het als
                    // Fault mee; een driver die zoiets oplevert is stuk.
                    strays += 1;
                    if let Some(b) = ev.buf {
                        forget(held, b.pa);
                    }
                    WireEvent {
                        kind: WireKind::Fault,
                        tag: ev.tag,
                        ..WireEvent::default()
                    }
                }
            };
            if let Some(b) = ev.buf
                && w.kind != WireKind::Fault
            {
                let input = was_input(held, b.pa);
                if ev.kind == Kind::Produced && b.size > 0 && !input {
                    // Een resultaat komt uit DRAM; de app leest gecached.
                    // Haar regels moeten weg vóór ze kijkt.
                    cache.clean_inv(b.pa, b.size);
                }
            }
            let at = n * EVENT_LEN;
            if let Some(dst) = data.get_mut(at..at + EVENT_LEN) {
                let _ = w.encode(dst);
            }
            n += 1;
        }
        if strays > 0 {
            self.strays += strays;
            if self.strays <= 3 {
                log.log(format_args!(
                    "codec: slot {slot}: {strays} event(s) outside the partition, sent as faults HOPOS_CODEC_STRAY"
                ));
            }
        }
        Ok((n as u64, n * EVENT_LEN))
    }

    /// De engine en de tabel van een levende levensduur tegelijk.
    fn parts(&mut self, slot: Slot, gen_: u32) -> Result<(&mut E, &mut Life), (u16, Why)> {
        // `life` ruimt eerst op en toetst de levensduur; daarna de velden los.
        self.life(slot, gen_, false)
            .ok_or((STATUS_NO_ENT, Why::NoSession))?;
        split(&mut self.engine, &mut self.tables, slot, gen_)
    }
}

fn config(a: OpenArgs) -> Config {
    Config {
        codec: Codec::from_raw(a.codec),
        dir: Direction::from_raw(a.dir),
        pixel: Pixel::from_raw(a.pixel),
        width: u32::from(a.width),
        height: u32::from(a.height),
    }
}

/// De engine en één tabel als twee losse leningen.
fn split<'a, E, const N: usize>(
    engine: &'a mut Option<E>,
    tables: &'a mut BoundedVec<Life, N>,
    slot: Slot,
    gen_: u32,
) -> Result<(&'a mut E, &'a mut Life), (u16, Why)> {
    let engine = engine.as_mut().ok_or((STATUS_ERROR, Why::NoCodec))?;
    let t = tables
        .as_mut_slice()
        .iter_mut()
        .find(|t| t.slot == slot && t.generation == gen_)
        .ok_or((STATUS_NO_ENT, Why::NoSession))?;
    Ok((engine, t))
}

/// Vertaalt één driver-event naar de draad; `None` als de buffer niet in
/// de partitie ligt (een driver die dat oplevert is stuk).
fn wire_event(ev: &Event, base: u64) -> Option<WireEvent> {
    let kind = match ev.kind {
        Kind::Consumed => WireKind::Consumed,
        Kind::Produced => WireKind::Produced,
        Kind::Format => WireKind::Format,
        Kind::Done => WireKind::Done,
        Kind::Fault => WireKind::Fault,
    };
    let l = &ev.layout;
    let mut w = WireEvent {
        kind,
        key: ev.key,
        pixel: l.pixel as u8,
        width: u16::try_from(l.width).unwrap_or(u16::MAX),
        height: u16::try_from(l.height).unwrap_or(u16::MAX),
        bytes: ev.bytes,
        tag: ev.tag,
        ..WireEvent::default()
    };
    for (i, p) in l.planes.iter().enumerate() {
        if let (Some(s), Some(o)) = (w.stride.get_mut(i), w.plane.get_mut(i)) {
            *s = u16::try_from(p.stride).unwrap_or(u16::MAX);
            *o = u32::try_from(p.off).unwrap_or(u32::MAX);
        }
    }
    if let Some(b) = ev.buf {
        w.off = b.pa.checked_sub(base)?;
        w.size = b.size;
    }
    // Een Format gaat over geen buffer: size is de buffermaat, bytes het
    // aantal dat het ijzer tegelijk wil.
    if ev.kind == Kind::Format {
        w.size = l.frame_size;
        w.bytes = u64::from(l.min_buffers);
    }
    Some(w)
}

/// Schrijft de kop van het antwoord; geeft de totale lengte.
fn answer(out: &mut [u8], req: &Req<'_>, status: u16, size: u64, n: usize) -> usize {
    let r = abi::hopabi::Resp {
        op: req.op,
        status,
        seq: req.seq,
        size,
        data: &[],
    };
    abi::hopabi::encode_resp_head(out, &r, n).unwrap_or(0)
}

/// Eén beurt van de dienst: wat de system-API aanroept.
pub trait Port {
    /// Bedient een codec-call; geeft de lengte van het antwoord in `out`.
    fn serve(&self, slot: Slot, generation: u32, req: &Req<'_>, out: &mut [u8]) -> usize;
    /// Ontbrekende blob voor een geldig openverzoek; geen lening over de read heen.
    fn firmware_needed(
        &self,
        _slot: Slot,
        _generation: u32,
        _req: &Req<'_>,
    ) -> Option<&'static str> {
        None
    }
    /// Een gevalideerde blob wordt vóór openen aan de engine overgedragen.
    fn install_firmware(&self, _name: &'static str, _bytes: Vec<u8>) -> driver_codec::Result {
        Err(CodecError::Unsupported)
    }
}

/// De dienst in zijn `static`: de leesbare cel plus de console van de kern.
pub struct CodecCell<E, L, C, K> {
    cell: LocalCell<CodecService<E, L, C>>,
    log: K,
}

impl<E: Engine, L: Lives, C: CacheMaint, K: Console> CodecCell<E, L, C, K> {
    /// Een lege dienst voor in een `static`.
    pub const fn new(lives: L, cache: C, log: K) -> Self {
        CodecCell {
            cell: LocalCell::cell(CodecService::new(lives, cache)),
            log,
        }
    }

    /// Eén synchrone beurt op de dienst (de engine erin zetten, opruimen,
    /// het meetinstrument). `None` als er al een beurt loopt.
    pub fn with<R>(&self, f: impl FnOnce(&mut CodecService<E, L, C>, &K) -> R) -> Option<R> {
        let mut s = self.cell.try_borrow_mut().ok()?;
        Some(f(&mut s, &self.log))
    }
}

impl<E: Engine, L: Lives, C: CacheMaint, K: Console> Port for CodecCell<E, L, C, K> {
    fn firmware_needed(&self, slot: Slot, generation: u32, req: &Req<'_>) -> Option<&'static str> {
        self.with(|s, _| s.firmware_needed(slot, generation, req))
            .flatten()
    }
    fn install_firmware(&self, name: &'static str, bytes: Vec<u8>) -> driver_codec::Result {
        self.with(|s, _| {
            s.engine()
                .ok_or(CodecError::Unsupported)?
                .install_firmware(name, bytes)
        })
        .unwrap_or(Err(CodecError::Busy))
    }
    fn serve(&self, slot: Slot, generation: u32, req: &Req<'_>, out: &mut [u8]) -> usize {
        match self.cell.try_borrow_mut() {
            Ok(mut s) => s.serve(slot, generation, req, out, &self.log),
            // Een lening die nog loopt, kan op één core alleen een bug zijn
            // (een beurt wacht nooit); antwoord "straks", geen paniek.
            Err(_) => refuse(out, req, b"codec service busy"),
        }
    }
}

/// Een antwoord met foutstatus en een vaste tekst.
fn refuse(out: &mut [u8], req: &Req<'_>, text: &[u8]) -> usize {
    let n = text.len().min(out.len().saturating_sub(HDR_LEN));
    if let Some(d) = out.get_mut(HDR_LEN..HDR_LEN + n) {
        d.copy_from_slice(text.get(..n).unwrap_or(&[]));
    }
    answer(out, req, STATUS_ERROR, 0, n)
}

/// De dienst van deze node, als er een is: één keer gezet bij boot.
static PORT: Local<RefCell<Option<&'static dyn Port>>> = Local::new(RefCell::new(None));

/// Zet de dienst (boot). Een tweede keer wordt geweigerd.
pub fn install(p: &'static dyn Port) -> bool {
    let mut slot = PORT.borrow_mut();
    if slot.is_some() {
        return false;
    }
    *slot = Some(p);
    true
}

/// Laadt firmware voor een open buiten de enginebeurt. De antwoordplek blijft
/// geleend tot de bestandsactor antwoordt, ook als de app zijn eigen timeout haalt.
/// De slotgeneratie wordt daarna opnieuw gecontroleerd door de gewone open.
/// Een poll met `n` > 0 wacht op events ([`poll_wait`]).
pub async fn serve_with_firmware<'a>(
    slot: Slot,
    generation: u32,
    req: &Req<'_>,
    out: &mut [u8],
    inbox: Option<&crate::rpc::FsInbox<'a>>,
    reply: &'a crate::slots::Reply,
    timer: &impl Timer,
) -> usize {
    let port = *PORT.borrow();
    let Some(port) = port else {
        return refuse(out, req, b"this node has no codec hardware");
    };
    let len = serve_loaded(port, slot, generation, req, out, inbox, reply).await;
    poll_wait(port, slot, generation, req, out, timer, len).await
}

/// De wachtende poll: `OP_CODEC_POLL` met `n` > 0 antwoordt pas als er een
/// event is, of na `n` ms (hoogstens [`POLL_WAIT_MAX`]). De app slaapt
/// zolang op haar antwoord, en dat wekt haar zoals elke ontvangst; de kern
/// pompt de engine elke [`POLL_PUMP`] in een eigen beurt. `len` is het
/// eerste antwoord; met `n` = 0, of met events of een fout, gaat het meteen.
async fn poll_wait(
    port: &dyn Port,
    slot: Slot,
    generation: u32,
    req: &Req<'_>,
    out: &mut [u8],
    timer: &impl Timer,
    mut len: usize,
) -> usize {
    if req.op != OP_CODEC_POLL || req.n == 0 {
        return len;
    }
    let empty = |out: &[u8], len: usize| {
        abi::hopabi::decode_resp(out.get(..len).unwrap_or_default())
            .is_ok_and(|r| r.status == STATUS_OK && r.size == 0)
    };
    let wait = Duration::from_millis(req.n).min(POLL_WAIT_MAX);
    let until = timer
        .now()
        .saturating_add(u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX));
    while empty(out, len) && timer.now() < until {
        timer.sleep(POLL_PUMP).await;
        len = port.serve(slot, generation, req, out);
    }
    len
}

async fn serve_loaded<'a>(
    port: &dyn Port,
    slot: Slot,
    generation: u32,
    req: &Req<'_>,
    out: &mut [u8],
    inbox: Option<&crate::rpc::FsInbox<'a>>,
    reply: &'a crate::slots::Reply,
) -> usize {
    if let Some(name) = port.firmware_needed(slot, generation, req)
        && let Some(inbox) = inbox
    {
        match read_firmware(inbox, reply, name).await {
            Ok(bytes) => {
                if let Err(e) = port.install_firmware(name, bytes) {
                    let n =
                        crate::fmt_into(out.get_mut(HDR_LEN..).unwrap_or(&mut []), &Why::Codec(e));
                    return answer(out, req, STATUS_ERROR, 0, n);
                }
            }
            Err(error) => {
                let n = crate::fmt_into(
                    out.get_mut(HDR_LEN..).unwrap_or(&mut []),
                    &Why::FirmwareRead { name, error },
                );
                return answer(out, req, STATUS_ERROR, 0, n);
            }
        }
    }
    port.serve(slot, generation, req, out)
}

async fn read_firmware<'a>(
    inbox: &crate::rpc::FsInbox<'a>,
    reply: &'a crate::slots::Reply,
    name: &str,
) -> crate::Result<Vec<u8>> {
    if name.is_empty()
        || name.len() > 32
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(crate::Error::Corrupt { at: 0 });
    }
    for dir in [b"/firmware/".as_slice(), b"/codec-firmware/".as_slice()] {
        let mut path = [0u8; 64];
        let n = dir.len() + name.len() + 4;
        path[..dir.len()].copy_from_slice(dir);
        path[dir.len()..n - 4].copy_from_slice(name.as_bytes());
        path[n - 4..n].copy_from_slice(b".fwb");
        match crate::rpc::read_file(inbox, reply, &path[..n], 4 << 20).await {
            Ok(bytes) if bytes.is_empty() => return Err(crate::Error::Corrupt { at: 0 }),
            Ok(bytes) => return Ok(bytes),
            Err(crate::Error::NoEnt) => {}
            Err(e) => return Err(e),
        }
    }
    Err(crate::Error::NoEnt)
}

#[cfg(test)]
mod tests;
