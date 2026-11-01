//! Het slot-plan van de LicheeRV Nano: 256 MB DDR3 op 0x8000_0000 tot
//! 0x9000_0000, en er blijft niets ongebruikt (Go, board/licheerv/hop/plan.go):
//!
//! ```text
//! 0x8000_0000  64 MB  pool B: hier decomprimeert de FSBL U-Boot vóór hij
//!                     ons aanspringt; daarna vrij DRAM voor partities, maar
//!                     nooit voor ons IMAGE (dat zou de FSBL overschrijven)
//! 0x8400_0000  41 MB  de kern: image, stack, heap (link-riscv.ld, 40 MB),
//!                     en de laatste MB de DMA-regio van de dwmac
//! 0x8690_0000  13 MB  de staging van de kern-flip
//! 0x8760_0000  10 MB  pool C
//! 0x8800_0000 126 MB  pool A: de partities; het linkadres van elk
//!                     riscv64-app-image (SlotBase in Go)
//! 0x8FE0_0000   2 MB  de staart: boot-scratch, control-pages, kooi-regio
//! ```
//!
//! De kern staat BEWUST op 0x8400_0000 en niet lager (19-08): met RUNADDR
//! 0x8040_0000 bootte het board niet. De FSBL woont in het lage DRAM en moet
//! na ons image nog LOADER_2ND laden; een image eronder schrijft door zijn
//! eigen code heen.

use abi::Region;
use abi::layout::{Plan, PlanSpec, pool_of};
use board::stage::StagedRole;

/// De staart van het DRAM voor de structuren van de kern.
pub const OS_BASE: u64 = 0x8FE0_0000;
/// De boot-scratch.
pub const BOOT_SCRATCH_PA: u64 = OS_BASE;
/// De control-pages van de eigen harts van de kern.
pub const NODE_CTRL_PA: u64 = OS_BASE + 0x4000;
/// De kooi-regio: sched-blokken, switch-code, ctx-blokken.
pub const CAGE_PA: u64 = OS_BASE + 0x2_0000;
/// De partitie-pool: A eerst, want de eerste regio is het linkadres.
pub const POOL: [Region; 3] = [
    Region::new(0x8800_0000, 0x07e0_0000),
    Region::new(0x8000_0000, 0x0400_0000),
    Region::new(STAGE_PA + STAGE_MAX, 0x8800_0000 - (STAGE_PA + STAGE_MAX)),
];
/// Het maatwoord van een staging: er is geen QEMU die iets neerlegt (het
/// image zit in de kern, [`staged_image`]). Een plek in de staart voor de
/// flip-lijm: het handoff-blob eronder (`abi::layout::flip_handoff_pa`).
pub const STAGE_HDR_PA: u64 = OS_BASE + 0x1C_0000;
/// De staging van de kern-flip: waar het nieuwe beeld plat ligt tot de
/// sprong. Het beeld met Hop erin is groter dan de staart (9,2 MB met
/// BSS en de Hop van 02-10), de kern-RAM is van de heap (QEMU riscv64 met
/// Hop: 31 MB in gebruik, 02-10) en wordt geveegd, dus: direct achter de
/// DMA-regio, die daarvoor van 8 MB (Go) naar 1 MB ging (de dwmac vraagt
/// 512 KB), en de kop van pool C (die van 16 naar 10 MB ging).
pub const STAGE_PA: u64 = 0x8690_0000;
/// De grootste staging: 13 MB, tot pool C.
pub const STAGE_MAX: u64 = 0x00D0_0000;

const _: () = assert!(FLIP_TRAMP_PA + 0x1000 <= NODE_CTRL_PA);
const _: () = assert!(NODE_CTRL_PA + 3 * 0x1000 <= CAGE_PA);
const _: () = assert!(CAGE_PA + 3 * abi::layout::CAGE_STRIDE <= STAGE_HDR_PA);

/// Het PA-plan: de kern op `os_core` (hart 0, de C906B), het andere hart
/// het app-hart.
pub fn plan(cores: usize, os_core: usize) -> abi::Result<Plan> {
    let app_cores = cores.saturating_sub(1).max(1);
    Plan::new(PlanSpec {
        node_ctrl_pa: NODE_CTRL_PA,
        cage_pa: CAGE_PA,
        boot_scratch_pa: BOOT_SCRATCH_PA,
        pool: pool_of(POOL)?,
        ram_base: 0x8000_0000,
        max_slots: (app_cores + 1).max(SLOTS),
        app_cores,
        os_core,
        ..PlanSpec::default()
    })
}

/// De kooi-capaciteit: 16, zoals Go (`SetMaxSlots(16)`), want de staart
/// van 2 MB draagt 28 control-pages en 26 kooi-blokken; de cores tellen
/// niet (kooien delen de ene app-hart).
pub const SLOTS: usize = 16;
const _: () = {
    assert!(NODE_CTRL_PA + (SLOTS as u64 + 1) * 0x1000 <= CAGE_PA);
    assert!(CAGE_PA + (SLOTS as u64 + 1) * abi::layout::CAGE_STRIDE <= STAGE_HDR_PA);
};

/// Het hart-id als "MPIDR" voor de gedeelde lijm.
#[must_use]
pub const fn mpidr(core: usize) -> u64 {
    core as u64
}

/// De inverse van [`mpidr`].
#[must_use]
pub const fn core_of(mpidr: u64) -> usize {
    mpidr as usize
}

/// Een image op een 8-grens: de ELF-lezer leest woorden.
#[repr(C, align(8))]
struct Aligned<T: ?Sized>(T);

/// Het image van de eerste bewoner, in de kern gebakken door
/// `image/licheerv-agent.sh` (`STAGE=` of `APP=`, via `HOPOS_EMBED` en
/// build.rs); leeg zonder. Het staat in `.rodata` van het image dat de FSBL
/// laadt, want er is geen QEMU die het in het RAM legt.
static STAGE: &Aligned<[u8]> = &Aligned(*include_bytes!(concat!(env!("OUT_DIR"), "/stage.bin")));

/// Het gebakken image, of `None`.
#[must_use]
pub fn staged_image() -> Option<&'static [u8]> {
    Some(&STAGE.0).filter(|b| !b.is_empty())
}

/// De rol van het gebakken image, uit build.rs (`HOPOS_EMBED_ROLE`): Hop in
/// slot 1 met de bevoegdheid (de release, `ROLE=hop`), of een gewone app
/// (het ABI-bewijs, twee keer door de kern geplaatst). Zonder image zegt de
/// plaatsing dat er niets is (`HOPOS_SLOT_NONE`).
pub fn staged_role() -> Result<StagedRole, u64> {
    Ok(if cfg!(lrv_stage_hop) && staged_image().is_some() {
        StagedRole::Hop
    } else {
        StagedRole::App
    })
}

// --- De kern-flip (hopos/src/flip.rs, docs/flip.md) ---------------------
//
// Alleen getallen, zoals op arm64 (board/qemuvirt/src/slots.rs). riscv64
// flipt (nog) niet (`flip::FLIPS`).

/// Het koude linkadres (`hopos/link-riscv.ld`, `KERN_BASE`).
pub const FLIP_LINK_BASE: u64 = 0x8400_0000;
/// Geen PIE.
pub const FLIP_PIE: bool = false;
/// Het einde van het beeld: het begin van de DMA-regio.
pub const FLIP_IMAGE_END: u64 = 0x8680_0000;
/// De vluchtrecorder.
pub const FLIP_RECORDER_PA: u64 = BOOT_SCRATCH_PA + 0x1000;
/// De trampoline van de sprong.
pub const FLIP_TRAMP_PA: u64 = BOOT_SCRATCH_PA + 0x2000;

/// Geen zwarte doos: nog geen plek die bewezen een reset overleeft, dus de
/// console-ring van de kern staat hier in .bss (hopos conport.rs) (de LicheeRV legt zijn kooien in dezelfde staart).
pub const BLACK_BOX: Region = Region::new(0, 0);

const _: () = {
    assert!(FLIP_RECORDER_PA >= BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN);
    assert!(FLIP_TRAMP_PA + 0x1000 <= abi::layout::flip_handoff_pa(STAGE_HDR_PA));
    assert!(FLIP_IMAGE_END < STAGE_PA && (STAGE_PA + STAGE_MAX).is_multiple_of(2 << 20));
};

#[cfg(test)]
mod tests {
    #[test]
    fn the_plan_fits_the_tail() {
        let p = super::plan(2, 0).unwrap();
        assert_eq!(p.app_cores(), 1);
        // De kern op de C906B: app-core 1 is de C906L.
        assert_eq!(p.phys_core(abi::layout::Core::new(1).unwrap()), 1);
    }
}
