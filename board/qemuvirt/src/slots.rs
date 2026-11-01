//! Het slot-plan van QEMU virt: waar de kern van dit board zijn kooien,
//! park-mailboxen, switch-code en partities fysiek legt, en waar QEMU een
//! app-image voor de eerste plaatsing neerzet.
//!
//! De adressen zijn die van de Go-kern (`OLD/metal/board/qemuvirt`): BEWUST
//! verschoven ten opzichte van de IPA-constanten van de apps (de kooi-regio
//! op 0xC200_0000 terwijl apps hun staart op 0x5000_0000+ zien, en een pool
//! in twee losse regio's). Op QEMU is identity de valkuil die IPA/PA-
//! verwisselingen verhult; ongelijk gemaakt knalt elke verwisseling in de
//! regressie in plaats van pas op een board. Vereist `-m 3G`.
//!
//! Dit bezit alleen getallen en de ene lezer van de staging. Welk slot
//! waar draait is van de lifecycle-actor van de kern.

use abi::Region;
use abi::layout::{Plan, PlanSpec, pool_of};
use board::stage::{self, StagedRole};

/// De control-pages van de eigen cores van de kern (node-SMP).
pub const NODE_CTRL_PA: u64 = 0xC000_0000;
/// De kooi-regio: blok 0 draagt de EL2-vectoren van de app-cores, de
/// parkeerlus, de sched-blokken en de switch-code; blok i de stage-2 en de
/// contexten van slot i.
pub const CAGE_PA: u64 = 0xC200_0000;
/// Het venster dat de identity map van de kern als Device mapt: de
/// control-pages en de kooi-regio. De app-cores lezen dit op EL2 met de MMU
/// uit (dus langs elke cache heen), en `cpu::el2` rekent erop dat de
/// park-mailbox coherent is zonder veeg (dispatch.rs `core_state`).
pub const DEVICE_WINDOW: Region = Region::new(0xC000_0000, 0x0400_0000);
/// De boot-scratch: buiten elke pool, zoals in Go (`BootScratchPA`).
pub const BOOT_SCRATCH_PA: u64 = 0xB000_0000;
/// De partitie-pool: het klassieke venster van 1,5 GB en een tweede regio
/// van 240 MB die bewijst dat de pool meer dan één stuk aankan.
pub const POOL: [Region; 2] = [
    Region::new(0x5000_0000, 0x6000_0000),
    Region::new(0xB100_0000, 0x0F00_0000),
];

/// Het woord waarin QEMU de maat van het gestagede image legt
/// (`-device loader,addr=…,data=<maat>,data-len=8` in image/qemu-run.sh).
/// Nul = er is niets gestaged.
pub const STAGE_HDR_PA: u64 = 0xB010_0000;
/// Het woord met de rol van het gestagede image, direct na de maat
/// (`-device loader,addr=…+8,data=<rol>,data-len=8`): 0 = een gewone app
/// voor het ABI-bewijs (tools/qemu-test.sh), 1 = Hop, de bevoorrechte
/// bewoner (PORT.md beslissing 1, tools/qemu-test-hop.sh). QEMU begint met
/// nul-RAM, dus een run zonder dit woord is een gewone app.
pub const STAGE_ROLE_PA: u64 = STAGE_HDR_PA + 8;
/// Waar QEMU het image rauw neerlegt (`-device loader,file=…,addr=…,
/// force-raw=on`): tussen de boot-scratch en de tweede pool-regio, dus
/// buiten alles wat de lifecycle uitdeelt of wist.
pub const STAGE_PA: u64 = 0xB020_0000;
/// De grootste staging: tot aan de tweede pool-regio.
pub const STAGE_MAX: u64 = 0xB100_0000 - STAGE_PA;

const _: () = assert!(STAGE_ROLE_PA + 8 <= STAGE_PA);
const _: () = assert!(BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN <= STAGE_HDR_PA);
const _: () = assert!(NODE_CTRL_PA >= DEVICE_WINDOW.base && CAGE_PA > NODE_CTRL_PA);

/// Het PA-plan voor een node met `cores` cores, met de kern op fysieke
/// core `os_core` (de OS-core, PORT.md beslissing 2): elke andere core is
/// een app-core met één kooi, en de OS-core draagt er één bij, want Hop
/// woont daar sinds 30-09 naast de kern. Twee cores geven zo twee kooien:
/// Hop op de OS-core en een app op de volle app-core.
pub fn plan(cores: usize, os_core: usize) -> abi::Result<Plan> {
    let app_cores = cores.saturating_sub(1).max(1);
    Plan::new(PlanSpec {
        node_ctrl_pa: NODE_CTRL_PA,
        cage_pa: CAGE_PA,
        device_window: DEVICE_WINDOW,
        boot_scratch_pa: BOOT_SCRATCH_PA,
        pool: pool_of(POOL)?,
        max_slots: (app_cores + 1).max(abi::layout::SLOTS_DEFAULT),
        app_cores,
        os_core,
        ..PlanSpec::default()
    })
}

/// Het MPIDR-target van core `core` op virt: `hw/arm/virt.c` legt bij een
/// GICv3 zestien cores per cluster (aff0), daarboven aff1.
#[must_use]
pub const fn mpidr(core: usize) -> u64 {
    ((core % 16) as u64) | (((core / 16) as u64) << 8)
}

/// De fysieke core-index bij een MPIDR van virt: de inverse van [`mpidr`].
#[must_use]
pub const fn core_of(mpidr: u64) -> usize {
    (mpidr & 0xf) as usize + ((mpidr >> 8) & 0xff) as usize * 16
}

/// Het image dat QEMU vóór de boot neerlegde, of `None` als het maatwoord
/// nul of onzin is (`board::stage::staged_at`).
#[must_use]
pub fn staged_image() -> Option<&'static [u8]> {
    stage::staged_at(STAGE_HDR_PA, STAGE_PA, STAGE_MAX)
}

/// De rol uit [`STAGE_ROLE_PA`]; een onbekend woord komt rauw terug (de
/// kern plaatst dan niets en zegt dat luid).
pub fn staged_role() -> Result<StagedRole, u64> {
    stage::role_at(STAGE_ROLE_PA)
}

// --- De kern-flip (hopos/src/flip.rs, docs/flip.md) ---------------------
//
// Van het flip-spoor, niet van het board: alleen getallen. De staging is
// de plek waar QEMU het image van Hop legde (na zijn plaatsing dood
// geheugen); de recorder, de trampoline en het handoff-blob liggen op de
// boot-scratch-pagina's ertussen. Alles buiten het kern-RAM en de pool.

/// Waar de nieuwe kern heen gaat: het koude linkadres (`hopos/link.ld`,
/// `KERN_BASE`), dus precies waar QEMU hem ook zou laden.
pub const FLIP_LINK_BASE: u64 = 0x4020_0000;
/// Geen PIE: de basis is [`FLIP_LINK_BASE`], niet wat een stub koos.
pub const FLIP_PIE: bool = false;
/// Het beeld op het koude adres blijft hieronder: de DMA-regio, waar de
/// NIC schrijft tot de nieuwe kern hem reset.
pub const FLIP_IMAGE_END: u64 = 0x4f00_0000;
/// De vluchtrecorder (twee woorden: de lopende stand en het archief).
pub const FLIP_RECORDER_PA: u64 = BOOT_SCRATCH_PA + 0x1000;
/// De trampoline van de sprong: een pagina, uitvoerbaar (Normal) gemapt.
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
    assert!(FLIP_TRAMP_PA >= FLIP_RECORDER_PA + 16);
    assert!(FLIP_TRAMP_PA + 0x1000 <= abi::layout::flip_handoff_pa(STAGE_HDR_PA));
};
