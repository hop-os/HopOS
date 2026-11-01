//! De meetbanken van de kern, aan met een bootparameter: `hopos.nvmebench=1`
//! (of de feature `nvmebench`, voor een meetkern die je flipt), de
//! watchdogtoets (feature `wdtest`: Hop stopt na een minuut, de watchdog
//! moet resetten),
//! (de schijf rauw en door hopfs, Go: `nvmeBench` en `hopfsBench` in
//! `OLD/metal/cmd/hopos/nvmebench.go`) en `hopos.idlestat=1` (de
//! idle-meetlat van de OS-core, Go: `idleStat` in `node.go`).
//!
//! Dit module bezit niets dat na zijn eigen aanroep blijft, behalve de
//! idlestat-taak; die leest alleen atomics (handboek §1.3).
//!
//! # De schijf in een bench-boot
//!
//! `probe_disk` mag één keer, dus de opslag probet (`storage::probe`), de
//! bench leent de schijf vóór de opslag hem mount en geeft hem terug
//! ([`start`]), en `storage::start` neemt hem aan. Zo mount een bench-boot
//! daarna gewoon, zoals in Go (`HOPOS_FS_UP` na `HOPOS_NVMEBENCH_DONE`).
//!
//! De bench meet het pad dat hopfs gebruikt, niet een eigen: de geleende
//! schijf achter een [`Queue`] met de pollende [`Spin`], elke opdracht een
//! ticket plus completion, afgedraaid met [`block_on`] (de executor draait
//! nog niet; waarom er geen synchrone vorm is: de crate-doc van `blkdev`).
//!
//! De bench schrijft alleen in de staart van de schijf: de helft, hoogstens
//! 1 GiB. hopfs legt zijn boom aan het begin en alloceert van voren, dus de
//! staart is op een schijf die niet vol is van niemand; maar een meetbank
//! die ongevraagd op de schijf schrijft is geen bench maar een bug, dus
//! alleen met de sleutel.

use crate::Disk;
use alloc::string::String;
use alloc::vec::Vec;
use blkdev::{AsyncBlockDevice, BlockIo, LBA_SIZE, Op, Queue, Spin, block_on};
use board::Board;
use core::sync::atomic::Ordering::Relaxed;
use core::task::Poll;
use core::time::Duration;
use cpu::println;
use executor::Executor;
use kern::hopfs::{self, Fs};
use kern::slots::{Reply, Request, Response};
use sync::{Futures, LocalCell};

/// De staart die de bench hoogstens beschrijft.
const SPAN_MAX: u64 = 1 << 30;

/// Per commandomaat van de Go-tabel: 16 MiB (Go: `16<<20`).
const PER_SIZE: u64 = 16 << 20;

/// De commandomaten van de Go-tabel.
const SIZES: [u64; 4] = [4 << 10, 64 << 10, 256 << 10, 1 << 20];

/// De buffer van de bench: 1 MiB (Go: `make([]byte, 1<<20)`), de grootste
/// commandomaat van de tabel en de calls van de hopfs-bench.
const BUF: usize = 1 << 20;

/// Hoogstens zoveel willekeurige 4 KiB-opdrachten (64 MiB).
const RANDOM_OPS: u64 = 16384;

/// Zoveel lezingen tegelijk in [`Bench::random_queue`], tot de diepte van
/// de hopfs-actor.
const QUEUE_DEPTH: usize = kern::rpc::FS_DEPTH;

/// De dieptes die [`Bench::random_queue`] meet: zo zie je hoe het device
/// schaalt.
const QUEUE_DEPTHS: [usize; 3] = [1, 4, QUEUE_DEPTH];

/// Start wat de bootparameters vragen: de schijf-bench (synchroon, nu, op
/// de geprobede schijf `disk`) en de idlestat-taak. Geeft de schijf terug
/// voor `storage::start`: de bench leent hem alleen.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor, mut disk: Option<Disk>) -> Option<Disk> {
    let idle = crate::BOARD.boot_param("hopos.idlestat");
    if !idle.is_empty() && idle != "0" {
        // `1` is elke seconde; een ander getal is de periode in seconden.
        let every = idle.parse::<u64>().unwrap_or(1).clamp(1, 3600);
        match exec.spawn(idlestat(exec, every)) {
            Ok(()) => println!("bench: idlestat every {every} s HOPOS_IDLESTAT_ON"),
            Err(_) => println!("bench: idlestat not spawned HOPOS_IDLESTAT_FAIL"),
        }
    }
    if cfg!(feature = "wdtest") {
        match exec.spawn(wdtest(exec)) {
            Ok(()) => println!(
                "bench: watchdog test armed: Hop stops in {} s, the watchdog must reset this node HOPOS_WDTEST_ARMED",
                WDTEST_AFTER.as_secs()
            ),
            Err(_) => println!("bench: wdtest not spawned HOPOS_WDTEST_FAIL"),
        }
    }
    if cfg!(feature = "nvmebench") || crate::BOARD.boot_param("hopos.nvmebench") == "1" {
        match disk.as_mut() {
            Some(d) => bench_disk(exec, d),
            None => println!("nvme bench: no disk on this board, skipped HOPOS_NVMEBENCH_NONE"),
        }
    }
    disk
}

/// Hoe lang de watchdogtoets de node gewoon laat draaien voordat hij Hop
/// stopt: lang genoeg voor de boot, de adoptie na een flip en de eerste
/// canary-rondes.
const WDTEST_AFTER: Duration = Duration::from_secs(60);

/// De brievenbus van de watchdogtoets bij de lifecycle-actor.
static WDTEST_REPLY: Reply = Reply::new();

/// De watchdogtoets (feature `wdtest`): na [`WDTEST_AFTER`] vraagt de kern
/// de lifecycle-actor Hop (slot 1) te stoppen. Daarna slaagt de canary
/// niet meer, de kern pet de watchdog niet meer, en de hardware-watchdog
/// moet de node binnen zijn termijn terugzetten op de kern van de kaart of
/// stick. Zo toets je de echte watchdog zonder een kabel te trekken.
async fn wdtest(exec: &'static Executor) {
    exec.after(WDTEST_AFTER).await;
    let Some(slot) = kern::Slot::new(1) else {
        return;
    };
    let req = Request::Stop {
        slot,
        timeout: Duration::from_secs(5),
    };
    match kern::slots::call(&crate::LIFECYCLE, &WDTEST_REPLY, req).await {
        Ok(Response::Failed(e)) | Err(e) => {
            println!("bench: watchdog test could not stop Hop: {e} HOPOS_WDTEST_FAIL");
        }
        Ok(_) => println!(
            "bench: Hop stopped for the watchdog test; no canary, no pets: the reset must follow HOPOS_WDTEST_HOP_STOPPED"
        ),
    }
}

/// De idle-meetlat van de OS-core, per `every` seconden: wekken en
/// polls van de executor, de interrupts, de lege RX-rondes, het werk van
/// de switch, en de rotatie van de OS-core (in, terug op irq/ipi/timer/
/// yield, de tijd van de bewoners, de kicks). Tempo's per seconde, plus één
/// regel met de cumulatieve drops. Dit is het getal waarop de NIC-interrupt
/// en de slaap afgerekend worden (Go: gepold ~3.300 wekken/s bij stilte,
/// op de interrupt ~100).
async fn idlestat(exec: &'static Executor, every: u64) {
    let hz = cpu::idle::freq().max(1);
    let mut prev = Snap::take(exec);
    let mut n: u64 = 0;
    let start = exec.now();
    loop {
        n += 1;
        exec.until(start.saturating_add(n.saturating_mul(every).saturating_mul(1_000_000_000)))
            .await;
        let now = Snap::take(exec);
        let dt = now.at.saturating_sub(prev.at).max(1);
        let r = |a: u64, b: u64| a.saturating_sub(b).saturating_mul(1_000_000_000) / dt;
        let res_ns = now.os[7]
            .saturating_sub(prev.os[7])
            .saturating_mul(1_000_000_000)
            / hz;
        let res_pm = res_ns.saturating_mul(1000) / dt;
        println!(
            "idle: {} wakes/s, {} polls/s, {} irq/s (timer {}, nic {}, other {}), {} empty rx rounds/s; switch by door {}/s; os core {} in/s, back on irq {}/s ipi {}/s timer {}/s yield {}/s, residents {}.{}%, idle rounds {}/s, kicks {}/s HOPOS_IDLESTAT",
            r(now.exec[0], prev.exec[0]),
            r(now.exec[1], prev.exec[1]),
            r(now.irq.iter().sum(), prev.irq.iter().sum()),
            r(now.irq[0], prev.irq[0]),
            r(now.irq[1], prev.irq[1]),
            r(now.irq[2], prev.irq[2]),
            r(now.net[0], prev.net[0]),
            r(now.net[1], prev.net[1]),
            r(now.os[0], prev.os[0]),
            r(now.os[1], prev.os[1]),
            r(now.os[2], prev.os[2]),
            r(now.os[3], prev.os[3]),
            r(now.os[4], prev.os[4]),
            res_pm / 10,
            res_pm % 10,
            r(now.os[8], prev.os[8]),
            r(now.os[9], prev.os[9]),
        );
        let s = &crate::net::STATS;
        let h = crate::HEAP.stats();
        println!(
            "idle: totals rx full {}, rx drops {}, uplink drops rx {} tx {}, nat oversize {} no-route {} flow-full {}, nic tx errors {}, host drops rx {} tx {}; os exits {} faults {}; kern heap {} KB used, {} refused",
            s.rx_full.load(Relaxed),
            s.rx_drops.load(Relaxed),
            s.uplink_rx_drops.load(Relaxed),
            s.uplink_tx_drops.load(Relaxed),
            s.nat_oversize.load(Relaxed),
            s.nat_no_route.load(Relaxed),
            s.nat_flow_full.load(Relaxed),
            s.nic_tx_errors.load(Relaxed),
            s.host_rx_drops.load(Relaxed),
            s.host_tx_drops.load(Relaxed),
            now.os[5],
            now.os[6],
            h.used >> 10,
            h.refused,
        );
        prev = now;
    }
}

/// Eén foto van alle tellers die idlestat leest.
struct Snap {
    /// Nu, in ns op de klok van de executor.
    at: u64,
    /// Executor: sleeps (de wekken), polls.
    exec: [u64; 2],
    /// De IRQ-dispatch: timer, nic, other.
    irq: [u64; 3],
    /// Het netwerkvlak: lege RX-rondes, werk na de bel.
    net: [u64; 2],
    /// De OS-core: in, irq, ipi, timer, yield, exit, fault, ticks, idle, kicks.
    os: [u64; 10],
}

impl Snap {
    fn take(exec: &'static Executor) -> Self {
        let s = &exec.stats;
        let o = &cpu::el2::OS_STATS;
        let n = &crate::net::STATS;
        Self {
            at: exec.now(),
            exec: [s.sleeps.load(Relaxed), s.polls.load(Relaxed)],
            irq: [
                crate::IRQS[0].load(Relaxed),
                crate::IRQS[1].load(Relaxed),
                crate::IRQS[2].load(Relaxed),
            ],
            net: [n.rx_idle.load(Relaxed), n.work_by_door.load(Relaxed)],
            os: [
                o.entries.load(Relaxed),
                o.irq.load(Relaxed),
                o.ipi.load(Relaxed),
                o.timer.load(Relaxed),
                o.yields.load(Relaxed),
                o.exits.load(Relaxed),
                o.faults.load(Relaxed),
                o.ticks.load(Relaxed),
                o.idle.load(Relaxed),
                o.kicks.load(Relaxed),
            ],
        }
    }
}

/// De schijf-bench: rauw (de Go-tabel per commandomaat, dan sequentieel
/// en willekeurig over de staart) en door hopfs. Vóór de executor, met
/// `block_on`: een bench-boot wacht erop, zoals in Go.
fn bench_disk(exec: &'static Executor, disk: &mut Disk) {
    let sectors = blkdev::Disk::sectors(&*disk);
    let bytes = sectors.saturating_mul(LBA_SIZE);
    // De grootste opdracht die de schijf neemt (Go: `disk.MaxTransfer`),
    // hoogstens de buffer.
    let max = AsyncBlockDevice::max_transfer(&*disk).min(BUF);
    // De staart: de helft van de schijf, hoogstens 1 GiB, op hele MiB.
    let span = (bytes / 2).min(SPAN_MAX) & !((1 << 20) - 1);
    if span < 4 << 20 {
        println!("nvme bench: disk of {bytes} bytes is too small, skipped HOPOS_NVMEBENCH_FAIL");
        return;
    }
    let base = sectors - span / LBA_SIZE;
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(BUF).is_err() {
        println!("nvme bench: no heap for a {BUF}-byte buffer HOPOS_NVMEBENCH_FAIL");
        return;
    }
    buf.resize(BUF, 0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7);
    }
    println!(
        "nvme bench: {} ({} MB), writing the tail LBA {base}..{} ({} MB); the disk goes to the storage after HOPOS_NVMEBENCH_START",
        blkdev::Disk::model(&*disk),
        bytes / 1_000_000,
        sectors - 1,
        span >> 20
    );
    let t = Bench { exec, base, max };
    // Eerst de blokken die een vorige run schreef (oud), straks dezelfde
    // net geschreven (vers): zo zie je of een lees uit de cache van de SSD
    // komt of van het flash.
    let ok = t.random_queue(&mut *disk, span, 1, "old")
        && {
            let q = Queue::new(&mut *disk, Spin);
            let mut io = &q;
            t.sizes(&mut io, &mut buf, span)
                && t.sequential(&mut io, &mut buf, span)
                && t.random(&mut io, &mut buf, span)
        }
        && QUEUE_DEPTHS
            .iter()
            .all(|&d| t.random_queue(&mut *disk, span, d, "fresh"));
    if ok {
        t.hopfs(&Queue::new(&mut *disk, Spin), &mut buf, span);
    }
    println!("nvme bench: done HOPOS_NVMEBENCH_DONE");
}

/// De staart en de klok van één bench-run.
struct Bench {
    exec: &'static Executor,
    base: u64,
    /// De grootste opdracht in bytes: `max_transfer` van de schijf,
    /// hoogstens [`BUF`].
    max: usize,
}

impl Bench {
    /// Nu, in ns.
    fn now(&self) -> u64 {
        self.exec.now()
    }

    /// `n` opdrachten van `sz` bytes schrijven, dan lezen, opdracht `k` op
    /// LBA `lba(k)`. Geeft (schrijf-ns, lees-ns), of `None` na een luide
    /// fout.
    fn pass<D: BlockIo>(
        &self,
        disk: &mut D,
        buf: &mut [u8],
        sz: usize,
        n: u64,
        lba: impl Fn(u64) -> u64,
    ) -> Option<(u64, u64)> {
        let chunk = buf.get_mut(..sz)?;
        let t0 = self.now();
        for k in 0..n {
            // Op een flip-boot loopt de watchdog van de vorige kern door en
            // blokkeert de bench de executor: zelf petten, anders reset de
            // node midden in de bench (de Altra, SBSA 12 s, 04-10).
            crate::watchdog::pet_now();
            if let Err(e) = block_on(disk.write(lba(k), chunk)) {
                println!(
                    "nvme bench: write {sz} at {}: {e} HOPOS_NVMEBENCH_FAIL",
                    lba(k)
                );
                return None;
            }
        }
        let t1 = self.now();
        for k in 0..n {
            crate::watchdog::pet_now();
            if let Err(e) = block_on(disk.read(lba(k), chunk)) {
                println!(
                    "nvme bench: read {sz} at {}: {e} HOPOS_NVMEBENCH_FAIL",
                    lba(k)
                );
                return None;
            }
        }
        Some((t1.saturating_sub(t0), self.now().saturating_sub(t1)))
    }

    /// De Go-tabel: per commandomaat 16 MiB schrijven en lezen (dezelfde
    /// regelvorm als `nvmeBench`, zodat de getallen naast elkaar passen).
    fn sizes<D: BlockIo>(&self, disk: &mut D, buf: &mut [u8], span: u64) -> bool {
        for sz in SIZES {
            if sz > self.max as u64 {
                continue;
            }
            let n = PER_SIZE.min(span) / sz;
            let step = sz / LBA_SIZE;
            let base = self.base;
            let Some((w, r)) = self.pass(disk, buf, sz as usize, n, |k| base + k * step) else {
                return false;
            };
            line("nvme bench:", sz, n, w, r, "cmd");
        }
        true
    }

    /// Sequentieel over de hele staart in de grootste opdrachten die de
    /// schijf neemt (hoogstens 1 MiB).
    fn sequential<D: BlockIo>(&self, disk: &mut D, buf: &mut [u8], span: u64) -> bool {
        let sz = self.max as u64;
        let n = span / sz;
        let step = sz / LBA_SIZE;
        let base = self.base;
        let Some((w, r)) = self.pass(disk, buf, self.max, n, |k| base + k * step) else {
            return false;
        };
        println!(
            "nvme bench: sequential {} MiB in {} KiB commands: write {} MB/s, read {} MB/s HOPOS_NVMEBENCH_SEQ",
            span >> 20,
            sz >> 10,
            rate(span, w),
            rate(span, r)
        );
        true
    }

    /// Willekeurig: 4 KiB-opdrachten op pseudo-willekeurige 4 KiB-plekken
    /// in de staart (xorshift, vast zaad: elke run dezelfde plekken).
    fn random<D: BlockIo>(&self, disk: &mut D, buf: &mut [u8], span: u64) -> bool {
        let sz: u64 = 4 << 10;
        let slots = span / sz;
        let n = slots.min(RANDOM_OPS);
        let (base, step) = (self.base, sz / LBA_SIZE);
        let at = |k: u64| base + (xorshift(k) % slots) * step;
        let Some((w, r)) = self.pass(disk, buf, sz as usize, n, at) else {
            return false;
        };
        let iops = |ns: u64| n.saturating_mul(1_000_000_000) / ns.max(1);
        println!(
            "nvme bench: random 4 KiB x{n}: write {} IOPS ({} MB/s), read {} IOPS ({} MB/s) HOPOS_NVMEBENCH_RAND",
            iops(w),
            rate(n * sz, w),
            iops(r),
            rate(n * sz, r)
        );
        true
    }

    /// Willekeurig lezen door de wachtrij: `depth` lezingen van 4 KiB
    /// tegelijk op de plekken van [`Self::random`], door de `blkdev::Queue`
    /// die hopfs ook gebruikt. Het plafond van het device voor de hele node
    /// (Linux: fio met iodepth); de bytes gaan niet naar een buffer, alleen
    /// het device telt.
    ///
    /// In een andere volgorde dan [`Self::random`] ze schreef: in
    /// schrijfvolgorde liggen opeenvolgende plekken op hetzelfde stuk flash
    /// en wachten zestien lezingen op een paar dies (GEMETEN 04-10 op de
    /// Altra: 186k in schrijfvolgorde, 289k door elkaar). De regel zegt ook
    /// waar de tijd zit: het device (van submit tot de completion gezien
    /// werd) en wij (submit en ophalen per lees, en hoeveel rondes van de
    /// wachter niets vonden).
    fn random_queue<D: AsyncBlockDevice>(
        &self,
        disk: &mut D,
        span: u64,
        depth: usize,
        what: &str,
    ) -> bool {
        let sz: u64 = 4 << 10;
        let slots = span / sz;
        let n = slots.min(RANDOM_OPS);
        let (base, step) = (self.base, sz / LBA_SIZE);
        // Een oneven vermenigvuldiger is een permutatie modulo 2^k.
        let mixed = |k: u64| {
            if n.is_power_of_two() {
                k.wrapping_mul(0x9e37_79b1) & (n - 1)
            } else {
                k
            }
        };
        let q = Queue::new(Meter::new(disk), Spin);
        let depth = q.depth().min(depth).min(QUEUE_DEPTH);
        let mut pool = core::pin::pin!(Futures::<_, QUEUE_DEPTH>::new());
        let (mut k, mut fail) = (0u64, None);
        let t0 = self.now();
        block_on(core::future::poll_fn(|cx| {
            loop {
                crate::watchdog::pet_now();
                while pool.len() < depth && k < n {
                    let lba = base + (xorshift(mixed(k)) % slots) * step;
                    let read = q.io(
                        Op::Read {
                            lba,
                            len: sz as usize,
                        },
                        &mut [],
                    );
                    if pool.as_mut().push(read).is_err() {
                        break;
                    }
                    k += 1;
                }
                match pool.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(()))) => {}
                    Poll::Ready(Some(Err(e))) => {
                        fail = Some(e);
                        return Poll::Ready(());
                    }
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }));
        let ns = self.now().saturating_sub(t0);
        if let Some(e) = fail {
            println!("nvme bench: random read {depth} at once: {e} HOPOS_NVMEBENCH_FAIL");
            return false;
        }
        let m = q.with_dev(|m| m.ns());
        let per = |x: u64, k: u64| x / k.max(1);
        println!(
            "nvme bench: random 4 KiB read ({what}), {depth} at once (peak {}) x{n}: {} IOPS ({} MB/s); device {} us per read, ours {} ns submit and {} ns reap, {}% of the reaps empty HOPOS_NVMEBENCH_RANDQ",
            q.peak(),
            n.saturating_mul(1_000_000_000) / ns.max(1),
            rate(n * sz, ns),
            per(m.device, m.done) / 1000,
            per(m.submit, m.starts),
            per(m.reap, m.reaps),
            per(m.empty * 100, m.reaps)
        );
        true
    }

    /// Door hopfs: een vluchtige bestandslaag op dezelfde staart, 16 MiB
    /// in calls van 1 MiB en 64 KiB (Go: `hopfsBench`). Het verschil met
    /// de rauwe regels is wat hopfs zelf kost, zonder servicer en
    /// transport. Vóór de executor draait: elke call met `block_on`
    /// afgedraaid.
    fn hopfs<D: AsyncBlockDevice>(&self, q: &Queue<D, Spin>, buf: &mut [u8], span: u64) {
        let fs = match Fs::new(q, self.base, span / LBA_SIZE, LBA_SIZE, self.max as u64) {
            Ok(f) => LocalCell::cell(f),
            Err(e) => {
                println!("hopfs bench: {e} HOPOS_NVMEBENCH_FAIL");
                return;
            }
        };
        let path: &[u8] = b"/.bench/hopfs.bin";
        if let Err(e) = fs.borrow_mut().mkdir_all(b"/.bench") {
            println!("hopfs bench: mkdir: {e} HOPOS_NVMEBENCH_FAIL");
            return;
        }
        for sz in [1u64 << 20, 64 << 10] {
            let n = PER_SIZE.min(span / 2) / sz;
            let Some(chunk) = buf.get_mut(..sz as usize) else {
                return;
            };
            let t0 = self.now();
            for k in 0..n {
                crate::watchdog::pet_now();
                if let Err(e) = block_on(hopfs::path::write(&fs, path, k * sz, chunk)) {
                    println!("hopfs bench: write: {e} HOPOS_NVMEBENCH_FAIL");
                    return;
                }
            }
            let t1 = self.now();
            for k in 0..n {
                crate::watchdog::pet_now();
                if let Err(e) = block_on(hopfs::path::read(&fs, path, k * sz, chunk)) {
                    println!("hopfs bench: read: {e} HOPOS_NVMEBENCH_FAIL");
                    return;
                }
            }
            let (w, r) = (t1.saturating_sub(t0), self.now().saturating_sub(t1));
            line("hopfs bench:", sz, n, w, r, "call");
        }
    }
}

/// De meetlat van [`Bench::random_queue`]: een schijf die telt wat de
/// wachtrij met hem doet, in tikken van de teller.
struct Meter<D> {
    dev: D,
    /// Per ticket de tik van zijn submit.
    sub: [u64; blkdev::MAX_DEPTH],
    starts: u64,
    submit: u64,
    reaps: u64,
    empty: u64,
    reap: u64,
    /// Opgeteld: van submit tot de reap die hem zag.
    device: u64,
    done: u64,
}

/// De tellers van een [`Meter`] in nanoseconden.
struct MeterNs {
    starts: u64,
    submit: u64,
    reaps: u64,
    empty: u64,
    reap: u64,
    device: u64,
    done: u64,
}

impl<D: AsyncBlockDevice> Meter<D> {
    fn new(dev: D) -> Self {
        Self {
            dev,
            sub: [0; blkdev::MAX_DEPTH],
            starts: 0,
            submit: 0,
            reaps: 0,
            empty: 0,
            reap: 0,
            device: 0,
            done: 0,
        }
    }

    fn ns(&self) -> MeterNs {
        let hz = u128::from(cpu::idle::freq().max(1));
        let ns = |t: u64| (u128::from(t) * 1_000_000_000 / hz) as u64;
        MeterNs {
            starts: self.starts,
            submit: ns(self.submit),
            reaps: self.reaps,
            empty: self.empty,
            reap: ns(self.reap),
            device: ns(self.device),
            done: self.done,
        }
    }
}

impl<D: AsyncBlockDevice> AsyncBlockDevice for Meter<D> {
    fn max_transfer(&self) -> usize {
        self.dev.max_transfer()
    }
    fn depth(&self) -> usize {
        self.dev.depth()
    }
    fn start_tag(&mut self, op: Op<'_>) -> blkdev::Result<usize> {
        let a = cpu::idle::counter();
        let r = self.dev.start_tag(op);
        let b = cpu::idle::counter();
        self.starts += 1;
        self.submit += b.saturating_sub(a);
        if let Ok(t) = r
            && let Some(s) = self.sub.get_mut(t)
        {
            *s = b;
        }
        r
    }
    fn poll_tag(&mut self, t: usize, into: &mut [u8]) -> Poll<blkdev::Result> {
        self.dev.poll_tag(t, into)
    }
    fn reap(&mut self) -> blkdev::Result<u64> {
        let a = cpu::idle::counter();
        let r = self.dev.reap();
        let b = cpu::idle::counter();
        self.reaps += 1;
        self.reap += b.saturating_sub(a);
        let mut m = *r.as_ref().unwrap_or(&0);
        self.empty += u64::from(m == 0);
        while m != 0 {
            let t = m.trailing_zeros() as usize;
            m &= m - 1;
            self.device += b.saturating_sub(self.sub.get(t).copied().unwrap_or(b));
            self.done += 1;
        }
        r
    }
}

/// De plek van opdracht `k` van de willekeurige fasen (xorshift, vast
/// zaad: elke run dezelfde plekken).
fn xorshift(k: u64) -> u64 {
    let mut x = k.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Eén regel van de Go-tabel: maat, aantal, en per richting MB/s en µs
/// per opdracht.
fn line(what: &str, sz: u64, n: u64, w: u64, r: u64, per: &str) {
    let us = |ns: u64| ns / 1000 / n.max(1);
    println!(
        "{what} {:4} KiB x{n:<4} write {:>6} MB/s ({:5} us/{per}), read {:>6} MB/s ({:5} us/{per}) HOPOS_NVMEBENCH",
        sz >> 10,
        rate(n * sz, w),
        us(w),
        rate(n * sz, r),
        us(r)
    );
}

/// Decimale MB/s met één decimaal, als tekst (geen float in de kern).
fn rate(bytes: u64, ns: u64) -> String {
    let tenths = u128::from(bytes) * 10_000 / u128::from(ns.max(1));
    alloc::format!("{}.{}", tenths / 10, tenths % 10)
}
