//! NVMe over PCIe (Linux `drivers/nvme/host/pci.c`): de O6N en de Altra.
//!
//! Het transport kent geen PCI: het board vindt de controller (klasse
//! 01:08:02), leest BAR0 die de firmware toewees, zet memory-decode en
//! bus-mastering aan, en geeft het adres. De SQ is een ring met een
//! tail-deurbel, de deurbellen liggen op de stride die de controller zelf
//! meldt (CAP.DSTRD); dat doet de core allemaal. Wat hier blijft, is de
//! opstart: CAP lezen en toetsen, en CC met onze eigen entry-maten.
//!
//! De lijn: de admin-CQ en de I/O-CQ melden zich allebei op vector 0
//! (Linux: de admin-queue deelt vector 0 met de eerste I/O-queue). Het
//! board zet die vector op MSI-X ([`Nvme::set_irq`]), en dan slaapt de
//! wachter van `blkdev::Queue` op de bel in plaats van na elke submit per
//! ronde te pollen. Niets maskeren, geen ack: een MSI-X is een flank, en de
//! wachter haalt de hele CQ leeg met één head-deurbel (Linux `nvme_irq`).

use super::{
    ADM_GET_FEATURES, CC_EN, CC_IOCQES, CC_IOSQES, COMMAND_TIMEOUT_NS, Cmd, DB, Error, Nvme,
    Q_ENTRIES, Result,
};
use core::convert::Infallible;
use dev::Pa;

/// Hoeveel van BAR0 het board moet mappen: registers en de doorbells van
/// queue 0 en 1, bij de grootste stride die we accepteren.
pub const MMIO_LEN: u64 = 0x2000;
/// De grootste doorbell-stride die in [`MMIO_LEN`] past (4 << 4 = 64 bytes).
const MAX_DSTRD: u32 = 4;

const _: () = assert!(DB + (4 << (2 + MAX_DSTRD)) <= MMIO_LEN);

/// Het PCIe-transport: niets dan de standaard.
#[derive(Debug)]
pub struct Pci;

impl super::Transport for Pci {
    type Error = Infallible;
}

/// Get Features: Number of Queues. Geen data, mag altijd.
const FEAT_NUM_QUEUES: u32 = 0x07;

impl Nvme<Pci> {
    /// Reset de controller, zet de admin-queue op, identificeert
    /// controller en namespace 1, en meldt het I/O-queue-paar aan. `now`
    /// geeft monotone nanoseconden.
    ///
    /// # Safety
    ///
    /// `base` is BAR0 van een NVMe-controller, gemapt als Device voor
    /// minstens [`MMIO_LEN`] bytes en voor altijd; memory-decode en
    /// bus-mastering staan aan. `[dma, dma+dma_size)` is gemapt geheugen dat
    /// alleen deze driver en de controller gebruiken, nu en zolang het
    /// programma draait, op een adres dat de controller ziet zoals de CPU;
    /// het datablok (vanaf [`DATA_OFF`](super::DATA_OFF)) Normal, de rest
    /// niet gecached.
    pub unsafe fn new(base: Pa, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        let mut n = Self::at(Pci, base, dma, dma_size, now)?;
        let r = n.regs();
        let [lo, hi] = &r.cap;
        let cap = u64::from(lo.read()) | (u64::from(hi.read()) << 32);
        if cap == u64::MAX {
            return Err(Error::OffBus);
        }
        let mpsmin = (cap >> 48) & 0xf;
        let dstrd = ((cap >> 32) & 0xf) as u32;
        let mqes = (cap & 0xffff) + 1;
        if mpsmin != 0 || dstrd > MAX_DSTRD || mqes < u64::from(Q_ENTRIES) {
            return Err(Error::Unsupported { cap });
        }
        n.dstrd = dstrd;
        // CAP.TO is de langste tijd die de controller voor RDY vraagt, in
        // halve seconden; een trage controller krijgt die ruimte.
        n.timeout_ns = COMMAND_TIMEOUT_NS.max(((cap >> 24) & 0xff) * 500_000_000);

        r.cc.write(0);
        n.wait_ready(false)?;
        n.enable(CC_EN | CC_IOSQES | CC_IOCQES)?;
        n.identify_namespace()?;
        n.create_io_queues()?;
        n.started = true;
        Ok(n)
    }

    /// De zelftest van de lijn: één admin-opdracht zonder data (Get
    /// Features, Number of Queues) die ter plekke wacht; zijn completion op
    /// de admin-CQ geeft een interrupt op vector 0.
    pub fn fire_irq(&mut self) -> Result {
        self.admin(Cmd {
            opc: ADM_GET_FEATURES,
            cdw10: FEAT_NUM_QUEUES,
            ..Cmd::default()
        })
    }
}

#[cfg(test)]
mod tests {
    //! Wat alleen PCI doet: CAP toetsen, CC met eigen maten, de stride. De
    //! rest is de core (`tests.rs`), tegen dezelfde nep-controller.

    use super::*;
    use crate::tests::{machine, now, with};
    use crate::{ADM_CREATE_CQ, ADM_CREATE_SQ, ADM_IDENTIFY, DMA_NEED, MAX_TRANSFER, PAGE};
    use blkdev::{AsyncBlockDevice, BlockIo, Spin, block_on};
    use std::vec;
    use std::vec::Vec;

    fn new(cap: u64, lbads: u8) -> (crate::tests::Mem, Result<Nvme<Pci>>) {
        let m = machine(MMIO_LEN, PAGE);
        dev::write64(m.base, cap);
        with(|c| (c.disk, c.lbads) = (vec![0; 4096 << lbads], lbads));
        // SAFETY: registers en DMA liggen in `m`, dat de toets overleeft.
        let n = unsafe { Nvme::<Pci>::new(m.base, m.dma, DMA_NEED, now) };
        (m, n)
    }

    #[test]
    fn new_brings_the_controller_up_with_its_own_cc_on_the_stride_of_cap() {
        let (m, n) = new(1023 | (2 << 32), 9);
        let mut n = n.unwrap();
        assert_eq!((n.block_size(), n.blocks()), (512, 4096));
        assert_eq!(
            (n.max_transfer(), n.model()),
            (MAX_TRANSFER, "HopOS Fake NVMe")
        );
        // CDW10 [7:0] en CDW11: de CQ met PC en IEN op vector 0.
        let admin: Vec<(u8, u64)> = with(|c| {
            c.log
                .iter()
                .map(|e| (e.opc, e.at & 0xffff_ffff_0000_00ff))
                .collect()
        });
        assert_eq!(
            admin,
            [
                (ADM_IDENTIFY, 1),
                (ADM_IDENTIFY, 0),
                (ADM_CREATE_CQ, (3 << 32) | 1),
                (ADM_CREATE_SQ, (0x1_0001 << 32) | 1)
            ]
        );
        let r = |off: u64| dev::read32(m.base.add(off));
        assert_eq!(r(0x24), (63 << 16) | 63);
        assert_eq!(u64::from(r(0x28)), (m.dma.0 + 0x4000) & 0xffff_ffff);
        assert_eq!(r(0x14), CC_EN | CC_IOSQES | CC_IOCQES);
        // DSTRD 2: deurbellen per 16 bytes. Admin: SQ-tail en CQ-head 4.
        assert_eq!((r(0x1000), r(0x1010)), (4, 4));
        let q = blkdev::Queue::new(&mut n, Spin);
        let mut io = &q;
        block_on(io.write(0, &[1; 512])).unwrap();
        drop(q);
        assert_eq!((r(0x1020), r(0x1030)), (1, 1));
    }

    /// Het board hangt de bel eraan; de wachter slaapt erop. De zelftest
    /// is één admin-opdracht op CID 0 zonder data.
    #[test]
    fn the_board_wires_the_bell_and_the_self_test_is_one_admin_command() {
        static BELL: sync::Signal = sync::Signal::new();
        let (_m, n) = new(1023, 9);
        let mut n = n.unwrap();
        assert!(n.irq().is_none());
        n.set_irq(&BELL);
        assert!(n.irq().is_some_and(|b| core::ptr::eq(b, &BELL)));
        with(|c| c.log.clear());
        n.fire_irq().unwrap();
        let got: Vec<(u8, usize, u16, u64, u64)> = with(|c| {
            c.log
                .iter()
                .map(|e| (e.opc, e.q, e.cid, e.at, e.prp1))
                .collect()
        });
        assert_eq!(got, [(ADM_GET_FEATURES, 0, 0, 7, 0)]);
    }

    #[test]
    fn the_controller_is_checked_before_it_is_trusted() {
        for (cap, lbads, want) in [
            (u64::MAX, 9, Error::OffBus),
            (
                1023 | (1 << 48),
                9,
                Error::Unsupported {
                    cap: 1023 | (1 << 48),
                },
            ),
            (
                1023 | (5 << 32),
                9,
                Error::Unsupported {
                    cap: 1023 | (5 << 32),
                },
            ),
            (31, 9, Error::Unsupported { cap: 31 }),
            (
                1023,
                8,
                Error::Namespace {
                    blocks: 4096,
                    lbads: 8,
                },
            ),
        ] {
            assert_eq!(new(cap, lbads).1.err(), Some(want), "CAP {cap:#x}");
        }
    }
}
