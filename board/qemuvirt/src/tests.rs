//! Host-tests van de board-logica die zonder ijzer te toetsen is.

use super::*;

#[test]
fn a_dtb_outside_the_kern_ram_is_refused() {
    let d = board::dtb::Dtb::new();
    assert!(d.find(0x0900_0000, KERN_RAM).is_none());
    assert!(d.find(DMA.base.0, KERN_RAM).is_none());
}

#[test]
fn the_slot_plan_validates_and_keeps_clear_of_the_kern() {
    use crate::slots::{self, STAGE_HDR_PA, STAGE_MAX, STAGE_PA};
    let p = slots::plan(4, 0).unwrap();
    assert_eq!(p.app_cores(), 3);
    assert_eq!(p.vec_base_pa().0, slots::CAGE_PA);
    let stage = abi::Region::new(STAGE_HDR_PA, STAGE_PA + STAGE_MAX - STAGE_HDR_PA);
    for r in p.pool() {
        assert!(r.base >= DMA.end().0, "pool {r:?} in the kern RAM or DMA");
        assert!(!r.overlaps(stage), "pool {r:?} over the staging");
        assert!(
            !r.overlaps(slots::DEVICE_WINDOW),
            "pool {r:?} over the cage"
        );
    }
    // Eén core: toch één app-core in het plan, zodat het plan valideert;
    // plaatsen faalt dan pas bij de core-toets.
    assert_eq!(slots::plan(1, 0).unwrap().app_cores(), 1);
    // Twee cores met de kern op core 1: app-core 1 is fysiek core 0.
    let p = slots::plan(2, 1).unwrap();
    assert_eq!((p.app_cores(), p.os_core()), (1, 1));
    assert_eq!(p.phys_core(abi::layout::Core::new(1).unwrap()), 0);
    assert!(slots::plan(2, 2).is_err());
}

#[test]
fn app_core_affinity_follows_virt() {
    use crate::slots::mpidr;
    assert_eq!(mpidr(1), 1);
    assert_eq!(mpidr(15), 15);
    assert_eq!(mpidr(17), 0x101);
    for c in [0, 1, 15, 16, 17, 31] {
        assert_eq!(crate::slots::core_of(mpidr(c)), c);
    }
}
