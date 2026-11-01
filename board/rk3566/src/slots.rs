//! Het slot-plan van de RK3566: waar de kern van dit board zijn kooien,
//! control-pages en partities fysiek legt. Dezelfde vorm als
//! `board_qemuvirt::slots`, zodat de kern-binary er met één `use` op staat.
//!
//! De indeling is die van de Go-kern (`OLD/metal/board/rk3566/plan.go`),
//! GEMETEN 05-08: U-Boot's /memory-node is 0x20_0000..0x8000_0000 op de
//! 2 GB-variant (de eerste 2 MB is TF-A) en /memreserve/ is verder leeg
//! (geen OP-TEE: TF-A meldt zelf "No OPTEE provided by BL2").
//!
//! De pool komt uit de DTB en niet uit een constante: dit bord bestaat in
//! 1, 2, 4 en 8 GB, en een vaste pool zou op de kleine variant fantoom-RAM
//! uitdelen. Uit de banken gaan de gaten: alles onder [`POOL_BASE`], en de
//! plekken waar U-Boot de DTB en de initrd liet (gemeten: de DTB op
//! ~0x7ce9d000, 17 MB ónder een vaste bovengrens die Go eerst had).
//!
//! De staging: U-Boot laadt de initrd (de container van [`crate::initrd`]:
//! `hopos.cfg` plus het image van de bewoner), `discover` kopieert hem naar
//! de heap en splitst hem, en [`staged_image`] geeft het image-deel: Hop, de
//! bevoorrechte bewoner (de kern plaatst alleen Hop, O7). Een initrd zonder
//! image (de oude kale `hopos.cfg`) laat de kern niets plaatsen, luid.

use crate::{POOL_BASE, RAM_MAPPED_END, STRUCT_WINDOW};
use abi::Region;
use abi::layout::{Plan, PlanSpec, Pool, carve_pool};

/// De control-pages van de eigen cores van de kern (Go: `nodeCtrlPA`).
pub const NODE_CTRL_PA: u64 = 0x0620_0000;
/// De kooi-regio (Go: `stage2PA`): blok 0 de EL2-vectoren en de
/// switch-code, blok i de stage-2 van slot i.
pub const CAGE_PA: u64 = 0x0622_0000;
/// De boot-scratch, in het Device-venster en buiten elke pool.
pub const BOOT_SCRATCH_PA: u64 = 0x062E_0000;
/// Waar de kooi-regio moet eindigen: de boot-scratch (Go had hier de
/// trap-vector van core 0 op 0x062F_0000; die blijft vrij).
const CAGE_END: u64 = BOOT_SCRATCH_PA;
/// De levenstekenwoorden van de app-cores (Go: `WakeBase`), BEWUST in het
/// Device-venster: in de eerste Go-iteratie lagen ze in gecachet geheugen,
/// en toen meldde PSCI "accepted" terwijl de core stil bleef (05-08).
pub const WAKE_PA: u64 = 0x0630_0000;
/// De vluchtrecorder van de kern-flip: moet een watchdog-reset overleven.
pub const FLIP_SCRATCH_PA: u64 = WAKE_PA + 0x1000;
/// De console-zwarte-doos, 32 KB.
pub const BLACK_BOX: Region = Region::new(WAKE_PA + 0x8000, 32 << 10);

/// Het maatwoord van de staging (de vorm van QEMU virt): een MB in het
/// staging-venster. Niemand vult het op dit board (het image staat in de
/// heap); de kern-flip gebruikt het venster erachter als plek voor een
/// platgelegde nieuwe kern, en het blob eronder.
pub const STAGE_HDR_PA: u64 = crate::STAGE_WINDOW.base.0 + 0x10_0000;
/// Waar een gestaged image begint.
pub const STAGE_PA: u64 = STAGE_HDR_PA + 0x10_0000;
/// De grootste staging: tot het einde van het venster.
pub const STAGE_MAX: u64 = crate::STAGE_WINDOW.base.0 + crate::STAGE_WINDOW.size - STAGE_PA;

/// De vaste pool als de DTB geen bruikbaar /memory heeft: 512 MB, LUID
/// (`HOPOS_POOL_FALLBACK`). Op dit bord hóórt de DTB er te zijn (booti geeft
/// hem in x0).
pub const POOL_FALLBACK: Region = Region::new(POOL_BASE, 0x2000_0000);

const _: () = {
    assert!(NODE_CTRL_PA == STRUCT_WINDOW.base.0);
    assert!(BLACK_BOX.base + BLACK_BOX.size <= STRUCT_WINDOW.base.0 + STRUCT_WINDOW.size);
    assert!(BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN <= WAKE_PA);
};

/// De pool uit de DRAM-banken van de DTB, met de gaten eruit: alles onder
/// [`POOL_BASE`], alles vanaf [`RAM_MAPPED_END`], en `holes` (de DTB, de
/// initrd, /memreserve/). Leeg of mislukt = de luide terugval.
pub fn pool(banks: &[Region], holes: &[Region]) -> (Pool, bool) {
    let mut all = [Region::new(0, 0); 4 + fw::fdt::MAX_RESERVE];
    all[0] = Region::new(0, POOL_BASE);
    all[1] = Region::new(RAM_MAPPED_END, u64::MAX - RAM_MAPPED_END);
    let mut n = 2;
    for h in holes.iter().filter(|h| h.size > 0) {
        if let Some(slot) = all.get_mut(n) {
            *slot = *h;
            n += 1;
        }
    }
    let carved = all
        .get(..n)
        .and_then(|h| carve_pool(banks, h, 2 << 20).ok())
        .filter(|p| !p.is_empty());
    match carved {
        Some(p) => (p, true),
        None => {
            let mut p = Pool::new();
            // Eén regio past altijd.
            let _ = p.push(POOL_FALLBACK);
            (p, false)
        }
    }
}

/// Het PA-plan voor een node met `cores` cores, met de kern op fysieke core
/// De cores van de RK3566: vier A55's. Het plan legt per core vast (de
/// control-pages en de kooi-regio); wat per slot is, komt uit de pool, dus
/// er is geen kooi-capaciteit meer (tot 01-11: 11, wat de regio droeg).
pub const CORES: usize = 4;
const _: () = {
    assert!(NODE_CTRL_PA + abi::layout::node_ctrl_len(CORES - 1) <= CAGE_PA);
    assert!(CAGE_PA + abi::layout::cage_len(CORES - 1) <= CAGE_END);
};

/// `os_core` (PORT.md beslissing 2): elke andere core is een app-core met
/// één kooi, en de OS-core draagt er één bij voor Hop.
pub fn plan(cores: usize, os_core: usize) -> abi::Result<Plan> {
    let app_cores = cores.saturating_sub(1).max(1);
    let plan = Plan::new(PlanSpec {
        node_ctrl_pa: NODE_CTRL_PA,
        cage_pa: CAGE_PA,
        boot_scratch_pa: BOOT_SCRATCH_PA,
        flip_scratch_pa: FLIP_SCRATCH_PA,
        black_box: BLACK_BOX,
        net_dma_pa: crate::NET_DMA.base.0,
        usb_dma_pa: crate::USB_DMA.base.0,
        ram_base: crate::DRAM_BASE,
        pool: crate::pool_now(),
        app_cores,
        os_core,
        ..PlanSpec::default()
    })?;
    // De kooi-regio moet vóór de trap-vector eindigen: Go's carve droeg
    // twaalf kooien, en kooi 13 overschreef hem.
    let cage_end = CAGE_PA + abi::layout::cage_len(plan.app_cores());
    if cage_end > CAGE_END {
        return Err(abi::Error::Overlap {
            a: Region::new(CAGE_PA, cage_end - CAGE_PA),
            b: Region::new(CAGE_END, 0x1000),
        });
    }
    Ok(plan)
}

/// Het MPIDR-target van core `core`. GEMETEN 05-08: dit silicium nummert
/// in aff1. PSCI CPU_ON accepteert 0x100/0x200/0x300 en weigert
/// 0x1/0x2/0x3 met INVALID_PARAMS; de gewekte cores melden zelf MPIDR
/// 0x81000100/0200/0300.
#[must_use]
pub const fn mpidr(core: usize) -> u64 {
    (core as u64) << 8
}

/// De fysieke core-index bij een MPIDR: de inverse van [`mpidr`].
#[must_use]
pub const fn core_of(mpidr: u64) -> usize {
    ((mpidr >> 8) & 0xff) as usize
}

/// Het image uit de initrd, of `None`. Het staat in de heap (Normal,
/// gecachet), niet in het DRAM waar U-Boot het liet: dat mapt de identity
/// map als Device, en de ELF-lezer leest ongealigneerd. Alleen de maat is
/// getoetst (`initrd::split`); de inhoud is onvertrouwd en gaat door de
/// ELF-lezer en `abi::place`.
#[must_use]
pub fn staged_image() -> Option<&'static [u8]> {
    crate::STAGE.get()
}

// --- De kern-flip (hopos/src/flip.rs, docs/flip.md) ---------------------
//
// Van het flip-spoor, niet van het board: alleen getallen. De staging, het
// blob en de recorder liggen in Device-vensters (woordgewijs geschreven en
// geveegd, dus goed); de trampoline moet uitvoerbaar zijn, en het enige
// Normal-RAM buiten de pool is de kern-RAM zelf: de bovenste pagina van de
// heap. Dat mag, want na de kopie van de trampoline draait er geen Rust
// meer, en het nieuwe beeld reikt er niet (`cpu::el2::chain`, `check`). Op
// ijzer nog niet geflipt.

/// Waar de nieuwe kern heen gaat: het koude linkadres
/// (`hopos/link-rk3566.ld`, `KERN_BASE`).
pub const FLIP_LINK_BASE: u64 = 0x0221_0000;
/// Geen PIE.
pub const FLIP_PIE: bool = false;
/// De trampoline: de bovenste pagina van de kern-RAM.
pub const FLIP_TRAMP_PA: u64 = crate::KERN_RAM.base.0 + crate::KERN_RAM.size - 0x1000;
/// Het beeld blijft onder de trampoline.
pub const FLIP_IMAGE_END: u64 = FLIP_TRAMP_PA;
/// De vluchtrecorder: het woord dat al voor een watchdog-reset vrijlag.
pub const FLIP_RECORDER_PA: u64 = FLIP_SCRATCH_PA;

const _: () = assert!(abi::layout::flip_handoff_pa(STAGE_HDR_PA) >= crate::STAGE_WINDOW.base.0);
