//! De telemetrie van de node: de thermiek op de tik en op de heartbeat van
//! Hop, en het klokbeleid (`driver_dvfs::run_after_boot`) als taak op de
//! OS-core.
//!
//! Dit bezit de laatste temperatuurmeting ([`temp_milli_c`]) en de twee
//! taken die hem en de klok bijhouden. De temperatuur gaat elke seconde op
//! de control-page van Hop (`CTRL_TEMP`), zodat Hop hem op zijn heartbeat
//! zet zoals de Go-agent deed (`Temp: board.TempMilliC`, zichtbaar in `hop
//! agents`). De control-page, niet `SLOT_STATUS`: die is van de system-API
//! (een ander spoor), en een woord op de page kost Hop geen call. De
//! thermiek-taak is ook de tik van het slot-zaad (`seed::refresh`); het
//! zaad zelf is van seed.rs.
//!
//! De thermometer en de knop zijn van het board (`board::Thermal`,
//! `board::ClockKnob`): de O6N meet via de SCP (SCMI) en heeft een knop
//! (`_CPC`), de Altra meet via de SMpro (PCC) en laat de klok aan de
//! firmware, de Mac mini meet één keer bij de boot (de SMC) en bewaakt zijn
//! p-states, de Pi's meten en klokken via de VideoCore-mailbox
//! (`board_raspi::clock`), de Radxa meet met de TSADC van de SoC
//! (`board_rk3566::tsadc`) en klokt via SCMI met vdd_cpu over I2C
//! (`board_rk3566::clock`), de LicheeRV meet met de TEMPSEN van de SoC
//! (`board_licheerv::temp`), de rest meet (nog) niets. Het beleid is voor
//! elk board met een knop hetzelfde ([`governor`]), met dezelfde tellers
//! ([`counters`]).

use crate::{BOARD, Machine};
use board::dvfs::{self, Knob as _, SAMPLE_NS};
use board::{Board, ClockKnob, Thermal};
use core::sync::atomic::{AtomicI32, Ordering::Relaxed};
use core::time::Duration;
use counters::SlotHost;
use cpu::println;
use executor::Executor;

/// De laatste meting in milligraden; 0 = geen.
static TEMP: AtomicI32 = AtomicI32::new(0);

/// De laatste temperatuur in milligraden (0 = geen meting), voor de tik.
pub(crate) fn temp_milli_c() -> i32 {
    TEMP.load(Relaxed)
}

/// De temperatuur als tekst voor de tik: `41.5C`, of `-` zonder meting.
pub(crate) struct Temp(pub(crate) i32);

impl core::fmt::Display for Temp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == 0 {
            return f.write_str("-");
        }
        write!(f, "{}.{}C", self.0 / 1000, (self.0 % 1000).abs() / 100)
    }
}

/// De klok vol vlak vóór de sprong van een flip (flip.rs): de vertrekkende
/// kern laat de nieuwe niet op een stille klok landen. Een eigen knop op
/// dezelfde mailbox, bus of fastchannels; de governor-taak komt niet meer
/// aan de beurt. Op een board zonder knop niets.
pub(crate) fn clock_full_for_flip() {
    if !Machine::HAS_KNOB {
        return;
    }
    if let Some(Ok(mut k)) = BOARD.clock_knob(None) {
        match k.full() {
            Some(l) => println!("dvfs: -> {l} (full, flip) HOPOS_CLOCK_EDGE"),
            None => println!("dvfs: clock change to full (flip) failed"),
        }
    }
}

/// Spawnt de thermiek-taak en, als het board een knop heeft, het
/// klokbeleid.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor) {
    BOARD.open_thermal();
    if let Err(e) = exec.spawn(thermal(exec)) {
        println!("hwmon: task not spawned ({e:?}), no temperature");
    }
    governor(exec);
}

/// Het klokbeleid met de knop van het board: `hopos.clock` (`dvfs`, `max`,
/// `quiet`, `firmware`) en `hopos.mhz` (het plafond). De boot-flank gaat
/// synchroon en vóór het net (main: `telemetry::start` gaat vóór
/// `net::start`): de NIC-init hoort op de volle klok, zoals op een koude
/// boot. Na een flip vanuit een stille kern (800 MHz) initialiseerde de NIC
/// op 800 en sprong de klok er meteen na, en twee keer meldde de NIC daarna
/// nooit meer (30-09, generatie 2, de Pi's). Een board zonder knop zegt
/// zelf wat het heeft (`ClockKnob::no_knob`).
fn governor(exec: &'static Executor) {
    if !Machine::HAS_KNOB {
        BOARD.no_knob(exec);
        return;
    }
    let v = BOARD.boot_param("hopos.clock");
    let (hold, ok) = dvfs::hold_of(v);
    if !ok {
        println!("dvfs: hopos.clock={v:?} is not dvfs, max, quiet or firmware; following load");
    }
    let Some(hold) = hold else {
        println!("dvfs: hopos.clock=firmware, the boot operating point stays HOPOS_CLOCK_FIRMWARE");
        return;
    };
    let mhz = BOARD.boot_param("hopos.mhz").parse::<u32>().ok();
    let mut knob = match BOARD.clock_knob(mhz) {
        Some(Ok(k)) => k,
        Some(Err(e)) => {
            println!("dvfs: {e}, the clock stays where the firmware left it HOPOS_CLOCK_NONE");
            return;
        }
        None => {
            BOARD.no_knob(exec);
            return;
        }
    };
    println!(
        "dvfs: {knob}, policy {hold:?}, cap {}, sample {} ms, window 50 ms, cooldown 30 s HOPOS_CLOCK_UP",
        mhz.map_or(0, |m| m),
        SAMPLE_NS / 1_000_000
    );
    let level = knob.full();
    match level {
        Some(l) => println!("dvfs: -> {l} (full, boot) HOPOS_CLOCK_EDGE"),
        None => println!("dvfs: clock change to full (boot) failed, the policy keeps its state"),
    }
    let task = async move {
        let mut host = SlotHost::new(exec);
        dvfs::run_after_boot(knob, hold, &mut host, level).await;
    };
    if let Err(e) = exec.spawn(task) {
        println!("dvfs: task not spawned ({e:?}), the clock stays at full");
    }
}

/// Elke seconde: meten, bewaren, en op de control-page van elke kooi
/// zetten (niet alleen die van Hop: vitals in slot 2 las 0, 30-09); en op
/// dezelfde tik vers zaad op de page van elke kooi (seed.rs), zodat een app
/// zonder verbinding kan herzaaien.
async fn thermal(exec: &'static Executor) {
    loop {
        crate::seed::refresh();
        let t = BOARD.temp_milli_c();
        TEMP.store(t, Relaxed);
        crate::clock::each_page(|_, page| {
            let at = page.add(abi::hopabi::CTRL_TEMP);
            dev::write64(at, i64::from(t) as u64);
            // Een bewoner leest zijn page ook met de MMU uit: naar DRAM.
            dev::push(at, 8);
        });
        exec.after(Duration::from_secs(1)).await;
    }
}

/// De tellers van het klokbeleid, voor elk board met een knop: de slaap van
/// de executor voor de kern (dezelfde als `busy_ms` van de tik en slot 0 van
/// SLOT_STATUS), en voor elk slot de idle-teller en de status op zijn
/// control-page en of zijn bewoner de core wil. De last van de node is de
/// kern plus elke bewoner.
mod counters {
    use board::dvfs::{Host, SAMPLE_NS, Sample};
    use core::time::Duration;
    use cpu::println;
    use executor::Executor;

    /// De tellers voor het beleid: de idle-teller en de status van elk slot
    /// op zijn control-page, en of zijn bewoner nu rekent (het ctx-blok).
    pub(super) struct SlotHost {
        exec: &'static Executor,
    }

    impl SlotHost {
        /// De tellers van `exec`.
        pub(super) fn new(exec: &'static Executor) -> Self {
            Self { exec }
        }

        /// Wil de bewoner van slot `i` de core? Hij rekent (`Running`), of hij
        /// staat klaar: onderbroken of zijn wektijd voorbij (`Saved` en
        /// `due`, dezelfde vraag als de rotatie stelt). Een bewoner die in
        /// zijn yield slaapt, publiceert zijn idle pas bij terugkomst, en
        /// zonder deze vraag las elke slapende app als 100% bezig (23-09).
        /// Alleen `Running` was te weinig: een bewoner van de OS-core is
        /// `Saved` zodra de kern (en dus deze taak) draait, ook als de CNTHP
        /// hem midden in zijn rekenwerk onderbrak.
        fn running(&self, i: kern::Slot) -> bool {
            use abi::layout::CtxState;
            let Some(ctx) = crate::glue::ctx_of(i) else {
                return true;
            };
            match cpu::el2::ctx_state(ctx) {
                None | Some(CtxState::Running) => true,
                Some(CtxState::Saved) => cpu::el2::due(ctx, cpu::idle::counter()).is_none(),
                Some(_) => false,
            }
        }

        /// De slaap van de executor (ns) in tikken van de teller, het
        /// tempo van de idle-tellers van de slots.
        fn kern_idle(&self) -> u64 {
            let ns = self
                .exec
                .stats
                .slept_ns
                .load(core::sync::atomic::Ordering::Relaxed);
            let ticks = u128::from(ns) * u128::from(cpu::idle::freq()) / 1_000_000_000;
            u64::try_from(ticks).unwrap_or(u64::MAX)
        }
    }

    fn word(page: dev::Pa, off: u64) -> u64 {
        dev::pull(page.add(off), 8);
        dev::read64(page.add(off))
    }

    impl Host for SlotHost {
        fn now(&self) -> u64 {
            self.exec.now()
        }

        fn sleep(&self, ns: u64) -> impl core::future::Future<Output = ()> {
            self.exec.after(Duration::from_nanos(ns))
        }

        fn expect(&self) -> u64 {
            cpu::idle::freq().saturating_mul(SAMPLE_NS) / 1_000_000_000
        }

        /// Bron 0 is de kern: de slaap van zijn executor, één core. Op een
        /// gedeelde OS-core is de beurt van een bewoner slaap van de kern;
        /// die bewoner (Hop, of een groep `system`) telt als zijn eigen slot,
        /// met zijn eigen teller.
        /// Bron `i` op plek `i`: de kern, dan elk slot tot de hoogste
        /// gebouwde kooi (een gat is geen bron). Geen vast aantal: zoveel
        /// bronnen als er kooien zijn; een plek die de heap niet geeft, telt
        /// dit sample niet mee.
        fn sample(&mut self, out: &mut alloc::vec::Vec<Sample>) {
            use abi::hopabi::{AppStatus, CTRL_CORES, CTRL_IDLE, CTRL_STATUS};
            let kern = Sample {
                live: true,
                idle: self.kern_idle(),
                cores: 1,
                running: true,
            };
            if out.try_reserve(1).is_err() {
                return;
            }
            out.push(kern);
            crate::clock::each_page(|slot, page| {
                let s = if AppStatus::from_raw(word(page, CTRL_STATUS)) == Some(AppStatus::Ready) {
                    Sample {
                        live: true,
                        idle: word(page, CTRL_IDLE),
                        cores: word(page, CTRL_CORES),
                        running: self.running(slot),
                    }
                } else {
                    Sample::default()
                };
                let i = slot.get();
                if out.try_reserve((i + 1).saturating_sub(out.len())).is_ok() {
                    out.resize(i, Sample::default());
                    out.push(s);
                }
            });
        }

        fn temp_milli_c(&mut self) -> i32 {
            super::temp_milli_c()
        }

        fn log(&self, args: core::fmt::Arguments<'_>) {
            println!("{args}");
        }
    }
}
