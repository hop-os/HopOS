//! Cores starten zonder PSCI: de CPU_ON van dit board.
//!
//! Apple silicon heeft geen PSCI en geen firmware-spin-table. Twee wegen,
//! en welke geldt hangt af van wie het bootobject is:
//!
//! - **onder m1n1** staan de secundaire cores in zíjn spin-table (WFE), het
//!   Linux-`cpu-release-addr`-protocol: argument naar target+8, dan het
//!   target-woord, dsb, sev ([`release`]). BEWEZEN op alle negen cores
//!   (28-08, boot 5). Let op, twee lessen van de hop-avond (29-08): m1n1
//!   ROEPT een vrijgegeven core AAN als functie, met zijn eigen MMU nog
//!   aan en op zijn eigen stack; en die core mag geen cache-onderhoud op
//!   set/way doen terwijl de ander loopt (de machine reset erop). De entry
//!   moet dus eerst zelf de SCTLR-bits M/C/I wissen.
//! - **als eigen bootobject** wijst RVBAR van élke core naar ons image
//!   (stub_reset op offset 0) en start je een core met drie schrijfacties in
//!   het PMGR-blok ([`pmgr_start`]); hij komt uit reset met de MMU uit en
//!   wacht op de brievenbus in de scratch ([`mailbox`]). Voor deze
//!   generatie zonder chicken bits (m1n1: `init = NULL` voor beide
//!   M4-core-typen). NIET op ijzer geweest: dat kan pas na de installatie.
//!
//! [`own_cores`] zegt welke weg er is, met de reden. RVBAR is op de M4
//! VERGRENDELD (m1n1's `features_m4` mist `apple_sysregs_unlocked`), dus
//! zolang m1n1 het bootobject is, landt élke core die wij uit reset halen in
//! zíjn vectoren. Of de cores van ons zijn, zegt RVBAR zelf: wijst hij naar
//! een HopOS-stub die zijn brievenbus in onze scratch zoekt, dan wel. Dat
//! geldt ook na een flip, waar de kern niet via de stub binnenkwam.

use crate::fwinfo;
use crate::head::{SCRATCH_PARK_ARG, SCRATCH_PARK_FOR, SCRATCH_PARK_PC, STUB_MAGIC};
use core::fmt;
use dev::Pa;

/// Het PMGR-startblok ligt op deze offset van `/arm-io/pmgr` reg[0] (de
/// familie t8112/t8122/t8132 deelt hem; m1n1 `smp.c`).
pub const PMGR_CPU_START_OFF: u64 = 0x34000;
const PMGR_ENABLE: u64 = 0x4;
const PMGR_START: u64 = 0x8;
/// m1n1 `PMGR_DIE_OFFSET`; de mini heeft één die.
const PMGR_DIE_STEP: u64 = 0x20_0000_0000;
const RVBAR_LOCK: u64 = 1 << 0;
const RVBAR_ADDR: u64 = 0x0000_ffff_ffff_f000;

/// "De brievenbus is vrij": een adres dat geen core heeft (Go 31-08: met 0
/// als "leeg" kon E-core 0, aff 0x0000, nooit werk aannemen).
pub const PARK_FREE: u64 = u64::MAX;
/// Zo lang wachten we op een bevestiging van de vorige core.
const PARK_SPINS: u32 = 1 << 21;

/// Waarom een core niet startte, in de vorm van PSCI-codes waar dat kan
/// (de kern telt ze zo).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Geen core met die index (PSCI -2, INVALID_PARAMS).
    NoCore(usize),
    /// Het is de core waar de kern zelf draait.
    SelfCore(usize),
    /// Onder m1n1 staat deze core niet in zijn spin-table.
    NotParked(usize),
    /// Als eigen bootobject: er is geen PMGR-startblok in de ADT.
    NoPmgr,
    /// De vorige overdracht werd nooit bevestigd.
    MailboxBusy,
    /// De core kreeg zijn startbit maar pakte de brievenbus niet; de
    /// brievenbus is ingetrokken, dus hij liep onze code niet.
    NoAck(usize),
    /// Geen van beide wegen bestaat (geen loader, RVBAR niet van ons).
    NoWay(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCore(c) => write!(f, "apple: no core {c}"),
            Self::SelfCore(c) => write!(f, "apple: core {c} runs the kern"),
            Self::NotParked(c) => write!(f, "apple: core {c} is not in m1n1's spin-table"),
            Self::NoPmgr => f.write_str("apple: no PMGR cpu-start block in the ADT"),
            Self::MailboxBusy => f.write_str("apple: the park mailbox was never acknowledged"),
            Self::NoAck(c) => write!(f, "apple: core {c} was started but never took the mailbox"),
            Self::NoWay(why) => write!(f, "apple: cannot start cores: {why}"),
        }
    }
}

/// De PSCI-code van een fout (-2 INVALID_PARAMS, -3 DENIED, -4 ALREADY_ON).
#[must_use]
pub const fn psci_code(e: Error) -> i32 {
    match e {
        Error::NoCore(_) | Error::NotParked(_) => -2,
        Error::NoPmgr | Error::MailboxBusy | Error::NoAck(_) | Error::NoWay(_) => -3,
        Error::SelfCore(_) => -4,
    }
}

/// Het resetadres van core `i` en of het vergrendeld is.
#[must_use]
pub fn rvbar(i: usize) -> Option<(u64, bool)> {
    let imp = fwinfo::cpu_impl(i).filter(|&a| a != 0)?;
    let v = u64::from(dev::read32(Pa(imp))) | u64::from(dev::read32(Pa(imp + 4))) << 32;
    Some((v & RVBAR_ADDR, v & RVBAR_LOCK != 0))
}

/// Is core `i` van ons? Ja als zijn RVBAR naar een HopOS-stub wijst die
/// zijn brievenbus in ONZE scratch zoekt: het parameterblok op RVBAR+0x100
/// draagt het magic en als doel [`crate::RAM_BASE`]. Dat is precies wat de
/// core uit reset gaat doen (`head.rs`, stub_reset). Vroeger was de maat
/// het adres dat de stub bij de boot opschreef, en een geflipte kern komt
/// niet langs de stub: GEMETEN 01-10 op de M4, M8 weigerde elke koude
/// start ("no boot stub ran") terwijl RVBAR nog naar de stub van de
/// geïnstalleerde kern wees. Zo niet, dan de reden.
pub fn own_cores(i: usize) -> Result<(), &'static str> {
    if fwinfo::cpus() == 0 {
        return Err("no core list in the ADT");
    }
    let (addr, locked) = rvbar(i).ok_or("no cpu-impl-reg in the ADT")?;
    if is_our_stub(addr) {
        return Ok(());
    }
    Err(if locked {
        "RVBAR is locked to another boot object (m1n1)"
    } else {
        "RVBAR does not point at a HopOS stub"
    })
}

/// Staat op `addr` een HopOS-stub met zijn scratch op [`crate::SCRATCH`]?
/// Alleen binnen het DRAM gelezen (daar Device gemapt, gealigneerd).
fn is_our_stub(addr: u64) -> bool {
    let dram = crate::DRAM_BASE..crate::DRAM_BASE + crate::mmu::MAX_DRAM_GB * crate::mmu::GB;
    dram.contains(&addr)
        && dev::read64(Pa(addr + 0x100)) == STUB_MAGIC
        && dev::read64(Pa(addr + 0x108)) == crate::RAM_BASE
}

/// Laat core `i` los uit m1n1's spin-table op `entry` met `ctx` in x0.
pub fn release(i: usize, entry: u64, ctx: u64) -> Result<(), Error> {
    let rel = fwinfo::release_addr(i);
    if rel == 0 {
        return Err(Error::NotParked(i));
    }
    let r = Pa(rel);
    dev::write64(r.add(8), ctx);
    dev::write64(r.add(16), 0);
    dev::write64(r.add(24), 0);
    dev::write64(r.add(32), 0);
    dev::mb();
    dev::write64(r, entry);
    dev::notify();
    Ok(())
}

/// Het PMGR-startblok, uit de ADT.
fn pmgr_base() -> Option<u64> {
    fwinfo::reg("/arm-io/pmgr", 0).map(|(b, _)| b + PMGR_CPU_START_OFF)
}

/// Zet core `i` aan in het PMGR-blok: eerst de systeemkant ("zonder dit
/// werken de interrupts van die core niet", m1n1), dan lopen. Hij begint op
/// zijn RVBAR.
pub fn pmgr_start(i: usize) -> Result<(), Error> {
    let reg = fwinfo::cpu_reg(i).ok_or(Error::NoCore(i))?;
    let base = pmgr_base().ok_or(Error::NoPmgr)?;
    let (core, cluster, die) = (reg & 0xff, (reg >> 8) & 0x7, (reg >> 11) & 0xf);
    if core > 7 {
        return Err(Error::NoCore(i));
    }
    let b = Pa(base + u64::from(die) * PMGR_DIE_STEP);
    dev::write32(b.add(PMGR_ENABLE), 1 << (4 * cluster + core));
    dev::mb();
    dev::write32(b.add(PMGR_START + 4 * u64::from(cluster)), 1 << core);
    dev::mb();
    Ok(())
}

/// De brievenbus in de scratch (stub_reset): argument, entry, en als
/// laatste vóór wie. Wacht eerst tot de vorige overdracht bevestigd is (Go
/// 31-08: wie meteen opgeeft laat de tweede core stil wegvallen). Beide
/// kanten lezen met de MMU uit of ongecachet: de scratch ligt in de
/// kern-RAM, dus vegen we de regel.
pub fn mailbox(aff: u64, entry: u64, ctx: u64) -> Result<(), Error> {
    let s = Pa(crate::SCRATCH);
    let mut free = false;
    for _ in 0..PARK_SPINS {
        dev::pull(s.add(SCRATCH_PARK_FOR), 8);
        let v = dev::read64(s.add(SCRATCH_PARK_FOR));
        if v == PARK_FREE || v == 0 && dev::read64(s.add(SCRATCH_PARK_PC)) == 0 {
            free = true;
            break;
        }
    }
    if !free {
        return Err(Error::MailboxBusy);
    }
    dev::write64(s.add(SCRATCH_PARK_ARG), ctx);
    dev::write64(s.add(SCRATCH_PARK_PC), entry);
    dev::push(s.add(SCRATCH_PARK_ARG), 16);
    dev::mb();
    dev::write64(s.add(SCRATCH_PARK_FOR), aff);
    dev::push(s.add(SCRATCH_PARK_FOR), 8);
    dev::notify();
    Ok(())
}

/// Wacht tot de stub de brievenbus bevestigt (hij zet [`PARK_FREE`] vlak
/// voor zijn sprong).
fn acked() -> bool {
    let s = Pa(crate::SCRATCH);
    (0..PARK_SPINS).any(|_| {
        dev::pull(s.add(SCRATCH_PARK_FOR), 8);
        dev::read64(s.add(SCRATCH_PARK_FOR)) == PARK_FREE
    })
}

/// Trekt de brievenbus in na een core die niet bevestigde, zodat hij onze
/// code nooit meer kan halen: eerst de entry weg (een stub die zijn adres al
/// zag, leest dan 0 en wacht verder), dan nog één wachtronde voor een stub
/// die de entry al las en nu bevestigt, en pas dan het adres weg. Geeft of
/// de core het toch pakte. Zonder dit kon een core die te laat kwam, nog
/// onze code halen vóór de reset die een mislukte start volgt (`fatal`).
fn retract() -> bool {
    let s = Pa(crate::SCRATCH);
    dev::write64(s.add(SCRATCH_PARK_PC), 0);
    dev::push(s.add(SCRATCH_PARK_PC), 8);
    dev::mb();
    if acked() {
        return true;
    }
    dev::write64(s.add(SCRATCH_PARK_FOR), PARK_FREE);
    dev::push(s.add(SCRATCH_PARK_FOR), 8);
    dev::mb();
    false
}

/// De CPU_ON van dit board: start core `i` op `entry` (fysiek adres, EL2,
/// MMU uit of die van m1n1, zie de module-doc) met `ctx` in x0.
pub fn cpu_on(i: usize, here: usize, entry: u64, ctx: u64) -> Result<(), Error> {
    if i >= fwinfo::cpus() {
        return Err(Error::NoCore(i));
    }
    if i == here {
        return Err(Error::SelfCore(i));
    }
    match own_cores(i) {
        Ok(()) => {
            let aff = u64::from(fwinfo::cpu_reg(i).ok_or(Error::NoCore(i))?);
            mailbox(aff, entry, ctx)?;
            if let Err(e) = pmgr_start(i) {
                retract();
                return Err(e);
            }
            if acked() || retract() {
                Ok(())
            } else {
                Err(Error::NoAck(i))
            }
        }
        // Onder m1n1: zijn spin-table (het param-blok van de loader).
        Err(_) if fwinfo::has_params() => release(i, entry, ctx),
        Err(why) => Err(Error::NoWay(why)),
    }
}

/// De CPU_ON van dit board in de vorm van `cpu::smp::CpuOn`: de haak die
/// `discover` zet, zodat de verhuizing naar de OS-core
/// (`cpu::smp::start_one`) en de koude start van een kooi (hopos
/// `cage.rs`) hier uitkomen in plaats van bij een SMC zonder EL3. `target`
/// is het MPIDR (`slots::mpidr`); de fout in PSCI-vorm ([`psci_code`]).
pub fn cpu_on_mpidr(target: u64, entry: u64, ctx: u64) -> Result<(), cpu::psci::Error> {
    let i = fwinfo::core_of(target).ok_or(cpu::psci::Error::InvalidParams)?;
    let here = board::Board::this_core(&crate::Apple::new());
    cpu_on(i, here, entry, ctx).map_err(|e| {
        cpu::println!("cores: cpu {i} (mpidr {target:#x}) not started: {e}");
        let code = i64::from(psci_code(e)) as u64;
        cpu::psci::Error::from_ret(code).unwrap_or(cpu::psci::Error::Other(0))
    })
}

/// Wekt de core met affiniteit `mpidr` met een fast IPI (m1n1's wek op dit
/// silicium: deep WFI plus IPI_RR_GLOBAL, ack via IPI_SR).
pub fn kick(mpidr: u64) {
    cpu::el2::kick(cpu::el2::Flavor::AppleVhe, mpidr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psci_codes() {
        assert_eq!(psci_code(Error::NoCore(11)), -2);
        assert_eq!(psci_code(Error::SelfCore(6)), -4);
        assert_eq!(psci_code(Error::NoWay("x")), -3);
    }

    #[test]
    fn without_a_tree_nothing_starts() {
        assert_eq!(cpu_on(1, 0, 0x1000, 0), Err(Error::NoCore(1)));
        assert!(own_cores(0).is_err());
        assert!(!is_our_stub(0x1000));
        // De haak van `cpu::smp`: een MPIDR zonder core is INVALID_PARAMS.
        assert_eq!(
            cpu_on_mpidr(0x8001_0101, 0x1000, 0),
            Err(cpu::psci::Error::InvalidParams)
        );
    }
}
