//! De USB-invoer van de Pi's: wat de Pi 4 en de Pi 5 delen. Het stuk
//! DMA-geheugen ([`USB_DMA`]), de firmware-handshake die de VL805 van de
//! Pi 4 zijn firmware laat laden ([`notify_xhci_reset`],
//! [`vl805_handshake`]), en een
//! Device-gigabyte voor een PCIe-venster buiten de vaste tabel
//! ([`map_device_gb`]). Welke controllers er zijn, weet de SoC
//! (`Soc::usb_hosts`: de RP1 op de Pi 5, de VL805 op de Pi 4).
//!
//! Alleen met de bouwsteen `usb` (de gui- of de media-smaak); kaal kent
//! het board geen USB.

use crate::Soc;
use board::{Region, UsbHosts};
use dev::Pa;

/// Het USB-DMA-stuk: 2 MB in de DMA-regio (Normal-NC in de vaste tabel,
/// onder 1 GB), boven de NIC-helft en de mailbox-buffer. De 2 MB-korrel is
/// die van `abi::layout::USB_DMA_SIZE`; de VL805 van de Pi 4 is 32-bit
/// DMA, dus laag is een eis (Go legde hem op 0x1480_0000, waar in v3 de
/// mailbox-buffer staat).
pub const USB_DMA: Region = Region {
    base: Pa(0x14A0_0000),
    size: abi::layout::USB_DMA_SIZE,
};

const _: () = {
    use crate::map::{DMA, VCMAIL_BUF};
    assert!(USB_DMA.base.0 >= VCMAIL_BUF + driver_vcmail::BUFFER_BYTES as u64);
    assert!(USB_DMA.base.0 >= DMA.base + abi::layout::NET_DMA_SIZE);
    assert!(USB_DMA.base.0 + USB_DMA.size <= DMA.base + DMA.size);
    assert!(USB_DMA.base.0.is_multiple_of(0x20_0000));
};

/// Wat een SoC van het board krijgt om zijn controllers te noemen.
#[derive(Copy, Clone, Debug)]
pub struct UsbCtx {
    /// Het DMA-geheugen voor alle controllers samen ([`USB_DMA`]).
    pub dma: Region,
    /// De klok, voor de PCIe-bring-up.
    pub now: fn() -> u64,
}

/// De controllers van dit board (`Board::usb_hosts`).
pub(crate) fn hosts<S: Soc>() -> UsbHosts {
    S::usb_hosts(&UsbCtx {
        dma: USB_DMA,
        now: cpu::idle::now,
    })
}

/// De property-tag die de VideoCore de firmware van de VL805 laat laden
/// (`RPI_FIRMWARE_NOTIFY_XHCI_RESET`, `include/soc/bcm2835/raspberrypi-firmware.h`).
pub const TAG_NOTIFY_XHCI_RESET: u32 = driver_vcmail::tag::NOTIFY_XHCI_RESET;

/// Het apparaatadres voor [`TAG_NOTIFY_XHCI_RESET`]: bus, device en
/// functie zoals Linux ze samenstelt (`rpi_firmware_init_vl805`).
#[must_use]
pub const fn vl805_dev_addr(bus: u8, dev: u8, func: u8) -> u32 {
    (bus as u32) << 20 | (dev as u32 & 0x1f) << 15 | (func as u32 & 0x7) << 12
}

/// Vraagt de VideoCore de firmware van de VL805 te laden (Linux
/// `rpi_reset_reset`, `drivers/reset/reset-raspberrypi.c`): op een Pi 4
/// zonder SPI-EEPROM voor de VL805 (de latere revisies en de CM4) heeft de
/// controller na de PCIe-reset geen firmware. De mailbox is die van het
/// board (één eigenaar, de executor van core 0). Geeft het woord dat de
/// firmware terugschreef; een fout is de reden, in het Engels.
pub fn notify_xhci_reset(dev_addr: u32) -> Result<u32, &'static str> {
    let Ok(mut cell) = crate::MBOX.try_borrow_mut() else {
        return Err("mailbox busy");
    };
    let Some(m) = cell.as_mut() else {
        return Err("no mailbox (discover did not run)");
    };
    m.notify_xhci_reset(dev_addr)
        .map_err(|_| "NOTIFY_XHCI_RESET refused by the firmware")
}

/// Staat er op deze core een SError klaar ([`crate::arch`], ISR_EL1.A)?
/// Voor de diagnose van een PCIe-toegang die asynchroon faalde: de kern
/// neemt hem pas bij de eerste stap naar EL1 (de zelftest van de
/// OS-core), en dan weet niemand meer welke toegang het was.
#[must_use]
pub fn serror_pending() -> bool {
    crate::arch::serror_pending()
}

/// Draait de firmware van de VL805? Config 0x50 (Linux
/// `VL805_PCI_CONFIG_VERSION_OFFSET`) is 0 zonder firmware, en all-ones
/// is geen versie maar een config-read die niemand beantwoordde
/// (`CFG_READ_UR_MODE`).
#[must_use]
pub const fn vl805_is_running(version: u32) -> bool {
    version != 0 && version != u32::MAX
}

/// Hoe lang en hoe vaak de handshake naar de versie kijkt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Vl805Wait {
    /// Stilte na de notify vóór de eerste lezing. Linux slaapt 200 tot
    /// 1000 µs (`usleep_range(200, 1000)`); wij nemen de bovengrens.
    pub first_ns: u64,
    /// Tussen twee lezingen daarna.
    pub step_ns: u64,
    /// Na de notify: wie dan nog geen versie heeft, krijgt er geen.
    pub limit_ns: u64,
}

/// De wachttijden van de Pi 4. Linux leest na hoogstens 1 ms en rekent
/// dan op een draaiende VL805; koud (30-09) stond er na 1 ms nog 0. De
/// grens is de seconde die PCIe een functie na een reset geeft voor ze
/// antwoordt (PCIe r5.0 6.6.1): de VideoCore zet de VL805 pas na onze
/// notify aan het werk, dus ruimer hoeft niet. Om de 10 ms een
/// config-read, zodat een VL805 die nog opstart niet onder de lezingen
/// bedolven raakt.
pub const VL805_WAIT: Vl805Wait = Vl805Wait {
    first_ns: 1_000_000,
    step_ns: 10_000_000,
    limit_ns: 1_000_000_000,
};

/// De stap na een lezing van config 0x50, voor `after` van
/// [`vl805_handshake`].
pub const STEP_VERSION: &str = "a config read of the VL805 version";
/// De stap na de notify, voor `after` van [`vl805_handshake`].
pub const STEP_NOTIFY: &str = "NOTIFY_XHCI_RESET";

/// Hoe de firmware-handshake van de VL805 afliep.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Vl805 {
    /// De firmware draaide al (een versie vóór de notify): niets gevraagd,
    /// zoals Linux.
    Running {
        /// Config 0x50.
        version: u32,
    },
    /// De VideoCore laadde hem en de VL805 meldt een versie.
    Loaded {
        /// Config 0x50.
        version: u32,
        /// Van het antwoord op de notify tot de versie er stond.
        waited_ns: u64,
        /// Het woord dat de firmware in de tag terugschreef.
        reply: u32,
    },
    /// De mailbox nam de notify niet.
    Refused(&'static str),
    /// De notify ging goed, maar tot de grens geen versie.
    Silent {
        /// De laatste lezing van config 0x50.
        last: u32,
        /// Hoe lang er gekeken is.
        waited_ns: u64,
        /// Het woord dat de firmware in de tag terugschreef.
        reply: u32,
    },
}

/// De handshake van de VL805 na een PCIe-reset (Linux
/// `rpi_firmware_init_vl805` en `rpi_reset_reset`): staat er een versie in
/// config 0x50, dan niets; anders de notify, [`Vl805Wait::first_ns`] stil,
/// en dan de versie lezen tot hij er is of de grens verstrijkt. `version`
/// leest config 0x50, `notify` stuurt [`TAG_NOTIFY_XHCI_RESET`], en
/// `after` hoort na elke toegang welke het was (de SError-wacht van het
/// board); zo test de volgorde op de host. De xHCI-registers raakt dit
/// niet aan: dat mag pas na [`Vl805::Running`] of [`Vl805::Loaded`].
pub fn vl805_handshake(
    version: &mut dyn FnMut() -> u32,
    notify: &mut dyn FnMut() -> Result<u32, &'static str>,
    after: &mut dyn FnMut(&'static str),
    now: fn() -> u64,
    wait: Vl805Wait,
) -> Vl805 {
    let v = version();
    after(STEP_VERSION);
    if vl805_is_running(v) {
        return Vl805::Running { version: v };
    }
    let reply = notify();
    after(STEP_NOTIFY);
    let reply = match reply {
        Ok(r) => r,
        Err(e) => return Vl805::Refused(e),
    };
    let t0 = now();
    let mut next = wait.first_ns;
    loop {
        let at = t0.saturating_add(next);
        while now() < at {
            core::hint::spin_loop();
        }
        let v = version();
        after(STEP_VERSION);
        let waited_ns = now().saturating_sub(t0);
        if vl805_is_running(v) {
            return Vl805::Loaded {
                version: v,
                waited_ns,
                reply,
            };
        }
        if waited_ns >= wait.limit_ns {
            return Vl805::Silent {
                last: v,
                waited_ns,
                reply,
            };
        }
        next = next.saturating_add(wait.step_ns).min(wait.limit_ns);
    }
}

/// Mapt gigabyte `gb` als Device in de niveau-1-tabel van dit board, als
/// die regel nog leeg is (een PCIe-venster buiten de vaste tabel: het
/// outbound-venster van de Pi 4 op 0x6_0000_0000). Geeft of de gigabyte
/// nu Device gemapt is.
pub fn map_device_gb<S: Soc>(gb: u64) -> bool {
    let Some(t) = S::tables() else {
        return false;
    };
    if gb >= 512 || t.l2.iter().flatten().any(|l| l.gb == gb) {
        return false;
    }
    let at = t.l1.add(8 * gb);
    let want = cpu::boot::block(gb << 30, cpu::boot::ATTR_DEVICE);
    match dev::read64(at) {
        0 => {
            dev::write64(at, want);
            crate::arch::tables_changed();
            true
        }
        cur => cur == want,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::{Cell, RefCell};

    #[test]
    fn the_vl805_behind_the_bridge_is_bus_1() {
        assert_eq!(vl805_dev_addr(1, 0, 0), 0x0010_0000);
        assert_eq!(vl805_dev_addr(0, 31, 7), 31 << 15 | 7 << 12);
        assert_eq!(TAG_NOTIFY_XHCI_RESET, 0x0003_0058);
    }

    thread_local! {
        /// De klok van deze testdraad, in ns.
        static NOW: Cell<u64> = const { Cell::new(0) };
        /// Wat er gebeurde, in volgorde: ("version", t) of ("notify", t).
        static LOG: RefCell<Vec<(&'static str, u64)>> = const { RefCell::new(Vec::new()) };
    }

    /// Een klok die bij elke lezing 100 µs verspringt.
    fn now() -> u64 {
        NOW.with(|n| {
            let v = n.get();
            n.set(v + 100_000);
            v
        })
    }

    fn at() -> u64 {
        NOW.with(Cell::get)
    }

    fn log(what: &'static str) {
        LOG.with(|l| l.borrow_mut().push((what, at())));
    }

    fn taken() -> Vec<(&'static str, u64)> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    /// Een VL805 die na `after_ns` (na de notify) versie `v` laat zien, en
    /// daarvoor 0; `None` = hij komt nooit.
    fn vl805(after_ns: Option<u64>, v: u32) -> impl FnMut() -> u32 {
        let mut notified = None;
        move || {
            log("version");
            LOG.with(|l| {
                if notified.is_none() {
                    notified = l.borrow().iter().find(|e| e.0 == "notify").map(|e| e.1);
                }
            });
            match (notified, after_ns) {
                (Some(n), Some(a)) if at() >= n + a => v,
                _ => 0,
            }
        }
    }

    fn notify_ok() -> Result<u32, &'static str> {
        log("notify");
        Ok(0)
    }

    /// De SError-wacht van de test: telt alleen de stappen mee.
    fn after(step: &'static str) {
        log(if step == STEP_NOTIFY {
            "after notify"
        } else {
            "after version"
        });
    }

    /// De toegangen zonder de `after`-regels.
    fn accesses(log: &[(&'static str, u64)]) -> Vec<(&'static str, u64)> {
        log.iter()
            .copied()
            .filter(|e| !e.0.starts_with("after"))
            .collect()
    }

    #[test]
    fn a_running_vl805_is_not_notified() {
        taken();
        let mut version = || {
            log("version");
            0x0001_38c0
        };
        let mut notify = notify_ok;
        let mut after = after;
        let r = vl805_handshake(&mut version, &mut notify, &mut after, now, VL805_WAIT);
        assert_eq!(
            r,
            Vl805::Running {
                version: 0x0001_38c0
            }
        );
        assert_eq!(taken().iter().filter(|e| e.0 == "notify").count(), 0);
    }

    #[test]
    fn linux_timing_reads_once_after_a_quiet_millisecond() {
        taken();
        // De warme flips van 30-09: de versie staat er binnen 1 ms.
        let mut version = vl805(Some(500_000), 0x0001_38c0);
        let mut notify = notify_ok;
        let mut after = after;
        let r = vl805_handshake(&mut version, &mut notify, &mut after, now, VL805_WAIT);
        let Vl805::Loaded {
            version: v,
            waited_ns,
            reply,
        } = r
        else {
            panic!("{r:?}");
        };
        assert_eq!((v, reply), (0x0001_38c0, 0));
        assert!((1_000_000..2_000_000).contains(&waited_ns), "{waited_ns}");
        let all = taken();
        let names: Vec<_> = all.iter().map(|e| e.0).collect();
        // Na elke toegang hoort het board welke het was.
        assert_eq!(
            names,
            [
                "version",
                "after version",
                "notify",
                "after notify",
                "version",
                "after version"
            ]
        );
        let log = accesses(&all);
        // Na de notify eerst 1 ms niets.
        assert!(log[2].1 - log[1].1 >= VL805_WAIT.first_ns);
    }

    #[test]
    fn a_slow_cold_vl805_is_waited_for_not_given_up_after_one_read() {
        taken();
        // Koud (30-09): na 1 ms nog 0. Hier komt hij na 250 ms.
        let mut version = vl805(Some(250_000_000), 0x0001_38c0);
        let mut notify = notify_ok;
        let mut after = after;
        let r = vl805_handshake(&mut version, &mut notify, &mut after, now, VL805_WAIT);
        let Vl805::Loaded { waited_ns, .. } = r else {
            panic!("{r:?}");
        };
        assert!(
            (250_000_000..270_000_000).contains(&waited_ns),
            "{waited_ns}"
        );
        let reads = taken().iter().filter(|e| e.0 == "version").count();
        // Eén vóór de notify, één na 1 ms, dan om de 10 ms: geen stortvloed.
        assert!(reads <= 2 + 26, "{reads} reads");
    }

    #[test]
    fn a_silent_vl805_ends_at_the_limit_and_says_so() {
        taken();
        let mut version = vl805(None, 0);
        let mut notify = || {
            log("notify");
            Ok(0x8000_0000)
        };
        let mut after = after;
        let r = vl805_handshake(&mut version, &mut notify, &mut after, now, VL805_WAIT);
        let Vl805::Silent {
            last,
            waited_ns,
            reply,
        } = r
        else {
            panic!("{r:?}");
        };
        assert_eq!((last, reply), (0, 0x8000_0000));
        assert!(waited_ns >= VL805_WAIT.limit_ns);
        assert!(waited_ns < VL805_WAIT.limit_ns + VL805_WAIT.step_ns);
        let reads = taken().iter().filter(|e| e.0 == "version").count();
        assert!(reads <= 2 + 100, "{reads} reads");
    }

    #[test]
    fn all_ones_is_no_version_and_a_refusal_stops_the_handshake() {
        taken();
        assert!(!vl805_is_running(u32::MAX) && !vl805_is_running(0));
        assert!(vl805_is_running(0x0001_38c0));
        let mut version = || {
            log("version");
            u32::MAX
        };
        let mut notify = || {
            log("notify");
            Err("mailbox busy")
        };
        let mut after = after;
        let r = vl805_handshake(&mut version, &mut notify, &mut after, now, VL805_WAIT);
        assert_eq!(r, Vl805::Refused("mailbox busy"));
        let names: Vec<_> = accesses(&taken()).iter().map(|e| e.0).collect();
        assert_eq!(names, ["version", "notify"], "no reads after a refusal");
    }
}
