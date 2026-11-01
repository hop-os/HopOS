//! Wat de stub vóór `ExitBootServices` over de machine leerde, als statics
//! die daarna alleen gelezen worden.
//!
//! Eén schrijver (de stub, op core 0, vóór `kmain`), daarna alleen lezers:
//! dat is de "gepubliceerde tabel" van het handboek, met atomics als
//! opslag omdat er geen `static mut` bestaat. `Relaxed` volstaat: de
//! schrijver en de lezers zijn dezelfde core, en de app-cores lezen dit
//! niet.
//!
//! De ACPI-kant: MADT (cores met MPIDR en efficiëntieklasse, GICD, de
//! GICR-reeks, de ITS), MCFG (de ECAM-vensters), SPCR (de console), GTDT
//! (de timer-PPI) en FADT (de PSCI-conduit). De tabellen liggen in
//! EfiACPIReclaimMemory; de stub leest ze met de MMU van de firmware.

use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed,
};
use dev::Pa;
use fw::acpi::{self, Phys, Tables};

/// Zoveel cores houden we bij: meer dan de Altra (128) en binnen wat het
/// plan draagt (`abi::layout::CORES_MAX`).
pub(crate) const MAX_CORES: usize = 129;
/// Zoveel ECAM-vensters (PCIe-segmenten): de Altra heeft er tot acht.
pub(crate) const MAX_ECAM: usize = 8;

/// De MPIDR-affiniteit per logische core; core 0 is de onze.
pub(crate) static CORE_MPIDR: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];
/// Het zaad uit het EFI_RNG_PROTOCOL (boot.rs, feature `efi-rng`):
/// 64 bytes in acht woorden; [`EFI_SEED_LEN`] 0 = geen.
pub(crate) static EFI_SEED: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// Hoeveel bytes van [`EFI_SEED`] gelden.
pub(crate) static EFI_SEED_LEN: AtomicUsize = AtomicUsize::new(0);
/// Kwam [`EFI_SEED`] van de vorige kern (een flip, `crate::flip`) en niet
/// van de firmware?
pub(crate) static EFI_SEED_CARRIED: AtomicBool = AtomicBool::new(false);
/// Het zaad voor de kern na de flip (`hopos/src/flip.rs`).
pub use crate::flip::carry_seed;
/// De efficiëntieklasse per logische core (MADT GICC offset 76).
pub(crate) static CORE_CLASS: [AtomicU8; MAX_CORES] = [const { AtomicU8::new(0) }; MAX_CORES];
/// Het aantal cores (enabled GICC's); 0 = geen MADT.
pub(crate) static CORES: AtomicUsize = AtomicUsize::new(0);
/// De GIC-distributor, en zijn versie uit de MADT.
pub(crate) static GICD: AtomicU64 = AtomicU64::new(0);
pub(crate) static GIC_VERSION: AtomicU8 = AtomicU8::new(0);
/// Het redistributor-frame van core 0 (uit zijn GICC of de GICR-reeks).
pub(crate) static GICR_BASE: AtomicU64 = AtomicU64::new(0);
/// De GICR-reeks uit de MADT: basis en lengte.
pub(crate) static GICR_RANGE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// De eerste ITS (0 = geen).
pub(crate) static ITS: AtomicU64 = AtomicU64::new(0);
/// De ECAM-vensters: basis, en segment<<16 | start<<8 | eind.
pub(crate) static ECAM: [[AtomicU64; 2]; MAX_ECAM] =
    [const { [const { AtomicU64::new(0) }, const { AtomicU64::new(0) }] }; MAX_ECAM];
/// Het aantal ECAM-vensters.
pub(crate) static ECAMS: AtomicUsize = AtomicUsize::new(0);
/// De SPCR-console: basis, type, registerstap. Basis 0 = geen.
pub(crate) static CONSOLE_BASE: AtomicU64 = AtomicU64::new(0);
pub(crate) static CONSOLE_TYPE: AtomicU8 = AtomicU8::new(0);
pub(crate) static CONSOLE_SHIFT: AtomicU8 = AtomicU8::new(0);
/// De PPI van de EL1-fysieke timer (GTDT offset 56; QEMU en SBSA: 30).
pub(crate) static TIMER_PPI: AtomicU32 = AtomicU32::new(30);
/// De PPI van de EL2-timer (CNTHP; GTDT offset 72, QEMU en SBSA: 26): de
/// deadline van de executor terwijl een bewoner de OS-core heeft.
pub(crate) static HYP_TIMER_PPI: AtomicU32 = AtomicU32::new(26);
/// PSCI via HVC (1) of SMC (0), uit de FADT; 2 = onbekend.
pub(crate) static PSCI_HVC: AtomicU8 = AtomicU8::new(2);
/// De OEM-ID van de XSDT (zes bytes, little-endian in een u64).
pub(crate) static OEM_ID: AtomicU64 = AtomicU64::new(0);

/// De RSDP uit de EFI-configuratietabel (0 = geen ACPI).
pub(crate) static RSDP: AtomicU64 = AtomicU64::new(0);
/// De firmware-kaart na de exit: adres, maat, stap.
pub(crate) static MAP: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Waar de firmware de image laadde en hoe groot hij is.
pub(crate) static IMAGE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Waar de firmware het image bij de koude boot laadde en hoe groot zijn
/// allocatie was (`SizeOfImage`, met de speling van hopos/efi.ld): het
/// venster waarin elke geflipte kern moet passen. De feitenpagina draagt
/// het over een flip heen. `None` zolang de stub het niet zette.
#[must_use]
pub fn image_window() -> Option<(u64, u64)> {
    let (base, size) = (
        IMAGE[0].load(core::sync::atomic::Ordering::Relaxed),
        IMAGE[1].load(core::sync::atomic::Ordering::Relaxed),
    );
    (size != 0).then_some((base, size))
}
/// `hopos.cfg` van de ESP: adres en lengte (0 = geen bestand).
pub(crate) static CFG: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Het gestagede app-image van de ESP: adres en lengte (0 = geen).
pub(crate) static STAGE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Het EL waarop de firmware ons aanriep.
pub(crate) static BOOT_EL: AtomicU8 = AtomicU8::new(0);
/// Het aantal tabellen van de identity map.
pub(crate) static MMU_TABLES: AtomicU64 = AtomicU64::new(0);

/// De lezer van firmware-geheugen voor `fw::acpi`: woordgewijs via `dev`
/// (ACPI-geheugen kan device-gemapt zijn; Go-les 13-07), na een
/// plausibiliteitstoets die `fw::acpi` zelf ook doet.
pub(crate) struct FwMem;

impl Phys for FwMem {
    fn read(&self, pa: u64, out: &mut [u8]) -> bool {
        let len = out.len() as u64;
        if pa < 0x1000 || pa.saturating_add(len) > 1 << 48 {
            return false;
        }
        dev::copy_out(out, Pa(pa));
        true
    }
}

/// Leest de ACPI-feiten uit de tabellen onder `rsdp` naar de statics.
/// `buf` is scratch voor één tabel tegelijk. `mpidr` is die van core 0.
pub(crate) fn discover(rsdp: u64, buf: &mut [u8], mpidr: u64) -> Result<Tables, acpi::Error> {
    let t = Tables::parse(&FwMem, rsdp)?;
    RSDP.store(rsdp, Relaxed);
    let mut oem = [0u8; 8];
    for (o, b) in oem.iter_mut().zip(t.oem_id().bytes().take(6)) {
        *o = b;
    }
    OEM_ID.store(u64::from_le_bytes(oem), Relaxed);
    let madt = t.load(&FwMem, b"APIC", buf).and_then(acpi::Madt::new)?;
    madt_facts(&madt, mpidr);
    if let Ok(mcfg) = t.load(&FwMem, b"MCFG", buf).and_then(acpi::mcfg) {
        let mut n = 0;
        for (e, slot) in mcfg.zip(ECAM.iter()) {
            slot[0].store(e.base, Relaxed);
            let tag =
                (u64::from(e.segment) << 16) | (u64::from(e.start_bus) << 8) | u64::from(e.end_bus);
            slot[1].store(tag, Relaxed);
            n += 1;
        }
        ECAMS.store(n, Relaxed);
    }
    if let Ok(c) = t.load(&FwMem, b"SPCR", buf).and_then(acpi::spcr) {
        // Alleen memory-mapped (GAS space 0): een I/O-poort bestaat op ARM
        // niet.
        if c.space == 0 {
            CONSOLE_BASE.store(c.base, Relaxed);
            CONSOLE_TYPE.store(c.if_type, Relaxed);
            CONSOLE_SHIFT.store(c.shift, Relaxed);
        }
    }
    if let Ok(g) = t.load(&FwMem, b"GTDT", buf) {
        // Non-Secure EL1 Timer GSIV op offset 56: de CNTP die de kern op EL2
        // gebruikt (zoals op virt: PPI 30).
        let ppi = |off: usize| {
            g.get(off..off + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .filter(|p| (16..32).contains(p))
        };
        if let Some(p) = ppi(56) {
            TIMER_PPI.store(p, Relaxed);
        }
        // EL2 Timer GSIV op offset 72 (ACPI 6.x).
        if let Some(p) = ppi(72) {
            HYP_TIMER_PPI.store(p, Relaxed);
        }
    }
    if let Ok(p) = t.load(&FwMem, b"FACP", buf).and_then(acpi::fadt_psci) {
        PSCI_HVC.store(u8::from(p.hvc), Relaxed);
    }
    Ok(t)
}

/// De MADT naar de statics: core 0 is de onze, de rest volgt in
/// MADT-volgorde (alleen enabled GICC's).
fn madt_facts(madt: &acpi::Madt<'_>, mpidr: u64) {
    const AFF: u64 = 0xff_00ff_ffff;
    let mine = mpidr & AFF;
    let mut n = 1;
    for c in madt.cpus().filter(|c| c.enabled) {
        let aff = c.mpidr & AFF;
        let idx = if aff == mine { 0 } else { n };
        if let (Some(m), Some(k)) = (CORE_MPIDR.get(idx), CORE_CLASS.get(idx)) {
            m.store(aff, Relaxed);
            k.store(c.eff_class, Relaxed);
            if idx == 0 {
                GICR_BASE.store(c.gicr, Relaxed);
            }
        }
        if idx != 0 {
            n += 1;
        }
    }
    CORE_MPIDR[0].store(mine, Relaxed);
    CORES.store(n.min(MAX_CORES), Relaxed);
    if let Some((base, v)) = madt.gicd() {
        GICD.store(base, Relaxed);
        GIC_VERSION.store(v, Relaxed);
    }
    if let Some((base, len)) = madt.gicr_ranges().next() {
        GICR_RANGE[0].store(base, Relaxed);
        GICR_RANGE[1].store(u64::from(len), Relaxed);
    }
    if let Some(its) = madt.its().next() {
        ITS.store(its, Relaxed);
    }
}

/// De ECAM-vensters: `(basis, segment, eerste bus, laatste bus)`.
pub(crate) fn ecams() -> impl Iterator<Item = (u64, u16, u8, u8)> {
    ECAM.iter().take(ECAMS.load(Relaxed)).map(|e| {
        let tag = e[1].load(Relaxed);
        (
            e[0].load(Relaxed),
            (tag >> 16) as u16,
            (tag >> 8) as u8,
            tag as u8,
        )
    })
}

/// De OEM-ID als tekst.
pub(crate) fn oem_id() -> [u8; 6] {
    let b = OEM_ID.load(Relaxed).to_le_bytes();
    [b[0], b[1], b[2], b[3], b[4], b[5]]
}

/// De tabel op `pa` als slice, na de toets van lengte en checksum; `None`
/// als hij krom is. Na de sprong is ACPI-geheugen (Reclaim, NVS) RAM in
/// onze map (Normal WB), dus een slice mag.
fn table_at(pa: u64) -> Option<&'static [u8]> {
    let mut head = [0u8; acpi::HEADER_LEN];
    if !FwMem.read(pa, &mut head) {
        return None;
    }
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if !(acpi::HEADER_LEN..=acpi::TABLE_MAX).contains(&len) || !pa.is_multiple_of(4) {
        return None;
    }
    // SAFETY: `[pa, pa + len)` is een ACPI-tabel die de firmware in
    // EfiACPIReclaim- of NVS-geheugen legde: RAM dat de identity map als
    // Normal mapt en dat niemand meer beschrijft (de pool neemt alleen
    // BootServices en Conventional).
    let b = unsafe { core::slice::from_raw_parts(pa as usize as *const u8, len) };
    (b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0).then_some(b)
}

/// Alle tabellen met signature `sig`, in XSDT-volgorde; `DSDT` via de
/// FADT (hij staat niet in de XSDT). Na de sprong aan te roepen.
pub(crate) fn tables(sig: acpi::Sig) -> impl Iterator<Item = &'static [u8]> {
    let t = Tables::parse(&FwMem, RSDP.load(Relaxed)).ok();
    let dsdt = if sig == *b"DSDT" {
        t.as_ref()
            .and_then(|t| t.find(b"FACP"))
            .and_then(table_at)
            .and_then(|f| {
                let x = f
                    .get(140..148)
                    .map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]));
                let d = f
                    .get(40..44)
                    .map(|b| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                x.filter(|&x| x != 0).or(d)
            })
            .and_then(table_at)
    } else {
        None
    };
    let mut pas = [0u64; acpi::MAX_TABLES];
    let mut n = 0;
    if let Some(t) = &t {
        for pa in t.all(&sig) {
            if let Some(slot) = pas.get_mut(n) {
                *slot = pa;
                n += 1;
            }
        }
    }
    dsdt.into_iter().chain(
        pas.into_iter()
            .take(n)
            .filter_map(table_at)
            .filter(move |b| b.get(..4) == Some(&sig[..])),
    )
}
