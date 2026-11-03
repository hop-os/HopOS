//! Het slot-plan van het UEFI-board: waar de kern zijn kooien,
//! park-mailboxen en partities legt, en waar de stub een app-image van de
//! ESP neerzette.
//!
//! Dezelfde vorm als `board_qemuvirt::slots` (en de Pi's), zodat de
//! slot-lijm van de binary (`hopos/src/slots.rs`, `cage.rs`, `flip.rs`)
//! hetzelfde pad loopt. De vaste adressen liggen in het kernvenster
//! ([`crate::WINDOW_PA`]); de pool komt uit de EFI-memory-map na de exit:
//! al het vrije DRAM (BootServices en Conventional), op 2 MB afgerond.
//!
//! De staging: de stub leest `hopos-stage.elf` van de ESP-root naar
//! [`STAGE_PA`] en schrijft de maat in [`STAGE_HDR_PA`], zoals QEMU's
//! `-device loader` het op virt doet. Een gestaged image is Hop, de
//! bevoorrechte bewoner: de kern plaatst alleen Hop (O7).

use crate::memmap::{self, Map};
use crate::{ADMIN, LOADER, facts};
use abi::Region;
use abi::layout::{POOL_MAX, Plan, PlanSpec, Pool, pool_of};
use board::stage;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// De control-pages van de eigen cores van de kern: het begin van het
/// kooi-venster.
pub const NODE_CTRL_PA: u64 = ADMIN.base.0;
/// De kooi-regio: 1 MB verder, blok 0 en de ctx-blokken van de cores
/// (`abi::layout::cage_len`). Wat per slot is, komt uit de pool.
pub const CAGE_PA: u64 = ADMIN.base.0 + 0x10_0000;
/// Het Device-venster van de kooi.
pub const DEVICE_WINDOW: Region = Region::new(ADMIN.base.0, ADMIN.size);
/// De boot-scratch: het begin van de loader-regio.
pub const BOOT_SCRATCH_PA: u64 = LOADER.base.0;
/// Het woord met de maat van het gestagede image (0 = niets).
pub const STAGE_HDR_PA: u64 = LOADER.base.0 + 0x10_0000;
/// Waar het image staat.
pub const STAGE_PA: u64 = LOADER.base.0 + 0x20_0000;
/// Het grootste gestagede image.
pub const STAGE_MAX: u64 = LOADER.base.0 + LOADER.size - STAGE_PA;

const _: () = {
    assert!(BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN <= STAGE_HDR_PA);
    assert!(STAGE_HDR_PA + 8 <= STAGE_PA);
    // Node-control (een pagina per core) vóór de kooi, de kooi binnen het
    // venster, ook bij het grootste aantal cores dat het plan draagt.
    let cores = abi::layout::CORES_MAX - 1;
    assert!(NODE_CTRL_PA + abi::layout::node_ctrl_len(cores) <= CAGE_PA);
    assert!(CAGE_PA + abi::layout::cage_len(cores) <= ADMIN.base.0 + ADMIN.size);
};

/// De memory map zoals hij bij de exit was.
pub(crate) fn final_map() -> Map {
    Map {
        pa: facts::MAP[0].load(Relaxed),
        size: facts::MAP[1].load(Relaxed),
        stride: facts::MAP[2].load(Relaxed),
    }
}

/// Heeft de pool zijn verlies al gemeld? Het plan wordt vaker gemaakt dan
/// één keer, de regel komt één keer.
static DROPPED_SAID: AtomicBool = AtomicBool::new(false);

/// De pool: het vrije DRAM uit de kaart van de exit. Wat er niet in past,
/// is één luide regel (Go 14-07: een stille pool gaf 12 van 127 taken een
/// partitie).
fn pool() -> abi::Result<Pool> {
    let (free, lost) = memmap::free_regions::<POOL_MAX>(&final_map());
    if lost.count != 0 && !DROPPED_SAID.swap(true, Relaxed) {
        cpu::println!(
            "uefi: memory map: {} free spans ({} MB) left out of the pool HOPOS_UEFI_MAP_DROPPED",
            lost.count,
            lost.bytes >> 20
        );
    }
    pool_of(free.iter().map(|r| Region::new(r.base.0, r.size)))
}

/// Het laagste RAM-adres uit de kaart (het meetpunt van `required_ram`).
fn ram_base() -> u64 {
    final_map()
        .iter()
        .filter(memmap::Desc::is_ram)
        .map(|d| d.base)
        .min()
        .unwrap_or(0)
}

/// Het PA-plan voor een node met `cores` cores (de kern-core meegeteld):
/// elke andere core is een app-core met één kooi, en één kooi extra voor
/// de bewoner die de OS-core `os_core` deelt (Hop, PORT.md beslissing 2),
/// zoals op virt.
pub fn plan(cores: usize, os_core: usize) -> abi::Result<Plan> {
    let app_cores = cores.saturating_sub(1).max(1);
    Plan::new(PlanSpec {
        node_ctrl_pa: NODE_CTRL_PA,
        cage_pa: CAGE_PA,
        device_window: DEVICE_WINDOW,
        boot_scratch_pa: BOOT_SCRATCH_PA,
        net_dma_pa: crate::NET_DMA.base.0,
        pool: pool()?,
        ram_base: ram_base(),
        app_cores,
        os_core,
        ..PlanSpec::default()
    })
}

/// Het MPIDR-target van logische core `core`: uit de MADT (core 0 is de
/// boot-core; op servers is MPIDR geen slotnummer, de Altra nummert aff1
/// en aff2).
#[must_use]
pub fn mpidr(core: usize) -> u64 {
    facts::CORE_MPIDR.get(core).map_or(0, |m| m.load(Relaxed))
}

/// De logische core met affiniteit `mpidr` (0 als hij onbekend is).
#[must_use]
pub fn core_of(mpidr: u64) -> usize {
    let aff = mpidr & 0xff_00ff_ffff;
    let n = facts::CORES.load(Relaxed).min(facts::MAX_CORES);
    (0..n).find(|&c| self::mpidr(c) == aff).unwrap_or(0)
}

/// Het image dat de stub van de ESP las, of `None` (`board::stage`).
#[must_use]
pub fn staged_image() -> Option<&'static [u8]> {
    stage::staged_at(STAGE_HDR_PA, STAGE_PA, STAGE_MAX)
}

/// Mapt `[pa, pa + size)` als Device (zie [`crate::map_device`]).
pub(crate) fn map_device(pa: u64, size: u64) -> bool {
    if pa.saturating_add(size) <= crate::mmu::DEVICE_SPAN {
        return true;
    }
    let lo = pa & !((1 << 30) - 1);
    let hi = pa.saturating_add(size).next_multiple_of(1 << 30);
    map(lo, hi.saturating_sub(lo), cpu::boot::ATTR_DEVICE)
}

/// Zet `[pa, pa + size)` op `attr` (`board::Board::remap`: het
/// kooi-geheugen, het glas, de arena van de codec) in de levende map met de
/// tabellen uit `crate::TABLES`. Niet via `cpu::memattr`: die neemt een
/// tabel voor een gesplitst blok uit de heap van de kern, en die heap
/// overschrijft de volgende kern bij een flip terwijl de map blijft (QEMU
/// UEFI 01-11: de 1 GB met de kern zelf werd gesplitst, en kern B hing na
/// de landing).
pub(crate) fn remap(pa: u64, size: u64, attr: cpu::memattr::Attr) -> bool {
    let idx = match attr {
        cpu::memattr::Attr::NormalWb => cpu::boot::ATTR_NORMAL,
        cpu::memattr::Attr::NormalNc => cpu::boot::ATTR_NORMAL_NC,
        cpu::memattr::Attr::Device => cpu::boot::ATTR_DEVICE,
    };
    // Op hele pagina's, zoals `gop::map`: de firmware deelt per pagina uit.
    let lo = pa & !0xFFF;
    let hi = pa.saturating_add(size).next_multiple_of(0x1000);
    map(lo, hi - lo, idx)
}

/// Mapt `[lo, lo + size)` met MAIR-index `attr` in de levende map.
fn map(lo: u64, size: u64, attr: u64) -> bool {
    let used = facts::MMU_TABLES.load(Relaxed);
    // SAFETY: de tabelpool is `crate::TABLES` en de eerste `used` tabellen
    // zijn de levende map (de stub bouwde hem daar); alleen de kern-core
    // schrijft hem, en alleen via deze functie.
    let Ok(mut m) =
        (unsafe { crate::mmu::Mmu::resume(crate::TABLES.base.0, crate::TABLES.size / 4096, used) })
    else {
        return false;
    };
    let ok = m.map(lo, size, crate::mmu::attrs(attr)).is_ok();
    facts::MMU_TABLES.store(m.tables(), Relaxed);
    crate::arch::tlbi_all();
    ok
}

// --- De kern-flip (hopos/src/flip.rs, docs/flip.md) ---------------------
//
// Van het flip-spoor, niet van het board: alleen getallen, in de
// loader-regio van het kernvenster zoals op virt (de staging is waar de
// stub `hopos-stage.elf` las, na de plaatsing van Hop dood geheugen). De
// kern is hier een PIE: de firmware koos zijn basis, en de nieuwe kern gaat
// op dezelfde basis (`__efi_head`), binnen de maat die de firmware toen
// gaf. De feiten van de stub (ACPI, de kaart, de config) overleven de
// sprong in een eigen pagina ([`FLIP_FACTS_PA`], `crate::flip`).

/// Geen vast linkadres: de basis komt uit de stub.
pub const FLIP_LINK_BASE: u64 = 0;
/// PIE: de basis is `__efi_head` van de draaiende kern.
pub const FLIP_PIE: bool = true;
/// Geen vaste grens: het nieuwe beeld moet in het oude passen.
pub const FLIP_IMAGE_END: u64 = 0;
/// De vluchtrecorder.
pub const FLIP_RECORDER_PA: u64 = BOOT_SCRATCH_PA + 0x1000;
/// De trampoline (Normal: de loader-regio is RAM uit de kaart).
pub const FLIP_TRAMP_PA: u64 = BOOT_SCRATCH_PA + 0x2000;
/// De feiten van de stub voor een kern die zonder firmware binnenkomt.
pub const FLIP_FACTS_PA: u64 = BOOT_SCRATCH_PA + 0x3000;
/// Hoeveel ruimte die feiten hebben.
pub const FLIP_FACTS_LEN: u64 = 0x2000;

/// De zwarte doos: de console-ring van de kern (`kern::conport`, hopos
/// conport.rs): hetzelfde gat als op virt en de Pi's (direct onder het
/// handoff-blob), maar boven de feitenpagina van de stub. De loader-regio
/// van het kernvenster is die van de recorder; de stub schrijft er alleen
/// het staging-woord en de feiten.
pub const BLACK_BOX: Region =
    Region::new(abi::layout::flip_handoff_pa(STAGE_HDR_PA) - 0x8000, 0x8000);

const _: () =
    assert!(BLACK_BOX.base >= FLIP_FACTS_PA + FLIP_FACTS_LEN && BLACK_BOX.base.is_multiple_of(64));

const _: () = {
    assert!(FLIP_RECORDER_PA >= BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN);
    assert!(FLIP_TRAMP_PA + 0x1000 <= FLIP_FACTS_PA);
    assert!(FLIP_FACTS_PA + FLIP_FACTS_LEN <= abi::layout::flip_handoff_pa(STAGE_HDR_PA));
};
