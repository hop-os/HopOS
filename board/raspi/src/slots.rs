//! Het slot-plan van de Pi's: waar de kern zijn kooien, park-mailboxen en
//! partities fysiek legt, en waar de firmware het image van Hop neerzette.
//!
//! Dezelfde vorm als `board_qemuvirt::slots`, zodat de slot-lijm van de
//! binary (`hopos/src/slots.rs`, `cage.rs`) hetzelfde pad loopt; alleen
//! `mpidr` verschilt per SoC en staat daarom in het board-crate.
//!
//! De staging: config.txt laadt het image als `initramfs` op
//! [`STAGE_PA`](crate::map::STAGE_PA) en de firmware schrijft het bereik in
//! `/chosen/linux,initrd-*`. Een gestaged image is Hop, de bevoorrechte
//! bewoner: de kern plaatst alleen Hop (O7).

use crate::map;
use abi::layout::{Plan, PlanSpec, Pool};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use sync::LocalCell;

pub use crate::map::{
    BOOT_SCRATCH_PA, CAGE_PA, DEVICE_WINDOW, NODE_CTRL_PA, STAGE_HDR_PA, STAGE_MAX, STAGE_PA,
};

/// De pool uit de DTB; `discover` vult hem, `plan` leest hem.
pub(crate) static POOL: LocalCell<Pool> = LocalCell::cell(Pool::new());

/// Het gestagede image: begin en lengte (0 = geen), gezet door `discover`.
pub(crate) static STAGE: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Het PA-plan voor een node met `cores` cores (de kern-core meegeteld):
/// elke andere core is een app-core, en elke app-core krijgt één kooi; één
/// kooi extra voor de bewoner die de OS-core `os_core` deelt (Hop, PORT.md
/// beslissing 2), zoals op QEMU virt.
pub fn plan(cores: usize, os_core: usize) -> abi::Result<Plan> {
    let app_cores = cores.saturating_sub(1).max(1);
    let pool = POOL.borrow().clone();
    Plan::new(PlanSpec {
        node_ctrl_pa: NODE_CTRL_PA,
        cage_pa: CAGE_PA,
        device_window: DEVICE_WINDOW,
        boot_scratch_pa: BOOT_SCRATCH_PA,
        net_dma_pa: map::NET_DMA.base,
        pool,
        ram_base: 0,
        app_cores,
        os_core,
        ..PlanSpec::default()
    })
}

/// Het image dat de firmware vóór de boot neerlegde, of `None`. Alleen de
/// maat is getoetst (in `discover`: binnen het laadvenster); de inhoud is
/// onvertrouwd en gaat door de ELF-lezer en `abi::place`.
#[must_use]
pub fn staged_image() -> Option<&'static [u8]> {
    let start = STAGE[0].load(Relaxed);
    let len = STAGE[1].load(Relaxed);
    if start == 0 || len == 0 {
        return None;
    }
    crate::arch::stage_slice(start, len)
}

/// Toetst een initrd-bereik: binnen het laadvenster, niet leeg, niet te
/// groot. Geeft begin en lengte.
#[must_use]
pub fn check_stage(start: u64, end: u64) -> Option<(u64, u64)> {
    let len = end.checked_sub(start)?;
    let lo = map::LOADER.base;
    let hi = map::LOADER.base + map::LOADER.size;
    (len > 0 && len <= STAGE_MAX + (STAGE_PA - lo) && start >= lo && end <= hi)
        .then_some((start, len))
}

// --- De kern-flip (hopos/src/flip.rs, docs/flip.md) ---------------------
//
// Van het flip-spoor, niet van het board: alleen getallen, in de vorm die
// `map` er al voor vrijhield (de recorder en de trampoline op de
// boot-scratch-pagina's, het blob onder het staging-maatwoord). De staging
// is waar config.txt het image van Hop laadde; na zijn plaatsing dood.
// Op ijzer nog niet geflipt.

/// Waar de nieuwe kern heen gaat: het koude linkadres
/// (`hopos/link-raspi.ld`, `KERN_BASE`), waar de firmware hem laadt.
pub const FLIP_LINK_BASE: u64 = map::KERN_BASE;
/// Geen PIE.
pub const FLIP_PIE: bool = false;
/// Het beeld blijft onder het einde van de kern-RAM.
pub const FLIP_IMAGE_END: u64 = map::KERN_END;
/// De vluchtrecorder.
pub const FLIP_RECORDER_PA: u64 = BOOT_SCRATCH_PA + 0x1000;
/// De trampoline (Normal, want het laadvenster is RAM).
pub const FLIP_TRAMP_PA: u64 = BOOT_SCRATCH_PA + 0x2000;

/// De zwarte doos: de console-ring van de kern (`kern::conport`, hopos
/// conport.rs), die een flip en een reset overleeft: de 32 KiB direct onder
/// het handoff-blob, boven de trampoline.
/// Die pagina's liggen met de recorder in hetzelfde gat van de
/// boot-scratch, en daar schrijft alleen de flip: de recorder en de
/// trampoline eronder, het blob erboven.
pub const BLACK_BOX: abi::Region =
    abi::Region::new(abi::layout::flip_handoff_pa(STAGE_HDR_PA) - 0x8000, 0x8000);

const _: () = assert!(
    BLACK_BOX.base >= FLIP_TRAMP_PA + 0x1000
        && BLACK_BOX.base > FLIP_RECORDER_PA
        && BLACK_BOX.base.is_multiple_of(64)
);
const _: () = {
    assert!(FLIP_RECORDER_PA >= BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN);
    assert!(FLIP_TRAMP_PA + 0x1000 <= abi::layout::flip_handoff_pa(STAGE_HDR_PA));
};
