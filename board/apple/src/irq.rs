//! De interrupts van de Mac mini: de AIC voor de lijnen van devices, en
//! twee FIQ-bronnen zonder controller: de timer van de kern en de fast IPI.
//!
//! Het model is dat van `cpu::irq`: de vector zet een vlag en keert terug
//! met I of F gemaskeerd; de dispatch-taak ([`dispatch`]) zet de timer uit,
//! ackt de IPI, laat de dispatcher de AIC-lijnen claimen, en opent I en F
//! weer. Geen logica in exception-context: op de M4 gaf dat onder load een
//! verloren heropening van het I-masker (21-09).
//!
//! Het doel van de AIC (4 bits in het config-woord van elke lijn) staat na
//! iBoot op 0, niemand. Het wordt gevonden met een software-IRQ per
//! kandidaat en ISR_EL1, dat los van DAIF zegt wat er aan déze core wacht
//! (Go `hop/irq.go`, 19-09; hier zonder de exception te nemen).
//!
//! De timer-FIQ is per core een meting ([`timer_wakes`]): op een core die de
//! firmware niet zelf opzette, bereikt de FIQ de core niet (Apple's
//! `CYC_OVRD` en `VM_TMR_FIQ_ENA_EL2` zijn op t8132 vergrendeld: een enkele
//! `mrs` gaf `ESR=0x02000000`, GEMETEN 29-08), en slaapt een WFI daar voor
//! eeuwig terwijl de timer wél afgaat. `apple.TimerWakes` in Go.

use crate::fwinfo;
use core::sync::atomic::Ordering::Relaxed;
use cpu::irq::Line;
use dev::Pa;
use driver_aic::{Aic, Props};
use driver_tg3::{IrqAck, Tg3};
use netdev::AckSlot;
use sync::Signal;

/// De AIC van dit board: leeg tot [`start`], daarna de controller van
/// `cpu::irq`.
pub(crate) static AIC: Aic = Aic::empty();

/// Het ISR_EL1-bit van een wachtende IRQ en FIQ.
const ISR_I: u64 = 1 << 7;
const ISR_F: u64 = 1 << 6;

/// Waarom de interrupts niet opkwamen.
pub(crate) fn start() -> Result<u32, &'static str> {
    let t = fwinfo::adt().ok_or("no device tree")?;
    let chain = t.trace("/arm-io/aic").ok_or("no /arm-io/aic")?;
    let n = chain.node();
    let (base, _) = t.reg_at(&chain, 0).ok_or("no reg on /arm-io/aic")?;
    let p = Props {
        cap0: t.u32(n, "cap0-offset").map_or(4, u64::from),
        max_num_irq: t.u32(n, "maxnumirq-offset").map_or(0xc, u64::from),
        extint_base: t.u32(n, "extint-baseaddress").map_or(0, u64::from),
        iack: t
            .u64(n, "aic-iack-offset")
            .or_else(|| t.u32(n, "aic-iack-offset").map(u64::from))
            .unwrap_or(0),
        glb_cfg: t.u32(n, "aicglbcfg-offset").map_or(0x14, u64::from),
    };
    // SAFETY: het blok komt uit de ADT (`/arm-io/aic` reg[0], GEMETEN 28-08
    // op 0x3_8100_0000), ligt onder 512 GB en is dus Device-gemapt
    // (`mmu::build`); alleen deze driver raakt het aan.
    unsafe { AIC.init(Pa(base), p) }.map_err(|_| "AIC sizes or offsets do not add up")?;
    let target = find_target().ok_or("no AIC target reaches this core")?;
    AIC.set_target(target);
    cpu::irq::use_controller(&AIC);
    Ok(target)
}

/// Zoekt het 4-bit doel dat een IRQ bij DEZE core brengt: per kandidaat een
/// software-IRQ op de hoogste lijn, en ISR_EL1.I lezen. Met I gemaskeerd:
/// de exception komt nooit, de meting wel.
fn find_target() -> Option<u32> {
    let probe = Line(AIC.nr_irq().checked_sub(1)?);
    let mut hit = None;
    for tgt in 0..16 {
        AIC.set_target(tgt);
        if cpu::irq::Controller::enable(&AIC, probe).is_err() {
            return None;
        }
        AIC.soft_raise(probe.0);
        let seen = (0..10_000).any(|_| arch::isr() & ISR_I != 0);
        AIC.soft_clear(probe.0);
        cpu::irq::Controller::disable(&AIC, probe);
        // Het event dat de software-IRQ achterliet, weglezen.
        while cpu::irq::Controller::claim(&AIC).is_some() {}
        if seen {
            hit = Some(tgt);
            break;
        }
    }
    hit
}

/// Hoe de tg3 zijn interrupt krijgt (`hopos.nicirq`). Er is geen "pollen":
/// elke NIC heeft een lijn (docs/sequence-hopos.md §14.1 O3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NicIrq {
    /// Leeg of `auto`: INTA van de ethernet-poort, uit de ADT
    /// (`pcie::port_irq_base` + `pcie::INTA`).
    Auto,
    /// Een getal: die AIC-lijn.
    Line(u32),
}

impl NicIrq {
    /// `None` = geen van de twee.
    pub(crate) fn parse(v: &str) -> Option<Self> {
        match v {
            "" | "auto" => Some(Self::Auto),
            n => n.parse().ok().filter(|&n| n > 0).map(Self::Line),
        }
    }
}

/// De ack van de tg3, gezet vóór de lijn scherp gaat; de dispatch roept
/// hem op HOP's core, waar ook de probe draait.
static NIC_ACK: AckSlot<IrqAck> = AckSlot::new();

/// De device-ack van de NIC-lijn: de interrupt-mailbox dicht, INTA valt.
fn tg3_ack() {
    NIC_ACK.ack();
}

/// De bel van de NIC-lijn: de dispatch luidt hem, de RX-pomp wacht erop.
static NIC_BELL: Signal = Signal::new();

/// Hoe lang de gedwongen eerste interrupt mag doen over de aflevering:
/// Linux' `tg3_test_interrupt` wacht 5 x 10 ms.
const NIC_TEST_NS: u64 = 50_000_000;

/// Maakt AIC-lijn `line` de lijn van de tg3, maar alleen als hij aankomt:
/// lijn scherp, de NIC open (`set_irq` dwingt meteen een interrupt af), en
/// dan de dispatch draaien tot de lijn vuurt. Dat is Go's "gevonden door
/// aflevering" (19-09) als zelftest: een verkeerd nummer is een NIC zonder
/// lijn, met een reden, en geen pomp die nooit iets hoort. Geeft de microseconden tot de aflevering, of de reden met
/// INTSTAT en INTMSK van de poort, gelezen vóór de NIC weer dichtgaat (bij
/// een poort die INTA wél zag, staat bit 0 dan nog).
pub(crate) fn wire_nic(nic: &mut Tg3, line: u32) -> Result<u64, (&'static str, (u32, u32))> {
    NIC_ACK.set(nic.irq_ack());
    let bell = &NIC_BELL;
    cpu::irq::enable(Line(line), Some(tg3_ack), Some(bell)).map_err(|e| {
        let why = match e {
            cpu::irq::Error::NoController => "no AIC",
            _ => "the AIC refused the line",
        };
        (why, crate::pcie::port_intx())
    })?;
    let _ = bell.take();
    nic.set_irq(bell);
    let why = match fires_within(bell, NIC_TEST_NS) {
        // De ack sloot de mailbox, dus nu moet de lijn stil zijn: één
        // naloper mag (de deassert reist nog door de poort), maar blijft
        // hij komen, dan trekt iets anders hem, en een lijn waarvan de ack
        // niets laat zakken, houdt de dispatch voor altijd bezig.
        Some(ns) if hits_within(bell, NIC_QUIET_NS) < NIC_QUIET_MAX => {
            // De eerste lege ring van de pomp heropent de mailbox.
            return Ok(ns / 1_000);
        }
        Some(_) => "the line keeps firing with the NIC masked, not the tg3's",
        None => "the forced interrupt did not arrive within 50 ms",
    };
    let port = crate::pcie::port_intx();
    cpu::irq::Controller::disable(&AIC, Line(line));
    nic.clear_irq();
    Err((why, port))
}

/// Hoe lang een gemaskeerde NIC-lijn stil moet blijven, en hoeveel
/// dispatch-rondes met een treffer daarin nog stil heten.
const NIC_QUIET_NS: u64 = 1_000_000;
const NIC_QUIET_MAX: u32 = 4;

/// Het aantal dispatch-rondes in `ns` waarin `bell` ging.
pub(crate) fn hits_within(bell: &Signal, ns: u64) -> u32 {
    let t0 = cpu::idle::now();
    let mut hits = 0;
    while cpu::idle::now().saturating_sub(t0) <= ns {
        let _ = dispatch();
        hits += u32::from(bell.take());
    }
    hits
}

/// Draait de dispatch tot `bell` gaat (de nanoseconden tot dan) of `ns`
/// verstreken is.
fn fires_within(bell: &Signal, ns: u64) -> Option<u64> {
    let t0 = cpu::idle::now();
    let fired = dev::poll_until(cpu::idle::now, ns, || {
        let _ = dispatch();
        bell.take()
    });
    fired.then(|| cpu::idle::now().saturating_sub(t0))
}

/// Wat één dispatch-ronde zag, voor `board::Dispatched`.
pub(crate) struct Round {
    pub(crate) timer: u32,
    pub(crate) ipi: u32,
    pub(crate) lines: u32,
    pub(crate) unknown: u32,
}

/// Eén dispatch-ronde: de timer uit als hij afging, de IPI geackt, de
/// AIC-lijnen via `cpu::irq`, en I en F weer open.
pub(crate) fn dispatch() -> Round {
    let mut r = Round {
        timer: 0,
        ipi: 0,
        lines: 0,
        unknown: 0,
    };
    if arch::timer_fired() {
        arch::timer_off();
        r.timer = 1;
    }
    if cpu::el2::apple_ipi_ack() {
        cpu::el2::OS_STATS.kicks.fetch_add(1, Relaxed);
        r.ipi = 1;
    }
    let before = cpu::irq::global().stats.unknown.load(Relaxed);
    let pass = cpu::irq::global().dispatch();
    let unknown = cpu::irq::global().stats.unknown.load(Relaxed) - before;
    r.lines = pass.claimed.saturating_sub(unknown as u32);
    r.unknown = unknown as u32;
    arch::unmask();
    r
}

/// Wekt de timer-FIQ op DEZE core een WFI? De veilige vorm van die vraag:
/// de timer op ~100 µs zetten, pollen tot hij afgaat (ISTATUS), en dan
/// ISR_EL1 lezen: staat de FIQ aan deze core, dan wekt hij WFI; zo niet, dan
/// is WFI hier een eeuwige slaap (GEMETEN 29-08 op de gehopte core: de timer
/// ging af, ISR_EL1 bleef 0 voor de FIQ die WFI moest wekken).
///
/// Geeft (de timer ging af, de FIQ stond aan de core).
pub(crate) fn timer_wakes() -> (bool, bool) {
    let (fired, isr) = arch::timer_probe(cpu::idle::freq() / 10_000);
    (fired, fired && isr & ISR_F != 0)
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod arch {
    use core::arch::asm;

    /// ISR_EL1: wat er aan deze core wacht, los van DAIF.
    pub(super) fn isr() -> u64 {
        let v: u64;
        // SAFETY: ISR_EL1 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, isr_el1", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// Ging de timer van de kern af? Onder E2H = 1 is `cntp_*_el0` vanaf
    /// EL2 de EL2-timer (CNTHP): precies die van `cpu::idle`'s WFI.
    pub(super) fn timer_fired() -> bool {
        let v: u64;
        // SAFETY: CNTP_CTL lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, cntp_ctl_el0", out(reg) v, options(nomem, nostack)) };
        v & 0b101 == 0b101
    }

    pub(super) fn timer_off() {
        // SAFETY: zet alleen de eigen timer van deze core uit.
        unsafe { asm!("msr cntp_ctl_el0, xzr", "isb", options(nomem, nostack)) };
    }

    /// Opent I en F. De vector keerde terug met één van beide dicht.
    pub(super) fn unmask() {
        // SAFETY: raakt alleen PSTATE van deze core; de dispatch heeft zijn
        // ronde gedaan, dus een nieuwe interrupt is welkom.
        unsafe { asm!("msr daifclr, #3", options(nomem, nostack)) };
    }

    /// Zie [`super::timer_wakes`]: (ging af, ISR_EL1 op dat moment).
    pub(super) fn timer_probe(ticks: u64) -> (bool, u64) {
        let (mut ctl, isr): (u64, u64);
        // SAFETY: alleen de eigen timer van deze core, met de maskers dicht
        // zodat de FIQ niet genomen wordt; hij staat na afloop weer uit.
        unsafe {
            asm!(
                "mrs {d}, daif",
                "msr daifset, #3",
                "msr cntp_tval_el0, {t}",
                "msr cntp_ctl_el0, {one}",
                "isb",
                "mov {n}, {t}",
                "lsl {n}, {n}, #6",
                "2:",
                "mrs {c}, cntp_ctl_el0",
                "tbnz {c}, #2, 3f",
                "subs {n}, {n}, #1",
                "b.ne 2b",
                "3:",
                "mrs {i}, isr_el1",
                "msr cntp_ctl_el0, xzr",
                "isb",
                "msr daif, {d}",
                d = out(reg) _,
                t = in(reg) ticks,
                one = in(reg) 1u64,
                n = out(reg) _,
                c = out(reg) ctl,
                i = out(reg) isr,
                options(nostack),
            );
        }
        ctl &= 0b100;
        (ctl != 0, isr)
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
mod arch {
    //! Host-stubs: geen timer, niets wacht.
    pub(super) fn isr() -> u64 {
        0
    }
    pub(super) fn timer_fired() -> bool {
        false
    }
    pub(super) fn timer_off() {}
    pub(super) fn unmask() {}
    pub(super) fn timer_probe(_ticks: u64) -> (bool, u64) {
        (false, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nicirq_reads_auto_or_a_line() {
        assert_eq!(NicIrq::parse(""), Some(NicIrq::Auto));
        assert_eq!(NicIrq::parse("auto"), Some(NicIrq::Auto));
        assert_eq!(NicIrq::parse("0"), None);
        assert_eq!(NicIrq::parse("off"), None);
        assert_eq!(NicIrq::parse("1253"), Some(NicIrq::Line(1253)));
        assert_eq!(NicIrq::parse("intx"), None);
        assert_eq!(NicIrq::parse("-1"), None);
    }

    #[test]
    fn without_a_tree_there_is_no_aic() {
        assert_eq!(start(), Err("no device tree"));
        assert_eq!(timer_wakes(), (false, false));
        let r = dispatch();
        assert_eq!((r.timer, r.ipi, r.lines, r.unknown), (0, 0, 0, 0));
    }
}
