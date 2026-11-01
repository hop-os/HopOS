//! Host-tests van de switcher-API: de descriptor en de som, de thunks, het
//! startschot en de ctx-lezers, over buffers. De assembly bewijst het board.

use super::*;
use crate::el2::harness::{Buf, Kit, plan};
use crate::el2::roster::{self, forget, residents};
use abi::checksum::fnv64;
use abi::layout::{CTX_BOOT_PC, CTX_NEXT_PA, CTX_OFF, CTX_SLEEPS, CTX_SMP, SCHED_COUNT};

fn core(i: usize) -> Core {
    Core::new(i).unwrap()
}

#[test]
fn magic_spells_hopswtc1() {
    assert_eq!(&SWITCH_MAGIC.to_le_bytes(), b"HOPSWTC1");
}

#[test]
fn descriptor_layout_and_sum() {
    let buf = Buf::new(SWITCH_CODE_MAX as usize, 64);
    let a = [0x11u8; 100];
    let b = [0x22u8; 64];
    let c = [0x33u8; 3];
    let blobs: [&[u8]; 3] = [&a, &b, &c];
    let got = install_blobs(buf.pa(), &blobs).unwrap();

    // Elke blob op een cacheline, na een kop van 64 bytes.
    let base = buf.pa();
    assert_eq!(got.entry, base.add(0x40));
    assert_eq!(got.tramp, base.add(0x40 + 128));
    assert_eq!(got.smp_tramp, base.add(0x40 + 128 + 64));
    assert_eq!(got.len, 0x40 + 128 + 64 + 64);
    assert_eq!(dev::read64(base), SWITCH_MAGIC);
    assert_eq!(dev::read64(base.add(SW_LEN)), got.len);
    assert_eq!(dev::read64(base.add(SW_ENTRY)), 0x40);
    assert_eq!(dev::read64(base.add(SW_TRAMP)), 0xC0);
    assert_eq!(dev::read64(base.add(SW_SMP)), 0x100);

    // De som is FNV-1a-64 over de blobs aaneen, in kopieervolgorde: dezelfde
    // som die een kern-flip over een nieuwe bundel rekent.
    let mut flat = Vec::new();
    flat.extend_from_slice(&a);
    flat.extend_from_slice(&b);
    flat.extend_from_slice(&c);
    assert_eq!(got.hash, fnv64(&flat));
    assert_eq!(dev::read64(base.add(SW_HASH)), got.hash);

    // De bytes staan er echt.
    let mut out = [0u8; 100];
    dev::copy_out(&mut out, got.entry);
    assert_eq!(out, a);
    let mut out = [0u8; 3];
    dev::copy_out(&mut out, got.smp_tramp);
    assert_eq!(out, c);
}

#[test]
fn adoption_reads_without_writing_and_refuses_foreign_code() {
    let buf = Buf::new(SWITCH_CODE_MAX as usize, 64);
    let a = [1u8; 40];
    let b = [2u8; 40];
    let c = [3u8; 40];
    let ours: [&[u8]; 3] = [&a, &b, &c];
    let installed = install_blobs(buf.pa(), &ours).unwrap();
    let before = buf.mem.clone();
    assert_eq!(adopt_blobs(buf.pa(), &ours), Ok(installed));
    assert_eq!(before, buf.mem, "adoption wrote to the resident copy");

    // Andere code: weigeren, met beide sommen in de fout.
    let d = [4u8; 40];
    let theirs: [&[u8]; 3] = [&a, &b, &d];
    match adopt_blobs(buf.pa(), &theirs) {
        Err(Error::SwitchCodeMismatch { resident, ours }) => {
            assert_eq!(resident, installed.hash);
            assert_ne!(ours, installed.hash);
        }
        other => panic!("foreign code adopted: {other:?}"),
    }

    // Geen magic: niets te adopteren.
    dev::write64(buf.pa(), 0);
    assert!(matches!(
        adopt_blobs(buf.pa(), &ours),
        Err(Error::SwitchCodeMismatch { .. })
    ));
}

#[test]
fn switch_code_that_does_not_fit_is_refused() {
    let big = vec![0u8; SWITCH_CODE_MAX as usize];
    let blobs: [&[u8]; 3] = [&big, &[1], &[2]];
    assert!(matches!(place(&blobs), Err(Error::SwitchCodeFull { .. })));
}

#[test]
fn host_has_no_switch_code() {
    let (_buf, p) = plan(3);
    assert_eq!(
        install_switch_code(&p, Flavor::Nvhe),
        Err(Error::NoSwitchCode)
    );
    assert_eq!(image_hash(Flavor::AppleVhe), Err(Error::NoSwitchCode));
    assert_eq!(installed_hash(&p), None);
}

#[test]
fn thunk_matches_the_go_generator() {
    // stage2.go: stp x2, x3, [sp, #16]; movz x2, #v; a64.Mov64(3, entry);
    // br x3. Nulhelften worden overgeslagen.
    let (w, n) = thunk(8, Pa(0x4000_A040));
    assert_eq!(
        &w[..n],
        &[
            0xA901_0FE2,
            0xD280_0102,
            0xD294_0803,
            0xF2A8_0003,
            0xD61F_0060
        ]
    );
    // Een adres boven de 4 GB (de Altra-pool): alle vier de helften.
    let (w, n) = thunk(15, Pa(0x0001_2345_6789_ABCD));
    assert_eq!(n, 7);
    assert_eq!(w[1], 0xD280_01E2);
    assert_eq!(w[2], movz(3, 0xABCD, 0));
    assert_eq!(w[3], movk(3, 0x6789, 16));
    assert_eq!(w[4], movk(3, 0x2345, 32));
    assert_eq!(w[5], movk(3, 0x0001, 48));
    assert_eq!(w[6], BR_X3);
}

#[test]
fn init_writes_thunks_park_schedules_and_empty_contexts() {
    let (_buf, p) = plan(3);
    // Vuil, zoals verse DRAM.
    dev::write64(p.park_mbox_pa(core(2)).unwrap().add(SCHED_COUNT), 77);
    dev::write64(p.smp_ctx_pa(core(3)).unwrap(), 0xdead);
    let entry = p.switch_code_pa().add(SW_HEAD);
    let park = [0xAAu8; 48];
    init_region(&p, entry, Some(&park)).unwrap();

    let vecs = p.vec_base_pa();
    for v in 0..VEC_COUNT {
        let (w, n) = thunk(v, entry);
        for (i, ins) in w.iter().take(n).enumerate() {
            assert_eq!(dev::read32(vecs.add(v * VEC_STRIDE + i as u64 * 4)), *ins);
        }
    }
    let mut out = [0u8; 48];
    dev::copy_out(&mut out, p.park_code_pa());
    assert_eq!(out, park);
    for c in 0..=3 {
        let mb = p.park_mbox_pa(core(c)).unwrap();
        assert_eq!(dev::read64(mb.add(SCHED_S2_PA)), vecs.0);
        assert_eq!(dev::read64(mb.add(SCHED_COUNT)), 0);
    }
    // De ctx-blokken van de cores; die van een slot begint de bouw schoon.
    for c in 2..=3 {
        let ctx = p.smp_ctx_pa(core(c)).unwrap();
        assert_eq!(dev::read64(ctx), CtxState::Empty.raw());
    }
}

#[test]
fn a_kept_park_loop_keeps_mailbox_word_zero() {
    let (_buf, p) = plan(3);
    let mb = p.park_mbox_pa(core(1)).unwrap();
    dev::write64(mb, PARK_PARKED);
    dev::write64(p.park_code_pa(), 0x1234);
    init_region(&p, Pa(0x4000_0000), None).unwrap();
    assert_eq!(
        dev::read64(mb),
        PARK_PARKED,
        "the park loop would read 0 as a dispatch"
    );
    assert_eq!(dev::read64(p.park_code_pa()), 0x1234);
    assert!(check_parked(&p).is_ok());
    dev::write64(p.park_mbox_pa(core(2)).unwrap(), 0x5000_1000);
    assert_eq!(
        check_parked(&p),
        Err(Error::UnparkedCore {
            core: 2,
            mbox: 0x5000_1000
        })
    );
}

#[test]
fn dispatch_resets_the_rotation_and_fires_the_mailbox() {
    let (_buf, p) = plan(3);
    init_region(&p, Pa(0x4000_0000), Some(&[0; 4])).unwrap();
    let kit = Kit::new(&p, 3);
    let ctx = kit.ctx(2);
    let mb = p.park_mbox_pa(core(1)).unwrap();
    roster::add(mb, kit.ctx(3)).unwrap();
    roster::add(mb, kit.ctx(1)).unwrap();

    let start = dispatch(&p, core(1), ctx, Pa(0x4000_A100), 0x5100_0000).unwrap();
    assert_eq!(start, Start::Cold);
    assert_eq!(dev::read64(mb.add(SCHED_MBOX_CTX)), 0x5100_0000);
    assert_eq!(dev::read64(mb.add(SCHED_MBOX_PC)), 0x4000_A100);
    assert_eq!(dev::read64(mb.add(SCHED_CURRENT)), ctx.0);
    assert_eq!(dev::read64(mb.add(SCHED_CURSOR)), 0);
    assert_eq!(dev::read64(mb.add(SCHED_COUNT)), 1);
    assert_eq!((roster::get(mb, 0), roster::get(mb, 1)), (ctx.0, 0));
    assert_eq!(ctx_state(ctx), Some(CtxState::Running));
    assert_eq!(core_state(&p, core(1)), Ok(CoreState::Running(0x5100_0000)));

    // Een draaiende core wordt niet gekaapt.
    assert_eq!(
        dispatch(&p, core(1), ctx, Pa(0x4000_A100), 0x5100_0000),
        Err(Error::CoreRunning {
            core: 1,
            mbox: 0x5100_0000
        })
    );
    // Geparkeerd: de SEV wekt hem.
    dev::write64(mb, PARK_PARKED);
    assert_eq!(core_state(&p, core(1)), Ok(CoreState::Parked));
    assert_eq!(
        dispatch(&p, core(1), ctx, Pa(0x4000_A100), 0x5100_0000),
        Ok(Start::Woken)
    );
    // x0 = 0 of 1 leest de lus als koud of geparkeerd.
    assert_eq!(
        dispatch(&p, core(2), ctx, Pa(0), PARK_PARKED),
        Err(Error::BadArg { arg: PARK_PARKED })
    );
}

#[test]
fn smp_handoff_owns_privileges_and_freezes_request() {
    // smp_context_test.go, geport.
    let request = Buf::new(256, 64);
    let handoff = Buf::new(256, 64);
    let (src, dst) = (request.pa(), handoff.pa());
    dev::write64(src.add(CTRL_SMP_SP), 0x5001_2000);
    dev::write64(src.add(CTRL_S2_TABLE), 0xBAD);
    prepare_smp(dst, src, 0x8000_0000, 3, Pa(0x8100_0000), Pa(0x8200_0000));
    dev::write64(src.add(CTRL_SMP_SP), 0x5002_2000);
    for (off, want) in [
        (CTRL_SMP_SP, 0x5001_2000),
        (CTRL_S2_TABLE, 0x8000_0000),
        (CTRL_VMID, 3),
        (CTRL_SMP_MBOX, 0x8100_0000),
        (CTRL_VEC_PA, 0x8200_0000),
    ] {
        assert_eq!(dev::read64(dst.add(off)), want, "handoff[{off:#x}]");
    }
    // Een node-core gebruikt hetzelfde mechanisme in zijn eigen page.
    prepare_smp(src, src, 0, 0, Pa(0), Pa(0x8300_0000));
    assert_eq!(dev::read64(src.add(CTRL_S2_TABLE)), 0);
    assert_eq!(dev::read64(src.add(CTRL_SMP_SP)), 0x5002_2000);
    // De handoff past in de eigen cachelines van het ctx-blok.
    const { assert!(abi::layout::CTX_SMP.is_multiple_of(64)) };
    const { assert!(abi::layout::CTX_SMP + 256 <= CTX_LEN) };
}

#[test]
fn rx_due_wants_an_armed_door_and_a_head_past_it() {
    let ctx = Buf::new(CTX_LEN as usize, 64);
    let page = Buf::new(0x1000, 64);
    let head = Buf::new(64, 64);
    let c = ctx.pa();
    // RX: gewapend en de kop voorbij de drempel.
    ctx_write(c, CTX_CTRL_PA, page.pa().0);
    ctx_write(c, CTX_RING_HEAD_PA, head.base);
    dev::write64(page.pa().add(CTRL_RX_DOOR), RX_DOOR_ARMED | 10);
    dev::write64(head.pa(), 10);
    assert!(
        !rx_due(c),
        "a head that did not pass the threshold is no traffic"
    );
    dev::write64(head.pa(), 11);
    assert!(rx_due(c));
    // Ongewapend: geen peek.
    dev::write64(page.pa().add(CTRL_RX_DOOR), 10);
    assert!(!rx_due(c));
    // Geen control-page: nooit RX.
    dev::write64(page.pa().add(CTRL_RX_DOOR), RX_DOOR_ARMED | 10);
    ctx_write(c, CTX_CTRL_PA, 0);
    assert!(!rx_due(c));
}

#[test]
fn ctx_state_reads_what_the_switcher_writes() {
    let ctx = Buf::new(CTX_LEN as usize, 64);
    for st in [
        CtxState::Empty,
        CtxState::BootPending,
        CtxState::Saved,
        CtxState::Running,
        CtxState::Dead,
    ] {
        ctx_write(ctx.pa(), CTX_STATE, st.raw());
        assert_eq!(ctx_state(ctx.pa()), Some(st));
    }
    ctx_write(ctx.pa(), CTX_STATE, 9);
    assert_eq!(ctx_state(ctx.pa()), None);
    ctx_write(ctx.pa(), CTX_SLEEPS, 42);
    assert_eq!(ctx_read(ctx.pa(), CTX_SLEEPS), 42);
    ctx_write(ctx.pa(), CTX_BOOT_PC, 7);
    ctx_write(ctx.pa(), CTX_NEXT_PA, ctx.base);
    assert_eq!(ctx_read(ctx.pa(), CTX_NEXT_PA), ctx.base);
}

#[test]
fn apple_kick_targets_core_and_cluster() {
    // aff0 = core 3, aff1 = cluster 1 (een P-core van de M4).
    assert_eq!(apple_ipi_target(0x8000_0103), 0x1_0003);
    assert_eq!(apple_ipi_target(0x0000_0000), 0);
}

#[test]
fn revoke_clears_the_tables_of_the_slot_only() {
    let (_buf, p) = plan(3);
    let kit = Kit::new(&p, 3);
    let block = kit.block(2);
    dev::write64(block, 0x1234_0003);
    dev::write64(kit.block(3), 0x5678_0003);
    dev::write64(block.add(CTX_OFF), CtxState::Saved.raw());
    revoke(block);
    assert_eq!(
        dev::read64(kit.block(3)),
        0x5678_0003,
        "a neighbour's table"
    );
    assert_eq!(dev::read64(block), 0);
    assert_eq!(
        dev::read64(block.add(CTX_OFF)),
        CtxState::Saved.raw(),
        "a half-saved context must survive the kill"
    );
}

/// De keuze van de rotatie (`switch.rs`, `.Lrotate`): [`crate::el2::next`]
/// is dezelfde regel in Rust (round-robin vanaf cursor+1, de eerste die
/// boot-pending is, of Saved en aan de beurt op `now`). Schrijft wat de
/// switcher dan schrijft (cursor, current, staat) en geeft het gekozen
/// ctx-blok, of `None` (slapen of parkeren).
fn rotate(p: &Plan, c: Core, now: u64) -> Option<Pa> {
    let mb = p.park_mbox_pa(c).unwrap();
    match crate::el2::next(mb, now) {
        crate::el2::Next::Turn { i, ctx, .. } => {
            dev::write64(mb.add(SCHED_CURSOR), i as u64);
            dev::write64(mb.add(SCHED_CURRENT), ctx.0);
            ctx_write(ctx, CTX_STATE, CtxState::Running.raw());
            Some(ctx)
        }
        crate::el2::Next::Idle { .. } => None,
    }
}

/// Een yield zoals de switcher hem bewaart: Saved met wektijd `wake`.
fn yield_at(ctx: Pa, wake: u64) {
    ctx_write(ctx, CTX_WAKE, wake);
    ctx_write(ctx, CTX_STATE, CtxState::Saved.raw());
}

// Twee kooien op één app-core (share.go, 30-09 naar Rust): de eerste is
// een gewoon startschot, de tweede komt erbij op de draaiende core als
// boot-pending, en daarna wisselen ze elkaar af op hun yields.
#[test]
fn two_residents_take_turns_on_one_app_core() {
    let (_buf, p) = plan(3);
    init_region(&p, Pa(0x4000_0000), Some(&[0; 4])).unwrap();
    let kit = Kit::new(&p, 2);
    let (a, b) = (kit.ctx(1), kit.ctx(2));
    let mb = p.park_mbox_pa(core(1)).unwrap();

    // Koud: join weigert te schrijven, het is een gewoon startschot.
    assert_eq!(
        join(&p, core(1), b, Pa(0x4000_A100), 0x5200_0000),
        Ok(Join::Idle)
    );
    assert_eq!(ctx_state(b), Some(CtxState::Empty));
    assert_eq!(
        dispatch(&p, core(1), a, Pa(0x4000_A100), 0x5100_0000),
        Ok(Start::Cold)
    );

    // De tweede erbij: boot-pending, met zijn trampoline en control-page.
    assert_eq!(
        join(&p, core(1), b, Pa(0x4000_A100), 0x5200_0000),
        Ok(Join::Joined)
    );
    assert_eq!(ctx_state(b), Some(CtxState::BootPending));
    assert_eq!(ctx_read(b, CTX_BOOT_PC), 0x4000_A100);
    assert_eq!(ctx_read(b, CTX_BOOT_ARG), 0x5200_0000);
    let mut ids = Vec::new();
    assert_eq!(residents(&p, core(1), |x| ids.push(x)), Ok(2));
    assert_eq!(ids, [a, b]);
    // Twee keer erbij is één keer.
    join(&p, core(1), b, Pa(0x4000_A100), 0x5200_0000).unwrap();
    assert_eq!(dev::read64(mb.add(SCHED_COUNT)), 2);

    // A yieldt tot t=100: B boot. B yieldt tot t=50: op t=10 is niemand aan
    // de beurt (de switcher slaapt), op t=60 B, op t=100 A.
    yield_at(a, 100);
    assert_eq!(rotate(&p, core(1), 0), Some(b));
    yield_at(b, 50);
    assert_eq!(rotate(&p, core(1), 10), None);
    assert_eq!(rotate(&p, core(1), 60), Some(b));
    yield_at(b, 500);
    assert_eq!(rotate(&p, core(1), 100), Some(a));
    // Om de beurt: met beide aan de beurt wint de volgende na de cursor.
    yield_at(a, 0);
    yield_at(b, 0);
    let turns: Vec<Pa> = (0..4)
        .map(|_| {
            let x = rotate(&p, core(1), 1000).unwrap();
            yield_at(x, 0);
            x
        })
        .collect();
    assert_eq!(turns, [b, a, b, a]);

    // Een lid eraf: A uit de lijst en dood (hij lag geyield), B draait door.
    assert_eq!(evict(&p, core(1), a), Ok(true));
    assert_eq!(ctx_state(a), Some(CtxState::Dead));
    assert_eq!(rotate(&p, core(1), 1000), Some(b));
    assert_eq!(rotate(&p, core(1), 1000), None, "B is running, A is gone");
    // Een draaiende bewoner blijft Running: hij faultt zelf op de intrekking.
    assert_eq!(evict(&p, core(1), b), Ok(true));
    assert_eq!(ctx_state(b), Some(CtxState::Running));
    // Het gat wordt hergebruikt: de lijst groeit niet.
    ctx_write(a, CTX_STATE, CtxState::Empty.raw());
    join(&p, core(1), a, Pa(0x4000_A100), 0x5100_0000).unwrap();
    assert_eq!(dev::read64(mb.add(SCHED_COUNT)), 2);
}

// De hercontrole: een slot dat van de ene core naar de andere verhuist,
// wordt eerst uit elke lijst gehaald; een oude rotatie die zijn byte nog
// las, ziet bij de hercontrole het gat en start hem niet.
#[test]
fn a_forgotten_slot_is_never_booted_by_its_old_core() {
    let (_buf, p) = plan(3);
    init_region(&p, Pa(0x4000_0000), Some(&[0; 4])).unwrap();
    let kit = Kit::new(&p, 3);
    let (a, b) = (kit.ctx(1), kit.ctx(2));
    dispatch(&p, core(1), a, Pa(0x4000_A100), 0x5100_0000).unwrap();
    join(&p, core(1), b, Pa(0x4000_A100), 0x5200_0000).unwrap();
    ctx_write(b, CTX_STATE, CtxState::Dead.raw());
    forget(&p, b).unwrap();
    let mut ids = Vec::new();
    residents(&p, core(1), |x| ids.push(x)).unwrap();
    assert_eq!(ids, [a]);
    // Het blok van B staat nu boot-pending op core 2; core 1 ziet hem niet
    // meer.
    dispatch(&p, core(2), kit.ctx(3), Pa(0x4000_A100), 0x5300_0000).unwrap();
    join(&p, core(2), b, Pa(0x4000_A100), 0x5200_0000).unwrap();
    yield_at(a, 1_000_000);
    assert_eq!(rotate(&p, core(1), 0), None);
    assert_eq!(ctx_state(b), Some(CtxState::BootPending));
}

// Een SMP-eenheid: de secundaire krijgt een eigen ctx-blok met de
// control-page en de eenheid van de primaire, zijn wekdoel meteen, geen
// RX-peek, en de keten loopt rond.
#[test]
fn secondary_contexts_chain_back_to_the_primary() {
    let (_buf, p) = plan(3);
    init_region(&p, Pa(0x4000_0000), Some(&[0; 4])).unwrap();
    let kit = Kit::new(&p, 1);
    let prim = kit.ctx(1);
    let ctrl = Pa(0x5100_0000);
    let vttbr = kit.block(1).0 | 7 << 48;
    dev::write64(p.smp_ctx_pa(core(2)).unwrap().add(CTX_KICK_PENDING), 1);
    let sec = prepare_secondary(&p, core(2), vttbr, ctrl, 0x2).unwrap();
    let sec3 = prepare_secondary(&p, core(3), vttbr, ctrl, 0x3).unwrap();
    assert_eq!(ctx_read(sec, CTX_CTRL_PA), ctrl.0);
    assert_eq!(ctx_read(sec, CTX_VTTBR), vttbr, "the unit's table and VMID");
    assert_eq!(ctx_read(sec, CTX_RING_HEAD_PA), 0);
    assert_eq!(ctx_read(sec, CTX_KICK_TARGET), 0x2);
    assert_eq!(ctx_read(sec, CTX_KICK_PENDING), 0, "a stale latch survived");
    assert_eq!(ctx_state(sec), Some(CtxState::Empty));
    chain(&[prim, sec, sec3]);
    assert_eq!(ctx_read(prim, CTX_NEXT_PA), sec.0);
    assert_eq!(ctx_read(sec, CTX_NEXT_PA), sec3.0);
    assert_eq!(ctx_read(sec3, CTX_NEXT_PA), prim.0);
    // Het startschot van de secundaire: zijn eigen ctx-blok op zijn core.
    dispatch(&p, core(2), sec, Pa(0x4000_A200), sec.0 + CTX_SMP).unwrap();
    let mb = p.park_mbox_pa(core(2)).unwrap();
    assert_eq!(dev::read64(mb.add(SCHED_CURRENT)), sec.0);
    // Eén context: geen schakel meer.
    chain(&[prim]);
    assert_eq!(ctx_read(prim, CTX_NEXT_PA), 0);
}
