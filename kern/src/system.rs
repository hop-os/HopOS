//! De system-API: het control- en datakanaal van een app naar HOP, over het
//! gewone interne LAN.
//!
//! Netwerkisolatie bepaalt de peer, de peer bepaalt het slot (het bron-IP,
//! [`slot_from_remote`]) en de levende servicer van dat slot bepaalt de
//! levensduur. Er is geen tweede identiteitspad.
//!
//! Bovenop de gewone calls staan de BEVOEGDE operaties
//! ([`abi::systemapi::PrivOp`], 0x40 tot en met 0x4D): een slot reserveren,
//! een image erin stromen, stoppen, status, logregels, de klok, de flip,
//! de store-opdrachten van de apps ophalen en afronden ([`crate::store`]),
//! en Hop's eigen staat en herstart (hieronder).
//! Alleen het slot met de [`Privilege`] mag ze: dat is Hop, de eerste
//! bewoner (PORT.md beslissing 1). De kern maakt het token één keer bij boot
//! en geeft het aan het slot van Hop; nergens anders bestaat er een. Een
//! privileged operatie is een functie die `&Privilege` eist: bewijs als
//! parameter (handboek §2.1), geen vlag die je vergeet te toetsen.
//!
//! # Een start
//!
//! `START_SLOT` kiest zelf een leeg slot en claimt het bij de
//! lifecycle-actor ([`Request::Claim`]); de [`ImageGrant`] blijft hier, in
//! een tabel van hoogstens [`MAX_STREAMS`] stromen. `STREAM_IMAGE` routeert
//! elke byte meteen naar zijn eindadres (de `Placer`, de Rust-vorm van
//! `OLD/metal/abi/place/stream.go`): binnen een PT_LOAD naar de partitie, de
//! staart van het bestand (sectieheaders, symbooltabel) naar een
//! schrapruimte bovenin het app-RAM. Na de laatste byte leest `leanelf` de
//! symbolen, bouwt [`abi::place::build`] het plan met álle toetsen, gaan de
//! patches en de env erin, en krijgt de actor de grant terug met
//! [`Request::Arm`]. Faalt iets, dan gaat de grant terug met
//! [`Request::Abort`]: de kern ruimt zijn eigen reserveringen op. De
//! boot (Hop in slot 1, de gestagede app op QEMU) loopt dezelfde weg, met
//! het hele image in één brok ([`place()`]): er is één plaatsing.
//!
//! De TCP-verbinding komt van `leannet`, over de [`Conn`]-trait.
//!
//! # Hop herstarten
//!
//! Hop legt zijn agent-staat hier neer (`STATE_PUT`): één blob, eigendom van
//! deze service, in kerngeheugen. Het overleeft een herstart van Hop en
//! sterft met de boot, zoals de fdstore van systemd de staat van een dienst
//! over zijn herstart vasthoudt en met de manager verdwijnt. `HOP_RESTART`
//! toetst het verzoek (de stroom van een nieuw image is compleet en rauw,
//! de vlaggen en de termijn kloppen) en geeft het aan [`Hooks::hop_restart`]:
//! de supervisor van de kern-binary wacht dan tot Hop zelf vertrekt
//! (hoogstens de termijn, anders de stop-bel) en plaatst hem opnieuw. Wat de oude Hop half deed (een open stroom), ruimt
//! [`System::drop_streams`] op vóór de nieuwe begint.
//!
//! # Verbindingen naast elkaar
//!
//! [`System`] is deelbaar: [`System::serve`] leent hem als `&self`, en elke
//! verbinding is een eigen taak uit een vaste pool met haar eigen buffers en
//! haar eigen [`Reply`]. De staat die tussen twee calls leeft (de stromen)
//! is een leesbare tabel in een `LocalCell` (handboek §1.1): elke lening
//! duurt één synchrone stap, nooit over een `.await`. Een verbinding hoort
//! bij één levensduur van haar slot: wisselt die (stop, herstart), dan sluit
//! de kern haar op de stopbel van die levensduur
//! ([`ServicerCtl::ended`]), ook als de peer zwijgt. Gemeten 29-09
//! vóór deze vorm: één verbinding tegelijk, en de half-open verbinding van
//! een geparkeerde app hield de listener voor altijd vast (de toets van
//! buiten zag daarna nooit meer een antwoord).

use crate::cage::{Console, CoreClass, PhysMem, Timer};
use crate::kernflip;
use crate::pool::{GroupName, Placement};
use crate::rpc::{self, FsCall, FsInbox};
use crate::slots::{
    self, Envelope, ImageGrant, Occupancy, Reply, Request, Response, ServicerCtl, Servicers,
    StartSpec, try_vec,
};
use crate::{Error, Result, Slot};
use abi::hopabi::{
    HDR_LEN, Req, Resp, STATUS_DENIED, STATUS_ERROR, STATUS_NO_ENT, STATUS_OK, decode_req,
};
use abi::layout::{ABI_CTRL_OFF, ABI_TAIL, LINK_BASE};
use abi::place::{self, MAX_SEGMENTS, Segment};
use abi::systemapi::{
    HOP_IMAGE_JOB, HOP_RESTART_GRACE_MAX_MS, HOP_RESTART_GRACE_MS, HOP_STATE_MAX, Header, Kind,
    PrivOp, SLOT_LEN, SlotInfo, SlotState, StartReq, StreamResp, StreamState, decode_header,
    encode_header,
};
use alloc::rc::Rc;
use alloc::vec::Vec;
use bounded::BoundedVec;
use core::fmt;
use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering::AcqRel};
use core::time::Duration;
use sync::mpsc::Mailbox;
use sync::{Either, LocalCell, select};

// De framing, de kop van een call en het adresplan zijn van `abi`: de kern
// codeert ze met dezelfde encoders als applib (review 01-10, "abi als
// enige waarheid").
pub use abi::systemapi::{MAX_IO_CHUNK, MAX_PAYLOAD};
/// Open system-verbindingen per levensduur. applib houdt er één open; de
/// tweede is voor een herverbinding waarvan HOP de FIN van de oude nog niet
/// zag. Elke verbinding houdt netwerk- en callbuffers vast.
pub const MAX_SYSTEM_CONNS: u8 = 2;
/// Zoveel system-verbindingen tegelijk voor het slot van Hop: één meer dan
/// een app, voor de store-taak die blijvend op `NEXT_STORE` wacht
/// ([`crate::store`]). Gemeten 30-09: met twee kreeg de kloktaak van Hop
/// geen verbinding meer zodra de store-taak er een hield.
pub const MAX_HOP_CONNS: u8 = MAX_SYSTEM_CONNS + 1;
/// Hoeveel images tegelijk mogen stromen. Hop begrenst zelf op vier
/// (`runner::MAX_CONCURRENT_DOWNLOADS`); de kern houdt ruimte voor een
/// flip-bundel en een herstart ernaast.
pub const MAX_STREAMS: usize = 8;
/// Hoeveel bytes van de kop gebufferd mogen worden vóór de program headers
/// compleet zijn. Een echte Go-ELF heeft ze binnen een kilobyte; dit is de
/// enige heap die een streamende plaatsing per stroom kost (`maxHead` in
/// `stream.go`).
pub const MAX_HEAD: usize = 64 << 10;
/// Hoe lang een verbinding zonder enig verkeer open blijft.
///
/// Go had geen time-out, wel `evict`; de stopbel van de levensduur is die
/// evict. Dit is de vangrail daarachter: een levend slot waarvan de peer
/// verdween zonder FIN (een app-stack die opnieuw begon) houdt anders een
/// van zijn twee plaatsen voor altijd bezet. Vijf minuten, want applib
/// houdt zijn verbinding stil open tussen twee logregels en merkt een
/// gesloten verbinding pas bij de volgende schrijf (die regel kan dan
/// verloren gaan); een app die minder dan eens per vijf minuten logt, betaalt
/// dat hoogstens één keer per vijf minuten.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// De bevoegdheid van Hop: het enige token waarmee de lifecycle over de
/// system-API bestuurd wordt.
///
/// Geen `Clone`, geen publieke constructor: [`Privilege::boot`] geeft er
/// precies één, aan precies één slot.
#[derive(Debug)]
pub struct Privilege {
    slot: Slot,
}

static MINTED: AtomicBool = AtomicBool::new(false);

impl Privilege {
    /// Geeft de bevoegdheid aan `hop`: de eerste aanroep sinds boot krijgt
    /// het token, elke volgende `None`.
    pub fn boot(hop: Slot) -> Option<Privilege> {
        if MINTED.swap(true, AcqRel) {
            return None;
        }
        Some(Privilege { slot: hop })
    }

    /// Het slot dat de bevoegdheid draagt.
    #[must_use]
    pub fn slot(&self) -> Slot {
        self.slot
    }

    #[cfg(test)]
    pub(crate) fn for_test(slot: Slot) -> Privilege {
        Privilege { slot }
    }
}

/// Een TCP-verbinding van `leannet`: een handvat met een rij.
pub trait Conn {
    /// Leest hooguit `buf.len()` bytes; 0 = de peer sloot.
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize>>;
    /// Schrijft een deel van `buf`; geeft terug hoeveel.
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = Result<usize>>;
    /// Het IPv4-bronadres van de peer (big-endian als getal).
    fn remote_ip4(&self) -> u32;
}

/// Wat de system-API buiten de lifecycle nodig heeft.
///
/// Alle verbindingen delen dezelfde haken, dus `&self`: een haak met staat
/// houdt die zelf in een `LocalCell` of stuurt een bericht aan zijn eigenaar.
pub trait Hooks {
    /// Zet de klok (Unix-nanoseconden).
    fn set_clock(&self, unix_ns: u64);
    /// Start een kern-flip naar `bundle` (rauw gestroomd in een slot dat
    /// met [`FLIP_BUNDLE_JOB`] gereserveerd is), die de SHA-256 `sha256`
    /// moet hebben. De haak toetst en legt de nieuwe kern klaar, synchroon:
    /// na zijn `Ok` geeft de system-API het slot terug aan de pool, dus wat
    /// de haak uit de bundel nodig heeft, moet dan al gekopieerd zijn. De
    /// sprong zelf komt later, van de eigenaar van de flip (hij moet dit
    /// antwoord nog naar Hop laten gaan).
    fn flip(&self, bundle: &FlipBundle, sha256: &[u8; 32]) -> Result;
    /// Herstart Hop: met `image` het nieuwe image (rauw gestroomd in een
    /// slot dat met [`HOP_IMAGE_JOB`] gereserveerd is, met de som die het
    /// moet hebben), zonder hetzelfde; `grace` is hoe lang Hop krijgt om
    /// zelf te vertrekken. Synchroon, zoals [`Hooks::flip`]: de haak toetst
    /// som en ELF en kopieert het image; na zijn `Ok` geeft de system-API
    /// het slot terug aan de pool. Het wachten op Hop's vertrek (en anders
    /// de stop-bel) en de plaatsing komen later, van de supervisor van Hop.
    /// Een weigering meldt de haak zelf op de console.
    fn hop_restart(
        &self,
        image: Option<&HopImage>,
        grace: Duration,
    ) -> core::result::Result<(), Fail>;
    /// Een HOP_RESTART die de system-API zelf weigerde, vóór de haak: `why`
    /// is de korte reden voor achter de marker, `err` wat Hop als antwoord
    /// krijgt. Zo is elke weigering één regel op de console, waar hij ook
    /// viel.
    fn hop_refused(&self, why: &'static str, err: &Fail);
    /// De meetlat van de kern zelf, voor slot 0 van SLOT_STATUS: `heartbeat`
    /// (de tik), `ram_size` (de kern-RAM), `mem_sys` (de heap in gebruik),
    /// `idle_ns` en `wakes` (de slaap van de executor, cumulatief) en `at_ns`
    /// (de klok waarop `idle_ns` telt, gelezen ná `idle_ns`). De rest zet
    /// de system-API ([`kern_status`]).
    fn kern(&self) -> crate::cage::Status;
}

// De jobnaam van een flip-bundel staat in `abi`, naast die van een nieuw
// image van Hop: Hop leest hem daar, zonder eigen kopie (23-10).
pub use abi::systemapi::FLIP_BUNDLE_JOB;

/// Een complete flip-bundel in een gereserveerde partitie.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FlipBundle {
    /// Het slot waarin hij stroomde.
    pub slot: Slot,
    /// De fysieke basis van de partitie: byte 0 van de bundel.
    pub base: u64,
    /// De lengte van de bundel.
    pub len: u64,
}

/// Een compleet nieuw image van Hop in een gereserveerde partitie
/// ([`HOP_IMAGE_JOB`]), voor [`Hooks::hop_restart`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HopImage {
    /// Het slot waarin het stroomde.
    pub slot: Slot,
    /// De fysieke basis van de partitie: byte 0 van het image.
    pub base: u64,
    /// De lengte van het image.
    pub len: u64,
    /// De SHA-256 die het moet hebben.
    pub sha256: [u8; 32],
}

/// Een rauwe stroom: de bytes gaan zonder plaatsing de partitie in, tot een
/// bevoegde op ze opeist. Welke op dat mag, zegt de jobnaam van de start.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Raw {
    /// Een kern-bundel ([`FLIP_BUNDLE_JOB`], voor `FLIP`).
    FlipBundle,
    /// Een nieuw image van Hop ([`HOP_IMAGE_JOB`], voor `HOP_RESTART`).
    HopImage,
}

impl Raw {
    /// De rauwe soort van een jobnaam, of `None` voor een gewone app.
    fn of(job: &[u8]) -> Option<Raw> {
        if job == FLIP_BUNDLE_JOB {
            Some(Raw::FlipBundle)
        } else if job == HOP_IMAGE_JOB {
            Some(Raw::HopImage)
        } else {
            None
        }
    }
}

/// Het slot achter een bron-IP, of `None` als het geen app-adres is.
#[must_use]
pub fn slot_from_remote(ip: u32) -> Option<Slot> {
    Slot::new(abi::layout::ip4_port(ip)?)
}

async fn read_full(c: &mut impl Conn, buf: &mut [u8]) -> Result {
    let mut got = 0;
    while got < buf.len() {
        let n = c.read(buf.get_mut(got..).unwrap_or(&mut [])).await?;
        if n == 0 {
            return Err(Error::Conn);
        }
        got += n;
    }
    Ok(())
}

async fn write_all(c: &mut impl Conn, mut buf: &[u8]) -> Result {
    while !buf.is_empty() {
        let n = c.write(buf).await?;
        if n == 0 {
            return Err(Error::Conn);
        }
        buf = buf.get(n..).unwrap_or(&[]);
    }
    Ok(())
}

/// Leest een framekop: soort en lengte, getoetst door `abi` (magie, versie,
/// soort en maat vóór er één payloadbyte gelezen is).
pub async fn read_header(c: &mut impl Conn) -> Result<Header> {
    let mut h = [0u8; abi::systemapi::HEADER_LEN];
    read_full(c, &mut h).await?;
    Ok(decode_header(&h)?)
}

/// Schrijft één frame; een te groot frame gaat nooit (half) de draad op.
pub async fn write_frame(c: &mut impl Conn, kind: Kind, payload: &[u8]) -> Result {
    let h = encode_header(kind, payload.len())?;
    write_all(c, &h).await?;
    write_all(c, payload).await
}

/// Schrijft de antwoordkop vóór de `n` databytes die al op `out[HDR_LEN..]`
/// staan; geeft de lengte van het antwoord.
fn answer(out: &mut [u8], op: u8, seq: u32, status: u16, size: u64, n: usize) -> usize {
    let r = Resp {
        op,
        status,
        seq,
        size,
        data: &[],
    };
    abi::hopabi::encode_resp_head(out, &r, n).unwrap_or(0)
}

/// Een antwoord met een fout-`status` en `text` in de data.
fn refuse(out: &mut [u8], op: u8, seq: u32, status: u16, text: &[u8]) -> usize {
    let r = Resp {
        op,
        status,
        seq,
        size: 0,
        data: text,
    };
    abi::hopabi::encode_resp(out, &r).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// De streamende plaatsing.
// ---------------------------------------------------------------------------

/// Waarom een plaatsing niet lukte; de tekst gaat naar Hop of, bij de
/// boot, naar de console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    /// De kern of de actor weigerde.
    Kern(Error),
    /// De plaatsingstoets ([`abi::place`]) weigerde.
    Place(abi::Error),
    /// De ELF-lezer weigerde.
    Elf(leanelf::Error),
    /// Een stroom die niet klopt, met twee getallen.
    Stream(&'static str, u64, u64),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Kern(e)
    }
}

impl From<abi::Error> for Fail {
    fn from(e: abi::Error) -> Fail {
        Fail::Place(e)
    }
}

impl From<leanelf::Error> for Fail {
    fn from(e: leanelf::Error) -> Fail {
        Fail::Elf(e)
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fail::Kern(e) => write!(f, "{e}"),
            Fail::Place(e) => write!(f, "placement: {e}"),
            Fail::Elf(e) => write!(f, "placement: {e}"),
            Fail::Stream(what, a, b) => write!(f, "stream placement: {what} ({a}, {b})"),
        }
    }
}

/// Een little-endian getal uit `b` op `i`, of 0.
fn le(b: &[u8], i: usize, n: usize) -> u64 {
    let mut w = [0u8; 8];
    if let (Some(d), Some(s)) = (w.get_mut(..n), b.get(i..i + n)) {
        d.copy_from_slice(s);
    }
    u64::from_le_bytes(w)
}

/// Leest `dst.len()` bytes op `pa`, ook ongealigneerd (woordgewijs).
fn read_bytes(mem: &impl PhysMem, pa: u64, dst: &mut [u8]) {
    let mut i = 0;
    while i < dst.len() {
        let a = pa.wrapping_add(i as u64);
        let (word, lead) = (a & !7, (a & 7) as usize);
        let w = mem.read64(word).to_le_bytes();
        let n = (8 - lead).min(dst.len() - i);
        if let (Some(d), Some(s)) = (dst.get_mut(i..i + n), w.get(lead..lead + n)) {
            d.copy_from_slice(s);
        }
        i += n;
    }
}

/// De naam op `at` in de stringtabel van `len` bytes op `pa`, als hij met
/// zijn NUL in `buf` past; een langere naam is geen van de gezochte.
fn name_at<'b>(
    mem: &impl PhysMem,
    pa: u64,
    len: u64,
    at: u64,
    buf: &'b mut [u8],
) -> Option<&'b [u8]> {
    let n = len.checked_sub(at)?.min(buf.len() as u64) as usize;
    let s = buf.get_mut(..n)?;
    read_bytes(mem, pa + at, s);
    let end = s.iter().position(|&c| c == 0)?;
    s.get(..end)
}

/// Nult `len` bytes op offset `off` van de partitie van `g`: de randen via
/// de grant, het gealigneerde midden in één `clear`.
fn zero(g: &mut ImageGrant, mem: &mut impl PhysMem, off: u64, len: u64) -> Result {
    const Z: [u8; 8] = [0; 8];
    let r = g.region();
    match off.checked_add(len) {
        Some(end) if end <= r.size => {}
        _ => {
            return Err(Error::Range {
                base: r.base.wrapping_add(off),
                size: len,
            });
        }
    }
    let (a, b) = ((off + 7) & !7, (off + len) & !7);
    if a >= b {
        let mut o = off;
        while o < off + len {
            let n = (off + len - o).min(8);
            g.write(mem, o, Z.get(..n as usize).unwrap_or(&Z))?;
            o += n;
        }
        return Ok(());
    }
    g.write(mem, off, Z.get(..(a - off) as usize).unwrap_or(&[]))?;
    mem.clear(r.base + a, b - a);
    g.write(mem, b, Z.get(..(off + len - b) as usize).unwrap_or(&[]))
}

/// De streamende plaatsing van één image: elke byte meteen op zijn plek.
///
/// Volgorde is de enige aanname (zoals in Go, getoetst op echte
/// tamago-images): de PT_LOAD's komen in oplopende bestandsoffset. Een image
/// dat dat niet doet, faalt luid in plaats van stil verkeerd geplaatst te
/// worden. De schrapruimte voor de staart is exact passend: de image-maat
/// is vooraf bekend en zodra de headers binnen zijn ook het einde van het
/// laatste PT_LOAD.
#[derive(Debug)]
struct Placer {
    /// De aangekondigde image-maat.
    size: u64,
    /// De app-RAM-maat van de partitie.
    app_ram: u64,
    /// Hoeveel bytes er gerouteerd zijn (na `ready`).
    pos: u64,
    /// De kop tot de program headers compleet zijn.
    head: Vec<u8>,
    /// De ELF-header, bewaard voor de sectietabel bij de laatste byte.
    ehdr: [u8; 64],
    /// De routeringstabel: de PT_LOAD's in bestandsvolgorde.
    segs: BoundedVec<Segment, MAX_SEGMENTS>,
    /// Waar het laatste PT_LOAD in het bestand eindigt.
    seg_end: u64,
    /// De schrapruimte (offset vanaf de linkbasis) voor de staart.
    scr_off: u64,
    /// De entry uit de header.
    entry: u64,
    ready: bool,
    /// Een rauwe stroom ([`Raw`]): bytes naar hun offset, geen ELF en geen
    /// start.
    raw: bool,
}

impl Placer {
    fn new(size: u64, app_ram: u64) -> core::result::Result<Placer, Fail> {
        if size == 0 {
            return Err(Fail::Place(abi::Error::ImageSize(0)));
        }
        place::Window::canonical(app_ram, 0, app_ram).check()?;
        Ok(Placer {
            size,
            app_ram,
            pos: 0,
            head: Vec::new(),
            ehdr: [0; 64],
            segs: BoundedVec::new(),
            seg_end: 0,
            scr_off: 0,
            entry: 0,
            ready: false,
            raw: false,
        })
    }

    /// Een rauwe stroom (een flip-bundel, een nieuw image van Hop): `size`
    /// bytes op offset 0 van een partitie met `app_ram` bytes onder de
    /// staart.
    fn raw(size: u64, app_ram: u64) -> core::result::Result<Placer, Fail> {
        if size > app_ram {
            return Err(Self::fail(
                "raw stream larger than its partition",
                size,
                app_ram,
            ));
        }
        let mut p = Placer::new(size, app_ram)?;
        p.raw = true;
        p.ready = true;
        Ok(p)
    }

    /// De plaatsing van `size` bytes in de partitie van `g`: het app-RAM is
    /// de partitie min de ABI-staart.
    fn for_grant(g: &ImageGrant, size: u64, raw: bool) -> core::result::Result<Placer, Fail> {
        let part = g.region().size;
        let ram = part
            .checked_sub(ABI_TAIL)
            .filter(|&ram| abi::layout::Tail::new(LINK_BASE, ram).is_some())
            .ok_or(Fail::Kern(Error::PartitionSize { size: part }))?;
        if raw {
            Placer::raw(size, ram)
        } else {
            Placer::new(size, ram)
        }
    }

    /// Hoeveel bytes er binnen zijn.
    fn received(&self) -> u64 {
        if self.ready {
            self.pos
        } else {
            self.head.len() as u64
        }
    }

    fn fail(what: &'static str, a: u64, b: u64) -> Fail {
        Fail::Stream(what, a, b)
    }

    /// Voert de volgende bytes in.
    fn feed(
        &mut self,
        g: &mut ImageGrant,
        mem: &mut impl PhysMem,
        chunk: &[u8],
    ) -> core::result::Result<(), Fail> {
        let have = self.received();
        if chunk.len() as u64 > self.size - have {
            return Err(Self::fail(
                "more bytes than announced",
                have + chunk.len() as u64,
                self.size,
            ));
        }
        if self.raw {
            g.write(mem, self.pos, chunk)?;
            self.pos += chunk.len() as u64;
            return Ok(());
        }
        if self.ready {
            return self.route(g, mem, chunk);
        }
        let take = chunk.len().min(MAX_HEAD - self.head.len());
        let (now, rest) = chunk.split_at(take);
        self.head
            .try_reserve(take)
            .map_err(|_| Error::OutOfMemory { bytes: take })?;
        self.head.extend_from_slice(now);
        if !self.parse_head()? {
            if !rest.is_empty() || self.head.len() >= MAX_HEAD {
                return Err(Self::fail(
                    "program headers not complete within",
                    MAX_HEAD as u64,
                    0,
                ));
            }
            if self.head.len() as u64 == self.size {
                return Err(Self::fail(
                    "image ended before its program headers",
                    self.size,
                    0,
                ));
            }
            return Ok(());
        }
        // De headers staan: de gebufferde kop alsnog routeren, daarna
        // stroomt de rest er rechtstreeks doorheen.
        let head = core::mem::take(&mut self.head);
        self.ready = true;
        self.route(g, mem, &head)?;
        self.route(g, mem, rest)
    }

    /// Kijkt of de program headers binnen zijn en bouwt dan de routering;
    /// `false` is "nog te weinig".
    fn parse_head(&mut self) -> core::result::Result<bool, Fail> {
        let f = match leanelf::File::parse(&self.head) {
            Ok(f) => f,
            Err(leanelf::Error::Truncated {
                part: leanelf::Part::Magic | leanelf::Part::Header | leanelf::Part::ProgramHeaders,
                ..
            }) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let lo = LINK_BASE;
        let mut last = 0;
        for s in f.segments().filter(|s| s.kind == leanelf::PT_LOAD) {
            let seg = Segment {
                paddr: s.paddr,
                off: s.off,
                filesz: s.filesz,
                memsz: s.memsz,
            };
            if seg.off < last {
                return Err(Self::fail(
                    "PT_LOAD out of file order, not streamable",
                    seg.off,
                    last,
                ));
            }
            // Grof vóór de eerste byte landt; de gezaghebbende toets doet
            // `place::build` bij de laatste.
            if seg.filesz > seg.memsz
                || seg.memsz > self.app_ram
                || seg.paddr < lo
                || seg.paddr > lo + self.app_ram - seg.memsz
            {
                return Err(Self::fail(
                    "segment outside the partition window",
                    seg.paddr,
                    seg.memsz,
                ));
            }
            if seg.off > self.size || seg.filesz > self.size - seg.off {
                return Err(Self::fail("segment outside the image", seg.off, seg.filesz));
            }
            last = seg.off + seg.filesz;
            self.segs.push(seg).map_err(|_| abi::Error::TooMany {
                what: "PT_LOAD segments",
                cap: MAX_SEGMENTS,
            })?;
        }
        if self.segs.is_empty() {
            return Err(Fail::Place(abi::Error::NoSegments));
        }
        // De staart (sectieheaders, symbooltabel) krijgt een exact passende
        // schrapruimte bovenin het app-RAM; de segmenten, met hun BSS,
        // blijven eronder.
        let tail = self.size - last;
        if tail >= self.app_ram {
            return Err(Self::fail(
                "image tail larger than the partition",
                tail,
                self.app_ram,
            ));
        }
        self.scr_off = (self.app_ram - tail) & !7;
        if let Some(s) = self
            .segs
            .iter()
            .find(|s| s.paddr + s.memsz > lo + self.scr_off)
        {
            return Err(Self::fail(
                "segments and image tail do not fit together",
                s.paddr + s.memsz,
                tail,
            ));
        }
        self.seg_end = last;
        self.entry = f.entry;
        if let Some(h) = self.head.get(..64) {
            self.ehdr.copy_from_slice(h);
        }
        Ok(true)
    }

    /// Het PT_LOAD dat bestandsoffset `off` draagt.
    fn seg_at(&self, off: u64) -> Option<Segment> {
        self.segs
            .iter()
            .find(|s| off >= s.off && off < s.off + s.filesz)
            .copied()
    }

    /// Hoeveel bytes vanaf `off` buiten elk segment vallen (hoogstens `max`).
    fn gap_len(&self, off: u64, max: u64) -> u64 {
        let next = self
            .segs
            .iter()
            .map(|s| s.off)
            .filter(|&o| o > off && o < off + max)
            .min()
            .unwrap_or(off + max);
        next - off
    }

    /// Stuurt elke byte naar zijn plek: binnen een PT_LOAD naar de
    /// partitie, voorbij het laatste naar de schrapruimte, uitlijngaten
    /// nergens heen (daar staan nullen in het bestand).
    fn route(
        &mut self,
        g: &mut ImageGrant,
        mem: &mut impl PhysMem,
        mut p: &[u8],
    ) -> core::result::Result<(), Fail> {
        while !p.is_empty() {
            let off = self.pos;
            let len = p.len() as u64;
            let (n, dst) = match self.seg_at(off) {
                Some(s) => (
                    (s.off + s.filesz - off).min(len),
                    Some(s.paddr - LINK_BASE + (off - s.off)),
                ),
                None => {
                    let n = self.gap_len(off, len);
                    (
                        n,
                        (off >= self.seg_end).then(|| self.scr_off + (off - self.seg_end)),
                    )
                }
            };
            let (now, rest) = p.split_at(n as usize);
            if let Some(d) = dst {
                g.write(mem, d, now)?;
            }
            p = rest;
            self.pos += n;
        }
        Ok(())
    }

    /// Het fysieke adres van `len` bytes op bestandsoffset `off`, binnen één
    /// PT_LOAD of in de schrapruimte.
    fn tail_pa(&self, g: &ImageGrant, off: u64, len: u64) -> core::result::Result<u64, Fail> {
        let end = off.saturating_add(len);
        // Een bestandsoffset binnen een PT_LOAD staat al op zijn plek in de
        // partitie; daarbuiten in de schrapruimte. De Go-linker zet de
        // sectietabel vlak achter de program headers, dus in het eerste
        // PT_LOAD (de Rust-linker achteraan); Go's `stream.go` vertaalde
        // allebei en de poort las eerst alleen de schrapruimte (02-10).
        let in_seg = self
            .segs
            .iter()
            .find(|s| off >= s.off && end <= s.off + s.filesz)
            .map(|s| s.paddr - LINK_BASE + (off - s.off));
        let src = match in_seg {
            Some(o) => o,
            None if off >= self.seg_end && end <= self.size => self.scr_off + (off - self.seg_end),
            None => {
                return Err(Self::fail(
                    "symbol tables inside a PT_LOAD, not streamable",
                    off,
                    len,
                ));
            }
        };
        Ok(g.region().base + src)
    }

    /// Zoekt de plaatsingssymbolen, met de symtab-fouten van leanelf, in
    /// brokken uit de partitie in plaats van de hele tabel op de heap: een
    /// riscv64-welcome draagt 42 000 lokale symbolen, en op de LicheeRV was
    /// dat 03-10 "out of memory (1012680 bytes)" bij elke plaatsing.
    fn symbols(
        &self,
        g: &ImageGrant,
        mem: &impl PhysMem,
    ) -> core::result::Result<[Option<leanelf::Symbol<'static>>; 4], Fail> {
        use leanelf::Error as E;
        const NAMES: [&str; 4] = [
            place::SYM_RAM_START,
            place::SYM_RAM_SIZE,
            place::SYM_SLOT_HINT,
            place::SYM_ABI,
        ];
        const LONGEST: usize = {
            let (mut m, mut i) = (0, 0);
            while i < NAMES.len() {
                if NAMES[i].len() > m {
                    m = NAMES[i].len();
                }
                i += 1;
            }
            m
        };
        let shoff = le(&self.ehdr, 40, 8);
        let shentsize = le(&self.ehdr, 58, 2) as u16;
        let shnum = le(&self.ehdr, 60, 2) as u16;
        if shoff == 0 || shnum == 0 {
            return Err(E::NoSectionHeaders.into());
        }
        if shnum > leanelf::MAX_SHNUM {
            return Err(E::TooManyShdrs(shnum).into());
        }
        if shentsize != 64 {
            return Err(E::ShentSize(shentsize).into());
        }
        let tab = self.tail_pa(g, shoff, u64::from(shnum) * 64)?;
        let shdr = |i: u64| {
            let mut h = [0u8; 64];
            read_bytes(mem, tab + i * 64, &mut h);
            h
        };
        let Some(sym) = (0..u64::from(shnum)).map(shdr).find(|h| le(h, 4, 4) == 2) else {
            return Err(E::NoSymtab.into());
        };
        if le(&sym, 56, 8) != 24 {
            return Err(E::SymEntSize(le(&sym, 56, 8)).into());
        }
        let link = le(&sym, 40, 4);
        if link >= u64::from(shnum) {
            return Err(E::SymtabLink {
                link: link as u32,
                count: shnum,
            }
            .into());
        }
        let strh = shdr(link);
        if le(&strh, 4, 4) != 3 {
            return Err(E::LinkNotStrtab.into());
        }
        let n = le(&sym, 32, 8) / 24;
        let syms = self.tail_pa(g, le(&sym, 24, 8), le(&sym, 32, 8))?;
        let str_len = le(&strh, 32, 8);
        let strs = self.tail_pa(g, le(&strh, 24, 8), str_len)?;

        let mut out = [None; 4];
        let (mut buf, mut name) = ([0u8; 64 * 24], [0u8; LONGEST + 1]);
        // Ingang nul is de ABI-schildwacht van allemaal nullen.
        for i in (1..n).step_by(64) {
            if out.iter().all(Option::is_some) {
                break;
            }
            let chunk = buf
                .get_mut(..((n - i).min(64) * 24) as usize)
                .unwrap_or(&mut []);
            read_bytes(mem, syms + i * 24, chunk);
            for e in chunk.chunks_exact(24).filter(|e| le(e, 6, 2) != 0) {
                let Some(raw) = name_at(mem, strs, str_len, le(e, 0, 4), &mut name) else {
                    continue;
                };
                let hit = NAMES
                    .iter()
                    .zip(out.iter_mut())
                    .find(|(w, o)| o.is_none() && w.as_bytes() == raw);
                if let Some((&w, o)) = hit {
                    *o = Some(leanelf::Symbol {
                        name: w,
                        value: le(e, 8, 8),
                        size: le(e, 16, 8),
                        info: le(e, 4, 1) as u8,
                        shndx: le(e, 6, 2) as u16,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Sluit de plaatsing af: symbolen lezen, het plan bouwen en toetsen,
    /// BSS nullen, patchen, de schrapruimte wissen. Geeft de entry.
    fn finish(
        &mut self,
        g: &mut ImageGrant,
        mem: &mut impl PhysMem,
        slot: Slot,
    ) -> core::result::Result<u64, Fail> {
        if !self.ready || self.pos != self.size {
            return Err(Self::fail("image incomplete", self.received(), self.size));
        }
        let [ram_start, ram_size, hint, stamp] = self.symbols(g, mem)?;
        // De stempel is inhoud, geen adres: hij staat al op zijn plek.
        let abi_value = stamp.and_then(|s| {
            let n = if s.size == 4 { 4 } else { 8 };
            let off = s.value.checked_sub(LINK_BASE)?;
            (off.checked_add(n)? <= self.app_ram).then(|| {
                let mut b = [0u8; 8];
                read_bytes(
                    mem,
                    g.region().base + off,
                    b.get_mut(..n as usize).unwrap_or(&mut []),
                );
                u64::from_le_bytes(b)
            })
        });
        let image = place::Image {
            size: self.size,
            entry: self.entry,
            segments: &self.segs,
            symbols: place::Symbols {
                ram_start: ram_start.map(|s| s.value),
                ram_size: ram_size.map(|s| s.value),
                slot_hint: hint.map(|s| s.value),
                abi: abi_value,
            },
        };
        let w = place::Window::canonical(self.app_ram, 0, self.scr_off);
        let plan = place::build(&image, &w, slot, Some(abi::ABI_VERSION))?;
        // Build zag dezelfde headers; toch getoetst, want uiteenlopen is
        // precies de klasse fouten die stil blijft.
        if plan.segments.as_slice() != self.segs.as_slice() {
            return Err(Self::fail(
                "plan disagrees with the stream routing",
                plan.segments.len() as u64,
                self.segs.len() as u64,
            ));
        }
        for s in plan.segments.iter() {
            zero(g, mem, s.paddr - LINK_BASE + s.filesz, s.memsz - s.filesz)?;
        }
        for p in plan.patches.iter() {
            g.write(mem, p.addr - LINK_BASE, &p.val.to_le_bytes())?;
        }
        zero(g, mem, self.scr_off, self.size - self.seg_end)?;
        for s in plan.segments.iter() {
            mem.clean_inv(g.region().base + (s.paddr - LINK_BASE), s.memsz);
        }
        Ok(plan.entry)
    }
}

/// Eén lopende stroom: de grant uit de claim, de plaatsing en de env.
struct Stream {
    grant: ImageGrant,
    placer: Placer,
    env: Vec<u8>,
    /// Een rauwe stroom, en voor welke op; `None` voor een app.
    raw: Option<Raw>,
}

impl Stream {
    fn slot(&self) -> Slot {
        self.grant.slot()
    }

    /// Zet de env-blob op de control-page (de kern schrijft, de app leest
    /// hem bij de start).
    fn put_env(&mut self, mem: &mut impl PhysMem) -> Result {
        use abi::hopabi::{CTRL_ENV_DATA, CTRL_ENV_LEN};
        let ctrl = self.placer.app_ram + ABI_CTRL_OFF;
        self.grant.write(mem, ctrl + CTRL_ENV_DATA, &self.env)?;
        self.grant.write(
            mem,
            ctrl + CTRL_ENV_LEN,
            &(self.env.len() as u64).to_le_bytes(),
        )?;
        mem.clean_inv(self.grant.region().base + ctrl, abi::layout::CTRL_STRIDE);
        Ok(())
    }

    /// Sluit een complete stroom af: plaatsen ([`Placer::finish`]), de env
    /// langs de grant-aanbieder van de actor en op de control-page, en
    /// armen. Faalt een stap vóór de arm, dan gaat de grant terug
    /// ([`Request::Abort`]): de kern ruimt zijn eigen reserveringen op.
    /// Geeft de entry.
    async fn arm<'r, const N: usize>(
        mut self,
        inbox: &Mailbox<Envelope<'r>, N>,
        reply: &'r Reply,
        mem: &mut impl PhysMem,
    ) -> core::result::Result<u64, Fail> {
        let slot = self.slot();
        let placed = match self.placer.finish(&mut self.grant, mem, slot) {
            // De grant-haak vóór de env op de control-page gaat (Go:
            // `prepareGrantedEnv`): de actor bezit de aanbieder en geeft de
            // complete env terug.
            Ok(entry) => match grant_env(inbox, reply, slot, &mut self.env).await {
                Ok(()) => self.put_env(mem).map(|()| entry).map_err(Fail::from),
                Err(e) => Err(Fail::from(e)),
            },
            Err(e) => Err(e),
        };
        let entry = match placed {
            Ok(entry) => entry,
            Err(e) => {
                let _ = slots::call(inbox, reply, Request::Abort(self.grant)).await;
                return Err(e);
            }
        };
        let grant = self.grant;
        match slots::call(inbox, reply, Request::Arm { grant, entry }).await? {
            Response::Done => Ok(entry),
            Response::Failed(e) => Err(e.into()),
            _ => Err(Error::Busy.into()),
        }
    }
}

/// Laat de grant-aanbieder van de actor de env van `slot` aanvullen
/// ([`Request::Env`]); `env` wordt de complete blob.
async fn grant_env<'r, const N: usize>(
    inbox: &Mailbox<Envelope<'r>, N>,
    reply: &'r Reply,
    slot: Slot,
    env: &mut Vec<u8>,
) -> Result {
    let asked = Request::Env {
        slot,
        env: core::mem::take(env),
    };
    match slots::call(inbox, reply, asked).await? {
        Response::Env(e) => {
            *env = e;
            Ok(())
        }
        Response::Failed(e) => Err(e),
        _ => Err(Error::Busy),
    }
}

/// De claim van een start: de grant, of waarom niet.
async fn claim<'r, const N: usize>(
    inbox: &Mailbox<Envelope<'r>, N>,
    reply: &'r Reply,
    spec: StartSpec,
) -> Result<ImageGrant> {
    match slots::call(inbox, reply, Request::Claim(spec)).await? {
        Response::Granted(g) => Ok(g),
        Response::Failed(e) => Err(e),
        _ => Err(Error::Busy),
    }
}

/// De env van een start, eigen gemaakt; meer dan de control-page draagt
/// is een weigering vóór de claim.
fn start_env(env: &[u8]) -> Result<Vec<u8>> {
    if env.len() as u64 > abi::hopabi::CTRL_ENV_MAX {
        return Err(Error::TooLarge {
            len: env.len(),
            max: abi::hopabi::CTRL_ENV_MAX as usize,
        });
    }
    try_vec(env)
}

/// Plaatst een image dat al heel in het geheugen staat: de boot (Hop in
/// slot 1, of de gestagede app op QEMU). Dezelfde weg als een start van
/// Hop over de system-API: de claim met `spec`, het image in één brok door
/// de `Placer` en `Stream::arm`, dus ook hier BSS en schrapruimte
/// gewist en de cache van de segmenten en de control-page geveegd. Faalt
/// een stap, dan gaat de grant terug. Geeft de entry.
///
/// # Errors
///
/// Wat de claim, de plaatsing of de arm weigerde.
pub async fn place<'r, const N: usize>(
    inbox: &Mailbox<Envelope<'r>, N>,
    reply: &'r Reply,
    mem: &mut impl PhysMem,
    spec: StartSpec,
    img: &[u8],
    env: &[u8],
) -> core::result::Result<u64, Fail> {
    let env = start_env(env)?;
    let mut grant = claim(inbox, reply, spec).await?;
    let fed = Placer::for_grant(&grant, img.len() as u64, false)
        .and_then(|mut p| p.feed(&mut grant, mem, img).map(|()| p));
    let placer = match fed {
        Ok(p) => p,
        Err(e) => {
            let _ = slots::call(inbox, reply, Request::Abort(grant)).await;
            return Err(e);
        }
    };
    Stream {
        grant,
        placer,
        env,
        raw: None,
    }
    .arm(inbox, reply, mem)
    .await
}

/// Toetst `img` als image voor een partitie van `partition` bytes, zonder
/// één byte te plaatsen: de kop, de program headers, elk PT_LOAD in het
/// venster en in het bestand, en de staart naast de segmenten. Dezelfde
/// toetsen als de eerste brok van een stroom ([`Placer`]); de symbolen
/// toetst pas de plaatsing zelf. Voor de haak van `HOP_RESTART`: een image
/// dat hier al faalt, wordt geweigerd vóór de oude Hop stopt.
///
/// # Errors
///
/// Waarom het image niet geplaatst kan worden.
pub fn check_image(img: &[u8], partition: u64) -> core::result::Result<(), Fail> {
    let ram = partition
        .checked_sub(ABI_TAIL)
        .filter(|&ram| abi::layout::Tail::new(LINK_BASE, ram).is_some())
        .ok_or(Fail::Kern(Error::PartitionSize { size: partition }))?;
    let mut p = Placer::new(img.len() as u64, ram)?;
    let head = img.get(..MAX_HEAD).unwrap_or(img);
    p.head
        .try_reserve_exact(head.len())
        .map_err(|_| Error::OutOfMemory { bytes: head.len() })?;
    p.head.extend_from_slice(head);
    if p.parse_head()? {
        Ok(())
    } else {
        Err(Placer::fail(
            "program headers not complete within",
            MAX_HEAD as u64,
            img.len() as u64,
        ))
    }
}

/// Wat een bevoegde op teruggeeft: `size` en hoeveel databytes er al op
/// `out[HDR_LEN..]` staan.
type Answer = core::result::Result<(u64, usize), Fail>;

/// Waarom een verbinding eindigde; de listener zet het in zijn regel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// De peer sloot (EOF) of het transport viel weg.
    Peer,
    /// De levensduur van het slot is voorbij (stop of herstart); de kern
    /// sloot, ook als de peer nog open stond.
    Evicted,
    /// Geen verkeer binnen [`IDLE_TIMEOUT`].
    Idle,
    /// Een ongeldig frame of een andere fout.
    Failed(Error),
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            End::Peer => f.write_str("peer closed"),
            End::Evicted => f.write_str("slot lifetime ended, closed by the kernel"),
            End::Idle => write!(f, "idle for {} s", IDLE_TIMEOUT.as_secs()),
            End::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Een verbinding onder toezicht: elke wachtende lees of schrijf wacht ook
/// op de stopbel van de levensduur waarvoor ze toegelaten werd, en op het
/// einde van [`IDLE_TIMEOUT`] sinds het laatste verkeer.
struct Watched<'a, C, T> {
    conn: &'a mut C,
    timer: &'a T,
    life: &'a ServicerCtl,
    /// Het laatste moment met verkeer (nanoseconden van `timer`).
    last: u64,
    /// Waarom het toezicht de verbinding sloot.
    end: Option<End>,
}

impl<C: Conn, T: Timer> Watched<'_, C, T> {
    /// Een verbinding hoort bij één levensduur: een oude app die na een
    /// herstart nog bytes stuurt, krijgt de nieuwe eigenaar van hetzelfde
    /// IP nooit cadeau, en een geparkeerde app houdt niets meer vast.
    fn alive(&mut self) -> Result {
        if !self.life.is_ended() {
            return Ok(());
        }
        self.end = Some(End::Evicted);
        Err(Error::Conn)
    }

    /// Hoe lang de verbinding nog stil mag zijn; nul is [`End::Idle`].
    fn idle_left(&mut self) -> Result<Duration> {
        let idle = u64::try_from(IDLE_TIMEOUT.as_nanos()).unwrap_or(u64::MAX);
        let left = idle.saturating_sub(self.timer.now().saturating_sub(self.last));
        if left == 0 {
            self.end = Some(End::Idle);
            return Err(Error::Conn);
        }
        Ok(Duration::from_nanos(left))
    }

    /// Wat het toezicht na een wek zegt: de stop, of de stilte.
    fn check(&mut self) -> Result {
        self.alive()?;
        self.idle_left().map(drop)
    }

    /// Noteert verkeer.
    fn touch(&mut self, r: &Result<usize>) {
        if matches!(r, Ok(n) if *n > 0) {
            self.last = self.timer.now();
        }
    }
}

impl<C: Conn, T: Timer> Conn for Watched<'_, C, T> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        loop {
            // De lees links: klaar werk gaat vóór de stop en de stilte. Een
            // gedropte lees verliest niets, want een `Conn`-lees kopieert pas
            // als hij klaar is.
            let left = self.idle_left()?;
            let watch = select(self.life.ended(), self.timer.sleep(left));
            match select(self.conn.read(buf), watch).await {
                Either::Left(r) => {
                    self.touch(&r);
                    return r;
                }
                Either::Right(_) => self.check()?,
            }
        }
    }

    async fn write(&mut self, buf: &[u8]) -> Result<usize> {
        loop {
            // Een peer die niet leest (een geparkeerde app met een vol
            // venster) houdt de schrijf anders voor altijd vast.
            let left = self.idle_left()?;
            let watch = select(self.life.ended(), self.timer.sleep(left));
            match select(self.conn.write(buf), watch).await {
                Either::Left(r) => {
                    self.touch(&r);
                    return r;
                }
                Either::Right(_) => self.check()?,
            }
        }
    }

    fn remote_ip4(&self) -> u32 {
        self.conn.remote_ip4()
    }
}

/// De system-API: de bevoegdheid, de stromen die tussen twee calls leven en
/// de staat van Hop.
///
/// Deelbaar tussen de verbindingstaken: alles hier is `&self`. De stromen
/// en de staat zijn leesbare tabellen (handboek §1.1); de bevoegdheid staat
/// vast vanaf de bouw.
pub struct System<'i, 'r, const N: usize> {
    inbox: &'i Mailbox<Envelope<'r>, N>,
    svc: &'i Servicers,
    privilege: Option<Privilege>,
    streams: LocalCell<[Option<Stream>; MAX_STREAMS]>,
    /// De staat van Hop (`STATE_PUT`): één blob, van deze service. Een
    /// schrijf bouwt een nieuwe `Vec` en wisselt hem in één lening, dus een
    /// lezer ziet het oude of het nieuwe blob, nooit een mengsel.
    state: LocalCell<Vec<u8>>,
    console: Option<&'i crate::conport::Ring>,
    fs: Option<&'i FsInbox<'r>>,
    devices: Option<&'i crate::deviceabi::Inbox<'r>>,
    store: Option<&'i crate::store::StoreQueue>,
}

impl<'i, 'r, const N: usize> System<'i, 'r, N> {
    /// Een system-API over deze actor en servicers, met de bevoegdheid van
    /// Hop (als die er al is). `const`, zodat de kern hem als `static` aan
    /// al zijn verbindingstaken kan geven.
    pub const fn new(
        inbox: &'i Mailbox<Envelope<'r>, N>,
        svc: &'i Servicers,
        privilege: Option<Privilege>,
    ) -> Self {
        System {
            inbox,
            svc,
            privilege,
            streams: LocalCell::cell([const { None }; MAX_STREAMS]),
            state: LocalCell::cell(Vec::new()),
            console: None,
            fs: None,
            devices: None,
            store: None,
        }
    }

    /// Koppelt de hopfs-actor voor de bestandscalls ([`crate::rpc`]).
    /// Zonder actor (een board zonder schijf) antwoordt elke bestandscall
    /// met een fout, zoals Go zonder `UseFS`.
    #[must_use]
    pub const fn with_fs(mut self, fs: &'i FsInbox<'r>) -> Self {
        self.fs = Some(fs);
        self
    }

    /// Koppelt de optische eigenaar, los van de bestandsactor.
    #[must_use]
    pub const fn with_devices(mut self, devices: &'i crate::deviceabi::Inbox<'r>) -> Self {
        self.devices = Some(devices);
        self
    }

    /// Koppelt de rij van de store-ops ([`crate::store`]). Zonder rij
    /// antwoordt elke store-call met een fout: deze node heeft geen
    /// object-store.
    #[must_use]
    pub const fn with_store(mut self, store: &'i crate::store::StoreQueue) -> Self {
        self.store = Some(store);
        self
    }

    /// Koppelt de console van de kern voor `NEXT_LOG` 0
    /// ([`crate::conport::Ring::hop_line`]). Zonder ring antwoordt
    /// `NEXT_LOG` 0 altijd leeg.
    #[must_use]
    pub const fn with_console(mut self, console: &'i crate::conport::Ring) -> Self {
        self.console = Some(console);
        self
    }

    /// Laat een verbinding toe: het slot uit het bron-IP, een levende
    /// servicer, en hooguit [`MAX_SYSTEM_CONNS`] van die levensduur (Hop
    /// [`MAX_HOP_CONNS`]); de verbindingen van een vorige levensduur tellen
    /// niet mee: zij houden hun eigen besturing ([`ServicerCtl::try_conn`]).
    /// Het resultaat geeft de
    /// verbinding weer vrij in zijn `Drop`; een weigering zegt waarom.
    ///
    /// # Errors
    ///
    /// Geen slot achter het adres, geen levende bewoner, of vol.
    pub fn admit(&self, ip: u32) -> core::result::Result<Admitted, Refusal> {
        let slot = slot_from_remote(ip).ok_or(Refusal::NoSlot)?;
        let ctl = self.svc.ctl(slot).ok_or(Refusal::NotLive { slot })?;
        let generation = ctl.generation();
        // Hop houdt één verbinding meer: zijn store-taak wacht er blijvend
        // op NEXT_STORE, naast de eigenaar-taak en de kloktaak.
        let hop = self.privilege.as_ref().is_some_and(|p| p.slot == slot);
        let cap = if hop { MAX_HOP_CONNS } else { MAX_SYSTEM_CONNS };
        // `then`, niet `then_some`: een gretig gebouwde `Admitted` die bij
        // een weigering wegvalt, geeft in zijn `Drop` een plaats terug die
        // nooit genomen was (gemeten 29-09: na één weigering liet de cap een
        // derde verbinding toe).
        ctl.try_conn(cap)
            .then(|| Admitted {
                slot,
                generation,
                ctl: Rc::clone(&ctl),
            })
            .ok_or(Refusal::Full { slot, cap })
    }

    /// Dient één verbinding: frames lezen tot de peer weggaat, de
    /// levensduur van het slot eindigt, de verbinding [`IDLE_TIMEOUT`] stil
    /// is of een frame ongeldig is. Meerdere verbindingen tegelijk mogen
    /// (elk in een eigen taak): `buf` is de callbuffer van déze verbinding,
    /// `out` haar antwoordbuffer, `reply` haar antwoordplek bij de actor.
    /// `timer` draagt de termijn van de stilte. Een log-frame (`Kind::Log`) gaat
    /// naar `log.app_line`, dus op de console (en zo bij Hop, `NEXT_LOG` 0).
    /// Een Rust-app logt via de outbox; `Kind::Log` blijft voor de
    /// Go-apps (`Logf` van de Go-SDK `metal/v2` schrijft elke regel zo,
    /// slot-ABI 10) en appspike toetst hem.
    #[expect(
        clippy::too_many_arguments,
        reason = "de verbinding brengt haar eigen buffers, antwoordplek en tik mee"
    )]
    pub async fn serve(
        &self,
        conn: &mut impl Conn,
        who: &Admitted,
        reply: &'r Reply,
        timer: &impl Timer,
        mem: &mut impl PhysMem,
        hooks: &impl Hooks,
        log: &impl Console,
        buf: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> End {
        let mut w = Watched {
            conn,
            timer,
            life: &who.ctl,
            last: timer.now(),
            end: None,
        };
        let r = self
            .frames(&mut w, who, reply, mem, hooks, log, buf, out)
            .await;
        match (w.end, r) {
            (Some(end), _) => end,
            (None, Ok(()) | Err(Error::Conn)) => End::Peer,
            (None, Err(e)) => End::Failed(e),
        }
    }

    /// De framelus van één verbinding.
    #[expect(
        clippy::too_many_arguments,
        reason = "de verbinding brengt haar eigen buffers en antwoordplek mee"
    )]
    async fn frames<C: Conn, T: Timer>(
        &self,
        w: &mut Watched<'_, C, T>,
        who: &Admitted,
        reply: &'r Reply,
        mem: &mut impl PhysMem,
        hooks: &impl Hooks,
        log: &impl Console,
        buf: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> Result {
        let slot = who.slot;
        loop {
            let Header { kind, len: n } = read_header(w).await?;
            if kind == Kind::Result {
                return Err(Error::Corrupt { at: 5 });
            }
            let payload = buf.get_mut(..n).ok_or(Error::TooLarge { len: n, max: 0 })?;
            read_full(w, payload).await?;
            w.alive()?;
            if kind == Kind::Log {
                log.app_line(slot, payload);
                continue;
            }
            let len = match decode_req(payload) {
                Ok(call) if rpc::is_fs_op(call.op) || call.op == abi::hopabi::OP_DEVICE_COMMAND => {
                    // Alleen getallen en bereiken: de lening van `buf` eindigt
                    // hier, want de buffer zelf gaat zo naar de hopfs-actor.
                    let path = HDR_LEN..HDR_LEN + call.path.len();
                    let head = FsHead {
                        op: call.op,
                        seq: call.seq,
                        off: call.off,
                        n: call.n,
                        data: path.end..n,
                        path,
                    };
                    self.fs_call(who, head, reply, buf, out).await
                }
                Ok(call) if crate::store::is_store_call(call.op) => {
                    // Zoals een bestandscall: alleen getallen en bereiken, want
                    // een lees of schrijf van Hop neemt de buffers mee.
                    let h = crate::store::Head::new(
                        call.op,
                        call.seq,
                        call.off,
                        call.n,
                        call.path.len(),
                        n,
                    );
                    self.store_call(who, h, reply, w.timer, log, buf, out).await
                }
                #[cfg(feature = "media")]
                // Open kan ontbrekende firmware laden vóór de driverbeurt.
                Ok(call) if crate::codecabi::is_codec_op(call.op) => {
                    crate::codecabi::serve_with_firmware(
                        slot,
                        who.generation,
                        &call,
                        out,
                        self.fs,
                        reply,
                        w.timer,
                    )
                    .await
                }
                Ok(call) => self.call(slot, &call, reply, mem, hooks, out).await,
                Err(_) => refuse(out, 0, 0, STATUS_ERROR, b"bad request"),
            };
            write_frame(w, Kind::Result, out.get(..len).unwrap_or(&[])).await?;
        }
    }

    /// Een bestandscall: de buffers van de verbinding gaan als waarde naar
    /// de hopfs-actor en komen met het antwoord terug.
    async fn fs_call(
        &self,
        who: &Admitted,
        h: FsHead,
        reply: &'r Reply,
        buf: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> usize {
        let device = match crate::deviceabi::target(
            self.svc,
            who.slot,
            who.generation,
            buf.get(h.path.clone()).unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(e) => return fail(out, h.op, h.seq, &Fail::Kern(e)),
        };
        if h.op == abi::hopabi::OP_DEVICE_COMMAND && device.is_none() {
            return fail(out, h.op, h.seq, &Fail::Kern(Error::Denied));
        }
        if (device.is_some() && self.devices.is_none()) || (device.is_none() && self.fs.is_none()) {
            return refuse(out, h.op, h.seq, STATUS_NO_ENT, b"no storage on this node");
        }
        let c = FsCall {
            slot: who.slot,
            generation: who.generation,
            op: h.op,
            off: h.off,
            n: h.n,
            path: h.path,
            data: h.data,
            buf: core::mem::take(buf),
            out: core::mem::take(out),
        };
        let done = if device.is_some() {
            match self.devices {
                Some(inbox) => crate::deviceabi::call(inbox, reply, c).await,
                None => Err(c),
            }
        } else {
            match self.fs {
                Some(inbox) => rpc::call(inbox, reply, c).await,
                None => Err(c),
            }
        };
        let result = match done {
            Ok(d) => {
                (*buf, *out) = (d.buf, d.out);
                d.result
            }
            Err(c) => {
                (*buf, *out) = (c.buf, c.out);
                Err(Error::Busy)
            }
        };
        match result {
            Ok((size, data_len)) => answer(out, h.op, h.seq, STATUS_OK, size, data_len),
            Err(e) => fail(out, h.op, h.seq, &Fail::Kern(e)),
        }
    }

    /// Een store-call ([`crate::store`]): een app die wacht op Hop, of Hop
    /// die een opdracht ophaalt, bytes verplaatst of afmeldt.
    #[expect(
        clippy::too_many_arguments,
        reason = "de verbinding brengt haar eigen buffers, antwoordplek en tik mee"
    )]
    async fn store_call<T: Timer, L: Console>(
        &self,
        who: &Admitted,
        h: crate::store::Head,
        reply: &'r Reply,
        timer: &T,
        log: &L,
        buf: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> usize {
        let Some(queue) = self.store else {
            return refuse(
                out,
                h.op,
                h.seq,
                STATUS_ERROR,
                b"no object store on this node (the kernel has no store queue)",
            );
        };
        let cx = crate::store::Ctx {
            queue,
            fs: self.fs,
            svc: self.svc,
            slot: who.slot,
            life: Rc::clone(&who.ctl),
            hop: self.privilege.as_ref().filter(|p| p.slot == who.slot),
            hop_slot: self.privilege.as_ref().map(Privilege::slot),
            reply,
            timer,
            log,
            news: self.console.map(crate::conport::Ring::news),
        };
        crate::store::serve(&cx, h, buf, out).await
    }

    async fn call(
        &self,
        slot: Slot,
        c: &Req<'_>,
        reply: &'r Reply,
        mem: &mut impl PhysMem,
        hooks: &impl Hooks,
        out: &mut [u8],
    ) -> usize {
        let r = match (PrivOp::from_op(c.op), &self.privilege) {
            // Een onbekende op (de codec-calls zonder de feature `media`;
            // de store-calls dient `frames` via `crate::store`).
            (None, _) => Err(Fail::Kern(Error::Kind)),
            (Some(op), Some(p)) if p.slot == slot => {
                self.privileged(p, op, c, reply, mem, hooks, out).await
            }
            (Some(_), _) => Err(Fail::Kern(Error::Privilege { slot: slot.get() })),
        };
        match r {
            Ok((size, data_len)) => answer(out, c.op, c.seq, STATUS_OK, size, data_len),
            Err(e) => fail(out, c.op, c.seq, &e),
        }
    }

    /// De bevoegde operaties. `&Privilege` is het bewijs.
    #[expect(
        clippy::too_many_arguments,
        reason = "de call brengt verbinding, geheugen en haken mee"
    )]
    async fn privileged(
        &self,
        _proof: &Privilege,
        op: PrivOp,
        c: &Req<'_>,
        reply: &'r Reply,
        mem: &mut impl PhysMem,
        hooks: &impl Hooks,
        out: &mut [u8],
    ) -> Answer {
        let data = out.get_mut(HDR_LEN..).unwrap_or(&mut []);
        match op {
            PrivOp::StartSlot => self.start(c, reply).await,
            PrivOp::StreamImage => self.stream(c, reply, mem, data).await,
            PrivOp::StopSlot => {
                let slot = target(c)?;
                if let Some(s) = self.take_stream(slot) {
                    return done(slots::call(self.inbox, reply, Request::Abort(s.grant)).await?);
                }
                let timeout = Duration::from_millis(c.n);
                done(slots::call(self.inbox, reply, Request::Stop { slot, timeout }).await?)
            }
            PrivOp::SlotStatus => self.status(c, reply, hooks, data).await,
            PrivOp::Slots => self.occupied(c, reply, data).await,
            PrivOp::NextLog => {
                let max = usize::try_from(c.n).unwrap_or(usize::MAX).min(data.len());
                let dst = data.get_mut(..max).unwrap_or(&mut []);
                // Alleen slot 0: de console, met de regels van de apps erin
                // (`slot N: ...`); Hop splitst ze zelf op slot.
                if c.off != 0 {
                    return Err(Error::SlotRange {
                        slot: usize::try_from(c.off).unwrap_or(usize::MAX),
                        max: 0,
                    }
                    .into());
                }
                match self.console.and_then(|k| k.hop_line(dst)) {
                    Some(n) => Ok((1, n)),
                    None => Ok((0, 0)),
                }
            }
            PrivOp::SetClock => {
                hooks.set_clock(c.n);
                Ok((0, 0))
            }
            // De store-ops van Hop dient `frames` via `crate::store`: ze
            // hebben de buffers van de verbinding nodig (de bytes gaan als
            // waarde naar de hopfs-actor). Hier komen ze nooit.
            PrivOp::NextStore | PrivOp::StoreRead | PrivOp::StoreWrite | PrivOp::StoreDone => {
                Err(Error::Kind.into())
            }
            PrivOp::Flip => {
                let sha = &flip_sum(c.path)?;
                let slot = target(c)?;
                // Een vlag is een weigering vóór de stroom wordt opgeëist: er
                // is alleen de warme flip (01-11), en een Hop die koud
                // vraagt, krijgt nee in plaats van iets anders.
                if c.n != 0 {
                    return Err(Error::Version { have: c.n, want: 0 }.into());
                }
                let complete = self.with_stream(slot, |s| {
                    s.raw == Some(Raw::FlipBundle) && s.placer.received() == s.placer.size
                });
                if complete != Some(true) {
                    return Err(Error::NotOwned { slot: slot.get() }.into());
                }
                let Some(s) = self.take_stream(slot) else {
                    return Err(Error::NotOwned { slot: slot.get() }.into());
                };
                let bundle = FlipBundle {
                    slot,
                    base: s.grant.region().base,
                    len: s.placer.size,
                };
                let r = hooks.flip(&bundle, sha);
                // Gelukt of niet: het slot gaat terug. Bij succes heeft de
                // haak de nieuwe kern al buiten de partitie klaargelegd.
                let _ = slots::call(self.inbox, reply, Request::Abort(s.grant)).await;
                r?;
                Ok((0, 0))
            }
            PrivOp::StatePut => {
                self.state_put(c.data)?;
                Ok((0, 0))
            }
            PrivOp::StateGet => self.state_get(c.n, data),
            PrivOp::HopRestart => {
                let ask = match hop_ask(c) {
                    Ok(a) => a,
                    Err((why, e)) => {
                        hooks.hop_refused(why, &e);
                        return Err(e);
                    }
                };
                self.hop_restart(ask, reply, hooks).await
            }
        }
    }

    // De staat van Hop. Eén lening per call, zonder `.await`: een lezer ziet
    // het blob van vóór of van na een schrijf.

    /// STATE_PUT: vervangt het blob door `data`; nul bytes wist het. Het
    /// nieuwe blob staat helemaal klaar (faalbaar gealloceerd) vóór het
    /// oude wijkt: een weigering laat het oude staan.
    fn state_put(&self, data: &[u8]) -> Result {
        if data.len() > HOP_STATE_MAX {
            return Err(Error::TooLarge {
                len: data.len(),
                max: HOP_STATE_MAX,
            });
        }
        let new = try_vec(data)?;
        let old = core::mem::replace(&mut *self.state.borrow_mut(), new);
        drop(old);
        Ok(())
    }

    /// STATE_GET: de maat van het hele blob en de bytes vanaf `at` in
    /// `data`, zoveel als passen en nooit meer dan [`HOP_STATE_MAX`] (de
    /// buffer van Hop). Niets klaar is `STATUS_OK` met maat 0: een fout
    /// leest Hop als een kern zonder staat. Een offset voorbij het einde is
    /// wel een fout, geen leeg antwoord: dan veranderde het blob onder de
    /// lezer.
    fn state_get(&self, at: u64, data: &mut [u8]) -> Answer {
        let blob = self.state.borrow();
        let size = blob.len() as u64;
        let rest = usize::try_from(at)
            .ok()
            .and_then(|at| blob.get(at..))
            .ok_or(Error::Range { base: at, size })?;
        let n = rest.len().min(data.len()).min(HOP_STATE_MAX);
        if let (Some(d), Some(s)) = (data.get_mut(..n), rest.get(..n)) {
            d.copy_from_slice(s);
        }
        Ok((size, n))
    }

    /// De maat van de staat van Hop die nu klaarligt (0: niets). Voor de
    /// supervisor van Hop, die hem bij elke herstart op de console zet.
    #[must_use]
    pub fn hop_state_len(&self) -> usize {
        self.state.borrow().len()
    }

    /// HOP_RESTART na de veldtoetsen ([`hop_ask`]): een nieuw image moet
    /// compleet in een [`Raw::HopImage`]-stroom liggen. De haak toetst en
    /// kopieert; gelukt of niet, daarna gaat het slot terug in de pool.
    async fn hop_restart(&self, ask: HopAsk, reply: &'r Reply, hooks: &impl Hooks) -> Answer {
        let Some((slot, sha256)) = ask.image else {
            hooks.hop_restart(None, ask.grace)?;
            return Ok((0, 0));
        };
        let complete = self.with_stream(slot, |s| {
            s.raw == Some(Raw::HopImage) && s.placer.received() == s.placer.size
        });
        let stream = match complete {
            Some(true) => self.take_stream(slot),
            _ => None,
        };
        let Some(s) = stream else {
            let e = Fail::from(Error::NotOwned { slot: slot.get() });
            hooks.hop_refused("no complete image stream", &e);
            return Err(e);
        };
        let image = HopImage {
            slot,
            base: s.grant.region().base,
            len: s.placer.size,
            sha256,
        };
        let r = hooks.hop_restart(Some(&image), ask.grace);
        // Gelukt of niet: het slot gaat terug. Bij succes heeft de haak het
        // image al buiten de partitie gekopieerd.
        let _ = slots::call(self.inbox, reply, Request::Abort(s.grant)).await;
        r?;
        Ok((0, 0))
    }

    /// Breekt elke open stroom af en geeft zijn slot terug aan de pool;
    /// geeft hoeveel. Voor de supervisor van Hop, tussen de stop van een
    /// Hop en de plaatsing van de volgende: een stroom is van de levensduur
    /// van de Hop die hem begon, en de nieuwe kent hem niet (hij zou zijn
    /// slot anders tot de volgende boot bezet houden).
    pub async fn drop_streams(&self, reply: &'r Reply) -> usize {
        let mut n = 0;
        loop {
            let s = self.streams.borrow_mut().iter_mut().find_map(Option::take);
            let Some(s) = s else { return n };
            let _ = slots::call(self.inbox, reply, Request::Abort(s.grant)).await;
            n += 1;
        }
    }

    // De stromentabel. Elke toegang is één synchrone lening; wie iets met
    // een stroom wil over een `.await` heen, haalt hem eruit (`take_stream`)
    // en is dan de enige eigenaar.

    /// Loopt er een stroom voor `slot`? (Voor de toetsen.)
    #[cfg(test)]
    fn has_stream(&self, slot: Slot) -> bool {
        self.streams
            .borrow()
            .iter()
            .flatten()
            .any(|s| s.slot() == slot)
    }

    /// Is er plaats voor nog een stroom?
    fn has_room(&self) -> bool {
        self.streams.borrow().iter().any(Option::is_none)
    }

    /// Doet `f` op de stroom van `slot`, binnen één lening.
    fn with_stream<R>(&self, slot: Slot, f: impl FnOnce(&mut Stream) -> R) -> Option<R> {
        self.streams
            .borrow_mut()
            .iter_mut()
            .flatten()
            .find(|s| s.slot() == slot)
            .map(f)
    }

    fn take_stream(&self, slot: Slot) -> Option<Stream> {
        self.streams
            .borrow_mut()
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|s| s.slot() == slot))
            .and_then(Option::take)
    }

    /// Zet een stroom in een vrije plaats; is de tabel vol, dan komt hij
    /// terug.
    fn put_stream(&self, s: Stream) -> Option<Stream> {
        match self.streams.borrow_mut().iter_mut().find(|e| e.is_none()) {
            Some(e) => {
                *e = Some(s);
                None
            }
            None => Some(s),
        }
    }

    /// START_SLOT: de kern kiest een leeg slot, claimt het en houdt de
    /// grant voor de stroom.
    async fn start(&self, c: &Req<'_>, reply: &'r Reply) -> Answer {
        let req = StartReq::decode(c)?;
        if req.image_size == 0 {
            return Err(Fail::Place(abi::Error::ImageSize(0)));
        }
        let env = start_env(req.env)?;
        if req.memory_limit <= ABI_TAIL {
            return Err(Error::PartitionSize {
                size: req.memory_limit,
            }
            .into());
        }
        // Vooraf, zodat een volle tabel niets claimt; de gezaghebbende
        // toets is `put_stream` hieronder, want tussen de twee kan een
        // andere verbinding van Hop een plaats nemen.
        if !self.has_room() {
            return Err(Error::Full { cap: MAX_STREAMS }.into());
        }
        // Een flipbundel draait nooit: hij is een rauwe partitie tot
        // PrivOp::FLIP hem opeist. Dus geen eigen core, maar die van Hop
        // (`Placement::hop`); anders weigert een node met elke app-core
        // bezet de flip al bij de reservering (QEMU riscv64 en de LicheeRV
        // met hun ene app-hart, 02-10: "no free run of 1 app core(s)").
        // Een nieuw image van Hop net zo: het draait pas in slot 1.
        let raw = Raw::of(req.job);
        let placement = if raw.is_some() {
            Placement::hop()?
        } else {
            placement(&req)?
        };
        // De partitie van een rauwe stroom bemeet de kern zelf: de bytes
        // plus de ABI-staart, op hele 2 MB. Wat Hop vraagt is een
        // ondergrens, geen maat: een Hop die precies `bundel + staart`
        // vroeg, kreeg na de guard-pagina 4 KiB te weinig en elke flip werd
        // geweigerd ("length 2252800 exceeds 2248704", de O6N 30-09).
        let mem = if raw.is_some() {
            req.memory_limit
                .max(req.image_size.saturating_add(ABI_TAIL))
                .next_multiple_of(2 << 20)
        } else {
            req.memory_limit
        };
        let job = try_vec(req.job)?;
        // Het slot kiest de actor: het laagste vrije, behalve dat van Hop
        // (dat tijdens zijn herstart even leeg staat). Geen maximum.
        let mut spec = StartSpec::free(mem, placement, self.privilege.as_ref().map(|p| p.slot));
        spec.job = job;
        spec.ports = start_ports(&req)?;
        spec.mounts = start_mounts(&req)?;
        let grant = claim(self.inbox, reply, spec).await?;
        let slot = grant.slot();
        let placer = match Placer::for_grant(&grant, req.image_size, raw.is_some()) {
            Ok(p) => p,
            Err(e) => {
                let _ = slots::call(self.inbox, reply, Request::Abort(grant)).await;
                return Err(e);
            }
        };
        if let Some(s) = self.put_stream(Stream {
            grant,
            placer,
            env,
            raw,
        }) {
            let _ = slots::call(self.inbox, reply, Request::Abort(s.grant)).await;
            return Err(Error::Full { cap: MAX_STREAMS }.into());
        }
        Ok((slot.get() as u64, 0))
    }

    /// STREAM_IMAGE: bytes naar hun plek; bij de laatste plaatsen en armen.
    async fn stream(
        &self,
        c: &Req<'_>,
        reply: &'r Reply,
        mem: &mut impl PhysMem,
        data: &mut [u8],
    ) -> Answer {
        let slot = target(c)?;
        let (fed, received, size) = self
            .with_stream(slot, |s| {
                let have = s.placer.received();
                let fed = if c.n == have {
                    s.placer.feed(&mut s.grant, mem, c.data)
                } else {
                    Err(Placer::fail(
                        "chunk offset differs from bytes received",
                        c.n,
                        have,
                    ))
                };
                (fed, s.placer.received(), s.placer.size)
            })
            .ok_or(Error::NotOwned { slot: slot.get() })?;
        if let Err(e) = fed {
            // Een geweigerde brok breekt de stroom af: de kern ruimt op.
            if let Some(s) = self.take_stream(slot) {
                let _ = slots::call(self.inbox, reply, Request::Abort(s.grant)).await;
            }
            return Err(e);
        }
        if received < size {
            return answer_stream(data, received, StreamState::More, None);
        }
        // Een complete rauwe stroom blijft in de tabel liggen tot FLIP of
        // HOP_RESTART hem opeist (of STOP_SLOT hem afbreekt): "geplaatst" is
        // hier "binnen".
        if self.with_stream(slot, |s| s.placer.raw) == Some(true) {
            return answer_stream(data, received, StreamState::Placed, None);
        }
        let Some(s) = self.take_stream(slot) else {
            return Err(Error::NotOwned { slot: slot.get() }.into());
        };
        match s.arm(self.inbox, reply, mem).await {
            Ok(_) => answer_stream(data, received, StreamState::Placed, None),
            Err(e) => answer_stream(data, received, StreamState::Failed, Some(e)),
        }
    }

    /// SLOT_STATUS: het grootboek van de actor plus de stand van een stroom;
    /// slot 0 is de kern zelf ([`kern_status`]).
    /// SLOTS: de bezette slots na `off`, zoveel als er in `n` en het
    /// antwoord passen.
    async fn occupied(&self, c: &Req<'_>, reply: &'r Reply, data: &mut [u8]) -> Answer {
        let after = usize::try_from(c.off).unwrap_or(usize::MAX);
        let room = usize::try_from(c.n).unwrap_or(usize::MAX).min(data.len()) / SLOT_LEN;
        let list =
            match slots::call(self.inbox, reply, Request::Occupied { after, max: room }).await? {
                Response::Occupied(v) => v,
                Response::Failed(e) => return Err(e.into()),
                _ => return Err(Error::Busy.into()),
            };
        for (s, d) in list.iter().zip(data.chunks_exact_mut(SLOT_LEN)) {
            d.copy_from_slice(&u16::try_from(s.get()).unwrap_or(u16::MAX).to_le_bytes());
        }
        Ok((list.len() as u64, list.len() * SLOT_LEN))
    }

    async fn status(
        &self,
        c: &Req<'_>,
        reply: &'r Reply,
        hooks: &impl Hooks,
        data: &mut [u8],
    ) -> Answer {
        let (st, (received, image_size)) = if c.off == 0 {
            (kern_status(hooks.kern()), (0, 0))
        } else {
            let slot = target(c)?;
            let st = match slots::call(self.inbox, reply, Request::Status(slot)).await? {
                Response::Status(st) => st,
                Response::Failed(e) => return Err(e.into()),
                _ => return Err(Error::Busy.into()),
            };
            let stream = self.with_stream(slot, |s| (s.placer.received(), s.placer.size));
            (st, stream.unwrap_or((0, 0)))
        };
        let (core, span) = st.core.map_or((0, 0), |(c, s)| (c.get() as u16, s as u16));
        let info = SlotInfo {
            state: match st.occupancy {
                Occupancy::Empty => SlotState::Empty,
                Occupancy::Streaming => SlotState::Streaming,
                Occupancy::Running => SlotState::Running,
            } as u8,
            core_on: u8::from(st.cage.core_on),
            reserved: 0,
            core,
            span,
            app: st.cage.app,
            exit_code: st.cage.exit_code,
            heartbeat: st.cage.heartbeat,
            ram_size: st.cage.ram_size,
            fault_vec: st.cage.fault_vec,
            fault_esr: st.cage.fault_esr,
            fault_far: st.cage.fault_far,
            partition: st.partition.map_or(0, |p| p.size),
            received,
            image_size,
            mem_sys: st.cage.mem_sys,
            idle_ns: st.cage.idle_ns,
            wakes: st.cage.wakes,
            cores: st.cage.cores,
            at_ns: st.cage.at_ns,
        };
        fit_info(&info.encode(), c.n, data)
    }
}

/// Slot 0 van SLOT_STATUS: de kern, levend op de OS-core, één core, met de
/// meetlat van [`Hooks::kern`]; geen partitie, geen stroom, geen fault.
fn kern_status(cage: crate::cage::Status) -> slots::SlotStatus {
    slots::SlotStatus {
        occupancy: Occupancy::Running,
        core: Some((crate::Core::OS, 1)),
        partition: None,
        cage: crate::cage::Status {
            core_on: true,
            app: abi::hopabi::AppStatus::Ready as u64,
            cores: 1,
            ..cage
        },
    }
}

/// Het SlotInfo zoals de lezer het vraagt: `want` (het `n` van het
/// verzoek) bytes, of het voorvoegsel van
/// [`abi::systemapi::SLOT_INFO_LEN_V1`] als hij geen maat noemt. Een Hop
/// tot v3.0.3 vraagt niets en leest met een buffer van 64; zijn client
/// (applib::sys, "response longer than the buffer") verbreekt de verbinding
/// op elk langer antwoord, en de kern mag hem niet breken: de Hop van de
/// kaart overleeft een kern-flip (gemeten 03-10: elke nieuwe plaatsing in
/// een herstartlus). Sinds v3.0.5 vraagt Hop de volle maat.
fn fit_info(b: &[u8; abi::systemapi::SLOT_INFO_LEN], want: u64, data: &mut [u8]) -> Answer {
    let max = data.len();
    let want = if want == 0 {
        abi::systemapi::SLOT_INFO_LEN_V1
    } else {
        usize::try_from(want).unwrap_or(usize::MAX)
    };
    let n = b.len().min(want);
    let (dst, src) = match (data.get_mut(..n), b.get(..n)) {
        (Some(d), Some(s)) if n >= abi::systemapi::SLOT_INFO_LEN_V1 => (d, s),
        _ => return Err(Error::TooLarge { len: b.len(), max }.into()),
    };
    dst.copy_from_slice(src);
    Ok((0, n))
}

/// De som van een FLIP of een HOP_RESTART: 32 rauwe bytes, of 64
/// hex-tekens (de client van applib draagt het pad als tekst, en Hop heeft
/// de som al als hex).
fn flip_sum(path: &[u8]) -> Result<[u8; 32]> {
    if let Ok(raw) = <[u8; 32]>::try_from(path) {
        return Ok(raw);
    }
    let bad = Error::TooLarge {
        len: path.len(),
        max: 64,
    };
    if path.len() != 64 {
        return Err(bad);
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (o, pair) in out.iter_mut().zip(path.chunks_exact(2)) {
        let (Some(h), Some(l)) = (nib(pair[0]), nib(pair[1])) else {
            return Err(bad);
        };
        *o = (h << 4) | l;
    }
    Ok(out)
}

/// Een HOP_RESTART na de veldtoetsen: het slot en de som van een nieuw
/// image (`None`: hetzelfde) en de termijn van de stop.
#[derive(Copy, Clone, Debug)]
struct HopAsk {
    image: Option<(Slot, [u8; 32])>,
    grace: Duration,
}

/// De velden van een HOP_RESTART (`abi::systemapi::PrivOp::HopRestart`):
/// de vlaggen in de hoge 32 bits van `n` (nu geen enkele), de termijn in de
/// lage (0: de standaard, hoogstens 30 s), en een som alleen bij een
/// image. Een weigering draagt een korte reden voor de console.
fn hop_ask(c: &Req<'_>) -> core::result::Result<HopAsk, (&'static str, Fail)> {
    let flags = c.n >> 32;
    if flags != 0 {
        return Err((
            "unknown flag",
            Error::Version {
                have: flags,
                want: 0,
            }
            .into(),
        ));
    }
    let ms = match c.n & 0xffff_ffff {
        0 => HOP_RESTART_GRACE_MS,
        ms => ms,
    };
    if ms > HOP_RESTART_GRACE_MAX_MS {
        return Err((
            "grace too long",
            Error::TooLarge {
                len: usize::try_from(ms).unwrap_or(usize::MAX),
                max: HOP_RESTART_GRACE_MAX_MS as usize,
            }
            .into(),
        ));
    }
    let grace = Duration::from_millis(ms);
    if c.off == 0 {
        if !c.path.is_empty() {
            return Err((
                "sum without an image",
                Error::TooLarge {
                    len: c.path.len(),
                    max: 0,
                }
                .into(),
            ));
        }
        return Ok(HopAsk { image: None, grace });
    }
    let slot = target(c).map_err(|e| ("bad slot", e.into()))?;
    let sha = flip_sum(c.path).map_err(|e| ("bad sum", e.into()))?;
    Ok(HopAsk {
        image: Some((slot, sha)),
        grace,
    })
}

/// De kop van een bestandscall zonder lening: getallen en de bereiken van
/// pad en data in de callbuffer.
struct FsHead {
    op: u8,
    seq: u32,
    off: u64,
    n: u64,
    path: core::ops::Range<usize>,
    data: core::ops::Range<usize>,
}

/// Een foutantwoord: de status uit de fout (bestaat niet, geweigerd, of
/// algemeen) en de tekst in de data.
fn fail(out: &mut [u8], op: u8, seq: u32, e: &Fail) -> usize {
    let status = match e {
        Fail::Kern(Error::NoEnt) => STATUS_NO_ENT,
        Fail::Kern(Error::Privilege { .. } | Error::Denied) => STATUS_DENIED,
        _ => STATUS_ERROR,
    };
    let mut msg = [0u8; 160];
    let n = crate::fmt_into(&mut msg, e);
    refuse(out, op, seq, status, msg.get(..n).unwrap_or(&[]))
}

/// Het doelslot van een bevoegde call (`off`).
fn target(c: &Req<'_>) -> Result<Slot> {
    usize::try_from(c.off)
        .ok()
        .and_then(Slot::new)
        .ok_or(Error::SlotRange {
            slot: usize::try_from(c.off).unwrap_or(usize::MAX),
            max: abi::layout::SLOT_MAX,
        })
}

/// De poorten van een start, elk één keer: twee namen in de jobspec met
/// hetzelfde nummer (`http` en `web` op 80) zijn één publicatie, geen
/// botsing met zichzelf. De toetsen (hoeveel, geen 0) deed `StartReq::decode`.
fn start_ports(req: &StartReq<'_>) -> Result<Vec<u16>> {
    let mut out: Vec<u16> = Vec::new();
    for p in req.ports() {
        if !out.contains(&p) {
            out.try_reserve(1)
                .map_err(|_| Error::OutOfMemory { bytes: 2 })?;
            out.push(p);
        }
    }
    Ok(out)
}

/// De volumes van een start als genormaliseerde mount-tabel
/// ([`rpc::mount_table`]: `/a/b`-vorm, langste `local` eerst). De ABI leent
/// de bytes en toetste alleen de vorm; hier vallen de weigeringen die de
/// toegangsgrens zijn (`/` als volume of als pad in de app, iets onder
/// [`rpc::TASKS_DIR`], `..`, een dubbel lokaal pad), vóór er iets geclaimd
/// is. Een pad dat na het normaliseren langer is dan de flip kan
/// overdragen ([`kernflip::MAX_FLIP_PATH`]), wordt hier al geweigerd: anders
/// zou een latere flip weigeren om iets dat bij de start vaststond.
fn start_mounts(req: &StartReq<'_>) -> Result<Vec<slots::Mount>> {
    let mut mounts: Vec<slots::Mount> = Vec::new();
    for m in abi::systemapi::Mounts::new(req.mounts) {
        let m = m.map_err(|_| Error::BadPath)?;
        slots::try_push(
            &mut mounts,
            slots::Mount {
                local: try_vec(m.local)?,
                shared: try_vec(m.shared)?,
            },
        )?;
    }
    let table = rpc::mount_table(&mounts)?;
    if let Some(m) = table
        .iter()
        .find(|m| m.local.len().max(m.shared.len()) > kernflip::MAX_FLIP_PATH)
    {
        return Err(Error::TooLarge {
            len: m.local.len().max(m.shared.len()),
            max: kernflip::MAX_FLIP_PATH,
        });
    }
    Ok(table)
}

fn placement(req: &StartReq<'_>) -> Result<Placement> {
    use abi::systemapi::CoreClass as Wire;
    let group = if req.group.is_empty() {
        None
    } else {
        let mut g = GroupName::new();
        for x in req.group {
            g.push(*x).map_err(|_| Error::TooLarge {
                len: req.group.len(),
                max: crate::pool::MAX_GROUP_NAME,
            })?;
        }
        Some(g)
    };
    Ok(Placement {
        group,
        pool_cores: usize::from(req.pool_cores),
        cores: usize::from(req.cores),
        class: match req.core_class {
            Wire::Any => None,
            Wire::Small => Some(CoreClass::Small),
            Wire::Mid => Some(CoreClass::Mid),
            Wire::Big => Some(CoreClass::Big),
        },
        prefer: None,
    })
}

/// Schrijft het stroom-antwoord in `data`.
fn answer_stream(data: &mut [u8], received: u64, state: StreamState, why: Option<Fail>) -> Answer {
    let mut msg = [0u8; 160];
    let n = why.map_or(0, |e| crate::fmt_into(&mut msg, &e));
    let r = StreamResp {
        received,
        state,
        why: msg.get(..n).unwrap_or(&[]),
    };
    let len = r.encode_data(data)?;
    Ok((received, len))
}

fn done(r: Response) -> Answer {
    match r {
        Response::Done => Ok((0, 0)),
        Response::Failed(e) => Err(e.into()),
        _ => Err(Error::Busy.into()),
    }
}

/// Een toegelaten verbinding; `Drop` geeft de plaats terug.
pub struct Admitted {
    slot: Slot,
    generation: u32,
    /// De besturing van de levensduur waarvoor de plaats geteld is.
    ctl: Rc<ServicerCtl>,
}

impl Admitted {
    /// Het slot van de peer.
    #[must_use]
    pub fn slot(&self) -> Slot {
        self.slot
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.ctl.drop_conn();
    }
}

/// Waarom [`System::admit`] een verbinding weigerde; de listener zet het
/// in zijn regel (`HOPOS_SYSTEM_REFUSED`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Het bron-IP is geen slot-adres.
    NoSlot,
    /// Het slot heeft geen levende bewoner (gestopt, of nog niet gestart).
    NotLive {
        /// Het slot.
        slot: Slot,
    },
    /// Deze levensduur heeft al `cap` verbindingen open.
    Full {
        /// Het slot.
        slot: Slot,
        /// Het plafond van dat slot.
        cap: u8,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NoSlot => f.write_str("not a slot address"),
            Refusal::NotLive { slot } => write!(f, "slot {slot} has no live resident"),
            Refusal::Full { slot, cap } => {
                write!(
                    f,
                    "slot {slot} already has {cap} connections open in this lifetime"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Region;
    use crate::cage::tests::SparseMem;
    use crate::slots::tests::{Actor, FakeConsole, Obey, actor, s};
    use crate::slots::{Outbox, servicer_task};
    use crate::testutil::{FakeTimer, SlowTimer};
    use abi::layout::port_ip4;
    use abi::systemapi::{StreamState, plain_req, stream_req};
    use core::cell::Cell;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::collections::VecDeque;
    use std::vec;
    use std::vec::Vec;

    const MIB: u64 = 1 << 20;

    /// Een verbinding in RAM die per `read` hooguit `chunk` bytes geeft.
    /// Met `hold` is een lege rij geen EOF maar een peer die zwijgt en open
    /// blijft: de half-open verbinding van een geparkeerde app.
    struct Pipe {
        rx: VecDeque<u8>,
        tx: Vec<u8>,
        chunk: usize,
        ip: u32,
        hold: bool,
    }

    impl Pipe {
        fn new(ip: u32, calls: &[Vec<u8>]) -> Pipe {
            let mut rx = Vec::new();
            for c in calls {
                rx.extend(frame(Kind::Call, c));
            }
            Pipe {
                rx: rx.into(),
                tx: Vec::new(),
                chunk: 7,
                ip,
                hold: false,
            }
        }

        /// Een peer die zwijgt en nooit sluit.
        fn silent(ip: u32) -> Pipe {
            Pipe {
                hold: true,
                ..Pipe::new(ip, &[])
            }
        }
    }

    impl Conn for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> {
            let n = buf.len().min(self.chunk).min(self.rx.len());
            for b in buf.iter_mut().take(n) {
                *b = self.rx.pop_front().unwrap();
            }
            let wait = n == 0 && self.hold && !buf.is_empty();
            core::future::poll_fn(move |_| {
                if wait {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(n))
                }
            })
        }
        fn write(&mut self, buf: &[u8]) -> impl Future<Output = Result<usize>> {
            self.tx.extend_from_slice(buf);
            core::future::ready(Ok(buf.len()))
        }
        fn remote_ip4(&self) -> u32 {
            self.ip
        }
    }

    fn frame(kind: Kind, p: &[u8]) -> Vec<u8> {
        let mut v = encode_header(kind, p.len()).unwrap().to_vec();
        v.extend_from_slice(p);
        v
    }

    fn enc(r: &abi::hopabi::Req<'_>) -> Vec<u8> {
        let mut v = vec![0u8; HDR_LEN + r.path.len() + r.data.len()];
        let n = abi::hopabi::encode_req(&mut v, r).unwrap();
        v.truncate(n);
        v
    }

    #[test]
    fn start_mounts_validate_shared_roots_and_duplicate_local_paths() {
        use abi::systemapi::{MountRef, mount_blob};
        let mut bytes = [0; 256];
        let len = mount_blob(
            &[MountRef {
                local: b"/media",
                shared: b"/volumes/media",
            }],
            &mut bytes,
        )
        .unwrap();
        let req = StartReq {
            mounts: &bytes[..len],
            ..Default::default()
        };
        assert_eq!(
            start_mounts(&req).unwrap(),
            vec![slots::Mount {
                local: b"/media".to_vec(),
                shared: b"/volumes/media".to_vec()
            }]
        );
        for pair in [
            MountRef {
                local: b"/",
                shared: b"/volumes/a",
            },
            MountRef {
                local: b"/data",
                shared: b"/",
            },
            MountRef {
                local: b"/data",
                shared: b"/.tasks/slot1",
            },
        ] {
            let len = mount_blob(&[pair], &mut bytes).unwrap();
            assert!(
                start_mounts(&StartReq {
                    mounts: &bytes[..len],
                    ..Default::default()
                })
                .is_err()
            );
        }
        let len = mount_blob(
            &[
                MountRef {
                    local: b"/data",
                    shared: b"/a",
                },
                MountRef {
                    local: b"/data/",
                    shared: b"/b",
                },
            ],
            &mut bytes,
        )
        .unwrap();
        assert!(
            start_mounts(&StartReq {
                mounts: &bytes[..len],
                ..Default::default()
            })
            .is_err()
        );
    }

    fn start_call(seq: u32, image_size: u64, env: &[u8]) -> Vec<u8> {
        start_call_ports(seq, image_size, env, &[])
    }

    /// Een START_SLOT met poorten in de draadvorm (`u16` little-endian).
    fn start_call_ports(seq: u32, image_size: u64, env: &[u8], ports: &[u8]) -> Vec<u8> {
        let r = StartReq {
            memory_limit: 8 * MIB,
            image_size,
            cores: 1,
            pool_cores: 1,
            core_class: abi::systemapi::CoreClass::Any,
            group: b"",
            env,
            ports,
            job: b"demo",
            mounts: &[],
        };
        let mut v = vec![0u8; 512];
        let n = r.encode(&mut v, seq).unwrap();
        v.truncate(n);
        v
    }

    /// Een START_SLOT met volumes (`local`, `shared`) in de draadvorm.
    fn start_call_mounts(seq: u32, image_size: u64, mounts: &[(&[u8], &[u8])]) -> Vec<u8> {
        use abi::systemapi::{MountRef, mount_blob};
        let refs: Vec<MountRef<'_>> = mounts
            .iter()
            .map(|&(local, shared)| MountRef { local, shared })
            .collect();
        let mut blob = vec![0u8; 1024];
        let len = mount_blob(&refs, &mut blob).unwrap();
        let r = StartReq {
            memory_limit: 8 * MIB,
            image_size,
            cores: 1,
            pool_cores: 1,
            job: b"demo",
            mounts: &blob[..len],
            ..Default::default()
        };
        let mut v = vec![0u8; 2048];
        let n = r.encode(&mut v, seq).unwrap();
        v.truncate(n);
        v
    }

    fn op(o: PrivOp, seq: u32, slot: u64, n: u64) -> Vec<u8> {
        enc(&plain_req(o, seq, slot, n))
    }

    #[test]
    fn an_older_hop_gets_the_slot_info_prefix() {
        let full = SlotInfo {
            partition: 8 * MIB,
            ..SlotInfo::default()
        }
        .encode();
        let mut wide = [0u8; abi::systemapi::SLOT_INFO_LEN + 8];
        // Wie de maat noemt (Hop sinds v3.0.5) krijgt alles.
        let n = abi::systemapi::SLOT_INFO_LEN as u64;
        assert_eq!(
            fit_info(&full, n, &mut wide).unwrap().1,
            abi::systemapi::SLOT_INFO_LEN
        );
        // Zonder maat (Hop tot v3.0.3, een buffer van 88): het voorvoegsel,
        // en dat decodeert daar.
        assert_eq!(
            fit_info(&full, 0, &mut wide).unwrap().1,
            abi::systemapi::SLOT_INFO_LEN_V1
        );
        assert_eq!(wide[..88], full[..abi::systemapi::SLOT_INFO_LEN_V1]);
        // Korter dan het voorvoegsel is een fout, geen half antwoord.
        let mut tiny = [0u8; 32];
        assert!(fit_info(&full, n, &mut tiny).is_err());
        assert!(fit_info(&full, 32, &mut wide).is_err());
    }

    /// Een antwoord: (op, status, seq, size, data).
    type Res = (u8, u16, u32, u64, Vec<u8>);

    /// Slot 0 is de kern: Hop leest hem als een levende app op de OS-core
    /// met één core, en twee standen geven een idle die de tijd niet
    /// inhaalt. Elke andere op op slot 0 blijft geweigerd, en een
    /// onbevoegde lezer blijft onbevoegd.
    #[test]
    fn slot_zero_is_the_kern() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let mut hop = Pipe::new(
            port_ip4(1),
            &[
                op(PrivOp::SlotStatus, 1, 0, 128),
                op(PrivOp::SlotStatus, 2, 0, 128),
                op(PrivOp::StopSlot, 3, 0, 10),
                op(PrivOp::NextLog, 4, 0, 64),
            ],
        );
        let r = drive(
            &sys, &mut a, &inbox, &reply, &mut hop, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(r, End::Peer);
        let res = results(&hop.tx);
        assert_eq!(res.len(), 4);
        let one = SlotInfo::decode(&res[0].4).unwrap();
        let two = SlotInfo::decode(&res[1].4).unwrap();
        assert_eq!(one.slot_state(), Some(SlotState::Running));
        assert_eq!((one.core_on, one.core, one.span, one.cores), (1, 0, 1, 1));
        assert_eq!(one.app, abi::hopabi::AppStatus::Ready as u64);
        assert_eq!(
            (one.ram_size, one.mem_sys, one.partition),
            (64 * MIB, 3 * MIB, 0)
        );
        assert!(one.at_ns > 0);
        let dt = two.at_ns - one.at_ns;
        assert!(dt > 0 && two.idle_ns - one.idle_ns <= dt);
        assert!(two.heartbeat > one.heartbeat && two.wakes > one.wakes);
        assert_ne!(res[2].1, STATUS_OK, "STOP_SLOT 0 stopped the kern");
        // NEXT_LOG 0 is de console van de kern; zonder ring is hij leeg.
        assert_eq!((res[3].1, res[3].3), (STATUS_OK, 0), "NEXT_LOG 0");
        // Een gewone app vraagt slot 0: onbevoegd, zoals elke bevoegde op.
        let mut app = Pipe::new(port_ip4(2), &[op(PrivOp::SlotStatus, 1, 0, 128)]);
        let r = drive(
            &sys, &mut a, &inbox, &reply, &mut app, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(r, End::Peer);
        assert_eq!(results(&app.tx)[0].1, STATUS_DENIED);
    }

    fn results(mut b: &[u8]) -> Vec<Res> {
        let mut out = Vec::new();
        const FH: usize = abi::systemapi::HEADER_LEN;
        while b.len() >= FH {
            let h = decode_header(b[..FH].try_into().unwrap()).unwrap();
            assert_eq!(h.kind, Kind::Result);
            let r = abi::hopabi::decode_resp(&b[FH..FH + h.len]).unwrap();
            out.push((r.op, r.status, r.seq, r.size, r.data.to_vec()));
            b = &b[FH + h.len..];
        }
        out
    }

    fn stream_state(r: &Res) -> StreamState {
        let resp = abi::hopabi::Resp {
            size: r.3,
            data: &r.4,
            ..Default::default()
        };
        StreamResp::decode(&resp).unwrap().state
    }

    /// De haken, met de klok van de kern in `.3`: elke [`Hooks::kern`] is
    /// een seconde later, waarvan de executor driekwart sliep. `.4` is de
    /// laatste HOP_RESTART die de haak bereikte (het image of `None`, en de
    /// termijn), `.5` de reden van de laatste weigering vóór de haak.
    #[derive(Default)]
    struct NoHooks(
        Cell<u64>,
        Cell<Option<(usize, [u8; 32])>>,
        Cell<bool>,
        Cell<u64>,
        Cell<Option<(Option<HopImage>, Duration)>>,
        Cell<Option<&'static str>>,
    );
    impl Hooks for NoHooks {
        fn set_clock(&self, unix_ns: u64) {
            self.0.set(unix_ns);
        }
        fn flip(&self, bundle: &FlipBundle, sha256: &[u8; 32]) -> Result {
            self.1.set(Some((bundle.slot.get(), *sha256)));
            self.2.set(true);
            Ok(())
        }
        fn hop_restart(
            &self,
            image: Option<&HopImage>,
            grace: Duration,
        ) -> core::result::Result<(), Fail> {
            self.4.set(Some((image.copied(), grace)));
            Ok(())
        }
        fn hop_refused(&self, why: &'static str, _: &Fail) {
            self.5.set(Some(why));
        }
        fn kern(&self) -> crate::cage::Status {
            let at_ns = self.3.get() + 1_000_000_000;
            self.3.set(at_ns);
            crate::cage::Status {
                heartbeat: at_ns / 1_000_000_000,
                ram_size: 64 * MIB,
                mem_sys: 3 * MIB,
                idle_ns: at_ns / 4 * 3,
                wakes: at_ns / 1_000_000,
                at_ns,
                ..Default::default()
            }
        }
    }

    /// Een outbox die één regel meldt en daarna leeft tot de stop.
    struct OneLine(Option<&'static [u8]>);
    impl Outbox for OneLine {
        fn read_into(&mut self, buf: &mut [u8]) -> Option<(u8, usize)> {
            let l = self.0.take()?;
            buf[..l.len()].copy_from_slice(l);
            Some((crate::slots::RING_LOG, l.len()))
        }
        fn corrupt(&self) -> bool {
            false
        }
        fn live(&self) -> bool {
            true
        }
        fn smp_pending(&self) -> bool {
            false
        }
    }

    // Het test-image: één PT_LOAD op LINK_BASE + 0x10000, de symbolen in
    // het segment, de symbool- en stringtabel en de sectieheaders erachter.
    const SEG_OFF: usize = 0x1000;
    const SEG_FILESZ: usize = 0x100;
    const SEG_MEMSZ: u64 = 0x200;
    const SEG_IPA: u64 = LINK_BASE + 0x10000;

    fn put(b: &mut [u8], at: usize, v: u64, n: usize) {
        b[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
    }

    fn elf(stamp: u64) -> Vec<u8> {
        elf_with(stamp, &[])
    }

    /// Als [`elf`], met `extra` (naam, `st_shndx`, `st_value`) vóór de
    /// gezochte symbolen in `.symtab`.
    fn elf_with(stamp: u64, extra: &[(&str, u64, u64)]) -> Vec<u8> {
        let mut img = vec![0u8; SEG_OFF + SEG_FILESZ];
        img[..4].copy_from_slice(b"\x7fELF");
        img[4..7].copy_from_slice(&[2, 1, 1]);
        put(&mut img, 16, 2, 2);
        put(&mut img, 18, u64::from(leanelf::MACHINE_AARCH64), 2);
        put(&mut img, 20, 1, 4);
        put(&mut img, 24, SEG_IPA, 8);
        put(&mut img, 32, 64, 8);
        put(&mut img, 52, 64, 2);
        put(&mut img, 54, 56, 2);
        put(&mut img, 56, 1, 2);
        put(&mut img, 58, 64, 2);
        put(&mut img, 60, 3, 2);
        let ph = 64;
        put(&mut img, ph, 1, 4);
        put(&mut img, ph + 4, 7, 4);
        put(&mut img, ph + 8, SEG_OFF as u64, 8);
        put(&mut img, ph + 16, SEG_IPA, 8);
        put(&mut img, ph + 24, SEG_IPA, 8);
        put(&mut img, ph + 32, SEG_FILESZ as u64, 8);
        put(&mut img, ph + 40, SEG_MEMSZ, 8);
        put(&mut img, ph + 48, 0x1000, 8);
        for (i, b) in img[SEG_OFF..].iter_mut().enumerate() {
            *b = 0xA0 | (i as u8 & 0xF);
        }
        img[SEG_OFF + 0x80..SEG_OFF + 0x90].fill(0);
        put(&mut img, SEG_OFF + 0x90, stamp, 8);
        // De stringtabel en de symbolen.
        let names = [place::SYM_RAM_START, place::SYM_RAM_SIZE, place::SYM_ABI];
        let wanted = names
            .iter()
            .enumerate()
            .map(|(i, &n)| (n, 1, SEG_IPA + 0x80 + 8 * i as u64));
        let mut strtab = vec![0u8];
        let symtab_off = img.len();
        img.extend_from_slice(&[0u8; 24]);
        for (n, shndx, value) in extra.iter().copied().chain(wanted) {
            let mut e = [0u8; 24];
            put(&mut e, 0, strtab.len() as u64, 4);
            e[4] = 0x11;
            put(&mut e, 6, shndx, 2);
            put(&mut e, 8, value, 8);
            put(&mut e, 16, 8, 8);
            img.extend_from_slice(&e);
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
        }
        let symtab_len = img.len() - symtab_off;
        let strtab_off = img.len();
        img.extend_from_slice(&strtab);
        while !img.len().is_multiple_of(8) {
            img.push(0);
        }
        let shoff = img.len();
        img.extend_from_slice(&[0u8; 64]);
        for (kind, off, len, link, ent) in [
            (2u64, symtab_off, symtab_len, 2u64, 24u64),
            (3, strtab_off, strtab.len(), 0, 0),
        ] {
            let mut e = [0u8; 64];
            put(&mut e, 4, kind, 4);
            put(&mut e, 24, off as u64, 8);
            put(&mut e, 32, len as u64, 8);
            put(&mut e, 40, link, 4);
            put(&mut e, 56, ent, 8);
            img.extend_from_slice(&e);
        }
        put(&mut img, 40, shoff as u64, 8);
        img
    }

    /// Een servicer-taak die over meerdere verbindingen heen leeft.
    type Servicer<'a> = core::pin::Pin<std::boxed::Box<dyn Future<Output = ()> + 'a>>;

    /// Dient `p` tot EOF, met de actor en (als die er is) een servicer
    /// ernaast. Geeft de uitkomst van `serve`.
    #[expect(clippy::too_many_arguments, reason = "de hele kern van een test")]
    fn drive<'i, 'r, G: crate::grants::Grants, const N: usize>(
        sys: &System<'i, 'r, N>,
        a: &mut Actor<'_, G>,
        inbox: &'i Mailbox<Envelope<'r>, N>,
        reply: &'r Reply,
        p: &mut Pipe,
        mem: &mut SparseMem,
        hooks: &NoHooks,
        tee: &&FakeConsole,
        mut svc: Option<&mut Servicer<'_>>,
    ) -> End {
        let who = sys.admit(p.ip).unwrap();
        let timer = FakeTimer::default();
        let (mut buf, mut out) = (vec![0u8; 64 << 10], vec![0u8; 8192]);
        let mut serve =
            pin!(sys.serve(p, &who, reply, &timer, mem, hooks, tee, &mut buf, &mut out));
        let mut run = pin!(a.run(inbox));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..100_000 {
            let _ = run.as_mut().poll(&mut cx);
            if let Some(f) = svc.as_mut() {
                let _ = f.as_mut().poll(&mut cx);
            }
            if let Poll::Ready(r) = serve.as_mut().poll(&mut cx) {
                return r;
            }
        }
        panic!("serve did not finish");
    }

    /// Een node met Hop in slot 1 en een gewone app in slot 2, beide levend.
    fn node<'s>(svc: &'s Servicers, con: &'s FakeConsole) -> Actor<'s> {
        let mut a = actor(svc, con, Obey::Exit, 64, 4);
        crate::slots::tests::start(&mut a, 1, 8, 1).unwrap();
        crate::slots::tests::start(&mut a, 2, 8, 1).unwrap();
        a
    }

    #[test]
    fn slot_from_remote_maps_the_internal_net() {
        let ip = |d: u32| (abi::layout::HOST_IP4 & !0xffff) | d;
        assert_eq!(slot_from_remote(ip(2)), Some(s(1)));
        assert_eq!(slot_from_remote(ip(9)), Some(s(8)));
        // Geen maximum aantal slots: elk adres van het /16 is een slot.
        assert_eq!(slot_from_remote(ip(0x9c41)), Some(s(40_000)));
        assert_eq!(slot_from_remote(ip(1)), None, "HOP itself");
        assert_eq!(slot_from_remote(ip(0)), None);
        assert_eq!(slot_from_remote(ip(0xffff)), None, "broadcast");
        assert_eq!(
            slot_from_remote((10 << 24) | (101 << 16) | 2),
            None,
            "other net"
        );
    }

    #[test]
    fn privilege_is_minted_once() {
        // Het token van deze test-binary: hooguit één keer.
        let a = Privilege::boot(s(1));
        let b = Privilege::boot(s(2));
        assert!(a.is_none() || b.is_none());
    }

    #[test]
    fn frames_survive_fragmentation_and_oversize_is_refused() {
        let big = vec![0xa5u8; MAX_IO_CHUNK];
        let mut p = Pipe {
            rx: frame(Kind::Call, &big).into(),
            tx: Vec::new(),
            chunk: 1,
            ip: 0,
            hold: false,
        };
        let h = crate::testutil::block_on(read_header(&mut p)).unwrap();
        assert_eq!((h.kind, h.len), (Kind::Call, MAX_IO_CHUNK));
        let mut w = Pipe::new(0, &[]);
        let too_big = vec![0u8; MAX_PAYLOAD + 1];
        assert!(crate::testutil::block_on(write_frame(&mut w, Kind::Call, &too_big)).is_err());
        assert!(w.tx.is_empty(), "oversized frame half written");
    }

    /// Fysiek geheugen dat de cache-veeg onthoudt.
    #[derive(Default)]
    struct Swept(SparseMem, Vec<(u64, u64)>);

    impl PhysMem for Swept {
        fn read64(&self, pa: u64) -> u64 {
            self.0.read64(pa)
        }
        fn write64(&mut self, pa: u64, v: u64) {
            self.0.write64(pa, v);
        }
        fn clear(&mut self, pa: u64, len: u64) {
            self.0.clear(pa, len);
        }
        fn clean_inv(&mut self, pa: u64, len: u64) {
            self.1.push((pa, len));
        }
    }

    /// De boot plaatst langs dezelfde weg als een start van Hop ([`place`]):
    /// het image in één brok, gepatcht, BSS en de schrapruimte met de
    /// symbooltabel gewist, segment en control-page geveegd, en de env met
    /// de regel van de grant-aanbieder op de page; de aanbieder wordt pas
    /// na de bouw gearmd. Weigert de ELF-lezer of de plaatsing, dan gaat
    /// het slot terug.
    #[test]
    fn the_boot_places_like_a_start() {
        /// Een aanbieder die bij `GUI=display` één regel geeft en telt.
        #[derive(Default)]
        struct Glass {
            armed: Vec<usize>,
        }
        impl crate::grants::Grants for Glass {
            fn env(&mut self, _: Slot, env: &[u8], out: &mut Vec<u8>) {
                if crate::grants::env_get(env, "GUI") == Some(b"display") {
                    out.extend_from_slice(b"FB_BASE=0x20000000\n");
                }
            }
            fn arm(&mut self, slot: Slot) -> Result {
                self.armed.push(slot.get());
                Ok(())
            }
            fn adopt(&mut self, _: Slot) -> Result {
                Ok(())
            }
            fn release(&mut self, _: Slot) {}
        }
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a =
            crate::slots::tests::actor_with(&svc, &con, Obey::Exit, 64, 4, Glass::default());
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let mut mem = Swept::default();
        let mut boot = |a: &mut Actor<'_, Glass>, slot: usize, img: &[u8]| {
            let spec = StartSpec::new(s(slot), 8 * MIB, crate::slots::tests::ded(1));
            let mut placed = pin!(place(&inbox, &reply, &mut mem, spec, img, b"GUI=display"));
            let mut run = pin!(a.run(&inbox));
            let mut cx = Context::from_waker(Waker::noop());
            for _ in 0..1000 {
                let _ = run.as_mut().poll(&mut cx);
                if let Poll::Ready(r) = placed.as_mut().poll(&mut cx) {
                    return r;
                }
            }
            panic!("place did not finish");
        };
        let img = elf(u64::from(abi::ABI_VERSION));
        assert_eq!(boot(&mut a, 1, &img), Ok(SEG_IPA));
        let bad = boot(&mut a, 2, b"not an elf image");
        assert!(matches!(bad, Err(Fail::Elf(_))), "{bad:?}");
        let bad = boot(&mut a, 2, &elf(9));
        assert!(matches!(bad, Err(Fail::Place(_))), "{bad:?}");
        assert_eq!(a.status(s(2)).occupancy, Occupancy::Empty);
        assert_eq!(a.grants().armed, [1]);

        let part = a.status(s(1)).partition.unwrap();
        assert_eq!(a.status(s(1)).occupancy, Occupancy::Running);
        let app_ram = part.size - ABI_TAIL;
        let at = |ipa: u64| mem.0.read64(part.base + (ipa - LINK_BASE));
        assert_eq!(at(SEG_IPA + 0x80), LINK_BASE, "RamStart patched");
        assert_eq!(at(SEG_IPA + 0x88), app_ram, "RamSize patched");
        assert_eq!(at(SEG_IPA + 0x90), u64::from(abi::ABI_VERSION));
        let ctrl = part.base + app_ram + ABI_CTRL_OFF;
        let want = b"GUI=display\nFB_BASE=0x20000000\n";
        assert_eq!(
            mem.0.read64(ctrl + abi::hopabi::CTRL_ENV_LEN),
            want.len() as u64
        );
        let mut got = [0u8; 31];
        read_bytes(&mem.0, ctrl + abi::hopabi::CTRL_ENV_DATA, &mut got);
        assert_eq!(&got, want);
        let seg = part.base + (SEG_IPA - LINK_BASE);
        assert!(
            mem.0.0.keys().all(|&k| k < part.base
                || k >= part.base + part.size
                || (seg..seg + SEG_FILESZ as u64).contains(&k)
                || (ctrl..ctrl + 0x1000).contains(&k)),
            "residue in the partition"
        );
        assert!(mem.1.contains(&(seg, SEG_MEMSZ)), "segment swept");
        assert!(
            mem.1.contains(&(ctrl, abi::layout::CTRL_STRIDE)),
            "control page swept"
        );
    }

    /// De hele levensloop: start (de kern kiest slot 3), het image in drie
    /// brokken, geplaatst en gestart, status, een logregel, de klok, stop;
    /// en slot 2 krijgt voor elke bevoegde op "denied".
    #[test]
    fn hop_drives_the_lifecycle_end_to_end() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let timer = FakeTimer::default();
        let mut sbuf = [0u8; 256];
        // De servicer van slot 3 bestaat pas vanaf de plaatsing (de kooi
        // spawnt hem per levensduur); deze wacht tot zijn besturing er is.
        let (svcr, sbufr) = (&svc, &mut sbuf);
        let mut servicer: Servicer<'_> = std::boxed::Box::pin(async {
            let ctl = loop {
                if let Some(c) = svcr.ctl(s(3)) {
                    break c;
                }
                sync::yield_now().await;
            };
            servicer_task(
                s(3),
                ctl,
                |_: Region| OneLine(Some(b"app says hi")),
                &timer,
                &tee,
                &inbox,
                sbufr,
            )
            .await;
            // `drive` pollt hem daarna gewoon door.
            core::future::pending::<()>().await;
        });
        let img = elf(u64::from(abi::ABI_VERSION));
        let (c1, c2) = (0x50, SEG_OFF + 0x80);
        let env = b"BUCKET=hop-apps\n";
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                // Twee namen op 80 (http en web) en 8443: twee publicaties.
                start_call_ports(1, img.len() as u64, env, &[80, 0, 80, 0, 0xfb, 0x20]),
                enc(&stream_req(2, 3, 0, &img[..c1])),
                enc(&stream_req(3, 3, c1 as u64, &img[c1..c2])),
                enc(&stream_req(4, 3, c2 as u64, &img[c2..])),
                op(PrivOp::SlotStatus, 5, 3, 128),
                op(PrivOp::NextLog, 6, 3, 64),
                op(PrivOp::SetClock, 7, 0, 1_759_000_000),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let r = drive(
            &sys,
            &mut a,
            &inbox,
            &reply,
            &mut p,
            &mut mem,
            &hooks,
            &tee,
            Some(&mut servicer),
        );
        assert_eq!(r, End::Peer, "stream should end at EOF");
        let res = results(&p.tx);
        assert_eq!(res.len(), 7);
        assert_eq!(
            (res[0].1, res[0].3),
            (STATUS_OK, 3),
            "the kern picks slot 3"
        );
        assert_eq!(stream_state(&res[1]), StreamState::More);
        assert_eq!(res[1].3, c1 as u64);
        assert_eq!(stream_state(&res[2]), StreamState::More);
        assert_eq!(stream_state(&res[3]), StreamState::Placed, "{:?}", res[3]);
        assert_eq!(res[3].3, img.len() as u64);
        let info = SlotInfo::decode(&res[4].4).unwrap();
        assert_eq!(info.slot_state(), Some(SlotState::Running));
        assert_eq!(info.partition, 8 * MIB);
        // De regel van de app gaat naar de console (daar leest Hop hem met
        // NEXT_LOG 0); een ring per slot is er niet meer.
        assert_ne!(res[5].1, STATUS_OK, "NEXT_LOG on slot 3");
        assert_eq!(con.app.borrow().as_slice(), &[(3, b"app says hi".to_vec())]);
        assert_eq!(res[6].1, STATUS_OK, "SET_CLOCK");
        assert_eq!(hooks.0.get(), 1_759_000_000);

        // De poorten gingen bij de plaatsing open, elk één keer.
        assert_eq!(a.cage().published, [(3, vec![80, 8443])]);
        assert!(con.saw("slot 3: 2 port(s) published tcp+udp on the uplink: :80 :8443"));

        // De brokken kwamen op hun plek, met de env van de start op de
        // page; de afronding (patches, BSS, schrapruimte, veeg) is die van
        // de boot (`the_boot_places_like_a_start`).
        let part = a.status(s(3)).partition.unwrap();
        assert_eq!(a.status(s(3)).occupancy, Occupancy::Running);
        let at = |ipa: u64| mem.read64(part.base + (ipa - LINK_BASE));
        assert_eq!(
            at(SEG_IPA),
            u64::from_le_bytes(img[SEG_OFF..SEG_OFF + 8].try_into().unwrap())
        );
        let ctrl = part.base + part.size - ABI_TAIL + ABI_CTRL_OFF;
        let mut got = [0u8; 16];
        read_bytes(&mem, ctrl + abi::hopabi::CTRL_ENV_DATA, &mut got);
        assert_eq!(&got, env);

        // Slot 2 is niet bevoegd: stop, start en status worden geweigerd.
        let mut other = Pipe::new(
            port_ip4(2),
            &[
                op(PrivOp::StopSlot, 9, 3, 10),
                start_call(10, 16, b""),
                op(PrivOp::SlotStatus, 11, 3, 128),
            ],
        );
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut other, &mut mem, &hooks, &tee, None,
        );
        assert!(results(&other.tx).iter().all(|r| r.1 == STATUS_DENIED));
        assert_eq!(a.status(s(3)).occupancy, Occupancy::Running);

        // Hop stopt slot 3: de kern bevestigt en geeft vrij.
        let mut stop = Pipe::new(
            port_ip4(1),
            &[
                op(PrivOp::StopSlot, 12, 3, 50),
                op(PrivOp::SlotStatus, 13, 3, 128),
            ],
        );
        let _ = drive(
            &sys,
            &mut a,
            &inbox,
            &reply,
            &mut stop,
            &mut mem,
            &hooks,
            &tee,
            Some(&mut servicer),
        );
        let res = results(&stop.tx);
        assert_eq!(res[0].1, STATUS_OK, "{:?}", core::str::from_utf8(&res[0].4));
        let info = SlotInfo::decode(&res[1].4).unwrap();
        assert_eq!(info.slot_state(), Some(SlotState::Empty));
    }

    /// SLOTS: de bezette slots in stukken, zoals `getdents`, van Hop; een
    /// ander slot krijgt "denied".
    #[test]
    fn slots_lists_the_occupied_slots_in_pieces() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                op(PrivOp::Slots, 1, 0, 64),
                op(PrivOp::Slots, 2, 0, 2),
                op(PrivOp::Slots, 3, 1, 64),
                op(PrivOp::Slots, 4, 2, 64),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        assert_eq!(
            (res[0].1, res[0].3, &res[0].4[..]),
            (STATUS_OK, 2, &[1, 0, 2, 0][..])
        );
        assert_eq!(
            (res[1].3, &res[1].4[..]),
            (1, &[1, 0][..]),
            "as many as fit in n"
        );
        assert_eq!((res[2].3, &res[2].4[..]), (1, &[2, 0][..]), "after slot 1");
        assert_eq!(res[3].3, 0, "none after the last");
        let mut other = Pipe::new(port_ip4(2), &[op(PrivOp::Slots, 5, 0, 64)]);
        drive(
            &sys, &mut a, &inbox, &reply, &mut other, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(results(&other.tx)[0].1, STATUS_DENIED);
    }

    /// Een start met volumes: van de draad tot in de mount-tabel van de
    /// hopfs-actor, met één regel. Een volume op `/`, onder `/.tasks` of met
    /// `..` wordt geweigerd vóór de claim: het slot blijft leeg.
    #[test]
    fn a_start_with_volumes_reaches_the_mount_table() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let img = elf(u64::from(abi::ABI_VERSION));
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                start_call_mounts(1, 16, &[(b"/data", b"/.tasks/slot2")]),
                start_call_mounts(2, 16, &[(b"/", b"/volumes/a")]),
                start_call_mounts(3, 16, &[(b"/data", b"/")]),
                start_call_mounts(4, 16, &[(b"/data", b"/volumes/../.tasks")]),
                start_call_mounts(
                    5,
                    img.len() as u64,
                    &[(b"data/", b"/volumes/demo"), (b"/media", b"/volumes/media")],
                ),
                enc(&stream_req(6, 3, 0, &img)),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        for r in &res[..4] {
            assert_ne!(
                r.1,
                STATUS_OK,
                "{:?}",
                std::string::String::from_utf8_lossy(&r.4)
            );
        }
        assert_eq!(res[0].1, STATUS_DENIED, "a volume under /.tasks");
        assert_eq!(
            (res[4].1, res[4].3),
            (STATUS_OK, 3),
            "the kern picks slot 3"
        );
        assert_eq!(stream_state(&res[5]), StreamState::Placed, "{:?}", res[5]);
        let m = |l: &[u8], sh: &[u8]| slots::Mount {
            local: l.to_vec(),
            shared: sh.to_vec(),
        };
        // Genormaliseerd, langste `local` eerst.
        assert_eq!(
            svc.with_mounts(s(3), <[slots::Mount]>::to_vec),
            Some(vec![
                m(b"/media", b"/volumes/media"),
                m(b"/data", b"/volumes/demo")
            ])
        );
        assert!(con.saw(
            "slot 3: 2 volume(s) mounted: /media -> /volumes/media, /data -> /volumes/demo HOPOS_SLOT_MOUNTS"
        ));
    }

    /// Een symbooltabel met 12 000 symbolen vóór de gezochte, zoals een
    /// riscv64-welcome met zijn 42 000 lokale (03-10, LicheeRV: "out of
    /// memory (1012680 bytes)"): de kern vindt ze in brokken, slaat een
    /// ongedefinieerde en een te lange naam over, en patcht dezelfde plekken.
    #[test]
    fn a_large_symtab_is_searched_in_chunks() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let fill: Vec<std::string::String> = (0..12_000)
            .map(|i| std::format!("core::fmt::rt::local{i}"))
            .collect();
        let long = std::format!("{}{}", place::SYM_RAM_START, "x".repeat(200));
        let wrong = SEG_IPA + 0x10;
        let mut extra: Vec<(&str, u64, u64)> = vec![
            (place::SYM_RAM_START, 0, wrong),
            (&long, 1, wrong),
            (&place::SYM_RAM_SIZE[1..], 1, wrong),
        ];
        extra.extend(fill.iter().map(|n| (n.as_str(), 1, wrong)));
        let img = elf_with(u64::from(abi::ABI_VERSION), &extra);
        assert!(img.len() > 12_000 * 24, "the symtab is large");
        let mut calls = vec![start_call(1, img.len() as u64, b"")];
        for (i, c) in img.chunks(32 << 10).enumerate() {
            calls.push(enc(&stream_req(2 + i as u32, 3, (i << 15) as u64, c)));
        }
        let mut p = Pipe::new(port_ip4(1), &calls);
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let r = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(r, End::Peer);
        let res = results(&p.tx);
        let last = res.last().unwrap();
        assert_eq!(stream_state(last), StreamState::Placed, "{last:?}");
        let part = a.status(s(3)).partition.unwrap();
        let at = |ipa: u64| mem.read64(part.base + (ipa - LINK_BASE));
        assert_eq!(at(SEG_IPA + 0x80), LINK_BASE, "RamStart patched");
        assert_eq!(at(SEG_IPA + 0x88), part.size - ABI_TAIL, "RamSize patched");
        let o = SEG_OFF + 0x10;
        assert_eq!(
            at(wrong),
            u64::from_le_bytes(img[o..o + 8].try_into().unwrap()),
            "decoys left alone"
        );
    }

    /// Een kapot image, een verkeerde ABI-stempel, een verkeerde offset en
    /// een stop halverwege: telkens ruimt de kern het slot zelf op.
    #[test]
    fn failed_streams_leave_nothing_behind() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let bad_abi = elf(9);
        let good = elf(u64::from(abi::ABI_VERSION));
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                // Geen ELF: de eerste brok wordt al geweigerd.
                start_call(1, 16, b""),
                enc(&stream_req(2, 3, 0, b"not an elf image")),
                // ABI 9: alle bytes binnen, de plaatsing weigert.
                start_call(3, bad_abi.len() as u64, b""),
                enc(&stream_req(4, 3, 0, &bad_abi)),
                // Een brok op de verkeerde offset.
                start_call(5, good.len() as u64, b""),
                enc(&stream_req(6, 3, 8, &good[..8])),
                // Stop halverwege.
                start_call(7, good.len() as u64, b""),
                enc(&stream_req(8, 3, 0, &good[..100])),
                op(PrivOp::StopSlot, 9, 3, 10),
                enc(&stream_req(10, 3, 100, &good[100..])),
                // Te veel bytes.
                start_call(11, 4, b""),
                enc(&stream_req(12, 3, 0, b"\x7fELF!")),
                op(PrivOp::SlotStatus, 13, 3, 128),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        let text = |r: &Res| std::string::String::from_utf8_lossy(&r.4).into_owned();
        assert_eq!(res[0].3, 3);
        assert_eq!(res[1].1, STATUS_ERROR);
        assert!(text(&res[1]).contains("not an ELF"), "{}", text(&res[1]));
        assert_eq!(res[2].3, 3, "slot 3 was given back");
        assert_eq!(res[3].1, STATUS_OK);
        assert_eq!(stream_state(&res[3]), StreamState::Failed);
        assert!(text(&res[3]).contains("ABI"), "{}", text(&res[3]));
        assert_eq!(res[5].1, STATUS_ERROR, "offset mismatch");
        assert_eq!(
            (res[6].1, res[7].1, res[8].1),
            (STATUS_OK, STATUS_OK, STATUS_OK)
        );
        assert_eq!(res[9].1, STATUS_ERROR, "stream after stop");
        assert_eq!(res[11].1, STATUS_ERROR, "more bytes than announced");
        let info = SlotInfo::decode(&res[12].4).unwrap();
        assert_eq!(info.slot_state(), Some(SlotState::Empty));
        assert_eq!(a.status(s(3)).occupancy, Occupancy::Empty);
    }

    #[test]
    fn file_calls_travel_to_the_hopfs_actor_and_back() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let fsin: FsInbox<'_> = Mailbox::new();
        let sys = System::new(&inbox, &svc, None).with_fs(&fsin);
        let req = |op: u8, seq: u32, path: &'static [u8], data: &'static [u8], n: u64| {
            enc(&abi::hopabi::Req {
                op,
                seq,
                n,
                path,
                data,
                ..Default::default()
            })
        };
        use abi::hopabi::{OP_LIST, OP_READ, OP_STAT, OP_WRITE};
        let calls = [
            req(OP_WRITE, 1, b"a/b.txt", b"hoi", 0),
            req(OP_READ, 2, b"/a/b.txt", b"", MAX_IO_CHUNK as u64 * 4),
            req(OP_STAT, 3, b"nope", b"", 0),
            req(OP_STAT, 4, b"../slot1/x", b"", 0),
            req(OP_LIST, 5, b"/", b"", 0),
        ];
        let mut p = Pipe::new(port_ip4(2), &calls);
        let (fs, _) = crate::rpc::tests::disk(16);
        let mut fsa = crate::rpc::FsActor::new(fs, &svc, &con);
        let mut run: Servicer<'_> = std::boxed::Box::pin(async { fsa.run(&fsin).await });
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let end = drive(
            &sys,
            &mut a,
            &inbox,
            &reply,
            &mut p,
            &mut mem,
            &hooks,
            &tee,
            Some(&mut run),
        );
        assert_eq!(end, End::Peer);
        let res = results(&p.tx);
        assert_eq!((res[0].1, res[0].3), (STATUS_OK, 3));
        assert_eq!(
            (res[1].1, res[1].3, &res[1].4[..]),
            (STATUS_OK, 3, &b"hoi"[..])
        );
        assert_eq!(res[2].1, STATUS_NO_ENT);
        assert_eq!(res[3].1, STATUS_DENIED);
        assert_eq!((res[4].1, &res[4].4[..]), (STATUS_OK, &b"a/"[..]));

        // Zonder schijf: niet gevonden, net als op een lege schijf, en de
        // verbinding leeft door.
        let bare = System::new(&inbox, &svc, None);
        let mut p = Pipe::new(port_ip4(2), &[req(OP_STAT, 1, b"x", b"", 0)]);
        let _ = drive(
            &bare, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        assert_eq!(
            (res[0].1, &res[0].4[..]),
            (STATUS_NO_ENT, &b"no storage on this node"[..])
        );
    }

    #[test]
    fn flip_without_a_bundle_never_reaches_the_hook() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let sha = [7u8; 32];
        let flip = abi::hopabi::Req {
            op: PrivOp::Flip.op(),
            seq: 1,
            off: 5,
            path: &sha,
            ..Default::default()
        };
        let short = abi::hopabi::Req {
            path: &sha[..31],
            ..flip
        };
        let mut p = Pipe::new(port_ip4(1), &[enc(&flip), enc(&short)]);
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        // Geen gestroomde bundel in slot 5: geweigerd; een som van 31 bytes
        // ook. De haak ziet geen van beide.
        assert_eq!((res[0].1, res[1].1), (STATUS_ERROR, STATUS_ERROR));
        assert_eq!(hooks.1.get(), None);
    }

    #[test]
    fn flip_bundle_streams_raw_and_reaches_the_hook_once() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        // Geen ELF: een flip-bundel gaat rauw de partitie in.
        let bundle = b"not an ELF but a flip bundle".repeat(3);
        let start = {
            let r = StartReq {
                memory_limit: 8 * MIB,
                image_size: bundle.len() as u64,
                cores: 1,
                pool_cores: 1,
                core_class: abi::systemapi::CoreClass::Any,
                group: b"",
                env: b"",
                ports: &[],
                job: FLIP_BUNDLE_JOB,
                mounts: &[],
            };
            let mut v = vec![0u8; 512];
            let n = r.encode(&mut v, 1).unwrap();
            v.truncate(n);
            v
        };
        let sha = [9u8; 32];
        // Als hex, zoals Hop hem stuurt; de rauwe vorm toetst de test
        // hierboven.
        let hex = b"09".repeat(32);
        let flip = enc(&abi::hopabi::Req {
            op: PrivOp::Flip.op(),
            seq: 5,
            off: 3,
            n: 0,
            path: &hex,
            ..Default::default()
        });
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                start,
                enc(&stream_req(2, 3, 0, &bundle[..40])),
                enc(&stream_req(3, 3, 40, &bundle[40..])),
                op(PrivOp::SlotStatus, 4, 3, 128),
                flip.clone(),
                flip,
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        assert_eq!(res[0].3, 3);
        assert_eq!(stream_state(&res[1]), StreamState::More);
        assert_eq!(stream_state(&res[2]), StreamState::Placed, "complete");
        let info = SlotInfo::decode(&res[3].4).unwrap();
        assert_eq!(info.state, SlotState::Streaming as u8, "not started");
        assert_eq!(
            (res[4].1, res[5].1),
            (STATUS_OK, STATUS_ERROR),
            "claimed once"
        );
        assert_eq!(hooks.1.get(), Some((3, sha)));
        assert!(hooks.2.get(), "the flip did not reach the hook");
        // De bytes staan rauw vanaf offset 0 van de partitie.
        let part = info.partition;
        assert!(part >= bundle.len() as u64);
    }

    /// Een bevoegde call met `data` (STATE_PUT) of `path` (HOP_RESTART).
    fn req(o: PrivOp, seq: u32, off: u64, n: u64, path: &[u8], data: &[u8]) -> Vec<u8> {
        enc(&abi::hopabi::Req {
            op: o.op(),
            seq,
            off,
            n,
            path,
            data,
        })
    }

    #[test]
    fn hop_state_round_trips_by_offset_and_is_wiped_by_zero_bytes() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                op(PrivOp::StateGet, 1, 0, 0),
                req(PrivOp::StatePut, 2, 0, 0, b"", b"agent state v1"),
                req(PrivOp::StatePut, 3, 0, 0, b"", b"agent state v2"),
                op(PrivOp::StateGet, 4, 0, 0),
                op(PrivOp::StateGet, 5, 0, 6),
                op(PrivOp::StateGet, 6, 0, 14),
                op(PrivOp::StateGet, 7, 0, 15),
                req(PrivOp::StatePut, 8, 0, 0, b"", b""),
                op(PrivOp::StateGet, 9, 0, 0),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        assert_eq!(res.len(), 9);
        assert_eq!(
            (res[0].1, res[0].3, res[0].4.len()),
            (STATUS_OK, 0, 0),
            "nothing yet"
        );
        assert_eq!((res[1].1, res[2].1), (STATUS_OK, STATUS_OK));
        // Vervangend: het tweede blob, heel.
        assert_eq!((res[3].3, &res[3].4[..]), (14, &b"agent state v2"[..]));
        // Vanaf een offset: de maat blijft die van het hele blob.
        assert_eq!((res[4].3, &res[4].4[..]), (14, &b"state v2"[..]));
        assert_eq!(
            (res[5].1, res[5].3, res[5].4.len()),
            (STATUS_OK, 14, 0),
            "at the end"
        );
        assert_eq!(res[6].1, STATUS_ERROR, "beyond the end");
        // Nul bytes wist.
        assert_eq!((res[7].1, res[8].3), (STATUS_OK, 0));
        assert_eq!(sys.hop_state_len(), 0);
    }

    #[test]
    fn a_too_large_state_is_refused_and_the_old_one_stays() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let _a = node(&svc, &con);
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        sys.state_put(b"kept").unwrap();
        let big = vec![7u8; HOP_STATE_MAX + 1];
        assert_eq!(
            sys.state_put(&big),
            Err(Error::TooLarge {
                len: HOP_STATE_MAX + 1,
                max: HOP_STATE_MAX
            })
        );
        assert_eq!(sys.hop_state_len(), 4, "the old blob is untouched");
        // Precies de grens past.
        sys.state_put(&big[..HOP_STATE_MAX]).unwrap();
        assert_eq!(sys.hop_state_len(), HOP_STATE_MAX);
    }

    #[test]
    fn an_app_may_not_touch_the_state_or_restart_hop() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let mut app = Pipe::new(
            port_ip4(2),
            &[
                req(PrivOp::StatePut, 1, 0, 0, b"", b"mine now"),
                op(PrivOp::StateGet, 2, 0, 0),
                op(PrivOp::HopRestart, 3, 0, 0),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut app, &mut mem, &hooks, &tee, None,
        );
        let res = results(&app.tx);
        assert!(res.iter().all(|r| r.1 == STATUS_DENIED), "{res:?}");
        assert_eq!(sys.hop_state_len(), 0);
        assert!(hooks.4.take().is_none(), "the restart reached the hook");
    }

    #[test]
    fn hop_restart_checks_its_fields_before_the_hook() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let sha = [5u8; 32];
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        for (call, why) in [
            (op(PrivOp::HopRestart, 1, 0, 1 << 32), "unknown flag"),
            (op(PrivOp::HopRestart, 2, 0, 30_001), "grace too long"),
            (
                req(PrivOp::HopRestart, 3, 0, 0, &sha, b""),
                "sum without an image",
            ),
            (req(PrivOp::HopRestart, 4, 3, 0, &sha[..31], b""), "bad sum"),
            // Geen stroom in slot 5: niets om op te eisen.
            (
                req(PrivOp::HopRestart, 5, 5, 0, &sha, b""),
                "no complete image stream",
            ),
        ] {
            let mut p = Pipe::new(port_ip4(1), &[call]);
            let _ = drive(
                &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
            );
            assert_eq!(results(&p.tx)[0].1, STATUS_ERROR, "{why}");
            assert_eq!(hooks.5.take(), Some(why));
            assert!(hooks.4.take().is_none(), "{why} reached the hook");
        }
        // Hetzelfde image, met de standaard en met een eigen termijn.
        let mut p = Pipe::new(port_ip4(1), &[op(PrivOp::HopRestart, 6, 0, 0)]);
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(results(&p.tx)[0].1, STATUS_OK);
        assert_eq!(hooks.4.take(), Some((None, Duration::from_secs(3))));
        let mut p = Pipe::new(port_ip4(1), &[op(PrivOp::HopRestart, 7, 0, 500)]);
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(hooks.4.take(), Some((None, Duration::from_millis(500))));
    }

    /// Een START_SLOT met een rauwe jobnaam.
    fn raw_start(seq: u32, size: u64, job: &[u8]) -> Vec<u8> {
        let r = StartReq {
            memory_limit: ABI_TAIL + 1,
            image_size: size,
            cores: 1,
            pool_cores: 1,
            job,
            ..Default::default()
        };
        let mut v = vec![0u8; 512];
        let n = r.encode(&mut v, seq).unwrap();
        v.truncate(n);
        v
    }

    #[test]
    fn a_hop_image_streams_raw_like_a_flip_bundle_and_is_claimed_once() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        // Geen geldige ELF: de kern plaatst hem niet, hij gaat rauw de
        // partitie in; som en ELF toetst de haak.
        let img = b"a new Hop, raw".repeat(5);
        let sha = [3u8; 32];
        let hex = b"03".repeat(32);
        let restart = req(PrivOp::HopRestart, 6, 3, 2000, &hex, b"");
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                raw_start(1, img.len() as u64, HOP_IMAGE_JOB),
                enc(&stream_req(2, 3, 0, &img[..20])),
                // Half binnen: geweigerd, en de stroom blijft staan.
                restart.clone(),
                enc(&stream_req(3, 3, 20, &img[20..])),
                op(PrivOp::SlotStatus, 4, 3, 128),
                // Een Hop-image is geen kern-bundel.
                req(PrivOp::Flip, 5, 3, 0, &sha, b""),
                restart.clone(),
                restart,
                op(PrivOp::SlotStatus, 7, 3, 128),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        assert_eq!(res.len(), 9);
        assert_eq!((res[0].1, res[0].3), (STATUS_OK, 3));
        assert_eq!(stream_state(&res[1]), StreamState::More);
        assert_eq!(res[2].1, STATUS_ERROR, "an incomplete stream was claimed");
        assert_eq!(stream_state(&res[3]), StreamState::Placed, "complete");
        let info = SlotInfo::decode(&res[4].4).unwrap();
        assert_eq!(info.state, SlotState::Streaming as u8, "not started");
        // De kern bemat de partitie zelf, zoals voor een bundel: het image
        // plus de staart, op hele 2 MB, ook al vroeg de start minder.
        assert_eq!(
            info.partition,
            (img.len() as u64 + ABI_TAIL).next_multiple_of(2 * MIB)
        );
        assert_eq!(res[5].1, STATUS_ERROR, "a Hop image went to the flip");
        assert_eq!(hooks.1.get(), None);
        assert_eq!(
            (res[6].1, res[7].1),
            (STATUS_OK, STATUS_ERROR),
            "claimed once"
        );
        let (image, grace) = hooks.4.take().unwrap();
        let image = image.unwrap();
        assert_eq!(
            (image.slot, image.len, image.sha256),
            (s(3), img.len() as u64, sha)
        );
        assert_eq!(grace, Duration::from_secs(2));
        // Het slot is terug in de pool.
        let info = SlotInfo::decode(&res[8].4).unwrap();
        assert_eq!(info.state, SlotState::Empty as u8);
    }

    #[test]
    fn the_open_streams_of_an_old_hop_are_dropped() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let img = elf(1);
        let mut p = Pipe::new(
            port_ip4(1),
            &[
                start_call(1, img.len() as u64, b""),
                raw_start(2, 64, HOP_IMAGE_JOB),
            ],
        );
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut p, &mut mem, &hooks, &tee, None,
        );
        let res = results(&p.tx);
        assert_eq!((res[0].3, res[1].3), (3, 4));
        assert_eq!(a.status(s(3)).occupancy, Occupancy::Streaming);
        let n = {
            let mut gone = pin!(sys.drop_streams(&reply));
            let mut run = pin!(a.run(&inbox));
            let mut cx = Context::from_waker(Waker::noop());
            loop {
                let _ = run.as_mut().poll(&mut cx);
                if let Poll::Ready(n) = gone.as_mut().poll(&mut cx) {
                    break n;
                }
            }
        };
        assert_eq!(n, 2);
        assert!(!sys.has_stream(s(3)) && !sys.has_stream(s(4)));
        assert_eq!(a.status(s(3)).occupancy, Occupancy::Empty);
        assert_eq!(a.status(s(4)).occupancy, Occupancy::Empty);
    }

    #[test]
    fn an_image_is_checked_without_placing_a_byte() {
        let img = elf(1);
        assert_eq!(check_image(&img, 8 * MIB), Ok(()));
        assert!(check_image(b"not an ELF", 8 * MIB).is_err());
        assert!(check_image(&img[..40], 8 * MIB).is_err(), "truncated");
        assert!(check_image(&img, ABI_TAIL).is_err(), "no room");
    }

    #[test]
    fn next_log_zero_is_the_console_of_the_kern_and_privileged() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let reply = Reply::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let kern = crate::conport::tests::ring(256);
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1)))).with_console(&kern);
        kern.append(b"net: 10.0.2.15 HOPOS_NET_UP\n");
        let (mut mem, hooks) = (SparseMem::default(), NoHooks::default());
        // Een app mag hem niet lezen.
        let mut app = Pipe::new(port_ip4(2), &[op(PrivOp::NextLog, 1, 0, 64)]);
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut app, &mut mem, &hooks, &tee, None,
        );
        assert_eq!(results(&app.tx)[0].1, STATUS_DENIED);
        let mut hop = Pipe::new(
            port_ip4(1),
            &[
                op(PrivOp::NextLog, 1, 0, 64),
                op(PrivOp::NextLog, 2, 2, 64),
                op(PrivOp::NextLog, 3, 0, 64),
                op(PrivOp::NextLog, 4, 0, 3),
            ],
        );
        kern.append(b"slot 2: an app line\n");
        let _ = drive(
            &sys, &mut a, &inbox, &reply, &mut hop, &mut mem, &hooks, &tee, None,
        );
        let res = results(&hop.tx);
        assert_eq!((res[0].1, res[0].3), (STATUS_OK, 1));
        assert_eq!(&res[0].4[..], b"net: 10.0.2.15 HOPOS_NET_UP");
        // Geen ring per slot meer: NEXT_LOG kent alleen slot 0.
        assert_ne!(res[1].1, STATUS_OK, "NEXT_LOG on slot 2");
        // De regel van de app staat op de console; Hop splitst op slot.
        assert_eq!(&res[2].4[..], b"slot 2: an app line");
        assert_eq!((res[3].1, res[3].3), (STATUS_OK, 0), "drained");
    }

    #[test]
    fn admission_caps_connections_per_lifetime() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        crate::slots::tests::start(&mut a, 1, 8, 1).unwrap();
        let inbox: Mailbox<Envelope<'_>, 2> = Mailbox::new();
        let sys = System::new(&inbox, &svc, None);
        let one = sys.admit(port_ip4(1)).unwrap();
        let two = sys.admit(port_ip4(1)).unwrap();
        let full = Refusal::Full {
            slot: s(1),
            cap: MAX_SYSTEM_CONNS,
        };
        assert_eq!(
            sys.admit(port_ip4(1)).err(),
            Some(full),
            "third connection admitted"
        );
        // Een weigering kost geen plaats: ook de vierde blijft buiten.
        assert_eq!(
            sys.admit(port_ip4(1)).err(),
            Some(full),
            "a refusal gave a seat back"
        );
        assert_eq!(svc.ctl(s(1)).unwrap().conns(), 2);
        drop(one);
        assert!(sys.admit(port_ip4(1)).is_ok());
        drop(two);
        assert_eq!(
            sys.admit(port_ip4(2)).err(),
            Some(Refusal::NotLive { slot: s(2) }),
            "slot without servicer admitted"
        );
        assert_eq!(sys.admit(0x0A64_0001).err(), Some(Refusal::NoSlot));
        // Het slot van Hop houdt er één meer, voor zijn store-taak.
        let hop = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let seats: Vec<_> = (0..MAX_HOP_CONNS).map(|_| hop.admit(port_ip4(1))).collect();
        assert!(seats.iter().all(core::result::Result::is_ok));
        assert!(
            hop.admit(port_ip4(1)).is_err(),
            "a fourth connection for Hop"
        );
    }

    /// De plaatsen van een vorige levensduur tellen niet tegen de nieuwe:
    /// een herstarte Hop belt binnen milliseconden (de ORION, 24-10), en
    /// de verbindingen van de oude zagen hun einde dan nog niet. Geven ze
    /// hun plaats later terug, dan loopt de nieuwe teller niet onder.
    #[test]
    fn a_new_lifetime_does_not_wait_for_the_seats_of_the_old_one() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
        crate::slots::tests::start(&mut a, 1, 8, 1).unwrap();
        let inbox: Mailbox<Envelope<'_>, 2> = Mailbox::new();
        let hop = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let old: Vec<_> = (0..MAX_HOP_CONNS)
            .map(|_| hop.admit(port_ip4(1)).unwrap())
            .collect();
        assert!(hop.admit(port_ip4(1)).is_err());
        // De oude Hop stopt en de nieuwe start, terwijl zijn verbindingen nog
        // open staan.
        crate::slots::tests::stop(&mut a, 1).unwrap();
        crate::slots::tests::start(&mut a, 1, 8, 1).unwrap();
        let new: Vec<_> = (0..MAX_HOP_CONNS).map(|_| hop.admit(port_ip4(1))).collect();
        assert!(
            new.iter().all(core::result::Result::is_ok),
            "the old lifetime still held the seats: {:?}",
            new.iter()
                .filter_map(|r| r.as_ref().err())
                .collect::<Vec<_>>()
        );
        assert!(hop.admit(port_ip4(1)).is_err(), "the new cap still holds");
        // De oude geven hun plaats terug: de nieuwe telt er nog drie.
        drop(old);
        assert_eq!(svc.ctl(s(1)).unwrap().conns(), u32::from(MAX_HOP_CONNS));
        assert!(
            hop.admit(port_ip4(1)).is_err(),
            "an old drop gave a new seat"
        );
        drop(new);
        assert_eq!(svc.ctl(s(1)).unwrap().conns(), 0);
    }

    /// De belofte van een start (ook die van een herstarte Hop): vanaf de
    /// eerste instructie van de bewoner, het startschot, wordt zijn
    /// verbinding toegelaten. De generatie staat vóór de dispatch, en de
    /// plaatsen van de vorige levensduur tellen niet. Geen `NotLive` tussen
    /// Arm en de eerste call; een weigering daarna is een fout van de kern.
    #[test]
    fn a_resident_is_admitted_from_its_first_instruction() {
        let svc: &'static Servicers = std::boxed::Box::leak(std::boxed::Box::new(Servicers::new()));
        let con: &'static FakeConsole = std::boxed::Box::leak(std::boxed::Box::default());
        let inbox: &'static Mailbox<Envelope<'static>, 2> =
            std::boxed::Box::leak(std::boxed::Box::new(Mailbox::new()));
        let hop: &'static System<'static, 'static, 2> = std::boxed::Box::leak(
            std::boxed::Box::new(System::new(inbox, svc, Some(Privilege::for_test(s(1))))),
        );
        let seen: &'static std::cell::RefCell<Vec<core::result::Result<(), Refusal>>> =
            std::boxed::Box::leak(std::boxed::Box::default());
        let mut a = actor(svc, con, Obey::Exit, 64, 4);
        a.cage().on_dispatch = Some(std::boxed::Box::new(move |slot| {
            // De bewoner belt meteen: al zijn verbindingen tegelijk.
            for _ in 0..MAX_HOP_CONNS {
                seen.borrow_mut()
                    .push(hop.admit(port_ip4(slot)).map(core::mem::drop));
            }
        }));
        crate::slots::tests::start(&mut a, 1, 8, 1).unwrap();
        // De oude Hop houdt zijn verbindingen nog vast als de nieuwe start.
        let old: Vec<_> = (0..MAX_HOP_CONNS)
            .map(|_| hop.admit(port_ip4(1)).unwrap())
            .collect();
        crate::slots::tests::stop(&mut a, 1).unwrap();
        crate::slots::tests::start(&mut a, 1, 8, 1).unwrap();
        drop(old);
        let seen = seen.borrow();
        assert_eq!(seen.len(), 2 * usize::from(MAX_HOP_CONNS));
        assert!(seen.iter().all(core::result::Result::is_ok), "{seen:?}");
    }

    /// Een lege omgeving van één verbinding: buffers, antwoordplek, geheugen.
    struct Seat {
        buf: Vec<u8>,
        out: Vec<u8>,
        reply: Reply,
        mem: SparseMem,
    }

    impl Seat {
        fn new() -> Seat {
            Seat {
                buf: vec![0u8; 4096],
                out: vec![0u8; 4096],
                reply: Reply::new(),
                mem: SparseMem::default(),
            }
        }
    }

    /// Het gebrek van 29-09: een half-open verbinding (de app parkeerde en
    /// zond nooit zijn FIN) mag een andere verbinding niet ophouden. Hop in
    /// slot 1 krijgt zijn antwoorden terwijl slot 2 zwijgt en openstaat.
    #[test]
    fn a_silent_connection_does_not_hold_up_another() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        // De antwoordplekken leven langer dan de inbox die ernaar wijst.
        let (mut s1, mut s2) = (Seat::new(), Seat::new());
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))));
        let (timer, hooks) = (SlowTimer::default(), NoHooks::default());
        let mut quiet = Pipe::silent(port_ip4(2));
        let mut hop = Pipe::new(
            port_ip4(1),
            &[
                op(PrivOp::SlotStatus, 1, 2, 128),
                op(PrivOp::SetClock, 2, 0, 42),
            ],
        );
        let (w1, w2) = (sys.admit(quiet.ip).unwrap(), sys.admit(hop.ip).unwrap());
        let mut done = None;
        {
            let mut f1 = pin!(sys.serve(
                &mut quiet,
                &w1,
                &s1.reply,
                &timer,
                &mut s1.mem,
                &hooks,
                &tee,
                &mut s1.buf,
                &mut s1.out,
            ));
            let mut f2 = pin!(sys.serve(
                &mut hop,
                &w2,
                &s2.reply,
                &timer,
                &mut s2.mem,
                &hooks,
                &tee,
                &mut s2.buf,
                &mut s2.out,
            ));
            let mut run = pin!(a.run(&inbox));
            let mut cx = Context::from_waker(Waker::noop());
            for _ in 0..1000 {
                let _ = run.as_mut().poll(&mut cx);
                assert!(f1.as_mut().poll(&mut cx).is_pending(), "silent peer closed");
                if let Poll::Ready(e) = f2.as_mut().poll(&mut cx) {
                    done = Some(e);
                    break;
                }
            }
            assert!(f1.as_mut().poll(&mut cx).is_pending());
            assert_eq!(
                svc.ctl(s(2)).unwrap().conns(),
                1,
                "silent peer lost its seat"
            );
        }
        assert_eq!(done, Some(End::Peer));
        let res = results(&hop.tx);
        assert_eq!(res.len(), 2);
        let info = SlotInfo::decode(&res[0].4).unwrap();
        assert_eq!(info.slot_state(), Some(SlotState::Running));
        assert_eq!((res[1].1, hooks.0.get()), (STATUS_OK, 42));
    }

    /// Een stop sluit elke verbinding van het slot, ook twee die zwijgen, en
    /// geeft hun plaatsen terug; daartussen houdt de cap de derde buiten.
    #[test]
    fn a_stop_closes_every_connection_of_the_slot() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let mut a = node(&svc, &con);
        let (mut s1, mut s2) = (Seat::new(), Seat::new());
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, None);
        let (timer, hooks) = (SlowTimer::default(), NoHooks::default());
        let (mut p1, mut p2) = (Pipe::silent(port_ip4(2)), Pipe::silent(port_ip4(2)));
        let ctl = svc.ctl(s(2)).unwrap();
        let (w1, w2) = (
            sys.admit(port_ip4(2)).unwrap(),
            sys.admit(port_ip4(2)).unwrap(),
        );
        assert!(sys.admit(port_ip4(2)).is_err(), "third connection admitted");
        let mut ends = [None, None];
        {
            let mut f1 = pin!(sys.serve(
                &mut p1,
                &w1,
                &s1.reply,
                &timer,
                &mut s1.mem,
                &hooks,
                &tee,
                &mut s1.buf,
                &mut s1.out,
            ));
            let mut f2 = pin!(sys.serve(
                &mut p2,
                &w2,
                &s2.reply,
                &timer,
                &mut s2.mem,
                &hooks,
                &tee,
                &mut s2.buf,
                &mut s2.out,
            ));
            let mut cx = Context::from_waker(Waker::noop());
            for _ in 0..100 {
                assert!(f1.as_mut().poll(&mut cx).is_pending());
                assert!(f2.as_mut().poll(&mut cx).is_pending());
            }
            let stop = Request::Stop {
                slot: s(2),
                timeout: Duration::from_millis(50),
            };
            let r = crate::testutil::block_on(a.handle(stop));
            assert!(matches!(r, Response::Done), "stop not confirmed");
            let [e1, e2] = &mut ends;
            for _ in 0..10 {
                for (f, e) in [(f1.as_mut(), &mut *e1), (f2.as_mut(), &mut *e2)] {
                    if e.is_none()
                        && let Poll::Ready(x) = f.poll(&mut cx)
                    {
                        *e = Some(x);
                    }
                }
            }
        }
        assert_eq!(ends, [Some(End::Evicted), Some(End::Evicted)]);
        drop((w1, w2));
        assert_eq!(ctl.conns(), 0, "seats not given back");
        assert!(sys.admit(port_ip4(2)).is_err(), "stopped slot admitted");
    }

    /// Een verbinding zonder verkeer van een levend slot sluit na
    /// [`IDLE_TIMEOUT`], niet eerder.
    #[test]
    fn a_silent_connection_of_a_live_slot_times_out() {
        let (svc, con) = (Servicers::new(), FakeConsole::default());
        let tee = &con;
        let _a = node(&svc, &con);
        let mut seat = Seat::new();
        let inbox: Mailbox<Envelope<'_>, 8> = Mailbox::new();
        let sys = System::new(&inbox, &svc, None);
        let (timer, hooks) = (FakeTimer::default(), NoHooks::default());
        let mut p = Pipe::silent(port_ip4(2));
        let who = sys.admit(port_ip4(2)).unwrap();
        let end = crate::testutil::block_on(sys.serve(
            &mut p,
            &who,
            &seat.reply,
            &timer,
            &mut seat.mem,
            &hooks,
            &tee,
            &mut seat.buf,
            &mut seat.out,
        ));
        assert_eq!(end, End::Idle);
        assert_eq!(
            timer.now.get(),
            IDLE_TIMEOUT.as_nanos() as u64,
            "closed at the end of the silence, not later"
        );
    }
}
