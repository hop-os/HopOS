//! De opslag van de kern-binary: de schijf van het board, hopfs erop, de
//! hopfs-actor die de bestandscalls van de system-API bedient, en de
//! committer die de boom vastlegt (Go: `useFS` en `fsCommitEvery` in
//! `OLD/metal/cmd/hopos/fspersist.go`).
//!
//! Eigendom: de actor bezit de [`Fs`] en daarmee de driver (de schijf gaat
//! als waarde de actor in); wie iets wil, stuurt een bericht naar
//! [`FS_INBOX`]. De committer bezit niets: hij kijkt naar de
//! servicer-tabel en stuurt een `Commit`.
//!
//! Wachten op de schijf: de driver zit in een [`Queue`] met de klok en de
//! timers van de executor ([`ExecPace`]). Elk blok-verzoek is een ticket
//! plus een await op zijn eigen completion; één wachter pollt het device
//! (op de bel van de lijn met vangrail 10 ms, of zonder lijn op een timer)
//! en wekt de anderen, en intussen draait de executor door. Zo heeft de
//! actor meerdere calls tegelijk op de schijf (de ANS: zestien tags). Les
//! van 30-09 (de soak): de synchrone commit hield op een trage schijf de
//! OS-core tot 7 s stil (`late_ms=6611` op de tik, Hop en de switch zonder
//! beurt). De mount bij de boot draait vóór `exec.run`: dezelfde futures,
//! afgedraaid met `blkdev::block_on` (de enige wachter is dan de pacer en
//! toetst het device bij elke ronde). Er is geen tweede, synchroon
//! driverpad (waarom: de crate-doc van `blkdev`).
//!
//! De kern-flip: vóór de sprong legt de actor de boom vast en neemt hij
//! niets meer aan ([`freeze_for_flip`]); de nieuwe kern mount dezelfde
//! schijf en vindt precies die generatie.
//!
//! Hard (docs/sequence-hopos.md §13.6): een schijf die er is maar faalt, een
//! mount die faalt (een te klein venster, een blokfout, een boom die niet
//! ontcijfert) en een actor of committer die niet spawnt, zijn `fatal`. Geen
//! schijf is geen fout: dan weigert elke bestandscall luid.
//!
//! Stateless of stateful, zoals in Go: `hopos.storage=stateless` (de default)
//! negeert bij een koude boot het indexbestand en begint leeg
//! (`HOPOS_FS_STATELESS`): een device begint schoon en vergeet alles.
//! `hopos.storage=stateful` laadt de laatst vastgelegde boom, en Hop's staat
//! op `/hop/` overleeft dan een herstart. Een flip is altijd stateful: de
//! nieuwe kern mount precies de generatie die de oude vastlegde.

use crate::Disk;
use alloc::boxed::Box;
use blkdev::{AsyncBlockDevice, LBA_SIZE, Pace, Queue, block_on};
use board::Board;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use core::time::Duration;
use cpu::println;
use executor::Executor;
use kern::hopfs::{Fs, Mounted};
use kern::rpc::{self, FsActor, FsInbox, committer};
use kern::slots::Reply;
use sync::mpsc::Mailbox;
use sync::{Either, select};

/// De brievenbus van de hopfs-actor: de verbindingstaken van de
/// system-API sturen er hun bestandscalls heen, de committer zijn commits.
pub(crate) static FS_INBOX: FsInbox<'static> = Mailbox::new();

/// Draait de hopfs-actor? Zonder actor valt er vóór een flip niets te
/// bevriezen.
static UP: AtomicBool = AtomicBool::new(false);

/// De antwoordplek van de flip-taak bij de actor (één aanroeper).
static FREEZE_REPLY: Reply = Reply::new();

/// Draait de hopfs-actor?
pub(crate) fn is_up() -> bool {
    UP.load(Relaxed)
}

/// De kern-flip vóór de sprong: de boom vastleggen en de actor dicht (elke
/// call daarna is `Busy`, luid; `HOPOS_FS_FROZEN`). Geeft de vastgelegde
/// generatie: de nieuwe kern mount precies die (`HOPOS_FS_UP fresh=0
/// generation=N`). `Ok(None)` zonder schijf. Een actor die niet binnen
/// `wait` antwoordt, of een commit die faalt, is een fout: dan geen flip,
/// want wat de apps schreven zou de nieuwe kern niet vinden.
///
/// Dat de actor tussen deze bevriezing en de sprong niets meer schrijft,
/// is de vorm: hij is de enige die de schijf aanraakt (PORT.md §3), en hij
/// weigert alles.
pub(crate) async fn freeze_for_flip(
    exec: &'static Executor,
    wait: Duration,
) -> kern::Result<Option<u64>> {
    if !is_up() {
        return Ok(None);
    }
    match select(rpc::freeze(&FS_INBOX, &FREEZE_REPLY), exec.after(wait)).await {
        Either::Left(Ok(g)) => Ok(Some(g)),
        Either::Left(Err(e)) => {
            println!("hopfs: not frozen for the flip: {e} HOPOS_FS_FREEZE_FAIL");
            Err(e)
        }
        Either::Right(()) => {
            println!(
                "hopfs: the actor did not freeze within {} ms HOPOS_FS_FREEZE_FAIL",
                wait.as_millis()
            );
            Err(kern::Error::Busy)
        }
    }
}

/// De flip ging niet door: de actor weer open. `false` als het bericht de
/// actor niet bereikte (volle brievenbus).
#[must_use]
pub(crate) fn thaw_after_flip() -> bool {
    !is_up() || rpc::thaw(&FS_INBOX)
}

/// De timers van de executor als `blkdev::Pace`: de termijn van de
/// hopfs-actor tijdens een blok-verzoek.
pub(crate) struct ExecPace(pub(crate) &'static Executor);

impl Pace for ExecPace {
    type Sleep = executor::After<512, 256>;
    fn sleep(&self, d: Duration) -> Self::Sleep {
        self.0.after(d)
    }
}

/// Hoe vaak de meetlat van de schijf een regel krijgt (als er iets
/// gebeurde): het ritme van de commit.
const STATS_EVERY: Duration = Duration::from_secs(10);

/// De meetlat van de schijf, één regel per [`STATS_EVERY`] als er
/// opdrachten waren: de driver (opdrachten, read-ahead, de traagste) en de
/// wachtrij (het hoogste aantal tegelijk, en hoe vaak de pacer keek).
async fn stats(exec: &'static Executor, disk: &'static DiskQueue) {
    let mut last = 0;
    loop {
        exec.after(STATS_EVERY).await;
        let now = disk.with_dev(|d| d.stats(&mut Sink).unwrap_or(0));
        if now == last {
            continue;
        }
        last = now;
        println!(
            "disk: {} queue_peak={} queue_polls={} HOPOS_DISK_STATS",
            DevStats(disk),
            disk.peak(),
            disk.polls()
        );
    }
}

/// Een schrijver die alles weggooit: alleen de teller telt.
struct Sink;

impl core::fmt::Write for Sink {
    fn write_str(&mut self, _s: &str) -> core::fmt::Result {
        Ok(())
    }
}

/// De meetlat van de driver als `Display`, zonder buffer.
struct DevStats(&'static DiskQueue);

impl core::fmt::Display for DevStats {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.with_dev(|d| d.stats(f).map(drop))
    }
}

/// Zoekt de schijf van het board, één keer voor de hele boot: `probe_disk`
/// mag maar één keer, en de schijf gaat naar [`start`]. Geen schijf is één
/// regel en `None`: de node draait door en weigert elke bestandscall luid.
/// Een schijf die faalt, is het ijzer: `fatal`; een schijf zonder lijn ook
/// (docs/sequence-hopos.md §13.3).
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn probe() -> Option<Disk> {
    match board::Board::probe_disk(&crate::BOARD) {
        Ok(Some(d)) if d.irq().is_none() => {
            cpu::fatal!("disk: the board wired no IRQ line, no disk without its line")
        }
        Err(board::Error::Irq(why)) => cpu::fatal!("disk: {why}, no disk without its line"),
        Ok(Some(d)) => Some(d),
        Ok(None) => {
            println!("disk: none found on this board, file calls refused HOPOS_DISK_NONE");
            None
        }
        Err(e) => cpu::fatal!("disk: {e}"),
    }
}

/// De wachtrij van de opslag, zoals [`start`] hem geeft.
pub(crate) type DiskQueue = Queue<Disk, ExecPace>;

/// Zet de opslag op de geprobede schijf ([`probe`]): hopfs mounten, actor
/// en committer spawnen. Geeft de wachtrij als de bestandscalls bediend
/// worden; zonder schijf draait de node door en weigert elke bestandscall
/// luid (de regel gaf [`probe`] al).
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor, disk: Option<Disk>) -> Option<&'static DiskQueue> {
    let board = &crate::BOARD;
    let disk = disk?;
    let sectors = blkdev::Disk::sectors(&disk);
    // De maat van het blokcontract, niet een eigen methode van de driver
    // (de ANS heeft er een met dezelfde naam: de MDTS zonder afronding).
    let max_transfer = AsyncBlockDevice::max_transfer(&disk);
    println!(
        "disk: up HOPOS_DISK_UP model={} blocks={sectors} block_size={LBA_SIZE} max_transfer={max_transfer}",
        blkdev::Disk::model(&disk)
    );
    // De wachtrij leeft zolang de kern: één keer bij de boot op de heap.
    let disk: &'static DiskQueue = Box::leak(Box::new(Queue::new(disk, ExecPace(exec))));
    println!(
        "disk: queue of {} request(s) at once HOPOS_DISK_QUEUE depth={}",
        disk.depth(),
        disk.depth()
    );
    // Vóór `exec.run`: de mount wacht zelf (block_on pollt het device over
    // hetzelfde pad als de actor straks), en er is nog niemand die stil zou
    // staan.
    // De default is stateless: bij een koude boot wordt het indexbestand
    // genegeerd en begint de boom leeg. Alleen `hopos.storage=stateful`
    // laadt hem. Een flip-landing (een gevonden overdracht, warm of koud)
    // houdt altijd de boom van de vorige kern; een firmware-boot niet, ook
    // niet op riscv waar elke boot als sprong telt (`flip::jumped`).
    let stateless =
        crate::BOARD.boot_param("hopos.storage") != "stateful" && !crate::flip::landed();
    if stateless {
        println!(
            "hopfs: stateless (hopos.storage), the index is ignored and the tree starts empty HOPOS_FS_STATELESS"
        );
    }
    let mounted = block_on(Fs::mount(
        disk,
        0,
        sectors,
        LBA_SIZE,
        max_transfer as u64,
        stateless,
    ));
    let (fs, found) = match mounted {
        Ok(m) => m,
        Err(e) => cpu::fatal!("hopfs: mount failed: {e}"),
    };
    let generation = fs.generation();
    let (fresh, what) = match &found {
        Mounted::Restored { generation, slot } => {
            println!(
                "hopfs: tree restored, generation {generation} from place {slot} (stateful: a reboot keeps the volumes)"
            );
            (0, "restored")
        }
        Mounted::Empty => (1, "empty disk"),
        Mounted::Fresh => (1, if stateless { "stateless" } else { "wiped" }),
    };
    println!(
        "hopfs: mounted {} MiB ({what}) HOPOS_FS_UP fresh={fresh} generation={generation}",
        (sectors * LBA_SIZE) >> 20
    );
    let max_slots = crate::slot_count(board);
    let actor = async move {
        let mut a = FsActor::new(fs, &crate::SERVICERS, crate::KernConsole);
        a.run(&FS_INBOX).await;
    };
    if exec.spawn(actor).is_err() {
        cpu::fatal!("hopfs: actor not spawned");
    }
    UP.store(true, Relaxed);
    if exec.spawn(stats(exec, disk)).is_err() {
        println!("disk: stats task not spawned HOPOS_DISK_STATS_FAIL");
    }
    let commit = async move {
        committer(
            &crate::SERVICERS,
            &FS_INBOX,
            &crate::clock::ExecTimer(exec),
            max_slots,
        )
        .await;
    };
    if exec.spawn(commit).is_err() {
        cpu::fatal!("hopfs: committer not spawned, the tree would not be kept");
    }
    Some(disk)
}
