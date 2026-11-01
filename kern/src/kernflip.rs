//! De kern-flip: de overdracht (het handoff-blob), de adoptie door de nieuwe
//! kern ([`adopted`]), de vluchtrecorder met zijn archief, en de zwarte doos
//! van de console ([`box_open`], [`box_write`], [`box_take`]).
//!
//! Alles in het blob is BOEKHOUDING, geen inhoud: de app-werelden blijven
//! staan waar ze staan. Bij een onbruikbare overdracht stopt de nieuwe boot;
//! de allocator mag mogelijk levende eigenaren nooit als vrije ruimte
//! behandelen (E8). Het herstel van de claims zelf is
//! [`crate::slots::Lifecycle::adopt`].
//!
//! Daarnaast de bundel ([`Bundle`]: de kern-ELF plus zijn relocatietabel,
//! `image/flip-bundle.sh`) en het platte neerleggen ervan ([`flatten`]); de
//! som die hem vertrouwd maakt is [`abi::sha256`]. Het relokeren en de
//! sprong met de MMU uit zijn van `cpu::el2::chain`; het ophalen doet Hop
//! (`POST /flip`), dat de bundel in een gereserveerd slot stroomt.

use crate::cage::PhysMem;
use crate::slots::{Mount, SlotState, try_push};
use crate::{Error, Result};
use alloc::vec::Vec;

/// "HOPHAND1" little-endian.
pub const HAND_MAGIC: u64 = 0x3144_4E41_4850_4F48;
/// De versie van het blob: 7 sinds 01-11 (de lease erin; de conntrack, de
/// koude vlag en de nulwoorden van oudere vormen eruit).
pub const HAND_VERSION: u64 = 7;
/// De plek van het blob op elk board (`abi::layout::FLIP_HANDOFF_LEN`):
/// 256 KB. Past het hok er niet in, dan is dat `HOPOS_FATAL` na de 202
/// (Derek, 01-11: geen begrenzing vooraf tot blijkt dat ze nodig is).
pub const HANDOFF_TAIL: usize = 0x40000;
// De grenzen van wat het blob per slot draagt. Eén set voor beide kanten:
// de export (`slots::Lifecycle::snapshot`) weigert erboven vóór de sprong,
// en `decode` neemt niet meer (tot 04-10 las hij 64 volumes van 4096
// bytes, terwijl er nooit meer dan 32 van 256 in gingen).
/// Hoogstens zoveel poorten per slot.
pub const MAX_FLIP_PORTS: usize = 64;
/// De langste job- en groepsnaam.
pub const MAX_FLIP_JOB: usize = 256;
/// Zoveel volumes: wat een start draagt (`abi`), want een volume dat de
/// start aannam maar de flip niet kan overdragen, zou een flip later
/// weigeren om iets dat bij de start al vaststond.
pub const MAX_FLIP_MOUNTS: usize = abi::systemapi::MAX_START_MOUNTS;
/// Het langste volumepad, om dezelfde reden gelijk aan dat van een start.
pub const MAX_FLIP_PATH: usize = abi::systemapi::MAX_MOUNT_PATH;
/// De kop: magic, versie, het aantal bewoners, de generatie, de wandklok en
/// de vijf woorden van de lease.
const HAND_HEAD: usize = 80;
const SLOT_HEAD: usize = 80;

/// Wat de vertrekkende kern achterliet: per onderdeel zijn eigen stuk. De
/// boot van de nieuwe kern loopt dezelfde reeks als een koude boot; per stap
/// begint hij leeg, of neemt hij dit stuk over (`hopos/src/flip.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handoff {
    /// De hoeveelste flip deze boot is (1 = de eerste).
    pub generation: u64,
    /// De wandklok van de vertrekkende kern: Unix-nanoseconden bij
    /// tellerstand 0 (`hopos/src/clock.rs`), zoals Hop hem via SNTP zette.
    /// De teller loopt door de sprong heen door, dus de offset blijft
    /// geldig. 0 = geen klok.
    pub wall_off: u64,
    /// De lease van de uplink (H1, 01-11): de nieuwe kern slaat DHCP over,
    /// zodat zijn system-listener er staat vóór Hop een beurt krijgt.
    /// `None`: de nieuwe kern haalt een lease.
    pub lease: Option<Lease>,
    /// Het hok: de bewoners, in hun kooien op de switch-code die blijft
    /// staan, met hun poorten.
    pub slots: Vec<SlotState>,
}

/// De lease van de uplink, zoals de DHCP-keeper hem hield: de velden van
/// `leandhcp::Lease` (adressen big-endian als getal) plus de tellerstand
/// van de laatste ACK, waarop zijn timers lopen.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Lease {
    /// Ons adres.
    pub ip: u32,
    /// Het subnetmasker.
    pub mask: u32,
    /// De router.
    pub gateway: u32,
    /// De resolver; 0 = geen.
    pub dns: u32,
    /// De server die de lease gaf.
    pub server: u32,
    /// Looptijd, T1 en T2 in seconden.
    pub secs: [u32; 3],
    /// De tellerstand (ns) van de ACK.
    pub bound_ns: u64,
}

/// De vijf kopwoorden van de lease ([`Handoff::lease`]), aan het eind van
/// de kop; een adres 0 is "geen lease".
const LEASE_OFF: usize = 40;
const _: () = assert!(LEASE_OFF + 5 * 8 == HAND_HEAD);

impl Lease {
    /// De vijf kopwoorden.
    fn words(&self) -> [u64; 5] {
        let pair = |a: u32, b: u32| u64::from(a) | u64::from(b) << 32;
        let [secs, t1, t2] = self.secs;
        [
            pair(self.ip, self.mask),
            pair(self.gateway, self.dns),
            pair(self.server, secs),
            pair(t1, t2),
            self.bound_ns,
        ]
    }

    /// Terug uit de vijf kopwoorden; een adres 0 is geen lease.
    fn from_words(w: [u64; 5]) -> Option<Lease> {
        let [a, b, c, d, bound_ns] = w;
        let lo = |v: u64| v as u32;
        let hi = |v: u64| (v >> 32) as u32;
        (lo(a) != 0).then_some(Lease {
            ip: lo(a),
            mask: hi(a),
            gateway: lo(b),
            dns: hi(b),
            server: lo(c),
            secs: [hi(c), lo(d), hi(d)],
            bound_ns,
        })
    }
}

fn put(b: &mut Vec<u8>, s: &[u8]) -> Result {
    b.try_reserve(s.len())
        .map_err(|_| Error::OutOfMemory { bytes: s.len() })?;
    b.extend_from_slice(s);
    Ok(())
}

fn put64(b: &mut Vec<u8>, v: u64) -> Result {
    put(b, &v.to_le_bytes())
}

fn pad8(b: &mut Vec<u8>) -> Result {
    while !b.len().is_multiple_of(8) {
        put(b, &[0])?;
    }
    Ok(())
}

/// Bouwt het blob. Past het niet in `max`, dan is dat een fout vóór de
/// sprong (en dus geen flip) in plaats van een half blob.
pub fn encode(h: &Handoff, max: usize) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    for v in [
        HAND_MAGIC,
        HAND_VERSION,
        h.slots.len() as u64,
        h.generation,
        h.wall_off,
    ] {
        put64(&mut b, v)?;
    }
    for v in h.lease.map_or([0; 5], |l| l.words()) {
        put64(&mut b, v)?;
    }
    for s in &h.slots {
        for v in [
            s.slot as u64,
            s.part_base,
            s.part_size,
            s.core as u64,
            s.ports.len() as u64,
            s.job.len() as u64,
            s.cores as u64,
            s.mounts.len() as u64,
            s.share_group.len() as u64,
            s.group_cores.len() as u64,
        ] {
            put64(&mut b, v)?;
        }
        for c in &s.group_cores {
            put64(&mut b, *c as u64)?;
        }
        put(&mut b, &s.share_group)?;
        pad8(&mut b)?;
        for p in &s.ports {
            put64(&mut b, u64::from(*p))?;
        }
        put(&mut b, &s.job)?;
        pad8(&mut b)?;
        for m in &s.mounts {
            put64(&mut b, m.local.len() as u64)?;
            put64(&mut b, m.shared.len() as u64)?;
            put(&mut b, &m.local)?;
            put(&mut b, &m.shared)?;
            pad8(&mut b)?;
        }
    }
    if b.len() > max {
        return Err(Error::TooLarge { len: b.len(), max });
    }
    Ok(b)
}

struct R<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn u64(&mut self) -> Result<u64> {
        let s = self
            .b
            .get(self.pos..self.pos + 8)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .ok_or(Error::Corrupt { at: self.pos })?;
        self.pos += 8;
        Ok(u64::from_le_bytes(s))
    }

    fn bytes(&mut self, n: u64) -> Result<&'a [u8]> {
        let n = usize::try_from(n).map_err(|_| Error::Corrupt { at: self.pos })?;
        let s = self
            .b
            .get(self.pos..self.pos.saturating_add(n))
            .ok_or(Error::Corrupt { at: self.pos })?;
        self.pos += n;
        Ok(s)
    }

    fn align(&mut self) {
        self.pos += (8 - self.pos % 8) % 8;
    }

    fn left(&self) -> u64 {
        self.b.len().saturating_sub(self.pos) as u64
    }
}

fn bounded(v: u64, max: u64, at: usize) -> Result<u64> {
    if v > max {
        return Err(Error::Corrupt { at });
    }
    Ok(v)
}

/// Leest het blob terug. Elke afwijking is een fout: de aanroeper stopt de
/// boot voordat een gedeeltelijke allocator bruikbaar wordt.
pub fn decode(b: &[u8]) -> Result<Handoff> {
    if b.len() < HAND_HEAD {
        return Err(Error::Corrupt { at: b.len() });
    }
    let mut r = R { b, pos: 0 };
    let magic = r.u64()?;
    if magic != HAND_MAGIC {
        return Err(Error::Version {
            have: magic,
            want: HAND_MAGIC,
        });
    }
    let v = r.u64()?;
    if v != HAND_VERSION {
        return Err(Error::Version {
            have: v,
            want: HAND_VERSION,
        });
    }
    let n = bounded(r.u64()?, 1024, r.pos)?;
    let mut h = Handoff {
        generation: r.u64()?,
        wall_off: r.u64()?,
        ..Handoff::default()
    };
    h.lease = Lease::from_words([r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?]);
    for _ in 0..n {
        if r.left() < SLOT_HEAD as u64 {
            return Err(Error::Corrupt { at: r.pos });
        }
        let mut s = SlotState {
            slot: r.u64()? as usize,
            part_base: r.u64()?,
            part_size: r.u64()?,
            core: r.u64()? as usize,
            ..SlotState::default()
        };
        let n_ports = r.u64()?;
        let job_len = r.u64()?;
        s.cores = r.u64()? as usize;
        let n_mounts = r.u64()?;
        let group_len = bounded(r.u64()?, MAX_FLIP_JOB as u64, r.pos)?;
        let n_group = bounded(r.u64()?, 1024, r.pos)?;
        if n_group * 8 + group_len > r.left() {
            return Err(Error::Corrupt { at: r.pos });
        }
        for _ in 0..n_group {
            try_push(&mut s.group_cores, r.u64()? as usize)?;
        }
        s.share_group = crate::slots::try_vec(r.bytes(group_len)?)?;
        r.align();
        let n_ports = bounded(n_ports, MAX_FLIP_PORTS as u64, r.pos)?;
        let job_len = bounded(job_len, MAX_FLIP_JOB as u64, r.pos)?;
        if n_ports * 8 + job_len > r.left() {
            return Err(Error::Corrupt { at: r.pos });
        }
        for _ in 0..n_ports {
            try_push(&mut s.ports, r.u64()? as u16)?;
        }
        s.job = crate::slots::try_vec(r.bytes(job_len)?)?;
        r.align();
        for _ in 0..bounded(n_mounts, MAX_FLIP_MOUNTS as u64, r.pos)? {
            let ll = bounded(r.u64()?, MAX_FLIP_PATH as u64, r.pos)?;
            let sl = bounded(r.u64()?, MAX_FLIP_PATH as u64, r.pos)?;
            let local = crate::slots::try_vec(r.bytes(ll)?)?;
            let shared = crate::slots::try_vec(r.bytes(sl)?)?;
            try_push(&mut s.mounts, Mount { local, shared })?;
            r.align();
        }
        try_push(&mut h.slots, s)?;
    }
    Ok(h)
}

/// Waar de flip zijn woorden houdt, uit `layout`.
#[derive(Copy, Clone, Debug)]
pub struct FlipPlan {
    /// Het pointer/magic-paar op de boot-scratch (`layout.HandoffPtrPA`).
    pub handoff_ptr_pa: u64,
    /// De vluchtrecorder (`layout.FlipStagePA`), 0 = geen.
    pub stage_pa: u64,
    /// Het einde van de eigen RAM-declaratie: daar MOET het blob liggen.
    pub own_ram_end: u64,
    /// De zwarte doos van de console ([`BOX_LEN`] bytes), 0 = geen. Zelfde
    /// soort plek als de recorder: buiten elk kern-RAM, de pool, de DMA en
    /// alles wat de firmware bij een verse boot beschrijft.
    pub black_box_pa: u64,
}

/// De uitlijning van het nieuwe beeld in de staging: een cacheregel is
/// genoeg voor de kopie, 64 KiB houdt het leesbaar in een dump.
pub const STAGE_ALIGN: u64 = 64 << 10;

/// Waar het platte beeld van `flat` bytes in de staging `[stage,
/// stage+max)` gaat: direct achter het gestagede image `hop` (begin, maat)
/// als dat in de staging ligt, anders vooraan. `None` als het er niet
/// past.
///
/// Waarom achter Hop en niet eroverheen (29-09): na een KOUDE flip plaatst
/// de nieuwe kern Hop opnieuw uit precies die staging, zoals bij een koude
/// boot. Wie het beeld over Hop legt, heeft daarna een extra kopie van Hop
/// nodig (de kern-RAM van de oude kern bestaat niet meer, de partitie van
/// Hop is van de pool). Zo blijft Hop ook na elke warme flip liggen, en kan
/// een latere koude flip nog.
#[must_use]
pub fn stage_slot(stage: u64, max: u64, flat: u64, hop: Option<(u64, u64)>) -> Option<u64> {
    let end = stage.checked_add(max)?;
    let at = match hop {
        Some((start, len)) if start < end && stage < start.saturating_add(len) => start
            .checked_add(len)?
            .checked_next_multiple_of(STAGE_ALIGN)?
            .max(stage),
        _ => stage,
    };
    at.checked_add(flat).filter(|e| *e <= end).map(|_| at)
}

/// Wat een boot over de flip weet.
#[derive(Debug, PartialEq, Eq)]
pub enum Boot {
    /// Een gewone (koude) boot, of het paar was al geconsumeerd.
    Cold,
    /// Een koude boot die een geldig paar vond: de overdracht van een sprong
    /// die niet de onze is (een firmware-boot na een harde reset). Gewist,
    /// niet gelezen.
    Stale,
    /// Een flip-boot met deze overdracht.
    Adopted(Handoff),
}

/// Consumeert het pointer/magic-paar (vóór het vertrouwen, ook als het niet
/// klopt: half garbage mag geen tweede boot besmetten) en leest het blob.
/// Eenmalig, zoals de kdump-overdracht van Linux: de lezer wist het paar, en
/// het wissen gaat meteen naar DRAM. Zonder die veeg bleef de nul in de
/// cache, overleefde het paar een watchdog-reset, en adopteerde de kern van
/// de stick een overdracht die al geland was (O6N, 04-10).
///
/// `jumped`: deze kern kwam uit de trampoline van een sprong
/// (`cpu::boot::FLIP_ENTERED`). Zonder sprong is elk paar oud
/// ([`Boot::Stale`]): een firmware-boot adopteert nooit, wat er ook in het
/// geheugen ligt, want na een reset draaien de bewoners niet meer.
///
/// Een kapot blob met een geldig paar ná een sprong is GEEN koude boot: er
/// is echt geflipt en er kunnen bewoners leven, en een koude boot veegt
/// juist hun regio. De fout gaat naar de aanroeper, die blijft staan tot de
/// watchdog komt (`HOPOS_FLIP_BLOB_BAD`).
pub fn adopted(mem: &mut impl PhysMem, plan: &FlipPlan, jumped: bool) -> Result<Boot> {
    let ptr = mem.read64(plan.handoff_ptr_pa);
    let magic = mem.read64(plan.handoff_ptr_pa + 8);
    if ptr == 0 && magic == 0 {
        return Ok(Boot::Cold);
    }
    mem.write64(plan.handoff_ptr_pa, 0);
    mem.write64(plan.handoff_ptr_pa + 8, 0);
    mem.clean_inv(plan.handoff_ptr_pa, 16);
    if magic != HAND_MAGIC {
        return Ok(Boot::Cold); // Een verdwaalde pointer.
    }
    if !jumped {
        return Ok(Boot::Stale);
    }
    // De pointer wijst exact op het einde van de eigen RAM-declaratie: een
    // eigenschap van de constructie, en de hele klasse "lees op een adres
    // dat een ander daar neerlegde" is weg.
    if ptr == 0 || !ptr.is_multiple_of(8) || (plan.own_ram_end != 0 && ptr != plan.own_ram_end) {
        return Err(Error::Range {
            base: ptr,
            size: HANDOFF_TAIL as u64,
        });
    }
    let mut b = Vec::new();
    b.try_reserve_exact(HANDOFF_TAIL)
        .map_err(|_| Error::OutOfMemory {
            bytes: HANDOFF_TAIL,
        })?;
    b.resize(HANDOFF_TAIL, 0);
    mem.copy_out(&mut b, ptr);
    match decode(&b) {
        Ok(h) => {
            archive_stage(mem, plan, h.generation);
            // Met de generatie (Go schreef 0): een koude boot na een dood
            // hier meldt dan welke kern het was, dezelfde als in de kop van
            // de zwarte doos.
            stage(mem, plan, Stage::Landed, h.generation);
            Ok(Boot::Adopted(h))
        }
        Err(e) => {
            stage(mem, plan, Stage::AdoptBlobBad, 0);
            Err(e)
        }
    }
}

/// De stappen van de flip, in volgorde. Een nieuwe stap hoort ACHTERAAN:
/// ertussen schuiven hernummert de rest, en dan meldt een oudere kern zijn
/// eigen stand verkeerd (06-09, het kostte een ronde). De vier stappen
/// zonder schrijver gingen eruit met `FLIP_ABI` 4 (01-11).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Stage {
    /// Bundel opgehaald en geverifieerd.
    Fetched = 1,
    /// Bundel gevalideerd.
    BundleOk,
    /// Segmenten geplaatst.
    Placed,
    /// Relocaties toegepast.
    Rebased,
    /// Bewoners en NAT vastgelegd.
    Captured,
    /// Blob geschreven.
    Handoff,
    /// Op het punt van springen.
    Jumping,
    /// De nieuwe kern haalde main.
    EarlyMain,
    /// De overdracht is geconsumeerd.
    Landed,
    /// Het net van de nieuwe kern is op.
    NetUp,
    /// Het blob decodeerde niet; de kern wacht op de watchdog.
    AdoptBlobBad,
}

impl Stage {
    fn from(n: u64) -> Option<Stage> {
        use Stage::*;
        [
            Fetched,
            BundleOk,
            Placed,
            Rebased,
            Captured,
            Handoff,
            Jumping,
            EarlyMain,
            Landed,
            NetUp,
            AdoptBlobBad,
        ]
        .into_iter()
        .find(|s| *s as u64 == n)
    }

    /// De consoletekst bij een stand.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Stage::Fetched => "bundle fetched and verified",
            Stage::BundleOk => "bundle validated",
            Stage::Placed => "segments placed",
            Stage::Rebased => "relocations applied",
            Stage::Captured => "residents and NAT captured",
            Stage::Handoff => "handoff blob written",
            Stage::Jumping => "about to jump into the new kernel",
            Stage::EarlyMain => "the new kernel reached main but died before the handover",
            Stage::Landed => {
                "jump landed and the handover was consumed — died later in the new kernel's boot"
            }
            Stage::NetUp => "the new kernel's network is up — died between net and agent",
            Stage::AdoptBlobBad => {
                "the handoff blob did not decode; the kernel refused to continue and waited for the watchdog"
            }
        }
    }
}

/// "FLIP" in de bovenste 32 bits: willekeurige DRAM-rommel na een koude
/// start wordt nooit als "vorige flip" gelezen.
const STAGE_TAG: u64 = 0x464C_4950 << 32;
const ARCHIVE_OFF: u64 = 8;

fn stage_of(v: u64) -> Option<(Stage, u64)> {
    if v & !0xFFFF_FFFF != STAGE_TAG {
        return None;
    }
    Some((Stage::from(v & 0xFFFF)?, (v >> 16) & 0xFFFF))
}

/// Legt de lopende stand vast, rechtstreeks naar DRAM geveegd: dit woord
/// moet een crash één instructie later nog overleven.
pub fn stage(mem: &mut impl PhysMem, plan: &FlipPlan, s: Stage, generation: u64) {
    if plan.stage_pa == 0 {
        return;
    }
    mem.write64(
        plan.stage_pa,
        STAGE_TAG | s as u64 | (generation & 0xFFFF) << 16,
    );
    mem.clean_inv(plan.stage_pa, 8);
}

/// Schuift een onafgemaakte stand naar het archief. Wat ONZE eigen
/// generatie schreef is eigen voortgang, geen mislukking.
///
/// Het archief bestaat omdat de flip die een gevallen node weer optilt zijn
/// eigen stappen over het spoor heen schrijft: de eerste twee gevallen flips
/// op de M4 (06-09) waren daardoor onverklaarbaar.
pub fn archive_stage(mem: &mut impl PhysMem, plan: &FlipPlan, mine: u64) {
    if plan.stage_pa == 0 {
        return;
    }
    let v = mem.read64(plan.stage_pa);
    match stage_of(v) {
        Some((_, generation)) if generation != mine => {
            mem.write64(plan.stage_pa + ARCHIVE_OFF, v);
            mem.clean_inv(plan.stage_pa + ARCHIVE_OFF, 8);
        }
        _ => {}
    }
}

/// Het archief (de laatste poging die niet landde), gewist na het lezen.
/// Elke boot vraagt het; `None` betekent dat elke eerdere flip netjes eindigde.
pub fn take_archived(mem: &mut impl PhysMem, plan: &FlipPlan) -> Option<(Stage, u64)> {
    if plan.stage_pa == 0 {
        return None;
    }
    let r = stage_of(mem.read64(plan.stage_pa + ARCHIVE_OFF))?;
    mem.write64(plan.stage_pa + ARCHIVE_OFF, 0);
    mem.clean_inv(plan.stage_pa + ARCHIVE_OFF, 8);
    Some(r)
}

/// De lopende stand, gewist na het lezen. Alleen op een KOUDE boot: op een
/// flip-boot staat daar de sprong die ons bracht.
pub fn take_last_flip(mem: &mut impl PhysMem, plan: &FlipPlan) -> Option<(Stage, u64)> {
    if plan.stage_pa == 0 {
        return None;
    }
    let r = stage_of(mem.read64(plan.stage_pa))?;
    mem.write64(plan.stage_pa, 0);
    mem.clean_inv(plan.stage_pa, 8);
    Some(r)
}

/// De eerste regel van main van een geflipte kern: stond er "springen",
/// dan zijn wij die sprong.
pub fn mark_early_boot(mem: &mut impl PhysMem, plan: &FlipPlan) {
    if plan.stage_pa != 0
        && let Some((Stage::Jumping, generation)) = stage_of(mem.read64(plan.stage_pa))
    {
        stage(mem, plan, Stage::EarlyMain, generation);
    }
}

// ---------------------------------------------------------------------------
// De zwarte doos: de console van de kern in een ring die een reset overleeft.
// ---------------------------------------------------------------------------
//
// De vluchtrecorder zegt WAAR een geflipte kern stierf (één woord); de doos
// zegt WAT hij daarvoor zei. Op de Pi 5 zonder UART was de TCP-console
// (hopos/src/conport.rs) het enige oor, en die ring ligt in de BSS van de
// dode kern: na de watchdog-reset overschreven (30-09). Dezelfde truc als de
// recorder, met tekst: een vaste plek buiten elk kernimage, elke schrijf
// meteen naar DRAM geveegd, want een watchdog-reset spoelt geen cache.
//
// Afgekeken van de Go-kern (`driver/conlog/blackbox.go`, "HOPBBX1"): een kop
// met magic en een monotone schrijfpositie, daarachter de ring. Anders dan
// in Go draagt de kop de generatie van de schrijver, begint elke boot een
// verse doos (pas NA het lezen, zie `hopos/src/flip.rs`) en veegt de schrijf
// zelf naar DRAM: in Go was de doos Device-gemapt, op de Pi's ligt hij in
// Normal-geheugen.

/// "HOPBOX02" little-endian: een andere vorm dan de doos van Go (die had
/// geen generatie), dus een andere magic.
pub const BOX_MAGIC: u64 = u64::from_le_bytes(*b"HOPBOX02");
/// De kop: magic, generatie, schrijfpositie, ringmaat; één cacheregel.
pub const BOX_HEAD: u64 = 64;
/// De ring: 16 KiB, zo'n vijftig seconden boot of een minuut tikken.
pub const BOX_RING: u64 = 16 << 10;
/// De hele doos: kop plus ring.
pub const BOX_LEN: u64 = BOX_HEAD + BOX_RING;
/// Hoeveel een koude boot er hoogstens van laat zien: de staart, want daar
/// stierf de kern, en 4 KiB is een scherm vol zonder de boot te verdrinken.
pub const BOX_SHOW: usize = 4096;
const BOX_GEN_OFF: u64 = 8;
const BOX_POS_OFF: u64 = 16;
const BOX_RING_OFF: u64 = 24;

const _: () = assert!(BOX_SHOW as u64 <= BOX_RING && BOX_RING.is_multiple_of(8));

/// Begint een verse doos voor `generation`: de kop eerst zonder magic, dan
/// de magic, elk woord naar DRAM geveegd. Een reset halverwege laat dus een
/// doos zonder magic achter, nooit een met een halve kop.
pub fn box_open(mem: &mut impl PhysMem, plan: &FlipPlan, generation: u64) {
    let base = plan.black_box_pa;
    if base == 0 {
        return;
    }
    mem.write64(base, 0);
    mem.write64(base + BOX_GEN_OFF, generation);
    mem.write64(base + BOX_POS_OFF, 0);
    mem.write64(base + BOX_RING_OFF, BOX_RING);
    mem.clean_inv(base, BOX_HEAD);
    mem.write64(base, BOX_MAGIC);
    mem.clean_inv(base, 8);
}

/// Schrijft `b` achter in de ring. Eerst de bytes, geveegd, dan pas de
/// positie, geveegd: een reset ertussen verliest hoogstens dit stuk, en de
/// positie wijst nooit voorbij wat er echt staat. Een doos zonder magic
/// (nooit geopend, of gewist) blijft onaangeroerd.
pub fn box_write(mem: &mut impl PhysMem, plan: &FlipPlan, b: &[u8]) {
    let base = plan.black_box_pa;
    if base == 0 || b.is_empty() || mem.read64(base) != BOX_MAGIC {
        return;
    }
    let pos = mem.read64(base + BOX_POS_OFF);
    // Meer dan de ring: alleen de staart telt.
    let b = b
        .get(b.len().saturating_sub(BOX_RING as usize)..)
        .unwrap_or(b);
    let at = pos % BOX_RING;
    let room = usize::try_from(BOX_RING - at).unwrap_or(usize::MAX);
    let (x, y) = b.split_at(b.len().min(room));
    let data = base + BOX_HEAD;
    for (pa, part) in [(data + at, x), (data, y)] {
        if !part.is_empty() {
            mem.copy_in(pa, part);
            mem.clean_inv(pa, part.len() as u64);
        }
    }
    mem.write64(base + BOX_POS_OFF, pos.wrapping_add(b.len() as u64));
    mem.clean_inv(base + BOX_POS_OFF, 8);
}

/// Wat een eerdere boot in de doos achterliet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BoxTail {
    /// De generatie van de kern die schreef.
    pub generation: u64,
    /// Hoeveel bytes er in de uitvoer staan (de staart).
    pub len: usize,
    /// Hoeveel die kern er in totaal schreef.
    pub written: u64,
}

/// Leest de staart van de doos in `out` (hoogstens `out.len()` bytes) en
/// wist de doos: een tweede boot drukt hem niet nog eens af. Alleen een doos
/// met de magic, de eigen ringmaat en een generatie (een kern heeft er altijd
/// een, 1 of meer); DRAM-rommel na een koude start is geen console.
pub fn box_take(mem: &mut impl PhysMem, plan: &FlipPlan, out: &mut [u8]) -> Option<BoxTail> {
    let base = plan.black_box_pa;
    if base == 0 || mem.read64(base) != BOX_MAGIC {
        return None;
    }
    let generation = mem.read64(base + BOX_GEN_OFF);
    let written = mem.read64(base + BOX_POS_OFF);
    let ring = mem.read64(base + BOX_RING_OFF);
    mem.write64(base, 0);
    mem.clean_inv(base, 8);
    if ring != BOX_RING || generation == 0 {
        return None;
    }
    let n = written.min(BOX_RING).min(out.len() as u64);
    let len = usize::try_from(n).ok()?;
    let start = written.wrapping_sub(n) % BOX_RING;
    let first = usize::try_from(BOX_RING - start)
        .unwrap_or(usize::MAX)
        .min(len);
    let data = base + BOX_HEAD;
    let (x, y) = out.get_mut(..len)?.split_at_mut(first);
    read_bytes(mem, data + start, x);
    read_bytes(mem, data, y);
    Some(BoxTail {
        generation,
        len,
        written,
    })
}

/// Leest bytes vanaf een willekeurig (ook niet-8-uitgelijnd) adres, per
/// uitgelijnd woord: `PhysMem::copy_out` eist een uitgelijnd begin, en de
/// staart van de ring begint waar hij begint. Een koud pad, 4 KiB.
fn read_bytes(mem: &impl PhysMem, pa: u64, out: &mut [u8]) {
    for (a, b) in (pa..).zip(out.iter_mut()) {
        *b = (mem.read64(a & !7) >> (8 * (a & 7))) as u8;
    }
}

// ---------------------------------------------------------------------------
// De bundel: de kern-ELF plus zijn relocatietabel (image/flip-bundle.sh).
// ---------------------------------------------------------------------------

/// De versie van het flip-contract van v3: de staart-vorm, het handoff-blob
/// en de ingangsconditie samen. De Go-generatie sprak ABI 2 (een geleend
/// venster, `RamStart`/`RamSize`-patches); v3 plaatst de nieuwe kern op het
/// koude adres van de oude, dus een Go-bundel wordt hier geweigerd.
/// 4 sinds 01-11: het blob van versie 7; een kern van ABI 3 weigert zo'n
/// bundel vóór de 202 in plaats van dat de nieuwe kern op een blob landt
/// dat hij niet leest.
pub const FLIP_ABI: u32 = 4;
/// "HOPRELO1" little-endian: hetzelfde woord als `mkkernel -elfreloc`.
pub const RELOC_MAGIC: u64 = 0x314F_4C45_5250_4F48;
/// De kop van de staart: magic, versie, flip-ABI, ELF-maat, linkbasis,
/// platte maat, entry, aantal relocaties.
pub const BUNDLE_HEAD: usize = 56;
/// De kop van versie 2: die van versie 1 plus de som van de switch-code
/// ([`Bundle::switch_sum`]).
pub const BUNDLE_HEAD_V2: usize = 64;
/// De versie van de staart die `image/flip-bundle.sh` schrijft.
pub const BUNDLE_VERSION: u32 = 2;
/// De voet: de offset van de kop en nog een keer de magic.
pub const BUNDLE_FOOT: usize = 16;
/// De grootste platte kern (image plus BSS en stack): een kern van v3 is
/// ruim een MiB (1,1 MiB gemeten 29-09); 64 MiB is een typefout, geen kern.
pub const MAX_FLAT: u64 = 64 << 20;

/// Een gevalideerde flip-bundel over de bytes waarin Hop hem stroomde.
///
/// # Invariants
///
/// `elf` en `relocs` liggen binnen de bundel; elke relocatie-offset is
/// 8-uitgelijnd en valt met zijn woord binnen `flat_size`; `entry` ligt in
/// `[link_load, link_load + flat_size)` en is 4-uitgelijnd.
#[derive(Debug)]
pub struct Bundle<'a> {
    /// De onaangeroerde kern-ELF (met symbooltabel).
    pub elf: &'a [u8],
    /// De flip-ABI van de bundel.
    pub flip_abi: u32,
    /// De linkbasis: het laagste laadadres van de ELF.
    pub link_load: u64,
    /// Het platte beeld inclusief BSS en stack.
    pub flat_size: u64,
    /// Het entrypoint, absoluut op de linkbasis.
    pub entry: u64,
    /// De FNV-1a-som over de EL2-switch-code van de nieuwe kern
    /// (`cpu::el2::image_hash`), zoals het bundelscript hem uit de
    /// symbolen van de koude link rekende. `None` in een bundel van versie
    /// 1: die kan de flip niet vóór de sprong toetsen, en wordt geweigerd.
    ///
    /// Waarom in de bundel: de nieuwe kern adopteert de zittende kopie
    /// alleen bij een gelijke som (`cpu::el2::adopt`), en zonder deze
    /// toets merkt pas de gelande kern een verschil, twee minuten later,
    /// als de guard koud herstart.
    pub switch_sum: Option<u64>,
    relocs: &'a [u8],
}

fn le64(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
        .map(u64::from_le_bytes)
        .ok_or(Error::Corrupt { at })
}

fn le32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::Corrupt { at })
}

impl<'a> Bundle<'a> {
    /// Valideert een bundel. Elke afwijking is een fout: een bundel is
    /// compleet geldig of bestaat niet, want de bytes komen van het net en
    /// dit pad springt er straks in (`bundle.go`, ParseBundleReader).
    pub fn parse(b: &'a [u8]) -> Result<Bundle<'a>> {
        if b.len() < BUNDLE_HEAD + BUNDLE_FOOT + 64 {
            return Err(Error::Corrupt { at: b.len() });
        }
        let foot = b.len() - BUNDLE_FOOT;
        let magic = le64(b, foot + 8)?;
        if magic != RELOC_MAGIC {
            // Een kale ELF in plaats van een bundel, of een afgekapte stroom.
            return Err(Error::Version {
                have: magic,
                want: RELOC_MAGIC,
            });
        }
        let head = usize::try_from(le64(b, foot)?).map_err(|_| Error::Corrupt { at: foot })?;
        if !head.is_multiple_of(8) || head > foot - BUNDLE_HEAD {
            return Err(Error::Corrupt { at: foot });
        }
        if le64(b, head)? != RELOC_MAGIC {
            return Err(Error::Corrupt { at: head });
        }
        let version = le32(b, head + 8)?;
        let head_len = match version {
            1 => BUNDLE_HEAD,
            2 => BUNDLE_HEAD_V2,
            _ => {
                return Err(Error::Version {
                    have: u64::from(version),
                    want: u64::from(BUNDLE_VERSION),
                });
            }
        };
        if head > foot - head_len {
            return Err(Error::Corrupt { at: foot });
        }
        let elf_size = le64(b, head + 16)?;
        let count = le64(b, head + 48)?;
        let bundle = Bundle {
            flip_abi: le32(b, head + 12)?,
            link_load: le64(b, head + 24)?,
            flat_size: le64(b, head + 32)?,
            entry: le64(b, head + 40)?,
            switch_sum: if version >= 2 {
                Some(le64(b, head + 56)?)
            } else {
                None
            },
            elf: b
                .get(..usize::try_from(elf_size).unwrap_or(usize::MAX))
                .filter(|_| elf_size != 0 && elf_size <= head as u64)
                .ok_or(Error::Corrupt { at: head + 16 })?,
            relocs: {
                let start = head + head_len;
                let room = (foot - start) as u64 / 4;
                if count > room {
                    return Err(Error::Corrupt { at: head + 48 });
                }
                b.get(start..start + count as usize * 4)
                    .ok_or(Error::Corrupt { at: start })?
            },
        };
        if bundle.flat_size == 0 || bundle.flat_size > MAX_FLAT {
            return Err(Error::TooLarge {
                len: usize::try_from(bundle.flat_size).unwrap_or(usize::MAX),
                max: MAX_FLAT as usize,
            });
        }
        let end = bundle.link_load.checked_add(bundle.flat_size);
        if !bundle.link_load.is_multiple_of(0x1000) || end.is_none() {
            return Err(Error::Range {
                base: bundle.link_load,
                size: bundle.flat_size,
            });
        }
        if bundle.entry < bundle.link_load
            || end.is_some_and(|e| bundle.entry >= e)
            || !bundle.entry.is_multiple_of(4)
        {
            return Err(Error::Range {
                base: bundle.entry,
                size: bundle.flat_size,
            });
        }
        for (i, off) in bundle.relocs().enumerate() {
            if !off.is_multiple_of(8) || u64::from(off) + 8 > bundle.flat_size {
                return Err(Error::Corrupt {
                    at: head + head_len + i * 4,
                });
            }
        }
        // INVARIANT: bereiken, uitlijning en elke relocatie zijn getoetst.
        Ok(bundle)
    }

    /// De relocatie-offsets: elk een 8-byte-woord in het platte beeld dat
    /// een absoluut adres op de linkbasis draagt.
    pub fn relocs(&self) -> impl Iterator<Item = u32> + Clone + '_ {
        self.relocs
            .chunks_exact(4)
            .filter_map(|c| <[u8; 4]>::try_from(c).ok())
            .map(u32::from_le_bytes)
    }
}

/// Legt de PT_LOAD-segmenten van de bundel plat neer op `dst` (het beeld
/// zoals het op de linkbasis zou staan, maar dan op `dst`): eerst het hele
/// beeld op nul (BSS, stack, gaten), dan de bytes. Geeft het aantal
/// segmenten. Relocaties zijn niet van hier (`cpu::el2::chain::relocate`).
///
/// De ELF-entry moet die van de staart zijn: twee bronnen die het oneens
/// zijn, is een bundel die niet van één build komt.
pub fn flatten(bundle: &Bundle<'_>, mem: &mut impl PhysMem, dst: u64) -> Result<usize> {
    let f = leanelf::File::parse(bundle.elf).map_err(|_| Error::Corrupt { at: 0 })?;
    if f.entry != bundle.entry {
        return Err(Error::Version {
            have: f.entry,
            want: bundle.entry,
        });
    }
    let end = bundle.link_load + bundle.flat_size;
    mem.clear(dst, bundle.flat_size.next_multiple_of(8));
    let mut n = 0;
    for s in f.segments().filter(|s| s.kind == leanelf::PT_LOAD) {
        let bad = Error::Range {
            base: s.paddr,
            size: s.memsz,
        };
        let s_end = s.paddr.checked_add(s.memsz).ok_or(bad)?;
        if s.paddr < bundle.link_load || s_end > end || s.filesz > s.memsz {
            return Err(bad);
        }
        let bytes = usize::try_from(s.off)
            .ok()
            .zip(usize::try_from(s.filesz).ok())
            .and_then(|(o, l)| bundle.elf.get(o..o.checked_add(l)?))
            .ok_or(Error::Corrupt { at: 0 })?;
        mem.copy_in(dst + (s.paddr - bundle.link_load), bytes);
        n += 1;
    }
    if n == 0 {
        return Err(Error::Corrupt { at: 0 });
    }
    Ok(n)
}

/// De eerste acht bytes van een som als getal: kort genoeg voor een
/// foutmelding (`Error::Version`).
#[must_use]
pub fn sum64(sum: &[u8; 32]) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(sum.get(..8).unwrap_or(&[0; 8]));
    u64::from_le_bytes(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cage::tests::SparseMem;
    use std::vec;

    fn m(l: &str, s: &str) -> Mount {
        Mount {
            local: l.as_bytes().to_vec(),
            shared: s.as_bytes().to_vec(),
        }
    }

    fn st(slot: usize, base: u64, mib: u64, core: usize) -> SlotState {
        SlotState {
            slot,
            part_base: base,
            part_size: mib << 20,
            core,
            cores: 1,
            ..SlotState::default()
        }
    }

    #[test]
    fn handoff_round_trip() {
        let mut first = st(1, 0xBC00_0000, 64, 1);
        first.job = b"welcome".to_vec();
        first.ports = vec![8080, 443];
        first.mounts = vec![m("/data", "/data"), m("/shared/logs", "/logs")];
        let h = Handoff {
            generation: 2,
            slots: vec![first, st(7, 0x9000_0000, 48, 3)],
            wall_off: 1_790_767_915_253_886_881,
            lease: Some(Lease {
                ip: 0x0A00_020F,
                mask: 0xFFFF_FF00,
                gateway: 0x0A00_0202,
                dns: 0x0A00_0203,
                server: 0x0A00_0202,
                secs: [86400, 43200, 75600],
                bound_ns: 1_234_567_890,
            }),
        };
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        assert_eq!(decode(&b).unwrap(), h);
    }

    /// De lezer neemt precies wat de export doorlaat: een slot op elke
    /// grens komt heel over, één erboven is een kapot blob.
    #[test]
    fn decode_takes_exactly_the_flip_limits() {
        let path = "/".repeat(MAX_FLIP_PATH);
        let mut full = st(1, 0xBC00_0000, 64, 1);
        full.job = vec![b'j'; MAX_FLIP_JOB];
        full.share_group = vec![b'g'; MAX_FLIP_JOB];
        full.group_cores = vec![1];
        full.ports = vec![80; MAX_FLIP_PORTS];
        full.mounts = vec![m(&path, &path); MAX_FLIP_MOUNTS];
        let h = Handoff {
            slots: vec![full.clone()],
            ..Handoff::default()
        };
        assert_eq!(decode(&encode(&h, HANDOFF_TAIL).unwrap()).unwrap(), h);
        let over: [fn(&mut SlotState); 5] = [
            |s| s.job.push(b'j'),
            |s| s.share_group.push(b'g'),
            |s| s.ports.push(80),
            |s| s.mounts.push(m("/a", "/b")),
            |s| s.mounts[0].shared.push(b'/'),
        ];
        for grow in over {
            let mut s = full.clone();
            grow(&mut s);
            let h = Handoff {
                slots: vec![s],
                ..Handoff::default()
            };
            let b = encode(&h, HANDOFF_TAIL).unwrap();
            assert!(matches!(decode(&b), Err(Error::Corrupt { .. })));
        }
    }

    #[test]
    fn the_new_image_goes_behind_hop_in_the_staging() {
        let (stage, max) = (0xB020_0000u64, 0x00E0_0000u64);
        // Geen Hop: vooraan.
        assert_eq!(stage_slot(stage, max, 0x12_0000, None), Some(stage));
        // Hop vooraan (1,1 MiB): het beeld erachter, op 64 KiB.
        let hop = Some((stage, 0x11_8123));
        assert_eq!(stage_slot(stage, max, 0x12_0000, hop), Some(0xB032_0000));
        // Past het niet meer achter Hop, dan nergens: geen stille overlap.
        assert_eq!(stage_slot(stage, max, max - 0x10_0000, hop), None);
        // Hop buiten de staging (de Pi: waar config.txt hem legde, als dat
        // ergens anders is) raakt de keuze niet.
        assert_eq!(
            stage_slot(stage, max, 0x1000, Some((0x0F20_0000, 0x1000))),
            Some(stage)
        );
        // Te groot, of een staging die om 2^64 heen loopt: nee.
        assert_eq!(stage_slot(stage, max, max + 8, None), None);
        assert_eq!(stage_slot(u64::MAX - 8, 64, 8, None), None);
    }

    #[test]
    fn a_lease_of_zero_is_no_lease() {
        let h = Handoff {
            lease: Some(Lease {
                ip: 0x0A00_020F,
                ..Lease::default()
            }),
            ..Handoff::default()
        };
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        assert_eq!(decode(&b).unwrap(), h);
        let mut old = b.clone();
        old[LEASE_OFF..HAND_HEAD].fill(0);
        assert_eq!(decode(&old).unwrap().lease, None);
    }

    #[test]
    fn handoff_weigert_onzin() {
        let h = Handoff {
            generation: 1,
            slots: vec![st(2, 0x9000_0000, 32, 2)],
            ..Handoff::default()
        };
        let good = encode(&h, HANDOFF_TAIL).unwrap();
        let breaks: [fn(&mut [u8]); 4] = [
            |b| b[0] ^= 0xFF,
            |b| b[8..16].copy_from_slice(&99u64.to_le_bytes()),
            |b| b[16..24].copy_from_slice(&(1u64 << 40).to_le_bytes()),
            |b| b[16..24].copy_from_slice(&4u64.to_le_bytes()),
        ];
        for brk in breaks {
            let mut b = good.clone();
            brk(&mut b);
            assert!(decode(&b).is_err());
        }
        assert!(decode(&good[..32]).is_err());
    }

    #[test]
    fn handoff_preserves_group_pool() {
        let mut s = st(4, 0, 0, 1);
        s.share_group = b"trusted".to_vec();
        s.group_cores = vec![1, 2, 3];
        let h = Handoff {
            slots: vec![s],
            ..Handoff::default()
        };
        let mut b = encode(&h, HANDOFF_TAIL).unwrap();
        let got = &decode(&b).unwrap().slots[0];
        assert_eq!(
            (got.share_group.as_slice(), got.group_cores.as_slice()),
            (&b"trusted"[..], &[1, 2, 3][..])
        );
        b[HAND_HEAD + 72..HAND_HEAD + 80].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&b).is_err(), "accepted impossible group length");
    }

    #[test]
    fn handoff_rejects_truncated_slots() {
        let mut one = st(2, 0x9000_0000, 32, 2);
        one.job = b"job".to_vec();
        let h = Handoff {
            slots: vec![one],
            ..Handoff::default()
        };
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        for n in [HAND_HEAD, HAND_HEAD + 24, b.len() - 8] {
            assert!(decode(&b[..n]).is_err(), "accepted truncation at {n}");
        }
    }

    #[test]
    fn handoff_refuses_what_the_tail_cannot_hold() {
        assert!(encode(&Handoff::default(), 64).is_err());
    }

    const PLAN: FlipPlan = FlipPlan {
        handoff_ptr_pa: 0x1000,
        stage_pa: 0x2000,
        own_ram_end: 0x10_0000,
        black_box_pa: 0x4_0000,
    };

    #[test]
    fn adopted_consumes_the_pair_before_trusting_it() {
        let mut mem = SparseMem::default();
        assert_eq!(adopted(&mut mem, &PLAN, true), Ok(Boot::Cold));
        // Een verdwaalde pointer: geconsumeerd, koude boot.
        mem.write64(PLAN.handoff_ptr_pa, 0x10_0000);
        mem.write64(PLAN.handoff_ptr_pa + 8, 0xdead);
        assert_eq!(adopted(&mut mem, &PLAN, true), Ok(Boot::Cold));
        // Een geldige overdracht.
        let h = Handoff {
            generation: 3,
            slots: vec![st(2, 0x9000_0000, 32, 2)],
            ..Handoff::default()
        };
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        mem.copy_in(PLAN.own_ram_end, &b);
        mem.write64(PLAN.handoff_ptr_pa, PLAN.own_ram_end);
        mem.write64(PLAN.handoff_ptr_pa + 8, HAND_MAGIC);
        assert_eq!(adopted(&mut mem, &PLAN, true), Ok(Boot::Adopted(h)));
        assert_eq!(mem.read64(PLAN.handoff_ptr_pa), 0, "pair not consumed");
        assert_eq!(
            stage_of(mem.read64(PLAN.stage_pa)),
            Some((Stage::Landed, 3)),
            "landed without the generation"
        );
        assert_eq!(
            adopted(&mut mem, &PLAN, true),
            Ok(Boot::Cold),
            "blob adopted twice"
        );
    }

    /// DRAM met een write-back-cache ervoor: een schrijf zonder veeg staat
    /// alleen in de cache, en een reset ([`CachedMem::reset`]) gooit hem weg,
    /// zoals de watchdog op ijzer. QEMU heeft geen cache; hier wel.
    #[derive(Default)]
    struct CachedMem {
        dram: SparseMem,
        dirty: std::collections::HashMap<u64, u64>,
    }

    impl CachedMem {
        fn reset(&mut self) {
            self.dirty.clear();
        }
    }

    impl PhysMem for CachedMem {
        fn read64(&self, pa: u64) -> u64 {
            match self.dirty.get(&pa) {
                Some(v) => *v,
                None => self.dram.read64(pa),
            }
        }
        fn write64(&mut self, pa: u64, v: u64) {
            self.dirty.insert(pa, v);
        }
        fn clean_inv(&mut self, pa: u64, len: u64) {
            let lines: Vec<u64> = self
                .dirty
                .keys()
                .copied()
                .filter(|a| (pa..pa + len).contains(a))
                .collect();
            for a in lines {
                if let Some(v) = self.dirty.remove(&a) {
                    self.dram.write64(a, v);
                }
            }
        }
    }

    /// Legt een overdracht van generatie `generation` neer zoals de sprong
    /// dat doet: blob en paar, naar DRAM geveegd.
    fn jump(mem: &mut impl PhysMem, generation: u64) -> Handoff {
        let h = Handoff {
            generation,
            slots: vec![st(2, 0x9000_0000, 32, 2)],
            ..Handoff::default()
        };
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        mem.copy_in(PLAN.own_ram_end, &b);
        mem.write64(PLAN.handoff_ptr_pa, PLAN.own_ram_end);
        mem.write64(PLAN.handoff_ptr_pa + 8, HAND_MAGIC);
        mem.clean_inv(PLAN.own_ram_end, HANDOFF_TAIL as u64);
        mem.clean_inv(PLAN.handoff_ptr_pa, 16);
        h
    }

    #[test]
    fn a_landed_handoff_does_not_survive_a_reset() {
        // De O6N (04-10): de sprong landt, de kern leeft een minuut en de
        // watchdog reset hem. De kern van de stick mag de overdracht niet
        // nog eens vinden, ook niet als hij zelf geen merkteken toetst.
        let mut mem = CachedMem::default();
        let h = jump(&mut mem, 4);
        assert_eq!(adopted(&mut mem, &PLAN, true), Ok(Boot::Adopted(h)));
        mem.reset();
        assert_eq!(
            mem.read64(PLAN.handoff_ptr_pa + 8),
            0,
            "the wipe stayed in the cache"
        );
        assert_eq!(adopted(&mut mem, &PLAN, true), Ok(Boot::Cold));
    }

    #[test]
    fn a_firmware_boot_never_adopts() {
        // Een paar dat er nog ligt (een oudere kern landde en veegde niet,
        // of de sprong stierf vóór de landing): zonder merkteken van de
        // trampoline is het oud. Gewist, niet gelezen, geen landing.
        let mut mem = SparseMem::default();
        jump(&mut mem, 4);
        assert_eq!(adopted(&mut mem, &PLAN, false), Ok(Boot::Stale));
        assert_eq!(
            mem.read64(PLAN.handoff_ptr_pa + 8),
            0,
            "stale pair not wiped"
        );
        assert_eq!(mem.read64(PLAN.stage_pa), 0, "a stale pair is no landing");
        assert_eq!(adopted(&mut mem, &PLAN, false), Ok(Boot::Cold));
        // Ook een blob dat niet decodeert: geen BLOB_BAD en geen reset-lus.
        mem.write64(PLAN.handoff_ptr_pa, PLAN.own_ram_end);
        mem.write64(PLAN.handoff_ptr_pa + 8, HAND_MAGIC);
        mem.clear(PLAN.own_ram_end, HANDOFF_TAIL as u64);
        assert_eq!(adopted(&mut mem, &PLAN, false), Ok(Boot::Stale));
    }

    #[test]
    fn adopted_refuses_foreign_pointer_and_bad_blob() {
        let mut mem = SparseMem::default();
        mem.write64(PLAN.handoff_ptr_pa, 0x20_0000);
        mem.write64(PLAN.handoff_ptr_pa + 8, HAND_MAGIC);
        assert!(
            adopted(&mut mem, &PLAN, true).is_err(),
            "read a blob somebody else placed"
        );
        mem.write64(PLAN.handoff_ptr_pa, PLAN.own_ram_end);
        mem.write64(PLAN.handoff_ptr_pa + 8, HAND_MAGIC);
        assert!(
            adopted(&mut mem, &PLAN, true).is_err(),
            "garbage blob became a cold boot"
        );
        assert_eq!(
            stage_of(mem.read64(PLAN.stage_pa)).map(|s| s.0),
            Some(Stage::AdoptBlobBad)
        );
    }

    #[test]
    fn recorder_archives_foreign_generation_only() {
        let mut mem = SparseMem::default();
        // Een eerdere poging (generatie 4) sprong en landde nooit.
        stage(&mut mem, &PLAN, Stage::Jumping, 4);
        // Onze eigen sprong (generatie 5) is voortgang, geen mislukking.
        archive_stage(&mut mem, &PLAN, 4);
        assert_eq!(take_archived(&mut mem, &PLAN), None);
        archive_stage(&mut mem, &PLAN, 5);
        assert_eq!(take_archived(&mut mem, &PLAN), Some((Stage::Jumping, 4)));
        assert_eq!(take_archived(&mut mem, &PLAN), None, "archive not cleared");
        // Rommel is geen stand.
        mem.write64(PLAN.stage_pa, 0x1234_5678_0000_0003);
        assert_eq!(take_last_flip(&mut mem, &PLAN), None);
        stage(&mut mem, &PLAN, Stage::Jumping, 9);
        mark_early_boot(&mut mem, &PLAN);
        assert_eq!(take_last_flip(&mut mem, &PLAN), Some((Stage::EarlyMain, 9)));
        assert_eq!(mem.read64(PLAN.stage_pa), 0);
        assert!(Stage::NetUp.describe().contains("network"));
    }
    /// Geheugen dat bijhoudt welke woorden nog niet naar DRAM geveegd zijn:
    /// een watchdog-reset spoelt geen cache, dus alles wat de doos schrijft
    /// moet na de aanroep geveegd zijn.
    #[derive(Default)]
    struct Dirty {
        mem: SparseMem,
        dirty: std::collections::BTreeSet<u64>,
    }

    impl PhysMem for Dirty {
        fn read64(&self, pa: u64) -> u64 {
            self.mem.read64(pa)
        }
        fn write64(&mut self, pa: u64, v: u64) {
            self.mem.write64(pa, v);
            self.dirty.insert(pa);
        }
        fn clean_inv(&mut self, pa: u64, len: u64) {
            // Per cacheregel van 64 bytes, zoals `dc civac`.
            let (lo, hi) = (pa & !63, pa + len);
            self.dirty
                .retain(|w| *w + 8 <= lo || *w >= hi.next_multiple_of(64));
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| b'a' + (i % 26) as u8).collect()
    }

    #[test]
    fn black_box_keeps_the_tail_and_is_read_once() {
        let mut mem = Dirty::default();
        let mut out = [0u8; BOX_SHOW];
        // Nooit geopend: schrijven doet niets, lezen geeft niets.
        box_write(&mut mem, &PLAN, b"lost");
        assert!(mem.mem.0.is_empty(), "wrote into a box nobody opened");
        assert_eq!(box_take(&mut mem, &PLAN, &mut out), None);

        box_open(&mut mem, &PLAN, 3);
        assert!(mem.dirty.is_empty(), "head not swept: {:x?}", mem.dirty);
        // Oneven stukken op oneven adressen, tot ver voorbij de ring.
        let all = pattern(40_000);
        for chunk in all.chunks(97) {
            box_write(&mut mem, &PLAN, chunk);
            assert!(mem.dirty.is_empty(), "not swept: {:x?}", mem.dirty);
        }
        let t = box_take(&mut mem, &PLAN, &mut out).unwrap();
        assert_eq!((t.generation, t.len, t.written), (3, BOX_SHOW, 40_000));
        assert_eq!(&out[..], &all[all.len() - BOX_SHOW..]);
        assert!(mem.dirty.is_empty(), "wipe not swept");
        // Gelezen is gewist; ook schrijven komt er niet meer in.
        assert_eq!(box_take(&mut mem, &PLAN, &mut out), None);
        box_write(&mut mem, &PLAN, b"after");
        assert_eq!(box_take(&mut mem, &PLAN, &mut out), None);
    }

    #[test]
    fn black_box_short_and_oversized_writes() {
        let mut mem = SparseMem::default();
        let mut out = [0u8; BOX_SHOW];
        box_open(&mut mem, &PLAN, 7);
        box_write(&mut mem, &PLAN, b"flip: landed\n");
        let t = box_take(&mut mem, &PLAN, &mut out).unwrap();
        assert_eq!((t.generation, t.len, t.written), (7, 13, 13));
        assert_eq!(&out[..t.len], b"flip: landed\n");
        // Een verse doos na een oude: de oude inhoud telt niet meer mee.
        box_open(&mut mem, &PLAN, 8);
        box_write(&mut mem, &PLAN, b"x");
        let big = pattern(BOX_RING as usize + 1000);
        box_write(&mut mem, &PLAN, &big);
        let mut all = [0u8; BOX_RING as usize];
        let t = box_take(&mut mem, &PLAN, &mut all).unwrap();
        assert_eq!((t.generation, t.len), (8, BOX_RING as usize));
        assert_eq!(t.written, 1 + BOX_RING);
        assert_eq!(&all[..], &big[1000..]);
    }

    #[test]
    fn black_box_refuses_rubbish_and_a_board_without_one() {
        let mut out = [0u8; 64];
        // Rommel met toevallig de magic maar een andere ring, of zonder
        // generatie: geen console, wel geconsumeerd.
        for (ring, generation) in [(BOX_RING / 2, 1), (BOX_RING, 0)] {
            let mut mem = SparseMem::default();
            let base = PLAN.black_box_pa;
            mem.write64(base, BOX_MAGIC);
            mem.write64(base + BOX_GEN_OFF, generation);
            mem.write64(base + BOX_POS_OFF, 40);
            mem.write64(base + BOX_RING_OFF, ring);
            assert_eq!(box_take(&mut mem, &PLAN, &mut out), None);
            assert_eq!(mem.read64(base), 0, "rubbish not consumed");
        }
        // Geen plek: alles is een no-op.
        let none = FlipPlan {
            black_box_pa: 0,
            ..PLAN
        };
        let mut mem = SparseMem::default();
        box_open(&mut mem, &none, 2);
        box_write(&mut mem, &none, b"nothing");
        assert!(mem.0.is_empty());
        assert_eq!(box_take(&mut mem, &none, &mut out), None);
    }

    /// Een minimale ELF64 met één PT_LOAD: `code` op `paddr`, `memsz` groot.
    fn mini_elf(entry: u64, paddr: u64, code: &[u8], memsz: u64) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..4].copy_from_slice(b"\x7fELF");
        e[4] = 2;
        e[5] = 1;
        e[6] = 1;
        e[16..18].copy_from_slice(&2u16.to_le_bytes());
        e[18..20].copy_from_slice(&183u16.to_le_bytes());
        e[20..24].copy_from_slice(&1u32.to_le_bytes());
        e[24..32].copy_from_slice(&entry.to_le_bytes());
        e[32..40].copy_from_slice(&64u64.to_le_bytes());
        e[52..54].copy_from_slice(&64u16.to_le_bytes());
        e[54..56].copy_from_slice(&56u16.to_le_bytes());
        e[56..58].copy_from_slice(&1u16.to_le_bytes());
        let ph = 64;
        e[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
        e[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes());
        e[ph + 8..ph + 16].copy_from_slice(&128u64.to_le_bytes());
        e[ph + 16..ph + 24].copy_from_slice(&paddr.to_le_bytes());
        e[ph + 24..ph + 32].copy_from_slice(&paddr.to_le_bytes());
        e[ph + 32..ph + 40].copy_from_slice(&(code.len() as u64).to_le_bytes());
        e[ph + 40..ph + 48].copy_from_slice(&memsz.to_le_bytes());
        e[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes());
        e.extend_from_slice(code);
        e
    }

    /// Een bundel zoals image/flip-bundle.sh hem schrijft.
    fn mini_bundle(
        elf: &[u8],
        abi: u32,
        link: u64,
        flat: u64,
        entry: u64,
        relocs: &[u32],
    ) -> Vec<u8> {
        let mut b = elf.to_vec();
        while !b.len().is_multiple_of(8) {
            b.push(0);
        }
        let head = b.len() as u64;
        b.extend_from_slice(&RELOC_MAGIC.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&abi.to_le_bytes());
        for v in [elf.len() as u64, link, flat, entry, relocs.len() as u64] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for r in relocs {
            b.extend_from_slice(&r.to_le_bytes());
        }
        while !b.len().is_multiple_of(8) {
            b.push(0);
        }
        b.extend_from_slice(&head.to_le_bytes());
        b.extend_from_slice(&RELOC_MAGIC.to_le_bytes());
        b
    }

    /// Een bundel van versie 2: met de som van de switch-code.
    fn mini_bundle2(elf: &[u8], link: u64, flat: u64, sum: u64, relocs: &[u32]) -> Vec<u8> {
        let mut b = elf.to_vec();
        while !b.len().is_multiple_of(8) {
            b.push(0);
        }
        let head = b.len() as u64;
        b.extend_from_slice(&RELOC_MAGIC.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&FLIP_ABI.to_le_bytes());
        for v in [elf.len() as u64, link, flat, link, relocs.len() as u64, sum] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for r in relocs {
            b.extend_from_slice(&r.to_le_bytes());
        }
        while !b.len().is_multiple_of(8) {
            b.push(0);
        }
        b.extend_from_slice(&head.to_le_bytes());
        b.extend_from_slice(&RELOC_MAGIC.to_le_bytes());
        b
    }

    #[test]
    fn a_version_2_bundle_carries_its_switch_code_sum() {
        let link = 0x6020_0000u64;
        let elf = mini_elf(link, link, &[0u8; 64], 0x2000);
        let b = mini_bundle2(&elf, link, 0x2000, 0xfeed_f00d, &[8, 16]);
        let bun = Bundle::parse(&b).unwrap();
        assert_eq!(bun.switch_sum, Some(0xfeed_f00d));
        assert_eq!(bun.relocs().collect::<Vec<_>>(), [8, 16]);
        let v1 = mini_bundle(&elf, FLIP_ABI, link, 0x2000, link, &[8]);
        assert_eq!(Bundle::parse(&v1).unwrap().switch_sum, None);
        let mut v3 = b.clone();
        let head = elf.len().next_multiple_of(8);
        v3[head + 8] = 3;
        assert!(Bundle::parse(&v3).is_err(), "an unknown tail version");
    }

    #[test]
    fn bundle_parses_flattens_and_lists_relocations() {
        let link = 0x6020_0000u64;
        let mut code = vec![0u8; 64];
        code[8..16].copy_from_slice(&(link + 0x20).to_le_bytes());
        let elf = mini_elf(link, link, &code, 0x2000);
        let b = mini_bundle(&elf, FLIP_ABI, link, 0x2000, link, &[8]);
        let bun = Bundle::parse(&b).unwrap();
        assert_eq!(
            (bun.flip_abi, bun.link_load, bun.flat_size, bun.entry),
            (FLIP_ABI, link, 0x2000, link)
        );
        assert_eq!(bun.relocs().collect::<Vec<_>>(), [8]);
        let mut mem = SparseMem::default();
        mem.write64(0x10_0000 + 0x1ff8, 0xdead); // rommel in de BSS
        assert_eq!(flatten(&bun, &mut mem, 0x10_0000).unwrap(), 1);
        assert_eq!(mem.read64(0x10_0008), link + 0x20);
        assert_eq!(mem.read64(0x10_1ff8), 0, "BSS not cleared");
    }

    #[test]
    fn bundle_refuses_what_it_cannot_jump_into() {
        let link = 0x4020_0000u64;
        let elf = mini_elf(link, link, &[0u8; 64], 0x1000);
        let good = mini_bundle(&elf, FLIP_ABI, link, 0x1000, link, &[0, 8]);
        assert!(Bundle::parse(&good).is_ok());
        // Een kale ELF, een afgekapte stroom, een omgevallen staart.
        assert!(Bundle::parse(&elf).is_err());
        assert!(Bundle::parse(&good[..good.len() - 1]).is_err());
        let bad: [(u64, u64, &[u32]); 5] = [
            (0x1000, link + 0x1000, &[]),   // entry buiten het beeld
            (0x1000, link + 2, &[]),        // entry niet uitgelijnd
            (0x1000, link, &[4]),           // relocatie niet uitgelijnd
            (0x1000, link, &[0x1000 - 4]),  // relocatie over de rand
            (MAX_FLAT + 0x1000, link, &[]), // een beeld dat geen kern is
        ];
        for (flat, entry, relocs) in bad {
            let b = mini_bundle(&elf, FLIP_ABI, link, flat, entry, relocs);
            assert!(
                Bundle::parse(&b).is_err(),
                "accepted {flat:#x} {entry:#x} {relocs:?}"
            );
        }
        // Een segment buiten het gedeclareerde beeld is een weigering bij het
        // neerleggen, niet een schrijf over de buren.
        let b = mini_bundle(&elf, FLIP_ABI, link, 0x800, link, &[]);
        let bun = Bundle::parse(&b).unwrap();
        assert!(flatten(&bun, &mut SparseMem::default(), 0x10_0000).is_err());
        // Twee entries die het oneens zijn: niet van één build.
        let b = mini_bundle(&elf, FLIP_ABI, link, 0x1000, link + 8, &[]);
        let bun = Bundle::parse(&b).unwrap();
        assert!(flatten(&bun, &mut SparseMem::default(), 0x10_0000).is_err());
    }
}
