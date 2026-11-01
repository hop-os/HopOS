//! Host-tests van de OS-core: de eerste beurt, de rotatie en de
//! intrekking, over buffers. De lijst zelf toetst `roster`. Op de host komt
//! elke beurt terug op een IRQ; de overgang zelf bewijst het board (de
//! zelftest bij boot).

use super::*;
use crate::el2::harness::{Kit, plan};

fn list(plan: &Plan) -> Vec<u64> {
    let sched = os_sched(plan).unwrap();
    (0..roster::len(sched))
        .map(|i| roster::get(sched, i))
        .collect()
}

/// Een bewoner van de OS-core zoals de kooi hem neerzet: de eerste beurt,
/// dan in de rotatie.
fn host_at(plan: &Plan, c: Pa, entry: u64, arg: u64) {
    prepare(c, entry, arg);
    host(plan, c).unwrap();
}

#[test]
fn host_prepares_the_first_turn_and_joins_the_list() {
    let (_b, plan) = plan(2);
    let kit = Kit::new(&plan, 3);
    let c = kit.ctx(1);
    // Rommel in het blok: verse DRAM is geen nul.
    for off in (0..CTX_FP_END).step_by(8) {
        dev::write64(c.add(off), 0xdead_beef);
    }
    host_at(&plan, c, 0x5001_0000, 0xbfe0_0000);
    // Lege FP-registers, en de overgang zet ze terug: niets van de vorige
    // huurder in q0..q31.
    assert!((0..CTX_FPRS_ARM_WORDS).all(|w| dev::read64(c.add(CTX_FPRS + 8 * w)) == 0));
    assert_eq!(dev::read64(c.add(CTX_FP_LIVE)), 1);
    assert_eq!(ctx_state(c), Some(CtxState::BootPending));
    assert_eq!(ctx_read(c, CTX_GPRS), 0xbfe0_0000);
    assert!((1..31).all(|r| ctx_read(c, CTX_GPRS + 8 * r) == 0));
    assert_eq!(ctx_read(c, CTX_RESUME), 0x5001_0000);
    assert_eq!(ctx_read(c, CTX_RESUME + 8), SPSR_EL1H_MASKED);
    assert_eq!(ctx_read(c, CTX_REGIME), SCTLR_EL1_CLEAN);
    assert!((1..CTX_REGIME_ARM_WORDS).all(|w| ctx_read(c, CTX_REGIME + 8 * w) == 0));
    assert_eq!(ctx_read(c, CTX_WAKE), 0);
    assert_eq!(list(&plan), [c.0]);
    assert!(hosts(&plan, c));
    // Idempotent: een tweede keer is geen tweede plek.
    host_at(&plan, c, 0x5001_0000, 0xbfe0_0000);
    assert_eq!(list(&plan), [c.0]);
}

#[test]
fn the_rotation_turns_round_robin_and_sleeps_on_the_earliest_wake() {
    let (_b, plan) = plan(2);
    let kit = Kit::new(&plan, 3);
    let mut os = OsCore::new(&plan, Flavor::Nvhe, None).unwrap();
    assert_eq!(os.run(0), Turn::Idle { wake: None });
    let (c1, c2) = (kit.ctx(1), kit.ctx(2));
    host_at(&plan, c1, 1, 0x1000);
    host_at(&plan, c2, 1, 0x2000);
    // Op de host komt elke beurt terug op een IRQ: Saved en meteen weer aan
    // de beurt. Round-robin vanaf de plek ná de cursor (de laatst
    // geplande, 0 bij de start), zoals de switcher: eerst plek 1, dan 0.
    assert_eq!(os.run(0), Turn::Ran(Back::Irq));
    assert_eq!(ctx_state(c2), Some(CtxState::Saved));
    assert_eq!(ctx_state(c1), Some(CtxState::BootPending));
    assert_eq!(os.run(0), Turn::Ran(Back::Irq));
    assert_eq!(ctx_state(c1), Some(CtxState::Saved));
    // Beide geyield met een wektijd in de toekomst (de host-teller staat
    // op 0): niemand aan de beurt, en de kern slaapt tot de vroegste.
    ctx_write(c1, CTX_WAKE, 500);
    ctx_write(c2, CTX_WAKE, 300);
    assert_eq!(os.run(0), Turn::Idle { wake: Some(300) });
    // Een kick wint van de wektijd.
    ctx_write(c1, CTX_KICK_PENDING, 1);
    assert_eq!(os.run(0), Turn::Ran(Back::Irq));
    assert_eq!(ctx_read(c1, CTX_KICK_PENDING), 0);
    // Een dode of uit de lijst gehaalde bewoner draait nooit meer.
    ctx_write(c1, CTX_STATE, CtxState::Dead.raw());
    roster::remove(os_sched(&plan).unwrap(), c2);
    assert_eq!(os.run(0), Turn::Idle { wake: None });
}

#[test]
fn next_is_the_rule_of_the_switcher() {
    let (_b, plan) = plan(2);
    let kit = Kit::new(&plan, 3);
    let sched = os_sched(&plan).unwrap();
    let (c1, c2) = (kit.ctx(1), kit.ctx(2));
    host_at(&plan, c1, 1, 0x1000);
    host_at(&plan, c2, 1, 0x2000);
    for c in [c1, c2] {
        ctx_write(c, CTX_STATE, CtxState::Saved.raw());
        ctx_write(c, CTX_WAKE, 500);
    }
    assert_eq!(next(sched, 0), Next::Idle { wake: Some(500) });
    // RX achter een gewapende deurbel maakt een slaper meteen due, ook
    // vóór zijn wektijd: de bel van de switch is geen tik.
    let mut page = vec![0u64; 512];
    let mut head = vec![0u64; 8];
    ctx_write(c2, CTX_CTRL_PA, page.as_mut_ptr() as u64);
    ctx_write(c2, abi::layout::CTX_RING_HEAD_PA, head.as_mut_ptr() as u64);
    page[(abi::hopabi::CTRL_RX_DOOR / 8) as usize] = 64 | abi::hopabi::RX_DOOR_ARMED;
    head[0] = 64;
    assert_eq!(next(sched, 0), Next::Idle { wake: Some(500) });
    head[0] = 128;
    assert!(matches!(
        next(sched, 0),
        Next::Turn {
            ctx,
            fresh: false,
            ..
        } if ctx == c2
    ));
    // Ingetrokken terwijl hij aan de beurt was: dood, zonder beurt.
    ctx_write(c2, CTX_REVOKE, 1);
    assert_eq!(next(sched, 0), Next::Idle { wake: Some(500) });
    assert_eq!(ctx_state(c2), Some(CtxState::Dead));
}

// De intrekking op de OS-core (KVM: een aanvraag plus een kick, gelezen
// vlak vóór de ingang): `recall` zet ze, alleen `next` doodt. Een slaper
// met een verre wektijd en een bewoner die nog nooit draaide, gaan dood
// zonder één instructie; de buren draaien door.
#[test]
fn a_recalled_resident_dies_in_next_without_a_turn() {
    let (_b, plan) = plan(2);
    let kit = Kit::new(&plan, 3);
    let sched = os_sched(&plan).unwrap();
    let (c1, c2, c3) = (kit.ctx(1), kit.ctx(2), kit.ctx(3));
    for c in [c1, c2] {
        host_at(&plan, c, 1, 0x1000);
        ctx_write(c, CTX_STATE, CtxState::Saved.raw());
        ctx_write(c, CTX_WAKE, 500);
    }
    // De aanvraag alleen: een slaper leest hem pas als hij aan de beurt is.
    ctx_write(c2, CTX_REVOKE, 1);
    assert_eq!(next(sched, 0), Next::Idle { wake: Some(500) });
    assert_eq!(ctx_state(c2), Some(CtxState::Saved));
    // De kick erbij (`recall`): nu aan de beurt, en dood in plaats van een
    // beurt; de buur slaapt door.
    recall(c2);
    assert_eq!(next(sched, 0), Next::Idle { wake: Some(500) });
    assert_eq!(ctx_state(c2), Some(CtxState::Dead));
    assert_eq!(ctx_state(c1), Some(CtxState::Saved));
    // Nog nooit gedraaid: geen koude start.
    host_at(&plan, c3, 1, 0x3000);
    recall(c3);
    assert_eq!(next(sched, 0), Next::Idle { wake: Some(500) });
    assert_eq!(ctx_state(c3), Some(CtxState::Dead));
    // De dode bytes blijven tot de volgende bouw van hun slot
    // (`roster::forget`); `hosts` zegt dan nog ja, en de kooi telt dood als
    // stil.
    assert!(hosts(&plan, c2));
    assert_eq!(list(&plan), [c1.0, c2.0, c3.0]);
}

#[test]
fn the_kick_is_told_apart_from_an_irq() {
    let (_b, plan) = plan(2);
    let kit = Kit::new(&plan, 3);
    fn kick() -> u32 {
        9
    }
    let bell = Bell {
        sgi1r: 1 << 24,
        sgir: 0,
        intid: 9,
        pending: kick,
    };
    let mut os = OsCore::new(&plan, Flavor::Nvhe, Some(bell)).unwrap();
    host_at(&plan, kit.ctx(1), 1, 0x1000);
    assert_eq!(os.run(0), Turn::Ran(Back::Ipi));
    // Buiten een beurt staat de kick niet scherp.
    let sched = os_sched(&plan).unwrap();
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK)), 0);
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK_PA)), 0);
    os.listen(true);
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK)), 1 << 24);
    os.listen(false);
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK)), 0);
}

#[test]
fn apple_shares_its_os_core_with_the_fast_ipi() {
    let (_b, plan) = plan(2);
    let kit = Kit::new(&plan, 3);
    // De OS-core op P-core 6 (cpu6, MPIDR 0x80010100, GEMETEN 28-08):
    // cluster 1, core 0.
    let bell = Bell::apple(0x8001_0100);
    assert_eq!(bell.sgir, 0, "geen MMIO: de kick is een systeemregister");
    assert_eq!(bell.intid, Bell::APPLE_INTID);
    let mut os = OsCore::new(&plan, Flavor::AppleVhe, Some(bell)).unwrap();
    let sched = os_sched(&plan).unwrap();
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK_PA)), 0);
    os.listen(true);
    let w = dev::read64(sched.add(SCHED_OS_KICK));
    assert_eq!(w, APPLE_KICK_ARMED | (1 << 16));
    assert_eq!(apple_kick_target(w), 1 << 16);
    os.listen(false);
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK)), 0);
    // Op de host wacht er geen IPI: een onderbreking is een device.
    host_at(&plan, kit.ctx(1), 1, 0x1000);
    assert_eq!(os.run(0), Turn::Ran(Back::Irq));
}

#[test]
fn the_apple_kick_word_is_never_zero() {
    // E-core 0 in cluster 0 heeft doel 0; zonder het scherp-bit las de
    // switcher dat als "niet kicken" (de brievenbus-val van Go, 31-08).
    let w = apple_kick_word(0x8000_0000);
    assert_ne!(w, 0);
    assert_eq!(apple_kick_target(w), 0);
    // cpu9: cluster 1, core 3 (MPIDR 0x80010103).
    assert_eq!(
        apple_kick_target(apple_kick_word(0x8001_0103)),
        3 | (1 << 16)
    );
    // Alleen aff0 en aff1 tellen; aff2 (de P-cores) niet.
    assert_eq!(
        apple_kick_target(apple_kick_word(0x8001_0000 | 0x0102)),
        2 | (1 << 16)
    );
}

#[test]
fn the_vector_index_tells_how_a_turn_ended() {
    assert_eq!(exit_of(VEC_SYNC_LOWER), Exit::Sync);
    // IRQ (de GIC, de AIC) en FIQ (Apple: de CNTHP en de fast IPI).
    assert_eq!(exit_of(VEC_IRQ_LOWER), Exit::Interrupt);
    assert_eq!(exit_of(VEC_FIQ_LOWER), Exit::Interrupt);
    // SError en AArch32: een fault.
    for v in 11..16 {
        assert_eq!(exit_of(v), Exit::Fault, "vector {v}");
    }
    // De huidige EL komt hier nooit (de vectoren sturen 0..7 door naar de
    // kern), maar mocht het: een fault, geen stille terugkeer.
    assert_eq!(exit_of(4), Exit::Fault);
    // De timer wint van de kick, de kick van een device.
    assert_eq!(interrupted(true, true), Back::Timer);
    assert_eq!(interrupted(false, true), Back::Ipi);
    assert_eq!(interrupted(false, false), Back::Irq);
}

#[test]
fn a_gicv2_bell_publishes_its_mmio_address() {
    let (_b, plan) = plan(2);

    fn none() -> u32 {
        1023
    }
    let bell = Bell {
        sgi1r: (1 << 16) | 8,
        sgir: 0xff84_1f00,
        intid: 8,
        pending: none,
    };
    let os = OsCore::new(&plan, Flavor::Nvhe, Some(bell)).unwrap();
    let sched = os_sched(&plan).unwrap();
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK_PA)), 0xff84_1f00);
    os.listen(true);
    assert_eq!(dev::read64(sched.add(SCHED_OS_KICK)), (1 << 16) | 8);
}

#[test]
fn the_selftest_spinner_waits_out_a_late_timer() {
    // 30-09: op een volle host kwam de CNTHP van de timer-toets (1 ms) pas na
    // 2042 us, en de spinner gaf op 2 ms al zelf op. De grens is nu twee keer
    // de termijn plus 50 ms; bij 62,5 MHz is dat voor 1 ms 3,2M ticks.
    let hz = 62_500_000;
    let ms = hz / 1000;
    let limit = spin_limit(1000, ms, hz);
    assert_eq!(limit, 1000 + 2 * ms + 50 * ms);
    // De gemeten vertraging past er ruim in, een timer die nooit komt niet.
    assert!(1000 + 2042 * hz / 1_000_000 < limit);
    // Geen overloop bij een absurde termijn: de teller loopt rond, de
    // termijn verzadigt.
    assert_eq!(spin_limit(0, u64::MAX, hz), u64::MAX);
}

#[test]
fn the_selftest_names_the_line_that_interrupted_it() {
    // 30-09, de eerste Pi 5-boot: drie keer `Irq` na 0 us, zonder te zeggen
    // welke lijn. Op de host komt elke beurt terug op vector 9; de bel
    // peekt hier de NIC-lijn van de Pi 5 (INTID 166), die al vóór de
    // overgang pending stond.
    let (_b, plan) = plan(2);

    fn nic() -> u32 {
        166
    }
    fn kick() -> u32 {
        8
    }
    let bell = |pending| Bell {
        sgi1r: (1 << 16) | 8,
        sgir: 0xff84_1f00,
        intid: 8,
        pending,
    };
    let mut os = OsCore::new(&plan, Flavor::Nvhe, Some(bell(nic))).unwrap();
    let p = os.selftest(false, 1000, &|| {}).unwrap();
    assert_eq!(p.back, Back::Irq);
    assert_eq!(p.vec, VEC_IRQ_LOWER);
    assert_eq!((p.before, p.after), (166, 166));
    assert_eq!(p.stale(8), Some(166));
    // De kick zelf is geen oude lijn: dat is de proef die slaagt.
    let mut os = OsCore::new(&plan, Flavor::Nvhe, Some(bell(kick))).unwrap();
    let p = os.selftest(false, 1000, &|| {}).unwrap();
    assert_eq!(p.back, Back::Ipi);
    assert_eq!(p.stale(8), None);
    // Niets pending (1023) en geen bel: niets te melden.
    let none = Probe {
        back: Back::Timer,
        ticks: 0,
        vec: VEC_IRQ_LOWER,
        before: 1023,
        after: 1023,
    };
    assert_eq!(none.stale(8), None);
    assert_eq!(
        Probe {
            before: Probe::NONE,
            ..none
        }
        .stale(8),
        None
    );
}

#[test]
fn the_selftest_drains_first_and_retries_a_device_line() {
    // 04-10, de LicheeRV: elke boot kwamen alle vier de proeven na 5 us
    // terug op `Irq`, de NIC-lijn die sinds `probe_nic` onbehandeld bij de
    // PLIC stond. Vóór elke poging gaat de dispatch-ronde eroverheen; een
    // `Irq` is een tweede poging waard, een ander antwoord niet.
    use core::cell::Cell;
    let drained = Cell::new(0);
    let answers = [Back::Irq, Back::Timer, Back::Yield];
    let at = Cell::new(0);
    let once = || {
        let b = answers[at.get()];
        at.set(at.get() + 1);
        Some(b)
    };
    let r = selftest_tries(|| drained.set(drained.get() + 1), once, |b| *b);
    assert_eq!(r, (Some(Back::Timer), 2));
    assert_eq!(drained.get(), 2);
    // Een lijn die blijft vuren: drie pogingen, en de regel zegt `Irq`.
    drained.set(0);
    let r = selftest_tries(
        || drained.set(drained.get() + 1),
        || Some(Back::Irq),
        |b| *b,
    );
    assert_eq!(r, (Some(Back::Irq), SELFTEST_TRIES));
    assert_eq!(drained.get(), SELFTEST_TRIES);
    // Geen uitkomst (geen heap, geen kooi): niet opnieuw.
    let r = selftest_tries(|| {}, || None::<Back>, |b| *b);
    assert_eq!(r, (None, 1));
}
