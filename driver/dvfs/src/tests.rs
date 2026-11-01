//! Het beleid op de host: een nep-knop die telt, en tellers die we zelf
//! laten lopen.

use super::*;

#[derive(Default)]
struct Knob2 {
    full: u32,
    quiet: u32,
    broken: bool,
}

impl Knob for Knob2 {
    fn full(&mut self) -> Option<Level> {
        self.full += 1;
        (!self.broken).then_some(Level {
            value: 2600,
            unit: "MHz",
        })
    }
    fn quiet(&mut self) -> Option<Level> {
        self.quiet += 1;
        (!self.broken).then_some(Level {
            value: 800,
            unit: "MHz",
        })
    }
}

const EXPECT: u64 = 240_000; // 24 MHz maal 10 ms

/// Een wereld met de kern-core en één slot, die elk sample een fractie
/// idle zijn.
struct World {
    g: Governor,
    k: Knob2,
    now: u64,
    idle: [u64; 2],
    slot: bool,
    running: bool,
    cores: u64,
}

impl World {
    fn new() -> Self {
        let mut w = Self {
            g: Governor::new(),
            k: Knob2::default(),
            now: 0,
            idle: [0; 2],
            slot: false,
            running: true,
            cores: 1,
        };
        let c = w.g.boot(0, &mut w.k);
        assert_eq!((c.full, c.why), (true, "boot"));
        w
    }

    /// Eén sample waarin de kern `hop` en het slot `app` promille idle was
    /// (`app` over al zijn cores).
    fn tick(&mut self, hop: u64, app: u64) -> Option<Change> {
        self.now += SAMPLE_NS;
        self.idle[0] += EXPECT * hop / 1000;
        self.idle[1] += EXPECT * self.cores * app / 1000;
        let s = [
            Sample {
                live: true,
                idle: self.idle[0],
                cores: 1,
                running: true,
            },
            Sample {
                live: self.slot,
                idle: self.idle[1],
                cores: self.cores,
                running: self.running,
            },
        ];
        self.g.step(self.now, EXPECT, &s, &mut self.k)
    }

    /// Stil tot de klok zakt; geeft het aantal samples.
    fn until_quiet(&mut self) -> u64 {
        for n in 1..10_000 {
            if let Some(c) = self.tick(1000, 1000) {
                assert_eq!((c.full, c.why), (false, "idle 30s"));
                return n;
            }
        }
        panic!("never went quiet");
    }
}

#[test]
fn boot_is_full_and_thirty_quiet_seconds_bring_it_down() {
    let mut w = World::new();
    assert!(w.g.is_full());
    let n = w.until_quiet();
    assert_eq!(n, COOLDOWN_NS / SAMPLE_NS + 1);
    assert!(!w.g.is_full());
    assert_eq!((w.k.full, w.k.quiet), (1, 1));
}

#[test]
fn sustained_load_clocks_up_within_two_samples() {
    let mut w = World::new();
    w.slot = true;
    w.until_quiet();
    // Volle last in een venster van vijf stille samples: na één sample is
    // het venster nog 80% idle, na twee 60%, en dan gaat hij vol.
    assert_eq!(w.tick(1000, 0), None);
    assert_eq!(w.tick(1000, 0).map(|c| c.why), Some("busy"));
    assert!(w.g.is_full());
    assert_eq!(w.g.busy.map(|b| b.source), Some(1));
}

#[test]
fn the_kern_counts_once_its_core_is_mostly_busy() {
    let mut w = World::new();
    w.until_quiet();
    // Een console-burst: 23 ms werk in 50 ms is geen last.
    for _ in 0..50 {
        assert_eq!(w.tick(540, 1000), None);
    }
    for _ in 0..WINDOW {
        w.tick(1000, 1000);
    }
    // Een volle OS-core: na vier samples is het venster 20% idle.
    for _ in 0..3 {
        assert_eq!(w.tick(0, 1000), None);
    }
    assert_eq!(w.tick(0, 1000).map(|c| c.why), Some("busy"));
    assert_eq!(w.g.busy.map(|b| b.source), Some(0));
}

#[test]
fn one_busy_sample_among_idle_ones_does_not_clock_up() {
    let mut w = World::new();
    w.until_quiet();
    for _ in 0..WINDOW {
        assert_eq!(w.tick(1000, 1000), None);
    }
    // Een klusje: één sample 40% idle, in een venster van 50 ms dat verder
    // leeg is, blijft boven de 70%.
    assert_eq!(w.tick(400, 1000), None);
    assert!(!w.g.is_full());
}

#[test]
fn a_fresh_slot_calibrates_first_then_counts() {
    let mut w = World::new();
    w.until_quiet();
    w.slot = true;
    w.idle[1] = 123_456_789; // een willekeurige stand: ijken, niet oordelen
    assert_eq!(w.tick(1000, 1000), None);
    // Een app die vanaf seconde één brandt: de teller blijft staan.
    let c = w.tick(1000, 0).unwrap();
    assert_eq!((c.full, c.why), (true, "busy"));
    assert_eq!(w.g.busy.map(|b| b.source), Some(1));
}

#[test]
fn a_sleeping_slot_is_idle_even_when_its_counter_stands_still() {
    let mut w = World::new();
    w.slot = true;
    w.running = false;
    w.tick(1000, 0);
    for _ in 0..50 {
        assert_eq!(w.tick(1000, 0), None, "a yielded app is not busy");
    }
}

#[test]
fn one_burning_core_of_a_four_core_slot_clocks_up() {
    // O6N 03-10: vitals rekende op één core van vier en de klok bleef stil.
    // Over de hele bron is dat 75% idle, boven de oude drempel van 70%.
    let mut w = World::new();
    w.slot = true;
    w.cores = 4;
    w.until_quiet();
    assert_eq!(w.tick(1000, 750), None);
    let c = w.tick(1000, 750).unwrap();
    assert_eq!((c.full, c.why), (true, "busy"));
    let b = w.g.busy.unwrap();
    assert_eq!((b.source, b.idle_permille, b.cores), (1, 900, 4));
}

#[test]
fn many_quiet_cores_with_jitter_stay_quiet() {
    // Tien cores die elk 98% idle zijn: samen 0,2 core werk, geen rekenaar.
    let mut w = World::new();
    w.slot = true;
    w.cores = 10;
    w.until_quiet();
    for _ in 0..50 {
        assert_eq!(w.tick(1000, 980), None);
    }
    assert!(!w.g.is_full());
}

#[test]
fn the_busiest_source_is_named() {
    let mut w = World::new();
    w.slot = true;
    w.until_quiet();
    w.tick(500, 0);
    let c = w.tick(500, 0).unwrap();
    assert_eq!(c.why, "busy");
    // De kern is 50% idle (ook druk), maar het slot brandt: het slot.
    assert_eq!(
        w.g.busy.map(|b| (b.source, b.idle_permille)),
        Some((1, 600))
    );
}

#[test]
fn hold_pins_the_clock_and_release_waits_a_full_cooldown() {
    let mut w = World::new();
    w.slot = true;
    w.g.hold = Hold::Quiet;
    let c = w.tick(0, 0).unwrap();
    assert_eq!((c.full, c.why), (false, "held"));
    assert_eq!(w.tick(0, 0), None, "held quiet under load");
    w.g.hold = Hold::Full;
    assert_eq!(w.tick(1000, 1000).map(|c| c.full), Some(true));
    w.g.hold = Hold::Auto;
    // De drukke samples van de pin staan nog in het venster: de eerste
    // ronde na het loslaten telt als druk, dus één sample langer.
    assert_eq!(w.until_quiet(), COOLDOWN_NS / SAMPLE_NS + 2);
}

#[test]
fn a_refusing_knob_keeps_the_state() {
    let mut w = World::new();
    w.k.broken = true;
    let c = w.until_quiet();
    assert!(c > 0);
    assert!(w.g.is_full(), "the knob refused, so the policy stays full");
    // En het blijft proberen, elke sample na de cooldown.
    assert!(w.tick(1000, 1000).is_some());
}

#[test]
fn the_window_clamps_one_long_sleep() {
    let mut h = History::default();
    // Een klonter van tien samples telt voor vier.
    assert_eq!(h.add(10 * EXPECT, EXPECT), (4 * EXPECT, EXPECT));
    for _ in 0..WINDOW {
        h.add(0, EXPECT);
    }
    assert_eq!(h.add(EXPECT, EXPECT), (EXPECT, WINDOW as u64 * EXPECT));
    assert_eq!(
        Level {
            value: 2600,
            unit: "MHz"
        }
        .to_string(),
        "2600 MHz"
    );
}

/// Een host voor [`run`]: de tijd loopt één sample per slaap, de bronnen
/// zijn een stille kern-core en een slot dat na `busy_from` gaat rekenen.
struct FakeHost {
    now: std::cell::Cell<u64>,
    idle: [u64; 2],
    busy_from: u64,
    log: std::cell::RefCell<std::vec::Vec<std::string::String>>,
}

/// Een slaap die één keer `Pending` zegt: zo is elke poll van `run` één
/// sample.
struct Once(bool);

impl Future for Once {
    type Output = ();
    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        _: &mut core::task::Context<'_>,
    ) -> core::task::Poll<()> {
        if self.0 {
            core::task::Poll::Ready(())
        } else {
            self.0 = true;
            core::task::Poll::Pending
        }
    }
}

impl Host for FakeHost {
    fn now(&self) -> u64 {
        self.now.get()
    }
    fn sleep(&self, ns: u64) -> impl Future<Output = ()> {
        self.now.set(self.now.get() + ns);
        Once(false)
    }
    fn expect(&self) -> u64 {
        EXPECT
    }
    fn sample(&mut self, out: &mut Vec<Sample>) {
        let busy = self.now.get() >= self.busy_from;
        self.idle[0] += EXPECT;
        self.idle[1] += if busy { 0 } else { EXPECT };
        for idle in self.idle {
            out.push(Sample {
                live: true,
                idle,
                cores: 1,
                running: true,
            });
        }
    }
    fn temp_milli_c(&mut self) -> i32 {
        41_500
    }
    fn log(&self, args: fmt::Arguments<'_>) {
        self.log.borrow_mut().push(std::format!("{args}"));
    }
}

#[test]
fn the_task_cools_down_reports_and_wakes_on_load() {
    use std::sync::Arc;
    use std::task::Wake;
    struct Nop;
    impl Wake for Nop {
        fn wake(self: Arc<Self>) {}
    }
    let w = std::task::Waker::from(Arc::new(Nop));
    let mut cx = core::task::Context::from_waker(&w);
    let mut host = FakeHost {
        now: std::cell::Cell::new(0),
        idle: [0; 2],
        busy_from: 40_000_000_000,
        log: std::cell::RefCell::default(),
    };
    let mut knob = Knob2::default();
    {
        let mut fut = core::pin::pin!(run(&mut knob, Hold::Auto, &mut host));
        // 55 s aan samples: 30 s stil (zakken), vanaf 40 s last (opklokken),
        // en de meetregel van 50 s.
        for _ in 0..5_500 {
            let _ = fut.as_mut().poll(&mut cx);
        }
    }
    let log = host.log.borrow();
    assert!(log[0].contains("-> 2600 MHz (full, boot)"), "{}", log[0]);
    assert!(
        log.iter()
            .any(|l| l.contains("-> 800 MHz (quiet, idle 30s)"))
    );
    assert!(
        log.iter().any(|l| l.contains("(full, busy: slot 1 (")),
        "{log:?}"
    );
    let report = log.iter().find(|l| l.ends_with("HOPOS_CLOCK")).unwrap();
    assert!(report.contains("temp 41.5 C"), "{report}");
    assert!(report.ends_with("busy none HOPOS_CLOCK"), "{report}");
    // Na de last: de meetregel noemt het slot dat de klok vol houdt.
    let last = log.iter().rfind(|l| l.ends_with("HOPOS_CLOCK")).unwrap();
    assert!(last.contains("(full)"), "{last}");
    assert!(last.contains("busy slot 1 ("), "{last}");
    assert_eq!((knob.full, knob.quiet), (2, 1));
}

#[test]
fn run_after_boot_skips_the_boot_edge() {
    // De aanroeper zette de klok al vol (vóór het net): geen tweede flank,
    // wel de val naar stil na 30 s en de flank bij last.
    use std::sync::Arc;
    struct Nop;
    impl std::task::Wake for Nop {
        fn wake(self: Arc<Self>) {}
    }
    let w = std::task::Waker::from(Arc::new(Nop));
    let mut cx = core::task::Context::from_waker(&w);
    let mut host = FakeHost {
        now: std::cell::Cell::new(0),
        idle: [0; 2],
        busy_from: 40_000_000_000,
        log: std::cell::RefCell::default(),
    };
    let mut knob = Knob2::default();
    let booted = Some(Level {
        value: 2600,
        unit: "MHz",
    });
    {
        let mut fut = core::pin::pin!(run_after_boot(&mut knob, Hold::Auto, &mut host, booted));
        for _ in 0..4_500 {
            let _ = fut.as_mut().poll(&mut cx);
        }
    }
    let log = host.log.borrow();
    assert!(!log.iter().any(|l| l.contains("boot")), "{log:?}");
    assert!(
        log.iter()
            .any(|l| l.contains("-> 800 MHz (quiet, idle 30s)")),
        "{log:?}"
    );
    assert!(log.iter().any(|l| l.contains("(full, busy: slot 1 (")));
    assert_eq!((knob.full, knob.quiet), (1, 1));
}

#[test]
fn the_report_names_the_last_lifter_while_cooling() {
    let b = Busy {
        source: 3,
        at_ns: 5_000_000_000,
        idle_permille: 0,
        cores: 1,
    };
    let at = |now| std::format!("{}", BusyText { last: Some(b), now });
    assert_eq!(at(6_000_000_000), "slot 3 (0 permille idle)");
    assert_eq!(at(19_000_000_000), "none (last slot 3, 14 s ago)");
    let four = Busy {
        source: 0,
        idle_permille: 750,
        cores: 4,
        ..b
    };
    assert_eq!(
        std::format!("{}", Who(Some(four))),
        "kern (750 permille idle over 4 cores)"
    );
    assert_eq!(std::format!("{}", BusyText { last: None, now: 0 }), "none");
}

#[test]
fn the_config_pins_the_clock() {
    assert_eq!(hold_of(""), (Some(Hold::Auto), true));
    assert_eq!(hold_of("max"), (Some(Hold::Full), true));
    assert_eq!(hold_of("quiet"), (Some(Hold::Quiet), true));
    assert_eq!(hold_of("firmware"), (None, true));
    assert_eq!(hold_of("turbo"), (Some(Hold::Auto), false));
}
