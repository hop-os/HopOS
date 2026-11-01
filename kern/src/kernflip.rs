//! De kern-flip: de overdracht (het handoff-blob), de adoptie door de nieuwe
//! kern ([`adopted`]) en de vluchtrecorder met zijn archief. De zwarte doos
//! van de console is de console-ring zelf ([`crate::conport`]).
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
/// De versie van het blob.
pub const HAND_VERSION: u64 = 6;
/// De ruimte boven de RAM-declaratie van de nieuwe kern voor het blob.
/// 256 KB: de volle conntrack (4096 flows van 24 bytes, ongeveer 96 KB) plus
/// de kop en de slot-records; 0,1% van een kernvenster, en daarvoor
/// overleeft elke verbinding door de switch een kernwissel.
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
const HAND_HEAD: usize = 128;
const SLOT_HEAD: usize = 80;

pub use abi::{FlowState, MAX_FLOWS, NatState};

/// Wat de vertrekkende kern achterliet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handoff {
    /// Het venster van de vorige kern.
    pub old_base: u64,
    /// De maat daarvan.
    pub old_size: u64,
    /// Het eigen venster, inclusief de handoff-staart.
    pub window: u64,
    /// De totale maat daarvan.
    pub total: u64,
    /// De hoeveelste flip deze boot is (1 = de eerste).
    pub generation: u64,
    /// De som van de bundel: "flip naar deze URL" in een boot-config mag
    /// geen eeuwige lus worden.
    pub bundle_sum: u64,
    /// De bewoners.
    pub slots: Vec<SlotState>,
    /// De conntrack: zonder deze tabel breekt elke verbinding door de
    /// masquerade bij een kernwissel, terwijl de app doorleeft.
    pub nat: NatState<Vec<FlowState>>,
    /// Een KOUDE flip: de vertrekkende kern stopte zijn bewoners en zette
    /// de app-cores uit, dus er valt niets te adopteren. De nieuwe kern
    /// boot koud (eigen switch-code, Hop koud uit de staging) en draagt
    /// alleen de generatie en de som verder (`hopos/src/flip.rs`).
    pub cold: bool,
    /// De wandklok van de vertrekkende kern: Unix-nanoseconden bij
    /// tellerstand 0 (`hopos/src/clock.rs`), zoals Hop hem via SNTP zette.
    /// De teller loopt door de sprong heen door, dus de offset blijft
    /// geldig; zonder dit veld begon elke nieuwe generatie weer op de vaste
    /// boot-klok (gezien 30-09 op de Pi 5: 29-09 00:00:40 op het glas). 0 =
    /// geen klok. Additief binnen versie 6, op het kopwoord na de vlaggen
    /// ([`WALL_OFF_OFF`]): een oudere kern schrijft daar nul.
    pub wall_off: u64,
}

/// De vlaggen van het blob, in het eerste vrije kopwoord ([`FLAGS_OFF`]).
/// Additief binnen versie 6 (29-09): een kern van vóór de vlag schrijft
/// daar nul, en dat is de warme flip. Geen nieuwe versie, want die zou een
/// warme flip vanaf alpha.9 laten vallen op `HOPOS_FLIP_BLOB_BAD`.
const FLAGS_OFF: usize = 72;
/// Vlag: een koude flip ([`Handoff::cold`]).
const FLAG_COLD: u64 = 1;
/// Het kopwoord met de wandklok ([`Handoff::wall_off`]), na de vlaggen.
const WALL_OFF_OFF: usize = FLAGS_OFF + 8;

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
        h.old_base,
        h.old_size,
        h.window,
        h.total,
        h.slots.len() as u64,
        h.generation,
        h.bundle_sum,
        if h.cold { FLAG_COLD } else { 0 },
        h.wall_off,
    ] {
        put64(&mut b, v)?;
    }
    put(&mut b, &[0; HAND_HEAD - WALL_OFF_OFF - 8])?;
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
    put64(&mut b, u64::from(h.nat.masq_next))?;
    // Het woord van de gateway-MAC: altijd "onbekend" (bit 56 nul). De
    // next-hops staan in de neighbour-tabel van de node-stack, en de nieuwe
    // kern vraagt ze opnieuw; het woord blijft voor de oudere kern.
    put64(&mut b, 0)?;
    put64(&mut b, h.nat.flows.len() as u64)?;
    for f in &h.nat.flows {
        put64(
            &mut b,
            u64::from(f.proto)
                | u64::from(f.slot) << 8
                | u64::from(f.fins) << 16
                | u64::from(f.slot_port) << 32
                | u64::from(f.dst_port) << 48,
        )?;
        put64(&mut b, u64::from(f.slot_ip) | u64::from(f.dst_ip) << 32)?;
        put64(&mut b, u64::from(f.node_port))?;
    }
    // De lengte van de agent-state: altijd nul. Hop bewaart zijn staat zelf
    // (`/hop/agent-state.json`); het woord blijft voor de oudere kern.
    put64(&mut b, 0)?;
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
    let mut h = Handoff {
        old_base: r.u64()?,
        old_size: r.u64()?,
        window: r.u64()?,
        total: r.u64()?,
        ..Handoff::default()
    };
    let n = bounded(r.u64()?, 1024, 48)?;
    h.generation = r.u64()?;
    h.bundle_sum = r.u64()?;
    // Een bit die deze kern niet kent, is een blob van een latere kern:
    // luid, niet stil als warm gelezen.
    let flags = r.u64()?;
    if flags & !FLAG_COLD != 0 {
        return Err(Error::Corrupt { at: FLAGS_OFF });
    }
    h.cold = flags & FLAG_COLD != 0;
    h.wall_off = r.u64()?;
    r.pos = HAND_HEAD;
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
    // Deze versie draagt altijd beide dienstblokken, ook leeg.
    h.nat.masq_next = r.u64()? as u16;
    r.u64()?; // de gateway-MAC van een oudere kern: niet overgenomen
    let nf = bounded(r.u64()?, MAX_FLOWS as u64, r.pos)?;
    if nf * 24 > r.left() {
        return Err(Error::Corrupt { at: r.pos });
    }
    for _ in 0..nf {
        let (w0, w1, w2) = (r.u64()?, r.u64()?, r.u64()?);
        try_push(
            &mut h.nat.flows,
            FlowState {
                proto: w0 as u8,
                slot: (w0 >> 8) as u8,
                fins: (w0 >> 16) as u8,
                slot_port: (w0 >> 32) as u16,
                dst_port: (w0 >> 48) as u16,
                slot_ip: w1 as u32,
                dst_ip: (w1 >> 32) as u32,
                node_port: w2 as u16,
            },
        )?;
    }
    // De agent-state van een oudere kern: altijd leeg, niet overgenomen.
    let na = r.u64()?;
    r.bytes(na)?;
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
            // hier meldt dan welke kern het was.
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
/// eigen stand verkeerd (06-09, het kostte een ronde).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Stage {
    /// Bundel opgehaald en geverifieerd.
    Fetched = 1,
    /// Het lifecycle-venster vast.
    WindowHeld,
    /// Bundel gevalideerd.
    BundleOk,
    /// Venster geleend.
    Borrowed,
    /// Vectoren geïnstalleerd.
    Vectors,
    /// Venster gewist.
    Scrubbed,
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
            WindowHeld,
            BundleOk,
            Borrowed,
            Vectors,
            Scrubbed,
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
            Stage::WindowHeld => "slot lifecycle window held",
            Stage::BundleOk => "bundle validated",
            Stage::Borrowed => "window borrowed from the pool",
            Stage::Vectors => "vectors installed",
            Stage::Scrubbed => "window scrubbed",
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
// De bundel: de kern-ELF plus zijn relocatietabel (image/flip-bundle.sh).
// ---------------------------------------------------------------------------

/// De versie van het flip-contract van v3: de staart-vorm, het handoff-blob
/// en de ingangsconditie samen. De Go-generatie sprak ABI 2 (een geleend
/// venster, `RamStart`/`RamSize`-patches); v3 plaatst de nieuwe kern op het
/// koude adres van de oude, dus een Go-bundel wordt hier geweigerd.
pub const FLIP_ABI: u32 = 3;
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

/// De eerste acht bytes van een som als getal: de `bundle_sum` in het
/// handoff-blob ("flip naar deze bundel" mag geen eeuwige lus worden).
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
            old_base: 0x4000_0000,
            old_size: 0x0F00_0000,
            window: 0xA0E0_0000,
            total: 0x0F20_0000,
            generation: 2,
            slots: vec![first, st(7, 0x9000_0000, 48, 3)],
            wall_off: 1_790_767_915_253_886_881,
            ..Handoff::default()
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
    fn a_blob_from_before_the_wall_clock_gives_no_clock() {
        // Een kern van vóór 30-09 schrijft nul op het kopwoord: dat is
        // "geen klok", en de rest van het blob blijft wat het was.
        let h = Handoff {
            generation: 3,
            slots: vec![st(1, 0xBC00_0000, 64, 1)],
            wall_off: 1_790_767_915_253_886_881,
            ..Handoff::default()
        };
        let mut b = encode(&h, HANDOFF_TAIL).unwrap();
        b[WALL_OFF_OFF..WALL_OFF_OFF + 8].fill(0);
        let d = decode(&b).unwrap();
        assert_eq!(d.wall_off, 0);
        assert_eq!(d.slots, h.slots);
        assert_eq!(d.generation, 3);
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
    fn a_cold_handoff_carries_its_flag_and_nothing_to_adopt() {
        let cold = Handoff {
            generation: 3,
            bundle_sum: 0xfeed,
            cold: true,
            ..Handoff::default()
        };
        let b = encode(&cold, HANDOFF_TAIL).unwrap();
        let back = decode(&b).unwrap();
        assert!(back.cold && back.slots.is_empty() && back.nat.flows.is_empty());
        assert_eq!(back, cold);
        // Een blob van vóór de vlag (nul op zijn plek) is warm.
        let warm = encode(&Handoff::default(), HANDOFF_TAIL).unwrap();
        assert_eq!(&warm[FLAGS_OFF..FLAGS_OFF + 8], &[0; 8]);
        assert!(!decode(&warm).unwrap().cold);
        // Een onbekende vlag is een blob van een latere kern: een fout.
        let mut later = b.clone();
        later[FLAGS_OFF] = 0x3;
        assert!(decode(&later).is_err());
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
            |b| b[48..56].copy_from_slice(&(1u64 << 40).to_le_bytes()),
            |b| b[48..56].copy_from_slice(&4u64.to_le_bytes()),
        ];
        for brk in breaks {
            let mut b = good.clone();
            brk(&mut b);
            assert!(decode(&b).is_err());
        }
        assert!(decode(&good[..32]).is_err());
    }

    #[test]
    fn handoff_nat_round_trip() {
        let h = Handoff {
            generation: 1,
            nat: NatState {
                masq_next: 20345,
                flows: vec![
                    FlowState {
                        proto: 6,
                        slot: 3,
                        fins: 2,
                        slot_port: 49152,
                        dst_port: 443,
                        node_port: 20001,
                        slot_ip: 0x0A64_0004,
                        dst_ip: 0x0808_0808,
                    },
                    FlowState {
                        proto: 17,
                        slot: 1,
                        fins: 0,
                        slot_port: 5353,
                        dst_port: 53,
                        node_port: 20002,
                        slot_ip: 0x0A64_0002,
                        dst_ip: 0x0A00_0203,
                    },
                ],
            },
            ..Handoff::default()
        };
        assert_eq!(decode(&encode(&h, HANDOFF_TAIL).unwrap()).unwrap(), h);
    }

    #[test]
    fn handoff_full_conntrack_fits() {
        let mut h = Handoff {
            generation: 1,
            ..Handoff::default()
        };
        for i in 0..MAX_FLOWS {
            h.nat.flows.push(FlowState {
                proto: 6,
                slot: (i % 64 + 1) as u8,
                slot_port: (30000 + i % 1000) as u16,
                dst_port: 443,
                node_port: (20000 + i % 9000) as u16,
                slot_ip: 0x0A64_0002,
                dst_ip: 0x0808_0808,
                fins: 0,
            });
        }
        for s in 1..=16 {
            let mut x = st(s, (s as u64) << 24, 64, s);
            x.job = b"some-job-name".to_vec();
            x.ports = vec![8080, 443];
            h.slots.push(x);
        }
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        assert_eq!(decode(&b).unwrap().nat.flows.len(), MAX_FLOWS);
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
    fn handoff_rejects_truncated_services() {
        let b = encode(&Handoff::default(), HANDOFF_TAIL).unwrap();
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
