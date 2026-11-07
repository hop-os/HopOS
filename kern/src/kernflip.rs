//! De kern-flip: de overdracht (het handoff-blob) en de adoptie door de
//! nieuwe kern ([`adopted`]). Wat een geflipte kern als laatste zei, staat in
//! de zwarte doos: de console-ring zelf ([`crate::conport`]), die een reset
//! overleeft.
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
use crate::cageheap::Chunk;
use crate::slots::{Mount, SlotState, try_push};
use crate::{Error, Result};
use abi::layout::{FACTS_FIRMWARE, FACTS_HANDOFF};
use alloc::vec::Vec;

/// "HOPHAND1" little-endian.
pub const HAND_MAGIC: u64 = 0x3144_4E41_4850_4F48;
/// De versie van het blob: 8 sinds 01-11 (per slot zijn kooi-blok en zijn
/// VMID, en de chunks van het kooi-geheugen: er is geen maximum aantal
/// slots, dus de kooi van een slot ligt niet meer vast in het plan). 7 had
/// de lease erin, de conntrack en de koude vlag eruit.
pub const HAND_VERSION: u64 = 8;
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
const SLOT_HEAD: usize = 96;

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
    /// Het kooi-geheugen van het hok (`kern::cageheap`): de chunks en hun
    /// bezetting, met de kooi-blokken van de bewoners en de bewonerslijsten
    /// van de cores erin.
    pub cage: Vec<Chunk>,
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
            s.cage_block,
            u64::from(s.vmid),
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
    put64(&mut b, h.cage.len() as u64)?;
    for c in &h.cage {
        put64(&mut b, c.base)?;
        put64(&mut b, u64::from(c.used))?;
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
        s.cage_block = r.u64()?;
        s.vmid = u32::try_from(r.u64()?).map_err(|_| Error::Corrupt { at: r.pos })?;
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
    let n = r.u64()?;
    if n.saturating_mul(16) > r.left() {
        return Err(Error::Corrupt { at: r.pos });
    }
    for _ in 0..n {
        let base = r.u64()?;
        let used = u32::try_from(r.u64()?).map_err(|_| Error::Corrupt { at: r.pos })?;
        try_push(&mut h.cage, Chunk { base, used })?;
    }
    Ok(h)
}

/// Waar de flip zijn woorden houdt, uit `layout`.
#[derive(Copy, Clone, Debug)]
pub struct FlipPlan {
    /// De feitenpagina (`abi::layout::facts_pa`): x0 van elke sprong.
    pub facts_pa: u64,
    /// De plek van het blob (`abi::layout::flip_handoff_pa`): een handoff
    /// die ergens anders wijst, is een kapotte.
    pub handoff_pa: u64,
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

/// De feitenpagina bij de ingang, vóór `discover` (O6, `docs/flip.md`).
/// Is `x0` de pagina zelf, dan kwam deze kern uit een sprong en draagt de
/// pagina wat kern A erin liet. Anders gaf de firmware x0 (de DTB, de
/// boot_args, op UEFI 0) en wordt de pagina opnieuw gebouwd, met handoff 0:
/// een oude handoff (een sprong die niet landde, een reset na een landing)
/// gaat zo vanzelf weg, en een firmware-boot adopteert nooit, wat er ook in
/// het geheugen ligt (gezien 04-10 op de O6N: de stickkern adopteerde de
/// overdracht van een flip die al geland was). Zo doet Linux het ook: de
/// kexec'ende kern geeft de kdump-kern `elfcorehdr=` mee, een herstart via
/// de firmware krijgt dat niet.
///
/// Geeft wat de firmware bij stroom aan gaf, en de handoff of 0.
pub fn facts(mem: &mut impl PhysMem, plan: &FlipPlan, x0: u64) -> (u64, u64) {
    let p = plan.facts_pa;
    if x0 != p {
        mem.write64(p + FACTS_HANDOFF, 0);
        mem.write64(p + FACTS_FIRMWARE, x0);
    }
    (
        mem.read64(p + FACTS_FIRMWARE),
        mem.read64(p + FACTS_HANDOFF),
    )
}

/// Kern A, vlak vóór de sprong: de handoff in de feitenpagina, en de pagina
/// naar DRAM (de ingang van Apple leest hem met de MMU uit).
pub fn hand_over(mem: &mut impl PhysMem, plan: &FlipPlan, handoff: u64) {
    mem.write64(plan.facts_pa + FACTS_HANDOFF, handoff);
    mem.clean_inv(plan.facts_pa, 16);
}

/// Leest het blob van de handoff uit [`facts`]; 0 is een koude boot.
///
/// Een handoff die niet op de plek van het blob wijst, of een blob dat niet
/// decodeert, is GEEN koude boot: er is echt geflipt en er kunnen bewoners
/// leven, en een koude boot veegt juist hun regio. De fout gaat naar de
/// aanroeper, die blijft staan tot de watchdog komt
/// (`HOPOS_FLIP_BLOB_BAD`).
pub fn adopted(mem: &mut impl PhysMem, plan: &FlipPlan, handoff: u64) -> Result<Option<Handoff>> {
    if handoff == 0 {
        return Ok(None);
    }
    // De handoff wijst exact op de plek van het blob: een eigenschap van de
    // constructie, en de hele klasse "lees op een adres dat een ander daar
    // neerlegde" is weg.
    if handoff != plan.handoff_pa {
        return Err(Error::Range {
            base: handoff,
            size: HANDOFF_TAIL as u64,
        });
    }
    let mut b = Vec::new();
    b.try_reserve_exact(HANDOFF_TAIL)
        .map_err(|_| Error::OutOfMemory {
            bytes: HANDOFF_TAIL,
        })?;
    b.resize(HANDOFF_TAIL, 0);
    mem.copy_out(&mut b, handoff);
    decode(&b).map(Some)
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
/// dat hij niet leest. 5 (01-11, v3.2.0): de ene ingang met de feitenpagina
/// in x0 (O6; een kern van ABI 4 springt met x3 = `HOPFLIPE` en de DTB in x0,
/// en een kern van de nieuwe vorm zou dat als een firmware-boot lezen, over
/// levende bewoners heen), en het blob van versie 8 met de indeling van de
/// kooi zonder maximum aantal slots (het kooi-blok per slot uit de pool, de
/// bewonerslijst als array van ctx-PA's, de VMID in het ctx-blok), die de
/// switch-code van een kern van ABI 4 niet leest.
pub const FLIP_ABI: u32 = 5;
/// "HOPRELO1" little-endian: hetzelfde woord als `mkkernel -elfreloc`.
pub const RELOC_MAGIC: u64 = 0x314F_4C45_5250_4F48;
/// De kop van de staart: magic, versie, flip-ABI, ELF-maat, linkbasis,
/// platte maat, entry, aantal relocaties en de som van de switch-code
/// ([`Bundle::switch_sum`]).
pub const BUNDLE_HEAD: usize = 64;
/// De versie van de staart die `image/flip-bundle.sh` schrijft. Versie 1
/// (zonder de som, 56 bytes) kwam van 3.0.x en valt al op [`FLIP_ABI`];
/// sinds 07-11 is ze een onbekende versie (F3).
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
    /// symbolen van de koude link rekende.
    ///
    /// Waarom in de bundel: de nieuwe kern adopteert de zittende kopie
    /// alleen bij een gelijke som (`cpu::el2::adopt`), en zonder deze
    /// toets merkt pas de gelande kern een verschil, twee minuten later,
    /// als de guard koud herstart.
    pub switch_sum: u64,
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
        if version != BUNDLE_VERSION {
            return Err(Error::Version {
                have: u64::from(version),
                want: u64::from(BUNDLE_VERSION),
            });
        }
        let elf_size = le64(b, head + 16)?;
        let count = le64(b, head + 48)?;
        let bundle = Bundle {
            flip_abi: le32(b, head + 12)?,
            link_load: le64(b, head + 24)?,
            flat_size: le64(b, head + 32)?,
            entry: le64(b, head + 40)?,
            switch_sum: le64(b, head + 56)?,
            elf: b
                .get(..usize::try_from(elf_size).unwrap_or(usize::MAX))
                .filter(|_| elf_size != 0 && elf_size <= head as u64)
                .ok_or(Error::Corrupt { at: head + 16 })?,
            relocs: {
                let start = head + BUNDLE_HEAD;
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
                    at: head + BUNDLE_HEAD + i * 4,
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
            cage: vec![
                Chunk {
                    base: 0xA000_0000,
                    used: 0b11,
                },
                Chunk {
                    base: 0xA020_0000,
                    used: 1 << 31,
                },
            ],
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
        facts_pa: 0x1000,
        handoff_pa: 0x10_0000,
    };

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
    /// dat doet: het blob en de handoff in de feitenpagina, naar DRAM.
    fn jump(mem: &mut impl PhysMem, generation: u64) -> Handoff {
        let h = Handoff {
            generation,
            slots: vec![st(2, 0x9000_0000, 32, 2)],
            ..Handoff::default()
        };
        let b = encode(&h, HANDOFF_TAIL).unwrap();
        mem.copy_in(PLAN.handoff_pa, &b);
        mem.clean_inv(PLAN.handoff_pa, HANDOFF_TAIL as u64);
        hand_over(mem, &PLAN, PLAN.handoff_pa);
        h
    }

    #[test]
    fn a_jump_lands_once_and_a_firmware_boot_never_adopts() {
        // Stroom aan: de firmware geeft de DTB, de pagina komt er leeg.
        let mut mem = CachedMem::default();
        assert_eq!(facts(&mut mem, &PLAN, 0x4000_0000), (0x4000_0000, 0));
        assert_eq!(adopted(&mut mem, &PLAN, 0), Ok(None));
        // De sprong: x0 is de pagina, de nieuwe kern vindt de DTB, de
        // handoff en het blob.
        let h = jump(&mut mem, 4);
        mem.reset();
        let (fw, handoff) = facts(&mut mem, &PLAN, PLAN.facts_pa);
        assert_eq!((fw, handoff), (0x4000_0000, PLAN.handoff_pa));
        assert_eq!(adopted(&mut mem, &PLAN, handoff), Ok(Some(h)));
        // De O6N (04-10): de kern leeft een minuut en de watchdog reset hem.
        // De firmware geeft geen pagina in x0: koud, wat er ook ligt.
        mem.reset();
        assert_eq!(facts(&mut mem, &PLAN, 0x4000_0000), (0x4000_0000, 0));
        // Ook een sprong die niet landde (het blob en de handoff liggen er
        // nog): de firmware-boot erna is koud.
        jump(&mut mem, 5);
        assert_eq!(facts(&mut mem, &PLAN, 0), (0, 0));
    }

    #[test]
    fn adopted_refuses_foreign_pointer_and_bad_blob() {
        let mut mem = SparseMem::default();
        assert!(
            adopted(&mut mem, &PLAN, 0x20_0000).is_err(),
            "read a blob somebody else placed"
        );
        assert!(
            adopted(&mut mem, &PLAN, PLAN.handoff_pa).is_err(),
            "garbage blob became a cold boot"
        );
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

    /// De som van de switch-code in de bundels van deze toetsen.
    const SUM: u64 = 0xfeed_f00d;

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
        b.extend_from_slice(&BUNDLE_VERSION.to_le_bytes());
        b.extend_from_slice(&abi.to_le_bytes());
        for v in [
            elf.len() as u64,
            link,
            flat,
            entry,
            relocs.len() as u64,
            SUM,
        ] {
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
    fn a_bundle_carries_its_switch_code_sum_in_one_version() {
        let link = 0x6020_0000u64;
        let elf = mini_elf(link, link, &[0u8; 64], 0x2000);
        let b = mini_bundle(&elf, FLIP_ABI, link, 0x2000, link, &[8, 16]);
        let bun = Bundle::parse(&b).unwrap();
        assert_eq!(bun.switch_sum, SUM);
        assert_eq!(bun.relocs().collect::<Vec<_>>(), [8, 16]);
        // Versie 1 (3.0.x, zonder som) en een onbekende versie: nee.
        let head = elf.len().next_multiple_of(8);
        for v in [1, 3] {
            let mut old = b.clone();
            old[head + 8] = v;
            assert!(Bundle::parse(&old).is_err(), "tail version {v}");
        }
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
