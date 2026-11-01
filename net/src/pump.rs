//! De RX-pomp: de uplink-NIC tussen de draad en de switch (Go: `rxLoop` en
//! de `WaitNIC`-goroutine in `hopnet`).
//!
//! De pomp bezit de NIC. Ontvangen frames gaan als waarde de ingress-rij in
//! (de switch doet de NAT en bezorgt); frames voor de draad komen uit de
//! egress-rij, die alleen de switch vult. Zo heeft de NIC één eigenaar en
//! elke rij één producer: de `uplinkTxMu` van de Go-kern bestaat niet meer.
//!
//! Wachten: op de NIC-interrupt en de bel van de switch, verder niets. Elke
//! NIC heeft een lijn (docs/sequence-hopos.md §14.1 O3): de kern start de
//! pomp niet zonder. Geen vangrail ernaast (§13.3): een verloren wek is een
//! fout die we repareren.
//!
//! Een volle TX-ring van de NIC is geen drop maar een stop, zoals Linux'
//! `netif_stop_queue` in een driver die geen descriptor meer heeft: de pomp
//! houdt het frame vast en probeert het de volgende ronde als eerste. Die
//! ronde komt van de lijn: elke driver zet de TX-completion op dezelfde
//! lijn als de ontvangst, en een vrijgekomen descriptor wekt de pomp zoals
//! de TX-clean van NAPI de queue wekt (`netif_wake_queue`). De egress-rij
//! loopt intussen vol, en de switch stopt met lezen uit de TX-ringen
//! ([`crate::switch::Xoff`]); de pomp opent hem weer zodra hij een frame uit
//! de rij nam. Tot 24-10 ging het frame na één flush weg ("drop, TCP
//! herstelt"), en elk gat kostte de TCP van de app een go-back-N of een
//! RTO; tot 01-11 keek de pomp zolang hij vasthield elke 300 µs.

use crate::switch::Xoff;
use crate::{Frame, Stats, UPLINK_QUEUE};
use core::sync::atomic::Ordering::Relaxed;
use netdev::{Device, TxError};
use sync::spsc::{Receiver, Sender};
use sync::{Signal, select, yield_now};

/// Frames per RX-burst. De RX-doorbell gaat per burst, niet per frame: per
/// frame scheelt dat één tot drie PCIe-acties, en op een 1 Gbit-link zijn
/// dat er 70.000 per seconde (L83, de 100 MB/s-jacht). Om de 32 frames toch
/// een flush, zodat de NIC nooit zonder descriptors zit.
pub const RX_BATCH: usize = 32;

/// De pomp.
pub struct Pump<'a, D: Device> {
    nic: D,
    /// De bel van de NIC-lijn.
    line: &'a Signal,
    ingress: Sender<'a, Frame, UPLINK_QUEUE>,
    egress: Receiver<'a, Frame, UPLINK_QUEUE>,
    /// De bel van de pomp: de switch zet hem na een ronde met egress-werk.
    bell: &'a Signal,
    /// De deur van de switch.
    door: &'a Signal,
    /// De stop van de switch: de pomp opent hem als er weer plaats is.
    xoff: &'a Xoff,
    /// Het frame dat de NIC niet nam (een volle TX-ring): de volgende ronde
    /// gaat het als eerste, vóór de egress-rij.
    held: Option<Frame>,
    stats: &'a Stats,
}

impl<'a, D: Device> Pump<'a, D> {
    /// Een pomp over `nic`, gewekt door zijn lijn ([`Device::irq`]);
    /// `None` voor een NIC zonder lijn: die pompt de kern niet.
    pub fn new(
        nic: D,
        ingress: Sender<'a, Frame, UPLINK_QUEUE>,
        egress: Receiver<'a, Frame, UPLINK_QUEUE>,
        bell: &'a Signal,
        door: &'a Signal,
        xoff: &'a Xoff,
        stats: &'a Stats,
    ) -> Option<Self> {
        Some(Self {
            line: nic.irq()?,
            nic,
            ingress,
            egress,
            bell,
            door,
            xoff,
            held: None,
            stats,
        })
    }

    /// De NIC.
    pub fn nic(&mut self) -> &mut D {
        &mut self.nic
    }

    /// Houdt de pomp een frame vast dat de NIC nog niet nam?
    #[must_use]
    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Zet wat de switch klaarlegde de draad op, het vastgehouden frame
    /// eerst. Een volle ring houdt het frame vast en stopt de ronde (de
    /// volgorde blijft); een echte driverfout telt en gooit weg. Nam de pomp
    /// iets uit de rij, dan opent hij de switch weer. `true` = er ging iets
    /// uit.
    fn tx_pass(&mut self) -> bool {
        let mut sent = false;
        let mut took = false;
        loop {
            // Alleen nemen als hij er is: een `take` van een lege plek
            // verplaatst toch de hele 1,5 KiB van `Option<Frame>`.
            let held = if self.held.is_some() {
                self.held.take()
            } else {
                None
            };
            let (f, fresh) = match held {
                Some(f) => (f, false),
                None => match self.egress.try_recv() {
                    Some(f) => {
                        took = true;
                        (f, true)
                    }
                    None => break,
                },
            };
            match self.nic.transmit(f.bytes()) {
                Ok(()) => sent = true,
                // Vol: één flush (de doorbell van wat al klaarstaat laat de
                // NIC beginnen) en nog één poging; dan vasthouden.
                Err(TxError::Full) => {
                    self.nic.flush();
                    if self.nic.transmit(f.bytes()).is_ok() {
                        sent = true;
                        continue;
                    }
                    if fresh {
                        self.stats.nic_tx_held.fetch_add(1, Relaxed);
                    }
                    self.held = Some(f);
                    break;
                }
                Err(_) => {
                    self.stats.nic_tx_errors.fetch_add(1, Relaxed);
                }
            }
        }
        if took {
            self.xoff.wake(self.door);
        }
        sent
    }

    /// Eén RX-burst de ingress-rij in. Een volle rij laat de frames in de
    /// NIC-ring liggen: dat is de backpressure, geen drop. Geeft het aantal.
    fn rx_pass(&mut self) -> usize {
        let mut n = 0;
        while n < RX_BATCH && self.ingress.free() > 0 {
            let mut f = Frame::new();
            let Some(len) = self.nic.receive(f.buf_mut()) else {
                break;
            };
            f.set_len(len);
            if self.ingress.try_send(f).is_err() {
                self.stats.uplink_rx_drops.fetch_add(1, Relaxed);
            }
            n += 1;
        }
        n
    }

    /// Eén ronde zonder wachten: een RX-burst, dan TX, dan de doorbells,
    /// en de meetlat van de driver naar de tellers. `true` = er was werk.
    ///
    /// RX eerst: een lege RX-ring heropent de lijn (in `receive`), en de
    /// TX-ronde daarna is de blik ná het openen. Een descriptor die de NIC
    /// vrijgaf terwijl de lijn dicht was, ziet die ronde; een die later
    /// vrijkomt, wekt de lijn (de netdev-regel van NAPI, nu ook voor TX).
    pub fn pass(&mut self) -> bool {
        let got = self.rx_pass();
        let sent = self.tx_pass();
        if sent || got > 0 {
            self.nic.flush();
        }
        let n = self.nic.stats();
        self.stats.nic_rx_bad.store(n.rx_bad, Relaxed);
        self.stats.nic_tx_full.store(n.tx_full, Relaxed);
        self.stats.nic_doorbells.store(n.doorbells, Relaxed);
        if got > 0 {
            // De burst ligt bij de switch: hem nú wekken, niet pas bij HOP's
            // idle. Onder inbound verkeer is HOP nooit idle, en dan lagen de
            // ACK's van de apps tot 20-09 tot de failsafe van 1 ms in hun
            // TX-ringen (gemeten: 37 MB/s = venster ÷ RTT).
            self.door.set();
        }
        sent || got > 0
    }

    /// De lus van de pomp; keert niet terug. Een vastgehouden frame wacht
    /// net als de rest op de lijn: de TX-completion van de NIC wekt hem.
    pub async fn run(&mut self) {
        let line = self.line;
        loop {
            if self.pass() {
                yield_now().await;
                continue;
            }
            self.stats.rx_idle.fetch_add(1, Relaxed);
            select(self.bell.wait(), line.wait()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Egress, Ingress};
    use executor::Executor;
    use netdev::Mac;
    use std::collections::VecDeque;

    struct FakeNic {
        line: &'static Signal,
        rx: VecDeque<Vec<u8>>,
        tx: Vec<Vec<u8>>,
        flushes: usize,
        full_once: bool,
    }

    impl Device for FakeNic {
        fn transmit(&mut self, f: &[u8]) -> Result<(), TxError> {
            if self.full_once {
                self.full_once = false;
                return Err(TxError::Full);
            }
            self.tx.push(f.to_vec());
            Ok(())
        }
        fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
            let f = self.rx.pop_front()?;
            buf[..f.len()].copy_from_slice(&f);
            Some(f.len())
        }
        fn flush(&mut self) {
            self.flushes += 1;
        }
        fn mac(&self) -> Mac {
            Mac([2, 0, 0, 0, 0, 9])
        }
        fn stats(&self) -> netdev::Stats {
            netdev::Stats {
                doorbells: self.flushes as u64,
                ..netdev::Stats::default()
            }
        }
        fn irq(&self) -> Option<&'static Signal> {
            Some(self.line)
        }
    }

    fn leak<T>(v: T) -> &'static T {
        Box::leak(Box::new(v))
    }

    #[test]
    fn rx_burst_gaat_de_switch_in_en_wekt_hem() {
        let (ing_tx, mut ing_rx) = leak(Ingress::new()).split().unwrap();
        let (_eg_tx, eg_rx) = leak(Egress::new()).split().unwrap();
        let door = leak(Signal::new());
        let stats = leak(Stats::new());
        let nic = FakeNic {
            line: leak(Signal::new()),
            rx: (0..40u8).map(|i| vec![i; 60]).collect(),
            tx: Vec::new(),
            flushes: 0,
            full_once: false,
        };
        let mut p = Pump::new(
            nic,
            ing_tx,
            eg_rx,
            leak(Signal::new()),
            door,
            leak(Xoff::new()),
            stats,
        )
        .unwrap();
        assert!(p.pass());
        assert_eq!(ing_rx.len(), RX_BATCH, "één burst is RX_BATCH frames");
        assert_eq!(p.nic().flushes, 1, "één doorbell per burst");
        assert_eq!(
            stats.nic_doorbells.load(Relaxed),
            1,
            "de meetlat van de NIC"
        );
        assert!(door.is_set());
        assert!(p.pass());
        assert_eq!(ing_rx.len(), 40);
        assert_eq!(ing_rx.try_recv().unwrap().bytes(), &[0u8; 60][..]);
        assert!(!p.pass(), "lege NIC is geen werk");
    }

    #[test]
    fn volle_ingress_laat_de_frames_in_de_nic() {
        let (ing_tx, mut ing_rx) = leak(Ingress::new()).split().unwrap();
        let (_eg_tx, eg_rx) = leak(Egress::new()).split().unwrap();
        let stats = leak(Stats::new());
        let nic = FakeNic {
            line: leak(Signal::new()),
            rx: (0..UPLINK_QUEUE + 5).map(|_| vec![1; 60]).collect(),
            tx: Vec::new(),
            flushes: 0,
            full_once: false,
        };
        let mut p = Pump::new(
            nic,
            ing_tx,
            eg_rx,
            leak(Signal::new()),
            leak(Signal::new()),
            leak(Xoff::new()),
            stats,
        )
        .unwrap();
        while p.pass() {}
        assert_eq!(ing_rx.len(), UPLINK_QUEUE);
        assert_eq!(
            p.nic().rx.len(),
            5,
            "de rest hoort in de NIC-ring te blijven"
        );
        assert_eq!(stats.uplink_rx_drops.load(Relaxed), 0);
        let _ = ing_rx.try_recv();
        assert!(p.pass());
        assert_eq!(p.nic().rx.len(), 4);
    }

    #[test]
    fn egress_gaat_de_draad_op_met_een_flush() {
        let (ing_tx, _ing_rx) = leak(Ingress::new()).split().unwrap();
        let (mut eg_tx, eg_rx) = leak(Egress::new()).split().unwrap();
        let stats = leak(Stats::new());
        let nic = FakeNic {
            line: leak(Signal::new()),
            rx: VecDeque::new(),
            tx: Vec::new(),
            flushes: 0,
            full_once: true,
        };
        let mut p = Pump::new(
            nic,
            ing_tx,
            eg_rx,
            leak(Signal::new()),
            leak(Signal::new()),
            leak(Xoff::new()),
            stats,
        )
        .unwrap();
        for i in 0..3u8 {
            assert!(eg_tx.try_send(Frame::from_slice(&[i; 42]).unwrap()).is_ok());
        }
        assert!(p.pass());
        assert_eq!(
            p.nic().tx.len(),
            3,
            "een volle TX-ring kreeg zijn tweede poging niet"
        );
        assert_eq!(p.nic().flushes, 2, "de flush bij vol plus die van de burst");
        assert_eq!(stats.nic_tx_errors.load(Relaxed), 0);
    }

    /// Een volle ring houdt het frame vast in plaats van het weg te
    /// gooien: de volgende ronde gaat het als eerste, de rest blijft in de
    /// egress-rij, en een stilstaande switch krijgt zijn bel zodra de pomp
    /// weer een frame uit de rij nam.
    #[test]
    fn een_volle_ring_houdt_vast_en_wekt_de_switch() {
        struct Stuck {
            free: usize,
            tx: Vec<u8>,
        }
        impl Device for Stuck {
            fn transmit(&mut self, f: &[u8]) -> Result<(), TxError> {
                if self.free == 0 {
                    return Err(TxError::Full);
                }
                self.free -= 1;
                self.tx.push(f[0]);
                Ok(())
            }
            fn receive(&mut self, _: &mut [u8]) -> Option<usize> {
                None
            }
            fn flush(&mut self) {}
            fn mac(&self) -> Mac {
                Mac([2, 0, 0, 0, 0, 9])
            }
            fn irq(&self) -> Option<&'static Signal> {
                Some(leak(Signal::new()))
            }
        }
        let (ing_tx, _ing_rx) = leak(Ingress::new()).split().unwrap();
        let (mut eg_tx, eg_rx) = leak(Egress::new()).split().unwrap();
        let (door, xoff, stats) = (leak(Signal::new()), leak(Xoff::new()), leak(Stats::new()));
        let nic = Stuck {
            free: 1,
            tx: Vec::new(),
        };
        let mut p = Pump::new(nic, ing_tx, eg_rx, leak(Signal::new()), door, xoff, stats).unwrap();
        for i in 0..4u8 {
            assert!(eg_tx.try_send(Frame::from_slice(&[i; 42]).unwrap()).is_ok());
        }
        assert!(p.pass(), "frame 0 went out");
        assert!(p.is_holding(), "frame 1 was not held");
        assert_eq!(eg_tx.free(), UPLINK_QUEUE - 2, "frames 2 and 3 stay queued");
        assert!(!p.pass(), "a held frame on a full ring is no work");
        assert_eq!(
            stats.nic_tx_held.load(Relaxed),
            1,
            "one hold, not one per poll"
        );
        assert_eq!(stats.nic_tx_errors.load(Relaxed), 0, "full is no error");
        // De switch stopt; de NIC geeft plaats vrij.
        assert!(xoff.stop());
        door.take();
        p.nic().free = 8;
        assert!(p.pass());
        assert_eq!(p.nic().tx, [0, 1, 2, 3], "the order held");
        assert!(!p.is_holding());
        assert!(
            !xoff.is_stopped() && door.is_set(),
            "the switch was not woken"
        );
    }

    /// Een vastgehouden frame wacht op de lijn, niet op een klok: zonder bel
    /// blijft het liggen, en de TX-completion (de lijn) zet het de draad op.
    #[test]
    fn de_tx_completion_wekt_een_vastgehouden_frame() {
        use core::cell::Cell;
        struct Ring {
            free: &'static Cell<usize>,
            line: &'static Signal,
            sent: &'static Cell<usize>,
        }
        impl Device for Ring {
            fn transmit(&mut self, _: &[u8]) -> Result<(), TxError> {
                if self.free.get() == 0 {
                    return Err(TxError::Full);
                }
                self.free.set(self.free.get() - 1);
                self.sent.set(self.sent.get() + 1);
                Ok(())
            }
            fn receive(&mut self, _: &mut [u8]) -> Option<usize> {
                None
            }
            fn mac(&self) -> Mac {
                Mac([2, 0, 0, 0, 0, 9])
            }
            fn irq(&self) -> Option<&'static Signal> {
                Some(self.line)
            }
        }
        let exec: &'static Executor<4, 4> = leak(Executor::new());
        let (ing_tx, _ing_rx) = leak(Ingress::new()).split().unwrap();
        let (mut eg_tx, eg_rx) = leak(Egress::new()).split().unwrap();
        let (free, sent, line) = (leak(Cell::new(1)), leak(Cell::new(0)), leak(Signal::new()));
        let nic = Ring { free, line, sent };
        let mut p = Pump::new(
            nic,
            ing_tx,
            eg_rx,
            leak(Signal::new()),
            leak(Signal::new()),
            leak(Xoff::new()),
            leak(Stats::new()),
        )
        .unwrap();
        for i in 0..2u8 {
            assert!(eg_tx.try_send(Frame::from_slice(&[i; 42]).unwrap()).is_ok());
        }
        exec.spawn(async move { p.run().await }).unwrap();
        while exec.step() {}
        assert_eq!(sent.get(), 1, "the second frame is held");
        while exec.step() {}
        assert_eq!(sent.get(), 1, "no clock retries it");
        // De NIC gaf een descriptor vrij en trok de lijn.
        free.set(1);
        line.set();
        while exec.step() {}
        assert_eq!(sent.get(), 2, "the line did not release the held frame");
    }

    /// De lus slaapt op de lijn: zonder bel geen ronde, en de bel van de
    /// lijn haalt het frame op.
    #[test]
    fn de_lijn_wekt_de_pomp() {
        let exec: &'static Executor<4, 4> = leak(Executor::new());
        let (ing_tx, ing_rx) = leak(Ingress::new()).split().unwrap();
        let (_eg_tx, eg_rx) = leak(Egress::new()).split().unwrap();
        let stats = leak(Stats::new());
        let line = leak(Signal::new());
        let nic = FakeNic {
            line,
            rx: VecDeque::new(),
            tx: Vec::new(),
            flushes: 0,
            full_once: false,
        };
        let mut p = Pump::new(
            nic,
            ing_tx,
            eg_rx,
            leak(Signal::new()),
            leak(Signal::new()),
            leak(Xoff::new()),
            stats,
        )
        .unwrap();
        p.nic().rx.push_back(vec![7; 64]);
        // De taak bezit de pomp; de test kijkt via de rij en de tellers.
        exec.spawn(async move { p.run().await }).unwrap();
        while exec.step() {}
        assert_eq!(ing_rx.len(), 1, "het frame kwam niet door");
        let idle = stats.rx_idle.load(Relaxed);
        while exec.step() {}
        assert_eq!(stats.rx_idle.load(Relaxed), idle, "zonder bel geen ronde");
        line.set();
        while exec.step() {}
        assert_eq!(
            stats.rx_idle.load(Relaxed),
            idle + 1,
            "de lijn wekte de pomp niet"
        );
    }
}
