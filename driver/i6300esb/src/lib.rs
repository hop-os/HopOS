//! De watchdog van de Intel 6300ESB (PCI `8086:25ab`): de enige watchdog
//! die QEMU virt kent (`-device i6300esb`, `hw/watchdog/wdt_i6300esb.c`).
//! Linux: `drivers/watchdog/i6300esb.c`, waarvan de registers, de volgorde
//! en de rekensom hier komen (gelezen 23-10, torvalds/linux master).
//!
//! Dit bezit de registers: twee in de PCI-config van de functie (CONFIG op
//! 0x60, LOCK op 0x68) en vier in BAR 0 (TIMER1, TIMER2, GINTSR, RELOAD).
//! Elke schrijf in BAR 0 behalve de ontsluiting zelf moet ontsloten zijn:
//! eerst 0x80 en dan 0x86 in RELOAD, en dan mag er precies één schrijf.
//! Niet van hier: het vinden van de functie en het toewijzen van zijn BAR
//! (het board, met `driver_pcie`), en het beleid (`kern::watchdog`).
//!
//! De teller loopt in twee trappen af: trap 1 (TIMER1, op ijzer een IRQ die
//! Linux uitzet) en dan trap 2 (TIMER2), waarna de reset. Linux schrijft in
//! beide hetzelfde, `secs << 9`, zodat de som van de twee `secs` seconden
//! is ([`timeout_ms`]).
//!
//! De config-registers lezen en schrijven ALLEEN in hun eigen maat: QEMU
//! herkent LOCK alleen als byte en CONFIG alleen als halfwoord, en geeft
//! een dword-toegang aan de gewone config door (`i6300esb_config_write`).
//! Daarom krijgt deze crate het adres van de config-ruimte zelf en geen
//! `driver_pcie::Config`, dat bytes via het dword eromheen leest.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::indexing_slicing))]

use dev::Pa;

/// De vendor (Intel).
pub const VENDOR: u16 = 0x8086;
/// Het device (`PCI_DEVICE_ID_INTEL_ESB_9`).
pub const DEVICE: u16 = 0x25ab;

/// PCI-config: het configregister (halfwoord).
pub const CONFIG: u64 = 0x60;
/// PCI-config: het slotregister van de WDT (byte).
pub const LOCK: u64 = 0x68;

/// BAR 0: de waarde van trap 1 na elke herlaad (dword).
pub const TIMER1: u64 = 0x00;
/// BAR 0: de waarde van trap 2 na elke herlaad (dword).
pub const TIMER2: u64 = 0x04;
/// BAR 0: de interrupt-status.
pub const GINTSR: u64 = 0x08;
/// BAR 0: het herlaadregister (halfwoord), ook de deur van de ontsluiting.
pub const RELOAD: u64 = 0x0c;

/// LOCK: de watchdog-functie (vrijlopend; Linux zet hem nooit).
pub const WDT_FUNC: u8 = 1 << 2;
/// LOCK: aan.
pub const WDT_ENABLE: u8 = 1 << 1;
/// LOCK: op slot tot een reset (nowayout).
pub const WDT_LOCK: u8 = 1 << 0;

/// CONFIG: 1 = GEEN reset na trap 2 (dus 0 laten).
pub const WDT_REBOOT: u16 = 1 << 5;
/// CONFIG: 1 = de teller op 1 MHz in plaats van ~1 kHz.
pub const WDT_FREQ: u16 = 1 << 2;
/// CONFIG: het soort interrupt na trap 1; 0b11 = geen.
pub const WDT_INTTYPE: u16 = 3;

/// RELOAD: de vorige reset kwam van deze watchdog (lezen; schrijven wist).
pub const WDT_TIMEOUT: u16 = 1 << 9;
/// RELOAD: herlaad de teller (de aai).
pub const WDT_RELOAD: u16 = 1 << 8;

/// De eerste stap van de ontsluiting.
pub const UNLOCK1: u16 = 0x80;
/// De tweede stap.
pub const UNLOCK2: u16 = 0x86;

/// De kortste timeout in seconden (Linux `ESB_HEARTBEAT_MIN`).
pub const SECS_MIN: u32 = 1;
/// De langste: de 20 bits van een trap, `secs << 9` (Linux
/// `ESB_HEARTBEAT_MAX`).
pub const SECS_MAX: u32 = 2046;

/// De waarde van één trap voor een timeout van `secs` seconden (Linux:
/// `val = time << 9`, in beide trappen).
#[must_use]
pub const fn preload(secs: u32) -> u32 {
    secs << 9
}

/// De duur van één trap met waarde `p` in nanoseconden, op de ~1 kHz-teller:
/// `p << 15` tikken van de PCI-klok van 30 ns (het model van QEMU,
/// `i6300esb_restart_timer`; op ijzer de 33 MHz-klok gedeeld door 2^15).
#[must_use]
pub const fn stage_ns(p: u32) -> u64 {
    ((p as u64) << 15) * 30
}

/// De timeout van `secs` seconden zoals de teller hem loopt: twee trappen,
/// ongeveer `secs` * 1007 ms.
#[must_use]
pub const fn timeout_ms(secs: u32) -> u64 {
    2 * stage_ns(preload(secs)) / 1_000_000
}

/// Het aantal seconden voor een timeout van minstens `ms`, binnen
/// [`SECS_MIN`] en [`SECS_MAX`].
#[must_use]
pub fn secs_for(ms: u64) -> u32 {
    let secs = u32::try_from(ms.div_ceil(1000)).unwrap_or(SECS_MAX);
    secs.clamp(SECS_MIN, SECS_MAX)
}

/// Wat [`Esb::init`] aantrof.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Hij liep al (de vorige kern, of de firmware): Linux laat hem dan
    /// lopen (`WDOG_HW_RUNNING`).
    pub running: bool,
    /// Hij stond op slot (nowayout): uitzetten kan niet meer.
    pub locked: bool,
    /// De vorige reset kwam van deze watchdog (`WDIOF_CARDRESET`).
    pub fired: bool,
}

/// Eén 6300ESB: zijn config-ruimte en zijn BAR 0.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Esb {
    cfg: Pa,
    regs: Pa,
}

impl Esb {
    /// De functie met zijn config-ruimte op `cfg` (de 4 KiB van de functie
    /// in ECAM) en zijn registers op `regs` (BAR 0, toegewezen, met
    /// memory-decode aan). Het board zorgt dat beide gemapt zijn als
    /// Device.
    #[must_use]
    pub const fn new(cfg: Pa, regs: Pa) -> Self {
        Self { cfg, regs }
    }

    /// De registers (BAR 0).
    #[must_use]
    pub const fn regs(&self) -> Pa {
        self.regs
    }

    /// Ontsluit BAR 0 voor één schrijf (Linux `esb_unlock_registers`).
    fn unlock(&self) {
        dev::write16(self.regs.add(RELOAD), UNLOCK1);
        dev::write16(self.regs.add(RELOAD), UNLOCK2);
    }

    /// LOCK, als byte.
    #[must_use]
    pub fn lock(&self) -> u8 {
        dev::read8(self.cfg.add(LOCK))
    }

    /// Staat hij aan?
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.lock() & WDT_ENABLE != 0
    }

    /// Zet hem klaar zoals Linux `esb_initdevice`: reset na trap 2, de
    /// ~1 kHz-teller, geen interrupt; uit als hij niet al liep; de vlag van
    /// een vorige reset gelezen en gewist, en de teller herladen. Geeft wat
    /// er stond.
    pub fn init(&self) -> Found {
        dev::write16(self.cfg.add(CONFIG), WDT_INTTYPE);
        let lock = self.lock();
        let running = lock & WDT_ENABLE != 0;
        if !running {
            dev::write8(self.cfg.add(LOCK), 0);
        }
        self.unlock();
        let fired = dev::read16(self.regs.add(RELOAD)) & WDT_TIMEOUT != 0;
        self.unlock();
        dev::write16(self.regs.add(RELOAD), WDT_TIMEOUT | WDT_RELOAD);
        Found {
            running,
            locked: lock & WDT_LOCK != 0,
            fired,
        }
    }

    /// Zet beide trappen op `secs` seconden en herlaadt (Linux
    /// `esb_timer_set_heartbeat`): elke schrijf eigen ontsloten.
    pub fn set_secs(&self, secs: u32) {
        let p = preload(secs.clamp(SECS_MIN, SECS_MAX));
        self.unlock();
        dev::write32(self.regs.add(TIMER1), p);
        self.unlock();
        dev::write32(self.regs.add(TIMER2), p);
        self.ping();
    }

    /// Wapent hem: herladen, dan ENABLE, met `nowayout` ook op slot tot
    /// een reset (Linux `esb_timer_start`).
    pub fn start(&self, nowayout: bool) {
        self.ping();
        let lock = if nowayout { WDT_LOCK } else { 0 };
        dev::write8(self.cfg.add(LOCK), WDT_ENABLE | lock);
    }

    /// De aai: ontsluiten en herladen (Linux `esb_timer_keepalive`).
    pub fn ping(&self) {
        self.unlock();
        dev::write16(self.regs.add(RELOAD), WDT_RELOAD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeout_is_two_stages_of_secs_shifted_by_nine() {
        // Eén seconde: 512 << 15 tikken van 30 ns per trap, 503 ms; samen
        // 1006 ms, de "heartbeat" van Linux.
        assert_eq!(preload(1), 512);
        assert_eq!(stage_ns(preload(1)), 503_316_480);
        assert_eq!(timeout_ms(1), 1006);
        // De 12 s van de kern: 12,08 s.
        assert_eq!(secs_for(12_000), 12);
        assert_eq!(timeout_ms(12), 12_079);
        // Naar boven, en binnen de grenzen.
        assert_eq!(secs_for(12_001), 13);
        assert_eq!(secs_for(0), SECS_MIN);
        assert_eq!(secs_for(u64::MAX), SECS_MAX);
        // De grootste past in de 20 bits van een trap.
        assert!(preload(SECS_MAX) <= 0xfffff);
    }

    #[test]
    fn the_registers_are_where_linux_has_them() {
        let mut cfg = [0u32; 64];
        let mut regs = [0u32; 4];
        let e = Esb::new(
            Pa(cfg.as_mut_ptr() as usize as u64),
            Pa(regs.as_mut_ptr() as usize as u64),
        );
        // Gewoon RAM is geen model: de ontsluiting (0x86) staat in RELOAD
        // als hij gelezen wordt, dus geen vorige reset. Dat de vlag van QEMU
        // (0x1200) wel aankomt, toetst de ring (tools/qemu-test-hop-fail.sh).
        let found = e.init();
        assert_eq!(
            found,
            Found {
                running: false,
                locked: false,
                fired: false
            }
        );
        // CONFIG: geen interrupt, reset na trap 2, ~1 kHz.
        assert_eq!(cfg[(CONFIG / 4) as usize] & 0xffff, u32::from(WDT_INTTYPE));
        e.set_secs(12);
        assert_eq!(regs[(TIMER1 / 4) as usize], 12 << 9);
        assert_eq!(regs[(TIMER2 / 4) as usize], 12 << 9);
        e.start(false);
        assert!(e.enabled());
        // De laatste schrijf in RELOAD is de aai, niet de ontsluiting.
        assert_eq!(regs[(RELOAD / 4) as usize] & 0xffff, u32::from(WDT_RELOAD));
        e.start(true);
        assert_eq!(e.lock(), WDT_ENABLE | WDT_LOCK);
    }
}
