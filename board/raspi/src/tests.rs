//! Host-tests van de Pi-logica die zonder ijzer te toetsen is.

use super::*;
use crate::map::{GB, L2, MB2, Tables};

fn tables() -> Tables {
    let l2 = [
        Some(L2 {
            gb: 0,
            table: Pa(0x10_0000),
            fixed: AbiRegion::new(0, map::FIXED_END),
        }),
        Some(L2 {
            gb: 3,
            table: Pa(0x20_0000),
            fixed: AbiRegion::new(0xFC00_0000, 0x0400_0000),
        }),
    ];
    Tables { l1: Pa(0x1000), l2 }
}

fn normal(pa: u64) -> u64 {
    pa | 1
}

#[test]
fn a_1gb_pi4_maps_only_its_bank_above_the_fixed_part() {
    // De Pi 4 met 1 GB: één bank tot 0x3B40_0000 (de GPU-helft eraf).
    let banks = [AbiRegion::new(0, 0x3B40_0000)];
    let mut put = Vec::new();
    let mapped = map::plan_ram(&banks, &tables(), normal, |pa, d| put.push((pa.0, d)));
    assert_eq!(
        mapped.as_slice(),
        [AbiRegion::new(map::FIXED_END, 0x3B40_0000 - map::FIXED_END)]
    );
    // Eerste regel: het blok op FIXED_END, in de GB0-tabel.
    assert_eq!(
        put[0],
        (0x10_0000 + 8 * (map::FIXED_END / MB2), map::FIXED_END | 1)
    );
    assert!(put.iter().all(|&(pa, _)| pa < 0x10_1000), "only GB0");
}

#[test]
fn an_8gb_pi4_gets_whole_gigabytes_and_the_ram_below_the_peripherals() {
    let banks = [
        AbiRegion::new(0, 0x3B40_0000),
        AbiRegion::new(0x4000_0000, 0xFC00_0000 - 0x4000_0000),
        AbiRegion::new(0x1_0000_0000, 0x1_0000_0000),
    ];
    let mut l1 = Vec::new();
    let mut gb3 = 0;
    let mapped = map::plan_ram(&banks, &tables(), normal, |pa, d| {
        if pa.0 < 0x2000 {
            l1.push((pa.0, d));
        } else if (0x20_0000..0x20_1000).contains(&pa.0) {
            gb3 += 1;
        }
    });
    // GB1, GB2 heel; GB4-7 heel; GB3 in 2 MB-blokken tot de peripherals.
    assert_eq!(
        l1,
        [1u64, 2, 4, 5, 6, 7]
            .iter()
            .map(|&g| (0x1000 + 8 * g, (g * GB) | 1))
            .collect::<Vec<_>>()
    );
    assert_eq!(gb3, (0xFC00_0000 - 0xC000_0000) / MB2);
    // De stukken lopen door: vanaf FIXED_END tot 0x3B40_0000, en 0x4000_0000
    // tot 0xFC00_0000, en de hoge 4 GB.
    assert_eq!(mapped.len(), 3);
    assert_eq!(
        mapped.as_slice()[1],
        AbiRegion::new(0x4000_0000, 0xBC00_0000)
    );
}

#[test]
fn the_pool_keeps_clear_of_the_fixed_part_the_dtb_and_the_reserve() {
    let mapped = [AbiRegion::new(map::FIXED_END, 0x3000_0000)];
    let reserve = [AbiRegion::new(0x2000_0000, 0x10_0000)];
    let holes = map::holes(
        &reserve,
        AbiRegion::new(map::DTB_PA, 0x1_0000),
        AbiRegion::new(map::STAGE_PA, 0x10_0000),
    );
    let pool = abi::layout::carve_pool(&mapped, holes.as_slice(), MB2).unwrap();
    assert_eq!(
        pool.as_slice(),
        [
            AbiRegion::new(map::FIXED_END, 0x2000_0000 - map::FIXED_END),
            AbiRegion::new(0x2020_0000, 0x4500_0000 - 0x2020_0000),
        ]
    );
}

#[test]
fn the_slot_plan_validates_and_keeps_the_cage_in_the_device_window() {
    *slots::POOL.borrow_mut() = {
        let mut p = abi::layout::Pool::new();
        p.push(AbiRegion::new(0x2000_0000, 0x2000_0000)).unwrap();
        p
    };
    let p = slots::plan(4, 0).unwrap();
    assert_eq!(p.app_cores(), 3);
    assert_eq!(p.vec_base_pa().0, map::CAGE_PA);
    for r in p.pool() {
        assert!(r.base >= map::FIXED_END);
        assert!(!r.overlaps(map::DEVICE_WINDOW));
    }
    // Zonder pool weigert het plan: een lege pool is geen plan.
    *slots::POOL.borrow_mut() = abi::layout::Pool::new();
    assert!(slots::plan(4, 0).is_err());
}

#[test]
fn staging_must_lie_in_the_loader_window() {
    assert_eq!(
        slots::check_stage(map::STAGE_PA, map::STAGE_PA + 0x1000),
        Some((map::STAGE_PA, 0x1000))
    );
    // QEMU raspi4b: -initrd op 128 MB.
    assert!(slots::check_stage(0x0800_0000, 0x0810_0000).is_some());
    assert!(slots::check_stage(0x0400_0000, 0x0410_0000).is_none());
    assert!(slots::check_stage(0x0F20_0000, 0x1000_1000).is_none());
    assert!(slots::check_stage(0x0F20_0000, 0x0F20_0000).is_none());
}

#[test]
fn cores_and_mac() {
    assert_eq!(cfg::cores(4, 0), 4);
    assert_eq!(cfg::cores(4, 2), 2);
    assert_eq!(cfg::cores(4, 9), 4);
    assert_eq!(cfg::cores(0, 3), 3);
    assert_eq!(
        cfg::mac_from_serial(Some("10000000c0ffee42"), 4),
        [0x02, 0x48, 0xc0, 0xff, 0xee, 0x42]
    );
    assert_eq!(
        cfg::mac_from_serial(Some("1000000zc0ffee42"), 4),
        [0x02, 0x48, 0xc0, 0xff, 0xee, 0x42],
        "only the last eight count"
    );
    assert_eq!(
        cfg::mac_from_serial(Some("c0ffeX42"), 5),
        [0x02, 0x48, 0x4f, 0x50, 0x00, 5]
    );
    let m = [0xdc, 0xa6, 0x32, 1, 2, 3];
    assert_eq!(cfg::mac_bytes(cfg::mac_word(m), 4), m);
    assert_eq!(cfg::mac_bytes(0, 4), [0x02, 0x48, 0x4f, 0x50, 0x00, 4]);
}

#[test]
fn a_dtb_outside_the_mapped_ram_is_refused() {
    let d = board::dtb::Dtb::new();
    assert!(d.find(0x7_0000, DTB_RAM).is_none());
    assert!(d.find(map::DEVICE_WINDOW.base, DTB_RAM).is_none());
    assert!(DTB_RAM.contains(Pa(map::DTB_PA)));
}

#[test]
fn the_board_plan_matches_the_map() {
    struct Fake;
    impl Soc for Fake {
        type Nic = NoNic;
        const NAME: &'static str = "fake";
        const SOC: &'static str = "none";
        const MAC_FALLBACK: u8 = 9;
        const VCMAIL: Pa = Pa(0);
        const RNG200: Pa = Pa(0);
        const PM: Pa = Pa(0);
        fn uart() -> &'static Pl011 {
            panic!("no hardware in a host test")
        }
        fn gic() -> &'static Gic {
            panic!("no hardware in a host test")
        }
        fn mpidr(core: usize) -> u64 {
            core as u64
        }
        fn core_of(mpidr: u64) -> usize {
            mpidr as usize
        }
        fn tables() -> Option<map::Tables> {
            None
        }
        fn probe_nic(_ctx: &NicCtx) -> Result<Option<NoNic>, Error> {
            Ok(None)
        }
    }
    struct NoNic;
    impl netdev::Device for NoNic {
        fn transmit(&mut self, _f: &[u8]) -> Result<(), netdev::TxError> {
            Err(netdev::TxError::Dead)
        }
        fn receive(&mut self, _b: &mut [u8]) -> Option<usize> {
            None
        }
        fn mac(&self) -> netdev::Mac {
            netdev::Mac::default()
        }
    }
    let b = Raspi::<Fake>::new();
    let p = b.plan();
    assert_eq!(p.kern_ram.end().0, map::LOADER.base);
    assert!(p.dma.contains(p.net_dma.base));
    assert_eq!(b.cores(), CORES_DEFAULT);
    assert!(b.probe_disk().unwrap().is_none());
    assert!(b.probe_nic().unwrap().is_none());
    // Zonder NIC is er niets geclaimd: een tweede probe kijkt opnieuw.
    assert!(b.probe_nic().unwrap().is_none());
    // Een RNG200 die de DTB uitzet, wordt niet aangeraakt: de DRBG zaait op
    // jitter, en de watchdog heeft zonder `discover` geen blok.
    rng::seed::<Fake>(Some(false));
    assert_eq!(cpu::drbg::source(), cpu::drbg::Source::Jitter);
    let mut buf = [0u8; 16];
    assert!(cpu::drbg::read(&mut buf).is_ok());
    assert_eq!(watchdog::arm(12_000), Err("no PM watchdog on this board"));
}
