//! virtio-net over virtio-mmio of virtio-pci: de virtqueues, DMA-buffers,
//! batching.
//!
//! HopOS' eigen virtio-net-driver (modern / VERSION_1), in de vorm die elke
//! NIC-driver krijgt: split virtqueues (descriptortabel plus avail- en
//! used-ring) en DMA-buffers in een regio die het board uitdeelt, buiten de
//! kern-RAM en niet gecached (`layout.NetDMABase`). Op QEMU is die regio
//! volledig coherent. De registers zelf zijn van het transport
//! ([`driver_virtiopci::Transport`]): virtio-mmio op QEMU virt
//! ([`VirtioNet::new`]), virtio-pci onder EDK2
//! ([`VirtioNet::with_transport`] met een [`driver_virtiopci::Pci`]).
//!
//! De driver is een actor-onderdeel: wie hem heeft (`&mut self`) is de
//! enige die de ringen aanraakt. Het interrupt-pad raakt alleen
//! InterruptStatus/InterruptACK, via [`IrqAck`] (van het transport), en die
//! registers delen niets met de ringen. Over PCI is de lijn MSI-X zonder
//! ack.
//!
//! De TX-completion zit vanzelf op de lijn: de driver zet
//! `VIRTQ_AVAIL_F_NO_INTERRUPT` nooit, dus een TX-buffer die het device in
//! de used-ring teruggeeft, meldt zich zoals een ontvangen frame (virtio 1.2
//! §2.7.7; over PCI krijgen beide queues de vector). Zo wekt een volle ring
//! die leegloopt de pomp.
//!
//! Batching: [`transmit`](netdev::Device::transmit) en de RX-recycle zetten
//! descriptors klaar; de doorbell (avail.idx publiceren plus QueueNotify)
//! valt pas in [`flush`](netdev::Device::flush), één keer per burst. Dat is
//! de meting uit de Go-kern: één doorbell per burst, niet per frame.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use core::mem::offset_of;
use dev::Pa;
pub use driver_virtiopci::{Error, IrqAck};
use driver_virtiopci::{Mmio, Transport, mmio};
use netdev::{Mac, TxError};
use sync::Signal;

/// De config van virtio-net (virtio 1.2 §5.1.4), alleen voor de offsets:
/// de driver leest hem via [`Transport::config_read8`]. De MAC per byte:
/// een 32-bit lees op een oneven offset is ongealigneerd en abort op
/// device-geheugen.
#[repr(C)]
struct NetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
    mtu: u16,
}

const _: () = {
    assert!(offset_of!(NetConfig, mac) == 0);
    assert!(offset_of!(NetConfig, status) == 6);
    assert!(offset_of!(NetConfig, max_virtqueue_pairs) == 8);
    assert!(offset_of!(NetConfig, mtu) == 10);
};

/// DeviceID van een netwerkkaart.
const DEVICE_NET: u32 = 1;

/// Descriptor-flag: het device schrijft in deze buffer.
const DESC_WRITE: u16 = 2;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;

/// `virtio_net_hdr_mrg_rxbuf` onder VERSION_1: 12 bytes vóór elk frame.
pub const HDR_LEN: usize = 12;
/// Eén RX- of TX-buffer.
pub const BUF_SIZE: usize = 2048;
/// De grootste queue die we gebruiken (QEMU biedt 256).
pub const MAX_QUEUE: u16 = 256;
const DESC_BYTES: u64 = 16;

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Is er op `base` een moderne virtio-netwerkkaart?
///
/// # Safety
///
/// `base` is een gemapt virtio-mmio-slot (minstens 0x200 bytes).
#[must_use]
pub unsafe fn is_modern_net(base: Pa) -> bool {
    // SAFETY: de voorwaarde van deze functie.
    unsafe { mmio::is_modern(base, DEVICE_NET) }
}

/// `size` bytes uit de DMA-regio van de NIC, gewist.
fn alloc(dma: &mut dev::Bump, size: u64, align: u64) -> Result<Pa> {
    let p = dma.take(size, align).ok_or(Error::DmaTooSmall {
        need: size,
        have: dma.left(),
    })?;
    dev::clear(Pa(p), size as usize);
    Ok(Pa(p))
}

/// Eén split-virtqueue.
#[derive(Default)]
struct Vq {
    desc: Pa,
    avail: Pa,
    used: Pa,
    bufs: Pa,
    /// De laatst geconsumeerde used.idx.
    last_used: u16,
    /// Onze avail.idx, lokaal; gepubliceerd in `flush`.
    avail_idx: u16,
    /// De laatst gepubliceerde avail.idx.
    published: u16,
}

impl Vq {
    fn buf(&self, i: u16) -> Pa {
        self.bufs.add(u64::from(i) * BUF_SIZE as u64)
    }

    fn set_desc(&self, i: u16, addr: Pa, len: u32, flags: u16) {
        let d = self.desc.add(u64::from(i) * DESC_BYTES);
        dev::write64(d, addr.0);
        dev::write32(d.add(8), len);
        dev::write16(d.add(12), flags);
        dev::write16(d.add(14), 0);
    }

    fn set_avail(&self, qsize: u16, slot: u16, desc: u16) {
        dev::write16(self.avail.add(4 + u64::from(slot % qsize) * 2), desc);
    }

    fn used_idx(&self) -> u16 {
        dev::read16(self.used.add(2))
    }

    /// Publiceert avail.idx als er iets nieuws is; `true` = de doorbell moet.
    fn publish(&mut self) -> bool {
        if self.avail_idx == self.published {
            return false;
        }
        driver_virtiopci::publish(self.avail, self.avail_idx);
        self.published = self.avail_idx;
        true
    }
}

/// Eén virtio-net, over een virtio-transport: virtio-mmio op QEMU virt,
/// virtio-pci onder EDK2.
pub struct VirtioNet<T: Transport = Mmio> {
    t: T,
    mac: Mac,
    qsize: u16,
    /// Hoeveel TX-descriptors er hoogstens uitstaan (standaard `qsize`): de
    /// "zendring van N plekken" van een NIC, zonder de RX-ring of de
    /// queue zelf te verkleinen ([`VirtioNet::limit_tx`]).
    tx_limit: u16,
    rx: Vq,
    tx: Vq,
    irq: Option<&'static Signal>,
    /// De meetlat; `rx_bad` telt used-entries met een id of lengte die
    /// niet klopt, `doorbells` de QueueNotifies van beide queues.
    stats: netdev::Stats,
}

impl VirtioNet<Mmio> {
    /// Zet het device op het virtio-mmio-slot `base` op: reset, VERSION_1
    /// onderhandelen, RX- en TX-queue in `dma`, de RX-buffers publiceren,
    /// DRIVER_OK. `now` geeft monotone nanoseconden, voor de reset.
    ///
    /// # Safety
    ///
    /// `base` is een gemapt virtio-mmio-blok dat voor altijd blijft, en
    /// `[dma, dma+dma_size)` is gemapt geheugen dat alleen deze driver en
    /// het device gebruiken, nu en zolang het programma draait.
    pub unsafe fn new(base: Pa, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        // SAFETY: de eerste helft van de voorwaarde van deze functie.
        let t = unsafe { Mmio::new(base) };
        t.check()?;
        // SAFETY: de tweede helft van de voorwaarde van deze functie.
        unsafe { Self::with_transport(t, dma, dma_size, now) }
    }

    /// Het interrupt-pad, voor het board.
    #[must_use]
    pub fn irq_ack(&self) -> IrqAck {
        self.t.irq_ack()
    }
}

impl<T: Transport> VirtioNet<T> {
    /// Zet het device achter transport `t` op: reset, VERSION_1
    /// onderhandelen, RX- en TX-queue in `dma`, de RX-buffers publiceren,
    /// DRIVER_OK. `now` geeft monotone nanoseconden, voor de reset.
    ///
    /// # Safety
    ///
    /// `[dma, dma+dma_size)` is gemapt geheugen dat alleen deze driver en
    /// het device gebruiken, nu en zolang het programma draait.
    pub unsafe fn with_transport(t: T, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        let id = t.device_id();
        if id != DEVICE_NET {
            return Err(Error::WrongDevice {
                want: DEVICE_NET,
                got: id,
            });
        }
        let mut n = Self {
            t,
            mac: Mac::default(),
            qsize: 0,
            tx_limit: 0,
            rx: Vq::default(),
            tx: Vq::default(),
            irq: None,
            stats: netdev::Stats::default(),
        };
        let mut dma = dev::Bump::new(dma.0, dma_size);

        // Alleen VERSION_1. De device-features leest deze driver bewust
        // niet: QEMU levert een vaste, bekende set en wij onderhandelen
        // alleen VERSION_1.
        n.t.negotiate(now, |_| 0)?;

        let mut mac = [0u8; 6];
        for (i, b) in (0u32..).zip(mac.iter_mut()) {
            *b = n.t.config_read8(offset_of!(NetConfig, mac) as u32 + i);
        }
        n.mac = Mac(mac);

        n.rx = n.setup_queue(RX_QUEUE, &mut dma)?;
        n.tx = n.setup_queue(TX_QUEUE, &mut dma)?;

        // Alle RX-buffers aan het device geven.
        for i in 0..n.qsize {
            n.rx.set_desc(i, n.rx.buf(i), BUF_SIZE as u32, DESC_WRITE);
            n.rx.set_avail(n.qsize, i, i);
        }
        n.rx.avail_idx = n.qsize;
        n.rx.publish();

        n.tx_limit = n.qsize;
        n.t.driver_ok();
        n.notify(RX_QUEUE);
        Ok(n)
    }

    fn setup_queue(&mut self, idx: u16, dma: &mut dev::Bump) -> Result<Vq> {
        self.t.select_queue(idx);
        let max = self.t.queue_num_max();
        if max == 0 {
            return Err(Error::NoQueue {
                queue: idx,
                offered: 0,
            });
        }
        if self.qsize == 0 {
            // Een macht van twee: de ringindexen lopen als u16 rond, en
            // `idx % qsize` klopt over die omloop alleen als qsize 65536
            // deelt.
            let q = max.min(MAX_QUEUE);
            self.qsize = 1 << (u16::BITS - 1 - q.leading_zeros());
        }
        let q = u64::from(self.qsize);
        self.t.set_queue_num(self.qsize);
        let vq = Vq {
            desc: alloc(dma, q * DESC_BYTES, 16)?,
            avail: alloc(dma, 6 + 2 * q, 16)?,
            used: alloc(dma, 6 + 8 * q, 16)?,
            bufs: alloc(dma, q * BUF_SIZE as u64, 16)?,
            ..Vq::default()
        };
        self.t.set_queue_addrs(vq.desc, vq.avail, vq.used);
        self.t.enable_queue();
        Ok(vq)
    }

    fn notify(&mut self, queue: u16) {
        self.t.notify(queue);
        self.stats.doorbells += 1;
    }

    /// Hangt de bel van de NIC-interrupt aan de driver: de RX-pomp wacht
    /// erop in plaats van te pollen.
    pub fn set_irq(&mut self, bell: &'static Signal) {
        self.irq = Some(bell);
    }

    /// De queuegrootte die het device gaf.
    #[must_use]
    pub fn queue_size(&self) -> u16 {
        self.qsize
    }

    /// Laat hoogstens `n` TX-descriptors tegelijk uitstaan (1 tot de
    /// queuegrootte): een kleinere zendring, zoals de 64 plekken van de
    /// RTL8125, op een device dat 256 biedt. Voor de proef onder een trage
    /// draad (`hopos.virtio.txq`, tools/qemu-test-bench.sh met `WIRE=1`);
    /// de RX-ring blijft zoals hij is. Geeft de grens die nu geldt.
    pub fn limit_tx(&mut self, n: u16) -> u16 {
        self.tx_limit = n.clamp(1, self.qsize.max(1));
        self.tx_limit
    }

    /// Het transport, voor wie het device verder wil bekijken (de
    /// interruptstatus, de config).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.t
    }

    /// Geeft een RX-buffer terug aan het device (gepubliceerd in `flush`).
    fn recycle_rx(&mut self, desc: u16) {
        self.rx
            .set_desc(desc, self.rx.buf(desc), BUF_SIZE as u32, DESC_WRITE);
        self.rx.set_avail(self.qsize, self.rx.avail_idx, desc);
        self.rx.avail_idx = self.rx.avail_idx.wrapping_add(1);
    }
}

impl<T: Transport> netdev::Device for VirtioNet<T> {
    /// Zet één frame op de TX-ring (gepipelined: posten en teruggeven, niet
    /// per frame op voltooiing wachten). Slot = avail_idx % qsize, dus de
    /// buffer van dit slot werd het laatst gebruikt op avail_idx - qsize; vol
    /// is de ring pas als het device die oudste nog niet verzond.
    fn transmit(&mut self, frame: &[u8]) -> core::result::Result<(), TxError> {
        if frame.is_empty() || frame.len() > BUF_SIZE - HDR_LEN {
            return Err(TxError::Size(frame.len()));
        }
        self.tx.last_used = self.tx.used_idx();
        if self.tx.avail_idx.wrapping_sub(self.tx.last_used) >= self.tx_limit {
            self.stats.tx_full += 1;
            return Err(TxError::Full);
        }
        let slot = self.tx.avail_idx % self.qsize;
        let buf = self.tx.buf(slot);
        dev::clear(buf, HDR_LEN);
        dev::copy_in(buf.add(HDR_LEN as u64), frame);
        self.tx
            .set_desc(slot, buf, (HDR_LEN + frame.len()) as u32, 0);
        self.tx.set_avail(self.qsize, self.tx.avail_idx, slot);
        self.tx.avail_idx = self.tx.avail_idx.wrapping_add(1);
        Ok(())
    }

    /// Haalt één frame op, of `None` als de RX-ring leeg is.
    ///
    /// De used-ring is device-eigen: een kromme of kwaadaardige id buiten
    /// `[0, qsize)` zou anders een index worden in de descriptortabel en de
    /// buffers. Zo'n entry wordt veilig gedropt en niet gerecycled (we weten
    /// niet welke buffer erbij hoort; we verliezen hooguit één descriptor).
    /// Een lengte buiten `(HDR_LEN, BUF_SIZE]` wordt gerecycled zonder kopie.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let used = self.rx.used_idx();
            if used == self.rx.last_used {
                return None;
            }
            dev::mb();
            let slot = u64::from(self.rx.last_used % self.qsize);
            let elem = self.rx.used.add(4 + slot * 8);
            let id = dev::read32(elem);
            let len = dev::read32(elem.add(4)) as usize;
            self.rx.last_used = self.rx.last_used.wrapping_add(1);
            let Ok(desc) = u16::try_from(id) else {
                self.stats.rx_bad += 1;
                continue;
            };
            if desc >= self.qsize {
                self.stats.rx_bad += 1;
                continue;
            }
            if len <= HDR_LEN || len > BUF_SIZE {
                self.stats.rx_bad += 1;
                self.recycle_rx(desc);
                continue;
            }
            let n = (len - HDR_LEN).min(buf.len());
            if let Some(dst) = buf.get_mut(..n) {
                dev::copy_out(dst, self.rx.buf(desc).add(HDR_LEN as u64));
            }
            self.recycle_rx(desc);
            return Some(n);
        }
    }

    /// Eén doorbell per queue per burst.
    fn flush(&mut self) {
        if self.tx.publish() {
            self.notify(TX_QUEUE);
        }
        if self.rx.publish() {
            self.notify(RX_QUEUE);
        }
    }

    fn mac(&self) -> Mac {
        self.mac
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }

    fn stats(&self) -> netdev::Stats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    //! `receive_bounds_test.go`, geport: de driver op nep-geheugen.
    use super::*;
    use core::cell::{Cell, RefCell};
    use driver_virtiopci::{FEAT_VERSION_1_HI, status};
    use netdev::Device as _;

    struct Fake {
        _regs: Vec<u64>,
        _mem: Vec<u64>,
        net: VirtioNet,
    }

    /// De reset van de nep-transport is synchroon; de klok telt niet.
    fn now() -> u64 {
        0
    }

    fn pa(v: &mut [u64]) -> Pa {
        Pa(v.as_mut_ptr() as usize as u64)
    }

    /// Een driver met qsize 2 zonder `new`: descriptors, ringen en buffers
    /// in één nep-regio, zoals de Go-test.
    fn fake(rx: bool) -> Fake {
        let mut regs = vec![0u64; 64];
        let mut mem = vec![0u64; (256 + 2 * BUF_SIZE) / 8 + 64];
        let base = pa(&mut mem);
        let q = Vq {
            desc: base,
            avail: base.add(64),
            used: base.add(128),
            bufs: base.add(256),
            ..Vq::default()
        };
        let mut net = VirtioNet {
            // SAFETY: `regs` leeft zolang de `Fake` en is een heel slot
            // groot (512 bytes); alleen QueueNotify wordt geraakt.
            t: unsafe { Mmio::new(pa(&mut regs)) },
            mac: Mac::default(),
            qsize: 2,
            tx_limit: 2,
            rx: Vq::default(),
            tx: Vq::default(),
            irq: None,
            stats: netdev::Stats::default(),
        };
        if rx {
            net.rx = q;
        } else {
            net.tx = q;
        }
        Fake {
            _regs: regs,
            _mem: mem,
            net,
        }
    }

    #[test]
    fn receive_validates_the_device_record_before_copying() {
        for (name, id, len, want, recycled) in [
            ("valid", 0u32, 76u32, Some(64usize), true),
            (
                "largest",
                0,
                BUF_SIZE as u32,
                Some(BUF_SIZE - HDR_LEN),
                true,
            ),
            ("oversize", 0, BUF_SIZE as u32 + 1, None, true),
            ("short", 0, HDR_LEN as u32 - 1, None, true),
            ("id-outside-ring", 2, 76, None, false),
            ("id-truncated-to-valid", 1 << 16, 76, None, false),
        ] {
            let mut f = fake(true);
            let n = &mut f.net;
            dev::write16(n.rx.used.add(2), 1);
            dev::write32(n.rx.used.add(4), id);
            dev::write32(n.rx.used.add(8), len);
            dev::copy_in(n.rx.bufs.add(HDR_LEN as u64), &[0x5a; BUF_SIZE - HDR_LEN]);
            let mut out = vec![0xccu8; 8192];
            let got = n.receive(&mut out);
            assert_eq!(got, want, "{name}");
            let k = got.unwrap_or(0);
            assert!(
                out[..k].iter().all(|&b| b == 0x5a),
                "{name}: packet differs"
            );
            assert!(out[k..].iter().all(|&b| b == 0xcc), "{name}: beyond packet");
            assert_eq!(n.rx.last_used, 1, "{name}");
            assert_eq!(n.rx.avail_idx == 1, recycled, "{name}: recycle");
            // Niets gepubliceerd vóór de flush: batching.
            assert_eq!(dev::read16(n.rx.avail.add(2)), 0, "{name}");
            n.flush();
            assert_eq!(
                dev::read16(n.rx.avail.add(2)),
                u16::from(recycled),
                "{name}"
            );
        }
    }

    #[test]
    fn transmit_rejects_bad_sizes_before_touching_dma() {
        for size in [0, BUF_SIZE - HDR_LEN + 1] {
            let mut f = fake(false);
            let n = &mut f.net;
            assert_eq!(n.transmit(&vec![0; size]), Err(TxError::Size(size)));
            assert_eq!(n.tx.avail_idx, 0);
        }
    }

    #[test]
    fn transmit_batches_until_flush_and_stops_when_full() {
        let mut f = fake(false);
        let n = &mut f.net;
        n.transmit(&[1; 60]).unwrap();
        n.transmit(&[2; 60]).unwrap();
        assert_eq!(n.transmit(&[3; 60]), Err(TxError::Full));
        assert_eq!(dev::read16(n.tx.avail.add(2)), 0);
        assert_eq!(n.stats.doorbells, 0);
        n.flush();
        assert_eq!(dev::read16(n.tx.avail.add(2)), 2);
        assert_eq!(n.stats.doorbells, 1);
        n.flush(); // niets nieuws: geen doorbell
        assert_eq!(n.stats.doorbells, 1);
        // Het device verzond er één: er is weer plaats.
        dev::write16(n.tx.used.add(2), 1);
        n.transmit(&[4; 60]).unwrap();
        // Descriptor 0 wijst naar zijn buffer met header plus frame.
        assert_eq!(dev::read32(n.tx.desc.add(8)), (HDR_LEN + 60) as u32);
    }

    #[test]
    fn a_tx_limit_makes_a_smaller_send_ring() {
        let mut f = fake(false);
        let n = &mut f.net;
        assert_eq!(n.limit_tx(1), 1);
        n.transmit(&[1; 60]).unwrap();
        assert_eq!(n.transmit(&[2; 60]), Err(TxError::Full), "one slot");
        assert_eq!(n.stats.tx_full, 1);
        // Het device verzond hem: de plek is weer vrij.
        dev::write16(n.tx.used.add(2), 1);
        n.transmit(&[2; 60]).unwrap();
        assert_eq!(n.limit_tx(0), 1, "at least one");
        assert_eq!(n.limit_tx(1000), 2, "at most the queue");
    }

    /// Een nep-transport: onthoudt wat de driver deed, zodat de
    /// init-volgorde over elke `Transport` getoetst wordt.
    struct FakeT {
        id: u32,
        max: u16,
        refuse: bool,
        status: Cell<u8>,
        statuses: RefCell<Vec<u8>>,
        features: RefCell<Vec<(u32, u32)>>,
        selected: u16,
        queues: RefCell<[FakeQueue; 2]>,
        notified: RefCell<Vec<u16>>,
    }

    /// Wat de driver in één queue zette.
    #[derive(Clone, Copy, Default)]
    struct FakeQueue {
        num: u16,
        desc: Pa,
        avail: Pa,
        used: Pa,
        on: bool,
    }

    impl FakeT {
        fn new(id: u32, max: u16) -> Self {
            Self {
                id,
                max,
                refuse: false,
                status: Cell::new(0x7f),
                statuses: RefCell::new(Vec::new()),
                features: RefCell::new(Vec::new()),
                selected: 0,
                queues: RefCell::new([FakeQueue::default(); 2]),
                notified: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for FakeT {
        fn device_id(&self) -> u32 {
            self.id
        }
        fn device_features(&self, _: u32) -> u32 {
            u32::MAX
        }
        fn set_driver_features(&self, window: u32, bits: u32) {
            self.features.borrow_mut().push((window, bits));
        }
        fn status(&self) -> u8 {
            self.status.get()
        }
        fn set_status(&self, s: u8) {
            self.statuses.borrow_mut().push(s);
            let refused = self.refuse && s & status::FEATURES_OK != 0;
            self.status
                .set(if refused { s & !status::FEATURES_OK } else { s });
        }
        fn select_queue(&mut self, q: u16) {
            self.selected = q;
        }
        fn queue_num_max(&self) -> u16 {
            if usize::from(self.selected) < 2 {
                self.max
            } else {
                0
            }
        }
        fn set_queue_num(&self, n: u16) {
            self.queues.borrow_mut()[usize::from(self.selected)].num = n;
        }
        fn set_queue_addrs(&self, d: Pa, a: Pa, u: Pa) {
            let mut q = self.queues.borrow_mut();
            let e = &mut q[usize::from(self.selected)];
            (e.desc, e.avail, e.used) = (d, a, u);
        }
        fn enable_queue(&mut self) {
            self.queues.borrow_mut()[usize::from(self.selected)].on = true;
        }
        fn notify(&self, q: u16) {
            self.notified.borrow_mut().push(q);
        }
        fn ack_interrupt(&self) -> u32 {
            0
        }
        fn config_generation(&self) -> u32 {
            0
        }
        fn config_read8(&self, off: u32) -> u8 {
            [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]
                .get(off as usize)
                .copied()
                .unwrap_or(0xff)
        }
        fn config_read32(&self, _: u32) -> u32 {
            u32::MAX
        }
    }

    #[test]
    fn init_over_any_transport_follows_the_spec_order() {
        let mut mem = vec![0u64; 3 * 1024 * 8];
        let dma = pa(&mut mem);
        // Zes biedt het device: de driver neemt de macht van twee eronder.
        // SAFETY: `mem` leeft de hele test en is ruim genoeg voor twee
        // queues van vier.
        let mut n = unsafe { VirtioNet::with_transport(FakeT::new(1, 6), dma, 3 * 1024 * 64, now) }
            .unwrap();
        assert_eq!(n.queue_size(), 4);
        assert_eq!(n.mac(), Mac([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]));
        let t = n.transport();
        let (a, d, f, ok) = (
            status::ACKNOWLEDGE,
            status::DRIVER,
            status::FEATURES_OK,
            status::DRIVER_OK,
        );
        assert_eq!(
            *t.statuses.borrow(),
            [0, a, a | d, a | d | f, a | d | f | ok]
        );
        assert_eq!(*t.features.borrow(), [(0, 0), (1, FEAT_VERSION_1_HI)]);
        let q = *t.queues.borrow();
        assert!(
            q.iter().all(|e| e.num == 4 && e.on),
            "both queues sized and on"
        );
        assert_eq!(
            (q[0].desc, q[0].avail, q[0].used),
            (n.rx.desc, n.rx.avail, n.rx.used)
        );
        assert_eq!(
            (q[1].desc, q[1].avail, q[1].used),
            (n.tx.desc, n.tx.avail, n.tx.used)
        );
        // De RX-ring staat vol en is gepubliceerd, met één doorbell.
        assert_eq!(dev::read16(n.rx.avail.add(2)), 4);
        assert_eq!(*t.notified.borrow(), [RX_QUEUE]);
        // Zenden gaat over de doorbell van het transport, pas in `flush`.
        n.transmit(&[7; 60]).unwrap();
        assert_eq!(n.transport().notified.borrow().len(), 1);
        n.flush();
        assert_eq!(*n.transport().notified.borrow(), [RX_QUEUE, TX_QUEUE]);
        assert_eq!(n.stats.doorbells, 2);
    }

    #[test]
    fn init_refuses_the_wrong_device_and_refused_features() {
        let mut mem = vec![0u64; 1024];
        let dma = pa(&mut mem);
        // SAFETY: `mem` leeft de hele test; er wordt niets in gezet.
        let e = unsafe { VirtioNet::with_transport(FakeT::new(2, 4), dma, 8192, now) }.err();
        assert_eq!(e, Some(Error::WrongDevice { want: 1, got: 2 }));
        let mut t = FakeT::new(1, 4);
        t.refuse = true;
        // SAFETY: zie boven.
        let e = unsafe { VirtioNet::with_transport(t, dma, 8192, now) }.err();
        assert_eq!(e, Some(Error::FeaturesRefused));
        // SAFETY: zie boven.
        let e = unsafe { VirtioNet::with_transport(FakeT::new(1, 0), dma, 8192, now) }.err();
        assert_eq!(
            e,
            Some(Error::NoQueue {
                queue: 0,
                offered: 0
            })
        );
    }
}
