//! Tests van de layout, geport uit `OLD/metal/abi/layout/*_test.go`.
//!
//! De offset-toetsen die in Go de bron parsten (`TestCtrlOffsetsUniek`,
//! `TestSchedOffsets`) zijn hier een expliciete lijst plus de
//! `offset_of!`-asserties in de bron: een veld dat niet in de struct staat,
//! compileert niet, dus de lijst kan niet stil driften.

use super::*;

/// Raakt een regio-lijst een adresbereik?
fn overlaps(regs: &[Region], base: u64, size: u64) -> bool {
    regs.iter().any(|r| r.overlaps(Region::new(base, size)))
}

fn test_spec() -> PlanSpec {
    let mut pool = Pool::new();
    pool.push(Region::new(0x5000_0000, 0x6000_0000)).unwrap();
    pool.push(Region::new(0xb100_0000, 0x0f00_0000)).unwrap();
    PlanSpec {
        node_ctrl_pa: 0xc000_0000,
        cage_pa: 0xc200_0000,
        trap_vec_pa: 0xc200_0800,
        boot_scratch_pa: 0xb000_0000,
        pool,
        ..PlanSpec::default()
    }
}

// --- carvepool_test.go ------------------------------------------------------

#[test]
fn carve_pool_respecteert_een_niet_uitgelijnd_gat() {
    // Eén bank van 2 GB minus de eerste 2 MB, de gemeten /memory van de
    // Radxa; het gat is de gemeten DTB, klein en niet 2 MB-uitgelijnd.
    let banks = [Region::new(0x20_0000, 0x8000_0000 - 0x20_0000)];
    let (dtb, dtb_size) = (0x7CE9_D000, 0x10000);
    let pool = carve_pool(
        &banks,
        &[Region::new(0, 0x0700_0000), Region::new(dtb, dtb_size)],
        2 << 20,
    )
    .unwrap();
    assert!(!pool.is_empty(), "pool is leeg");
    assert!(
        !overlaps(&pool, dtb, dtb_size),
        "pool raakt de DTB: {pool:?}"
    );
    let mut total = 0;
    for r in pool.iter() {
        total += r.size;
        assert!(r.base.is_multiple_of(2 << 20) && r.size.is_multiple_of(2 << 20));
    }
    assert!(total > 0 && total <= 0x8000_0000 - 0x0700_0000);
}

#[test]
fn carve_pool_houdt_de_stukken_buiten_elk_gat() {
    let banks = [
        Region::new(0x0000_0000, 0x4000_0000),
        Region::new(0x4000_0000, 0x4000_0000),
    ];
    let holes = [
        Region::new(0x0000_0000, 0x0800_0000), // onderkant
        Region::new(0x3FF0_0000, 0x0020_0000), // over de bankgrens
        Region::new(0x7A12_3456, 0x0000_1000), // rommelig, bovenin
    ];
    let pool = carve_pool(&banks, &holes, 2 << 20).unwrap();
    for h in holes {
        assert!(!overlaps(&pool, h.base, h.size), "raakt {h:?}: {pool:?}");
    }
}

#[test]
fn carve_pool_dropt_stukken_onder_de_minimummaat() {
    let banks = [Region::new(0x1000_0000, 0x0040_0000)];
    let holes = [Region::new(0x1010_0000, 0x0020_0000)];
    assert!(carve_pool(&banks, &holes, 2 << 20).unwrap().is_empty());
}

// --- coalesce_test.go -------------------------------------------------------

#[test]
fn coalesce_gestreepte_map() {
    // 1000 aangrenzende snippers van 1 MB: het Altra-scenario (14-07).
    let base = 0x8000_0000u64;
    let mut regs: Vec<Region> = (0..1000u64)
        .map(|i| Region::new(base + i * (1 << 20), 1 << 20))
        .collect();
    let n = coalesce(&mut regs).unwrap();
    assert_eq!(n, 1);
    assert_eq!(regs[0], Region::new(base, 1000 << 20));
}

#[test]
fn coalesce_gaten_blijven_gaten() {
    let mut regs = [
        Region::new(0x10_0000, 0x10_0000),
        Region::new(0x30_0000, 0x10_0000),
    ];
    assert_eq!(coalesce(&mut regs).unwrap(), 2);
}

#[test]
fn coalesce_overlap_en_volgorde() {
    let mut regs = [
        Region::new(8 << 20, 1 << 20),
        Region::new(4 << 20, 2 << 20),
        Region::new(5 << 20, 3 << 20),
    ];
    assert_eq!(coalesce(&mut regs).unwrap(), 1);
    assert_eq!(regs[0], Region::new(4 << 20, 5 << 20));
    let mut regs = [
        Region::new(0x100_0000, 0x100_0000),
        Region::new(0x140_0000, 0x10_0000),
    ];
    assert_eq!(coalesce(&mut regs).unwrap(), 1);
    assert_eq!(regs[0].size, 0x100_0000);
}

// --- ctrl_offsets_test.go: in hopabi (de control-page woont daar). ---------

// --- ctx_offsets_test.go ----------------------------------------------------

#[test]
fn ctx_revoke_own_line() {
    const LINE: u64 = 64;
    // Alles wat de switcher zelf in het ctx-blok schrijft, met zijn maat.
    let written = [
        ("CTX_STATE", CTX_STATE, 8),
        ("CTX_GPRS", CTX_GPRS, 31 * 8),
        ("CTX_SP", CTX_SP, 16),
        ("CTX_RESUME", CTX_RESUME, 16),
        ("CTX_REGIME", CTX_REGIME, CTX_REGIME_ARM_WORDS * 8),
        ("CTX_WAKE", CTX_WAKE, 8),
        ("CTX_SLEEPS", CTX_SLEEPS, 8),
        ("CTX_KICK_TARGET", CTX_KICK_TARGET, 8),
        ("CTX_WAKES", CTX_WAKES, 8),
    ];
    let rl = CTX_REVOKE / LINE;
    for (name, off, size) in written {
        for o in (off..off + size).step_by(8) {
            assert_ne!(o / LINE, rl, "CTX_REVOKE deelt regel {rl} met {name}");
        }
    }
    const {
        assert!(CTX_REVOKE.is_multiple_of(8));
        assert!(CTX_OFF + CTX_REVOKE + 8 <= CAGE_STRIDE);
    }
}

// --- plan_test.go -----------------------------------------------------------

#[test]
fn use_plan_pool_cannot_overlap_admin_or_itself() {
    type Change = fn(&mut PlanSpec);
    let cases: [(&str, Change); 14] = [
        ("kernel overlaps pool", |p| p.kernel = p.pool[0]),
        ("kernel overlaps scratch", |p| {
            p.kernel = Region::new(p.boot_scratch_pa, 2 << 20);
        }),
        ("kernel alignment", |p| {
            p.kernel = Region::new(0x4000_0001, 32 << 20);
        }),
        ("pool overlap", |p| {
            let r = p.pool[0];
            p.pool.push(r).unwrap();
        }),
        ("overflow", |p| {
            p.pool.clear();
            p.pool
                .push(Region::new(!((2u64 << 20) - 1), 2 << 20))
                .unwrap();
        }),
        ("alignment", |p| p.pool[0].base += 1),
        ("empty region", |p| p.pool[0].size = 0),
        ("control", |p| p.node_ctrl_pa = 0x5000_0000),
        ("cage", |p| p.cage_pa = 0x5000_0000 - 0x800),
        ("scratch", |p| p.boot_scratch_pa = 0x5000_0000 - 0x80),
        ("trap", |p| p.trap_vec_pa = 0x5000_0000),
        ("usb tail", |p| p.usb_dma_pa = 0x5000_0000 - 0x1000),
        ("black box", |p| {
            p.black_box = Region::new(0x5000_0000 - 8, 16);
        }),
        ("admin overflow", |p| p.cage_pa = !0x7ff),
    ];
    for (name, change) in cases {
        let mut p = test_spec();
        change(&mut p);
        assert!(Plan::new(p).is_err(), "{name}: invalid geometry accepted");
    }
}

#[test]
fn the_cage_stays_inside_the_device_window() {
    // QEMU virt: 64 MB vanaf de control-pages, de kooi op +32 MB.
    let mut p = test_spec();
    p.device_window = Region::new(0xc000_0000, 0x0400_0000);
    assert!(Plan::new(p.clone()).is_ok());
    // Een venster dat vóór het einde van de kooi stopt (SLOT_CAP + 1 blokken).
    p.device_window = Region::new(0xc000_0000, 0x0200_0000 + CAGE_STRIDE);
    assert!(matches!(
        Plan::new(p.clone()),
        Err(crate::Error::Overlap { .. })
    ));
    // Een venster boven het begin van de kooi.
    p.device_window = Region::new(0xc200_1000, 0x0400_0000);
    assert!(Plan::new(p).is_err());
}

#[test]
fn pool_of_counts_its_regions() {
    let r = Region::new(0x5000_0000, 2 << 20);
    assert_eq!(pool_of([r, r]).unwrap().len(), 2);
    assert!(matches!(
        pool_of(core::iter::repeat_n(r, POOL_MAX + 1)),
        Err(crate::Error::TooMany { .. })
    ));
}

#[test]
fn use_plan_preserves_ordered_banks_and_nested_admin() {
    let mut p = test_spec();
    p.pool.swap(0, 1);
    let first = p.pool[0];
    // De trap-vectoren liggen bewust in kooi-blok 0.
    let plan = Plan::new(p).unwrap();
    assert_eq!(plan.pool()[0], first, "plan veranderde de eerste bank");
}

#[test]
fn carve_pool_coalesces_duplicate_and_adjacent_banks() {
    let banks = [
        Region::new(4 << 20, 4 << 20),
        Region::new(2 << 20, 4 << 20),
        Region::new(8 << 20, 2 << 20),
    ];
    let pool = carve_pool(&banks, &[Region::new(5 << 20, 0)], 2 << 20).unwrap();
    assert_eq!(pool.as_slice(), &[Region::new(2 << 20, 8 << 20)]);
    assert_eq!(banks[0].base, 4 << 20, "firmware-banken gewijzigd");
}

#[test]
fn carve_pool_rejects_overflow_rather_than_fallback() {
    let bad = Region::new(!7, 16);
    assert!(carve_pool(&[bad], &[], 2 << 20).is_err());
    assert!(carve_pool(&[Region::new(0, 8 << 20)], &[bad], 2 << 20).is_err());
    let out = carve_pool(&[Region::new(!7, 7)], &[], 0).unwrap();
    assert!(
        out.is_empty(),
        "uitlijning liep om naar laag geheugen: {out:?}"
    );
}

#[test]
fn stage_addr_rejects_window_overflow() {
    assert!(stage_addr(!7, 32, 8).is_none());
    assert_eq!(
        stage_addr(0x1000, 0x100, 9),
        Some((0x1000 + 0x100 - 16, 16))
    );
    assert!(stage_addr(0x1000, 0x100, 0).is_none());
    assert!(stage_addr(0x1000, 0x100, 0x100).is_none());
}

#[test]
fn rk3566_reserved_capacity() {
    let (ctrl, cage, trap) = (0x0620_0000u64, 0x0622_0000u64, 0x062f_0000u64);
    let capacity = (trap - cage) / CAGE_STRIDE - 1;
    assert_eq!(capacity, 12);
    assert!(ctrl + (capacity + 1) * CTRL_STRIDE <= cage);
    assert!(cage + (capacity + 1) * CAGE_STRIDE <= trap);
    let mut pool = Pool::new();
    pool.push(Region::new(0x0780_0000, 0x2000_0000)).unwrap();
    let plan = Plan::new(PlanSpec {
        node_ctrl_pa: ctrl,
        cage_pa: cage,
        trap_vec_pa: trap,
        boot_scratch_pa: 0x7f000,
        net_dma_pa: 0x0640_0000,
        usb_dma_pa: 0x06c0_0000,
        pool,
        max_slots: capacity as usize,
        ..PlanSpec::default()
    })
    .unwrap();
    assert_eq!(plan.max_slots(), 12);
    // Slot 13 ligt buiten de reservering, en het plan zegt dat.
    assert!(plan.cage_table_pa(Slot::new(13).unwrap()).is_err());
    assert!(plan.cage_table_pa(Slot::new(12).unwrap()).is_ok());
}

// --- sched_offsets_test.go --------------------------------------------------

#[test]
fn sched_offsets() {
    const LINE: u64 = 64;
    // (naam, offset, maat, geschreven door de kern?)
    let fields = [
        ("SCHED_MBOX_CTX", SCHED_MBOX_CTX, 8, false),
        ("SCHED_MBOX_PC", SCHED_MBOX_PC, 8, false),
        ("SCHED_SCRATCH", SCHED_SCRATCH, 32, false),
        ("SCHED_CURRENT", SCHED_CURRENT, 8, false),
        ("SCHED_ROTOR", SCHED_ROTOR, 8, false),
        ("SCHED_TICK_TICKS", SCHED_TICK_TICKS, 8, true),
        ("SCHED_CURSOR", SCHED_CURSOR, 8, true),
        ("SCHED_COUNT", SCHED_COUNT, 8, true),
        ("SCHED_LIST", SCHED_LIST, SLOT_CAP as u64, true),
        ("SCHED_S2_PA", SCHED_S2_PA, 8, true),
        ("SCHED_CLINT_PA", SCHED_CLINT_PA, 8, true),
        ("SCHED_SLEEP_CAP", SCHED_SLEEP_CAP, 8, true),
        ("SCHED_MSIP_PA", SCHED_MSIP_PA, 8, true),
    ];
    for (i, &(name, off, size, hop)) in fields.iter().enumerate() {
        assert!(off.is_multiple_of(8), "{name} niet gealigneerd");
        assert!(off + size <= PARK_MBOX_LEN, "{name} buiten het blok");
        // De park-mailbox hoort formeel bij de kern maar bestaat alleen op
        // ARM, waar het blok device-gemapt is; de grens geldt voor de rest.
        if !name.starts_with("SCHED_MBOX") {
            assert_eq!(off >= LINE, hop, "SCHRIJVERSGRENS: {name} = {off}");
        }
        for &(other, o, s, _) in &fields[i + 1..] {
            assert!(
                off + size <= o || o + s <= off,
                "OFFSET-COLLISIE: {name} en {other}"
            );
        }
    }
}

// --- lottery_mirror_test.go: niet geport, v3 kent geen boot-hart-loterij. ---

#[test]
fn tail_rekent_uit_twee_waarden() {
    let t = Tail::new(LINK_BASE, 94 << 20).unwrap();
    assert_eq!(t.base(), Pa(LINK_BASE + (94 << 20)));
    assert_eq!(t.ctrl_page(), t.base());
    assert_eq!(t.outbox(), t.base().add(0x1000));
    assert_eq!(t.net_tx(), t.base().add(0x20000));
    assert_eq!(t.net_rx(), t.base().add(0x20000 + 0xF0000));
    assert!(Tail::new(u64::MAX - 0x1000, 0x1000).is_none());
    assert!(Tail::new(0x1000, 8).is_none(), "staart niet op een pagina");
}

#[test]
fn slot_en_core_hebben_een_bewezen_bereik() {
    assert!(Slot::new(0).is_none());
    assert!(Slot::new(SLOT_CAP + 1).is_none());
    let s = Slot::new(SLOT_CAP).unwrap();
    assert_eq!(s.get(), 128);
    assert_eq!(Slot::FIRST.base(), LINK_BASE);
    assert_eq!(Slot::new(2).unwrap().base(), SLOTS_BASE + SLOT_STRIDE);
    assert_eq!(Core::new(2).unwrap().smp_context_id(), Some(129));
    assert_eq!(Core::new(128).unwrap().smp_context_id(), Some(255));
    assert_eq!(Core::new(1).unwrap().smp_context_id(), None);
    assert!(Core::new(SLOT_CAP + 1).is_none());
}

/// De getallen die de Go-kant hard noemt, en de inverse die de kern
/// (de bron van een system-call) en de switch (de gateway) gebruiken.
#[test]
fn net_plan_is_deterministisch() {
    let s = Slot::new(3).unwrap();
    assert_eq!(Ip4(slot_ip4(s)).to_string(), "10.100.0.4");
    assert_eq!(Ip4(HOST_IP4).to_string(), "10.100.0.1");
    assert_eq!(slot_mac(s), [2, 0, 0, 0, 0, 3]);
    assert_eq!(port_ip4(0), HOST_IP4);
    assert_eq!(port_ip4(1), 0x0A64_0002);
    assert_eq!(port_mac(0), HOST_MAC);
    for i in 0..=SLOT_CAP {
        assert_eq!(ip4_port(port_ip4(i)), Some(i));
        if let Some(s) = Slot::new(i) {
            assert_eq!((slot_ip4(s), slot_mac(s)), (port_ip4(i), port_mac(i)));
        }
    }
    assert_eq!(ip4_port(HOST_IP4 - 1), None, ".0");
    assert_eq!(ip4_port(0x0A65_0002), None, "ander subnet");
}

#[test]
fn plan_adressen_en_uitsluiten() {
    let mut plan = Plan::new(test_spec()).unwrap();
    let s1 = Slot::FIRST;
    assert_eq!(
        plan.cage_table_pa(s1).unwrap(),
        Pa(0xc200_0000 + CAGE_STRIDE)
    );
    assert_eq!(
        plan.ctx_pa(s1).unwrap(),
        Pa(0xc200_0000 + CAGE_STRIDE + CTX_OFF)
    );
    let c2 = Core::new(2).unwrap();
    assert_eq!(
        plan.smp_ctx_pa(c2).unwrap(),
        Pa(0xc200_0000 + 2 * CAGE_STRIDE + SMP_CTX_OFF)
    );
    assert!(plan.smp_ctx_pa(Core::new(1).unwrap()).is_err());
    assert_eq!(
        plan.park_mbox_pa(c2).unwrap(),
        Pa(0xc200_0000 + PARK_MBOX_OFF + 2 * PARK_MBOX_LEN)
    );
    assert_eq!(facts_pa(0xb000_0000), 0xb000_0080);
    assert_eq!(plan.flip_stage_pa(), None);
    assert_eq!(plan.ram_base(), HOP_RAM_START);
    assert_eq!(plan.top_addr(), 0xc200_0000 + 129 * CAGE_STRIDE);
    plan.exclude_from_pool(0x6000_0000, 0x20_0000).unwrap();
    assert_eq!(
        plan.pool(),
        &[
            Region::new(0x5000_0000, 0x1000_0000),
            Region::new(0x6020_0000, 0x4fe0_0000),
            Region::new(0xb100_0000, 0x0f00_0000),
        ]
    );
}

// De OS-core (PORT.md beslissing 2): logische core 0 is hij, de app-cores
// zijn de andere fysieke cores op volgorde, en de afbeelding is haar eigen
// inverse.
#[test]
fn os_core_maps_logical_to_physical() {
    let mut spec = test_spec();
    spec.app_cores = 3;
    let plan = Plan::new(spec.clone()).unwrap();
    assert_eq!(plan.os_core(), 0);
    for i in 0..=3 {
        assert_eq!(plan.phys_core(Core::new(i).unwrap()), i);
    }
    spec.os_core = 2;
    let plan = Plan::new(spec.clone()).unwrap();
    let phys: Vec<usize> = (0..=3)
        .map(|i| plan.phys_core(Core::new(i).unwrap()))
        .collect();
    assert_eq!(phys, [2, 0, 1, 3]);
    for p in 0..=3 {
        assert_eq!(plan.phys_core(plan.logical_core(p).unwrap()), p);
    }
    assert_eq!(plan.logical_core(4), None);
    spec.os_core = 4;
    assert!(Plan::new(spec).is_err());
}
