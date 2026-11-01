//! De mount-resolutie, de padtoetsen en de hopfs-actor op een nep-schijf.

use super::*;
use crate::hopfs::Fs;
use crate::slots::tests::{FakeConsole, Obey, actor, start, stop};
use abi::hopabi::STATUS_ERROR;
use std::vec;
use std::vec::Vec;

fn m(local: &str, shared: &str) -> Mount {
    Mount {
        local: local.as_bytes().to_vec(),
        shared: shared.as_bytes().to_vec(),
    }
}

fn s(i: usize) -> Slot {
    Slot::new(i).unwrap()
}

fn clean(p: &str) -> Result<Vec<u8>> {
    let mut b = PathBuf::new();
    clean_abs(p.as_bytes(), &mut b)?;
    Ok(b.as_bytes().to_vec())
}

fn res(slot: usize, t: &[Mount], p: &str) -> Result<(Vec<u8>, Option<usize>)> {
    let mut b = PathBuf::new();
    let v = resolve(s(slot), t, p.as_bytes(), &mut b)?;
    Ok((b.as_bytes().to_vec(), v))
}

#[test]
fn clean_abs_normalizes_and_refuses_dot_dot() {
    assert_eq!(clean("").unwrap(), b"/");
    assert_eq!(clean("/").unwrap(), b"/");
    assert_eq!(clean("a//b/./c/").unwrap(), b"/a/b/c");
    assert_eq!(clean("hallo.txt").unwrap(), b"/hallo.txt");
    assert_eq!(clean("../x"), Err(Error::Denied));
    assert_eq!(clean("/a/../../etc"), Err(Error::Denied));
    let long = "a/".repeat(MAX_PATH);
    assert!(matches!(clean(&long), Err(Error::TooLarge { .. })));
}

#[test]
fn own_root_per_slot() {
    assert_eq!(
        res(2, &[], "hallo.txt").unwrap(),
        (b"/.tasks/slot2/hallo.txt".to_vec(), None)
    );
    assert_eq!(res(2, &[], "/").unwrap(), (b"/.tasks/slot2".to_vec(), None));
    assert_eq!(res(128, &[], "/x").unwrap().0, b"/.tasks/slot128/x");
    // Een app kan niet naar de root van een ander: `..` is een weigering,
    // en een absoluut pad valt altijd onder de eigen root.
    assert_eq!(res(2, &[], "/../slot3/x"), Err(Error::Denied));
    assert_eq!(
        res(2, &[], "/.tasks/slot3/x").unwrap().0,
        b"/.tasks/slot2/.tasks/slot3/x"
    );
}

#[test]
fn mounts_resolve_longest_prefix_first() {
    let t = mount_table(&[
        m("/data", "/volumes/data"),
        m("/data/logs", "/volumes/logs"),
    ])
    .unwrap();
    assert_eq!(t[0].local, b"/data/logs", "longest local first");
    assert_eq!(
        res(1, &t, "/data/logs/a").unwrap(),
        (b"/volumes/logs/a".to_vec(), Some(0))
    );
    assert_eq!(
        res(1, &t, "/data/x").unwrap(),
        (b"/volumes/data/x".to_vec(), Some(1))
    );
    assert_eq!(
        res(1, &t, "/data").unwrap(),
        (b"/volumes/data".to_vec(), Some(1))
    );
    // Een prefix op een grens: /database is niet /data.
    assert_eq!(
        res(1, &t, "/database").unwrap(),
        (b"/.tasks/slot1/database".to_vec(), None)
    );
    // Hop: /hop is een volume, de rest zijn eigen root.
    let hop = mount_table(&[m("/hop/", "/volumes/hop")]).unwrap();
    assert_eq!(
        res(1, &hop, "/hop/agent-state.json").unwrap().0,
        b"/volumes/hop/agent-state.json"
    );
}

#[test]
fn mount_table_refuses_escapes() {
    assert_eq!(
        mount_table(&[m("/", "/x")]),
        Err(Error::Denied),
        "overmount /"
    );
    assert_eq!(
        mount_table(&[m("/a", "/")]),
        Err(Error::Denied),
        "share everything"
    );
    assert_eq!(
        mount_table(&[m("/a", "/.tasks/slot3")]),
        Err(Error::Denied),
        "another root"
    );
    assert_eq!(mount_table(&[m("/a", "/.tasks")]), Err(Error::Denied));
    assert_eq!(mount_table(&[m("/a/../b", "/x")]), Err(Error::Denied));
    assert_eq!(
        mount_table(&[m("/a", "/x"), m("a/", "/y")]),
        Err(Error::BadPath),
        "duplicate local"
    );
    // `/.tasksx` is geen taakroot.
    assert!(mount_table(&[m("/a", "/.tasksx")]).is_ok());
}

// ---------------------------------------------------------------------------
// De actor op een schijf in RAM.
// ---------------------------------------------------------------------------

/// Wat een opdracht op de nep-controller is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Read,
    Write,
    Flush,
}

/// Eén opdracht op de nep-controller: soort, lba, de bytes van een
/// schrijf, en of hij terug is.
type Tag = (Kind, u64, Vec<u8>, bool);

/// De staat van de nep-controller: een schijf in RAM met tickets. Een
/// opdracht is klaar bij de eerste `reap` erna, tenzij de test zijn soort
/// vasthoudt; een schrijf staat pas bij zijn completion op de schijf.
#[derive(Default)]
pub(crate) struct RamCtl {
    pub(crate) data: Vec<u8>,
    pub(crate) flushes: usize,
    /// Per tag de opdracht.
    tags: Vec<Option<Tag>>,
    /// Deze soorten blijven op het device tot de test ze loslaat.
    pub(crate) hold: Vec<Kind>,
    /// Wat er gebeurde, in volgorde: ("start" of "done", soort, lba).
    pub(crate) log: Vec<(&'static str, Kind, u64)>,
    /// Het hoogste aantal opdrachten tegelijk op het device.
    pub(crate) peak: usize,
    /// Een lees van deze LBA's faalt (een kapot blok).
    pub(crate) bad: Vec<u64>,
}

/// Het handvat op de nep-controller: de wachtrij bezit hem, de test kijkt
/// mee.
#[derive(Clone)]
pub(crate) struct Ram(pub(crate) std::rc::Rc<core::cell::RefCell<RamCtl>>);

/// De schijf van de tests: de wachtrij over de nep-controller, zoals de
/// kern hem over de ANS heeft.
pub(crate) type Disk = &'static blkdev::Queue<Ram, blkdev::Spin>;

impl Ram {
    pub(crate) fn new(bytes: usize) -> Ram {
        Ram(std::rc::Rc::new(core::cell::RefCell::new(RamCtl {
            data: vec![0; bytes],
            tags: vec![None; 16],
            ..RamCtl::default()
        })))
    }

    /// De wachtrij erover, voor de hele test.
    pub(crate) fn queue(&self) -> Disk {
        std::boxed::Box::leak(std::boxed::Box::new(blkdev::Queue::new(
            self.clone(),
            blkdev::Spin,
        )))
    }

    pub(crate) fn hold(&self, k: Kind) {
        self.0.borrow_mut().hold.push(k);
    }

    pub(crate) fn release(&self, k: Kind) {
        self.0.borrow_mut().hold.retain(|h| *h != k);
    }

    pub(crate) fn on_device(&self) -> usize {
        self.0
            .borrow()
            .tags
            .iter()
            .flatten()
            .filter(|t| !t.3)
            .count()
    }

    pub(crate) fn flushes(&self) -> usize {
        self.0.borrow().flushes
    }

    pub(crate) fn log(&self) -> Vec<(&'static str, Kind, u64)> {
        self.0.borrow().log.clone()
    }
}

impl blkdev::AsyncBlockDevice for Ram {
    fn max_transfer(&self) -> usize {
        1 << 20
    }
    fn depth(&self) -> usize {
        16
    }
    fn start_tag(&mut self, op: blkdev::Op<'_>) -> blkdev::Result<usize> {
        let mut c = self.0.borrow_mut();
        let t = c
            .tags
            .iter()
            .position(Option::is_none)
            .ok_or(blkdev::Error::Busy)?;
        let rec = match op {
            blkdev::Op::Read { lba, len } => {
                if lba as usize * 512 + len > c.data.len() || c.bad.contains(&lba) {
                    return Err(blkdev::Error::Io { lba });
                }
                (Kind::Read, lba, vec![0; len], false)
            }
            blkdev::Op::Write { lba, data } => (Kind::Write, lba, data.to_vec(), false),
            blkdev::Op::Flush => (Kind::Flush, 0, Vec::new(), false),
        };
        c.log.push(("start", rec.0, rec.1));
        c.tags[t] = Some(rec);
        c.peak = c.peak.max(c.tags.iter().flatten().filter(|t| !t.3).count());
        Ok(t)
    }
    fn reap(&mut self) -> blkdev::Result<u64> {
        let mut c = self.0.borrow_mut();
        let mut m = 0u64;
        for t in 0..c.tags.len() {
            let Some((k, lba, bytes, false)) = c.tags[t].clone() else {
                continue;
            };
            if c.hold.contains(&k) {
                continue;
            }
            let o = lba as usize * 512;
            match k {
                Kind::Write => {
                    let Some(d) = c.data.get_mut(o..o + bytes.len()) else {
                        return Err(blkdev::Error::Io { lba });
                    };
                    d.copy_from_slice(&bytes);
                }
                Kind::Flush => c.flushes += 1,
                Kind::Read => {}
            }
            c.log.push(("done", k, lba));
            c.tags[t] = Some((k, lba, bytes, true));
            m |= 1 << t;
        }
        Ok(m)
    }
    fn poll_tag(&mut self, t: usize, into: &mut [u8]) -> core::task::Poll<blkdev::Result> {
        let mut c = self.0.borrow_mut();
        let Some((k, lba, bytes, true)) = c.tags[t].clone() else {
            return core::task::Poll::Pending;
        };
        if k == Kind::Read {
            let o = lba as usize * 512;
            let n = bytes.len().min(into.len());
            into[..n].copy_from_slice(&c.data[o..o + n]);
        }
        c.tags[t] = None;
        core::task::Poll::Ready(Ok(()))
    }
}

/// Een future van de actor of van hopfs afdraaien: pollen tot hij klaar
/// is (de nep-controller rondt af bij elke blik, tenzij hij vasthoudt).
pub(crate) fn on<F: core::future::Future>(f: F) -> F::Output {
    blkdev::block_on(f)
}

pub(crate) fn disk(mib: usize) -> (Fs<Disk>, crate::hopfs::Mounted) {
    let r = Ram::new(mib << 20);
    let sectors = (mib << 20) as u64 / 512;
    on(Fs::mount(r.queue(), 0, sectors, 512, 1 << 20, false)).unwrap()
}

/// Schrijft `data` als `path` op een schijf van [`disk`], vóór de actor hem
/// krijgt: dezelfde weg als een call (`hopfs::path::write`).
pub(crate) fn put(fs: Fs<Disk>, path: &[u8], data: &[u8]) -> Fs<Disk> {
    let t = LocalCell::cell(fs);
    on(crate::hopfs::path::write(&t, path, 0, data)).unwrap();
    t.into_inner().into_inner()
}

/// De nep-controller onder een schijf van [`disk`].
pub(crate) fn ram(fs: &Fs<Disk>) -> Ram {
    fs.disk().with_dev(|r| r.clone())
}

/// De les van 30-09: een commit die op de schijf wacht, houdt de actor niet
/// vast in één poll. Hij geeft af (de executor draait door: de tik, Hop,
/// de switch) en maakt de commit af zodra het device klaar is.
#[test]
fn a_commit_waits_for_the_disk_without_holding_the_core() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let (fs, _) = disk(64);
    let r = ram(&fs);
    let fs = put(fs, b"/volumes/hop/state", b"staat");
    let mut f = FsActor::new(fs, &svc, &con);
    let inbox: FsInbox<'_> = Mailbox::new();
    assert!(
        inbox
            .try_send(FsEnvelope {
                msg: FsMsg::Commit(CommitWhy::Periodic),
                reply: None,
            })
            .is_ok()
    );
    r.hold(Kind::Flush);
    let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
    let mut run = core::pin::pin!(f.run(&inbox));
    for _ in 0..8 {
        assert!(run.as_mut().poll(&mut cx).is_pending());
    }
    assert!(!con.saw("HOPOS_FS_COMMIT"), "committed without the disk");
    r.release(Kind::Flush);
    for _ in 0..8 {
        let _ = run.as_mut().poll(&mut cx);
    }
    assert!(con.saw("hopfs: tree committed as generation 1 (every 10 s) HOPOS_FS_COMMIT"));
}

/// Een call zoals een verbinding hem stuurt: pad en data in de callbuffer.
fn fs_call(
    slot: usize,
    generation: u32,
    op: u8,
    path: &str,
    off: u64,
    n: u64,
    data: &[u8],
) -> FsCall {
    let mut buf = vec![0u8; HDR_LEN];
    buf.extend_from_slice(path.as_bytes());
    let p = HDR_LEN..buf.len();
    buf.extend_from_slice(data);
    FsCall {
        slot: s(slot),
        generation,
        op,
        off,
        n,
        data: p.end..buf.len(),
        path: p,
        buf,
        out: vec![0u8; HDR_LEN + (64 << 10)],
    }
}

fn data(c: &FsCall, n: usize) -> &[u8] {
    &c.out[HDR_LEN..HDR_LEN + n]
}

#[test]
fn actor_serves_the_file_calls_in_the_own_root() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 8, 1).unwrap();
    let g = svc.current(s(2)).unwrap();
    let (fs, _) = disk(64);
    let mut f = FsActor::new(fs, &svc, &con);

    let mut c = fs_call(2, g, OP_STAT, "hallo.txt", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::NoEnt));
    let mut c = fs_call(2, g, OP_WRITE, "hallo.txt", 0, 0, b"hallo wereld");
    assert_eq!(on(f.handle(&mut c)), Ok((12, 0)));
    let mut c = fs_call(2, g, OP_STAT, "/hallo.txt", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((12, 0)));
    let mut c = fs_call(2, g, OP_READ, "hallo.txt", 6, 100, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((6, 6)));
    assert_eq!(data(&c, 6), b"wereld");
    let mut c = fs_call(2, g, OP_WRITE, "sub/b", 0, 0, b"x");
    on(f.handle(&mut c)).unwrap();
    let mut c = fs_call(2, g, OP_LIST, "/", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((2, 14)));
    assert_eq!(data(&c, 14), b"hallo.txt\nsub/");
    let mut c = fs_call(2, g, OP_TRUNCATE, "hallo.txt", 0, 5, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((5, 0)));
    let mut c = fs_call(2, g, OP_READ, "hallo.txt", 0, 100, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((5, 5)));
    let mut c = fs_call(2, g, OP_REMOVE, "sub", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::NotEmpty));
    let mut c = fs_call(2, g, OP_REMOVE, "hallo.txt", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((0, 0)));
    let mut c = fs_call(2, g, OP_REMOVE, "/", 0, 0, &[]);
    assert_eq!(
        on(f.handle(&mut c)),
        Err(Error::Denied),
        "the own root stays"
    );
    let mut c = fs_call(2, g, OP_READ, "../slot1/x", 0, 1, &[]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::Denied));
    // Alles stond in de eigen root.
    assert_eq!(f.fs().stat(b"/.tasks/slot2/sub/b").unwrap(), (1, false));
}

#[test]
fn a_new_lifetime_starts_with_an_empty_root_and_keeps_its_volume() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    crate::slots::tests::start_with_mounts(&mut a, 1, 8, 1, vec![m("/hop", "/volumes/hop")])
        .unwrap();
    let g = svc.current(s(1)).unwrap();
    let (fs, _) = disk(64);
    let mut f = FsActor::new(fs, &svc, &con);
    for (p, d) in [("/hop/agent-state.json", &b"{}"[..]), ("scratch", b"weg")] {
        let mut c = fs_call(1, g, OP_WRITE, p, 0, 0, d);
        on(f.handle(&mut c)).unwrap();
    }
    assert!(con.saw("hopfs: slot 1 saved /hop/agent-state.json as /volumes/hop/agent-state.json"));
    assert_eq!(
        f.fs().stat(b"/volumes/hop/agent-state.json").unwrap(),
        (2, false)
    );
    let mut c = fs_call(1, g, OP_REMOVE, "/hop", 0, 0, &[]);
    assert_eq!(
        on(f.handle(&mut c)),
        Err(Error::Denied),
        "the volume itself stays"
    );
    // Een oude generatie krijgt niets meer.
    let mut c = fs_call(1, g.wrapping_sub(1), OP_STAT, "scratch", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::Denied));
    // Stop en opnieuw: de root is leeg, het volume niet.
    crate::slots::tests::stop(&mut a, 1).unwrap();
    crate::slots::tests::start_with_mounts(&mut a, 1, 8, 1, vec![m("/hop", "/volumes/hop")])
        .unwrap();
    let g2 = svc.current(s(1)).unwrap();
    assert_ne!(g, g2);
    let mut c = fs_call(1, g2, OP_STAT, "scratch", 0, 0, &[]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::NoEnt), "root wiped");
    let mut c = fs_call(1, g2, OP_READ, "hop/agent-state.json", 0, 64, &[]);
    assert_eq!(on(f.handle(&mut c)), Ok((2, 2)));
    assert_eq!(data(&c, 2), b"{}");
}

#[test]
fn commit_logs_once_per_generation_and_survives_a_remount() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 8, 1).unwrap();
    let g = svc.current(s(2)).unwrap();
    let (fs, _) = disk(64);
    let mut f = FsActor::new(fs, &svc, &con);
    let mut c = fs_call(2, g, OP_WRITE, "blijft", 0, 0, b"data");
    on(f.handle(&mut c)).unwrap();
    on(f.commit(CommitWhy::Stopped(s(2))));
    on(f.commit(CommitWhy::Periodic)); // Niets veranderd: geen tweede regel.
    assert!(con.saw("hopfs: tree committed as generation 1 (slot 2 stopped) HOPOS_FS_COMMIT"));
    assert!(!con.saw("(every 10 s)"));
    let disk = f.into_fs().into_disk();
    let (mut g2, m) = on(Fs::mount(disk, 0, (64 << 20) / 512, 512, 1 << 20, false)).unwrap();
    assert!(matches!(m, crate::hopfs::Mounted::Restored { .. }));
    assert_eq!(g2.stat(b"/.tasks/slot2/blijft").unwrap(), (4, false));
}

#[test]
fn list_resp_wire_limit() {
    // `TestListRespWireLimit`: precies op de grens past, één byte erover is
    // een nette fout en geen half antwoord.
    let (fs, _) = disk(64);
    let limit = 64usize;
    let mut fs = put(fs, &[b"/d/".as_slice(), &[b'x'; 64]].concat(), b"1");
    let mut dst = vec![0u8; limit];
    assert_eq!(fs.list_into(b"/d", &mut dst).unwrap(), (1, 64));
    let mut fs = put(fs, b"/d/y", b"1");
    assert!(matches!(
        fs.list_into(b"/d", &mut dst),
        Err(Error::TooLarge { .. })
    ));
    let _ = STATUS_ERROR;
}

#[test]
fn unknown_and_store_ops_are_not_file_calls() {
    for op in [OP_STAT, OP_READ, OP_WRITE, OP_LIST, OP_REMOVE, OP_TRUNCATE] {
        assert!(is_fs_op(op));
    }
    for op in [0, 6, 8, 9, 10, 11, 14, 0x40] {
        assert!(!is_fs_op(op), "op {op}");
    }
}

/// De kern-flip: de bevriezing legt eerst vast (de nieuwe kern mount die
/// generatie) en zegt welke generatie het werd; zonder verandering blijft
/// de generatie staan.
#[test]
fn freeze_commits_first_and_names_the_generation() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 8, 1).unwrap();
    let g = svc.current(s(2)).unwrap();
    let (fs, _) = disk(64);
    let mut f = FsActor::new(fs, &svc, &con);
    let mut c = fs_call(2, g, OP_WRITE, "voor-de-flip", 0, 0, b"staat");
    on(f.handle(&mut c)).unwrap();
    assert_eq!(on(f.freeze()), Ok((1, 0)));
    assert!(f.desk.frozen);
    assert!(con.saw("frozen for the kernel flip HOPOS_FS_FROZEN generation=1"));
    assert_eq!(
        on(f.freeze()),
        Ok((1, 0)),
        "nothing changed: the same generation"
    );
    let disk = f.into_fs().into_disk();
    let (mut g2, m) = on(Fs::mount(disk, 0, (64 << 20) / 512, 512, 1 << 20, false)).unwrap();
    assert!(matches!(
        m,
        crate::hopfs::Mounted::Restored { generation: 1, .. }
    ));
    assert_eq!(g2.stat(b"/.tasks/slot2/voor-de-flip").unwrap(), (5, false));
}

/// De warme kern-flip (F2 in docs/sequence-hopos.md): kern B adopteert de
/// bewoner met een eigen generatie en begint met een verse actor, maar het
/// is dezelfde app die doordraait. Zijn eerste bestandscall op kern B laat
/// zijn root staan (de volumes bestaan, zoals bij elke levensduur). Een
/// echte nieuwe levensduur daarna op kern B begint wel leeg, en het volume
/// blijft.
#[test]
fn an_adopted_lifetime_keeps_its_root_and_a_new_start_wipes_it() {
    let vol = || vec![m("/data", "/volumes/demo")];
    // Kern A: de app schrijft in zijn root en zijn volume; de flip bevriest.
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    crate::slots::tests::start_with_mounts(&mut a, 2, 8, 1, vol()).unwrap();
    let g = svc.current(s(2)).unwrap();
    assert!(!svc.root_kept(s(2), g), "a start is a new lifetime");
    let (fs, _) = disk(64);
    let mut f = FsActor::new(fs, &svc, &con);
    for (p, d) in [("scratch", &b"blijft"[..]), ("/data/db", b"ook")] {
        let mut c = fs_call(2, g, OP_WRITE, p, 0, 0, d);
        on(f.handle(&mut c)).unwrap();
    }
    on(f.freeze()).unwrap();
    let states = a.snapshot().unwrap();
    let disk = f.into_fs().into_disk();

    // Kern B: dezelfde schijf, een verse actor met een lege `prepared`.
    let (svc2, con2) = (Servicers::new(), FakeConsole::default());
    let mut b = actor(&svc2, &con2, Obey::Exit, 64, 4);
    crate::slots::tests::adopt(&mut b, &states).unwrap();
    let g2 = svc2.current(s(2)).unwrap();
    assert!(svc2.root_kept(s(2), g2));
    assert!(
        !svc2.root_kept(s(2), g2.wrapping_add(1)),
        "only the adopted generation"
    );
    let (fs2, _) = on(Fs::mount(disk, 0, (64 << 20) / 512, 512, 1 << 20, false)).unwrap();
    let mut f2 = FsActor::new(fs2, &svc2, &con2);
    let mut c = fs_call(2, g2, OP_READ, "scratch", 0, 64, &[]);
    assert_eq!(on(f2.handle(&mut c)), Ok((6, 6)), "root kept over the flip");
    assert_eq!(data(&c, 6), b"blijft");
    assert!(con2.saw("hopfs: slot 2 keeps its root over the kernel flip HOPOS_FS_ROOT_KEPT"));
    let mut c = fs_call(2, g2, OP_READ, "/data/db", 0, 64, &[]);
    assert_eq!(on(f2.handle(&mut c)), Ok((3, 3)));

    // Een echte nieuwe levensduur op kern B: de root leeg, het volume niet.
    crate::slots::tests::stop(&mut b, 2).unwrap();
    assert!(
        !svc2.root_kept(s(2), g2),
        "a stopped lifetime keeps nothing"
    );
    crate::slots::tests::start_with_mounts(&mut b, 2, 8, 1, vol()).unwrap();
    let g3 = svc2.current(s(2)).unwrap();
    assert_ne!(g3, g2);
    assert!(!svc2.root_kept(s(2), g3));
    let mut c = fs_call(2, g3, OP_STAT, "scratch", 0, 0, &[]);
    assert_eq!(on(f2.handle(&mut c)), Err(Error::NoEnt), "root wiped");
    let mut c = fs_call(2, g3, OP_READ, "data/db", 0, 64, &[]);
    assert_eq!(on(f2.handle(&mut c)), Ok((3, 3)));
    assert_eq!(data(&c, 3), b"ook");
}

// ---------------------------------------------------------------------------
// De kern als lezer: de firmware van de codec (docs/media.md, haak 3).
// ---------------------------------------------------------------------------

/// Pollt `f` tot hij klaar is en laat de actor ertussen draaien: de
/// executor van de kern in het klein, met een waker die niets doet (elke
/// bel wordt bij de volgende poll gezien).
fn drive<F: core::future::Future>(
    actor: &mut FsActor<'_, Disk, &FakeConsole>,
    inbox: &FsInbox<'_>,
    f: F,
) -> F::Output {
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());
    let mut f = pin!(f);
    let mut run = pin!(actor.run(inbox));
    for _ in 0..10_000 {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        let _ = run.as_mut().poll(&mut cx);
    }
    panic!("de lezing kwam nooit terug");
}

#[test]
fn the_kern_reads_a_firmware_blob_without_a_slot() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let (fs, _) = disk(64);
    // Een blob van 300 KB, zoals de echte: over meer blokken dan één
    // leesstap en met een staart die geen heel blok is.
    let blob: Vec<u8> = (0..300 * 1024 + 17).map(|i| (i * 7 % 251) as u8).collect();
    let fs = put(fs, b"/firmware/hevcdec.fwb", &blob);
    let fs = put(fs, b"/.tasks/slot2/geheim", b"van de app");
    let mut f = FsActor::new(fs, &svc, &con);

    // Het handvat van de actor: maat, data, en de weigeringen.
    let mut head = [0u8; 4];
    assert_eq!(
        on(f.kern_read(b"/firmware/hevcdec.fwb", 0, &mut [])),
        Ok((blob.len() as u64, 0)),
        "leeg is een stat"
    );
    assert_eq!(
        on(f.kern_read(b"firmware//./hevcdec.fwb", 1, &mut head)),
        Ok((blob.len() as u64, 4))
    );
    assert_eq!(head, blob[1..5]);
    assert_eq!(
        on(f.kern_read(b"/firmware/av1dec.fwb", 0, &mut head)),
        Err(Error::NoEnt)
    );
    assert_eq!(
        on(f.kern_read(b"/firmware", 0, &mut head)),
        Err(Error::Kind)
    );
    assert_eq!(
        on(f.kern_read(b"/.tasks/slot2/geheim", 0, &mut head)),
        Err(Error::Denied)
    );
    assert_eq!(
        on(f.kern_read(b"/firmware/../.tasks", 0, &mut head)),
        Err(Error::Denied)
    );

    // De hele weg: brievenbus, actor, antwoordplek, één blob in RAM.
    let reply = Reply::new();
    let inbox: FsInbox<'_> = Mailbox::new();
    let got = drive(
        &mut f,
        &inbox,
        read_file(&inbox, &reply, b"/firmware/hevcdec.fwb", 4 << 20),
    );
    assert_eq!(got.unwrap(), blob);
    let got = drive(
        &mut f,
        &inbox,
        read_file(&inbox, &reply, b"/firmware/hevcdec.fwb", 1024),
    );
    assert_eq!(
        got,
        Err(Error::TooLarge {
            len: blob.len(),
            max: 1024
        }),
        "te groot: niets gealloceerd"
    );
    let got = drive(
        &mut f,
        &inbox,
        read_file(&inbox, &reply, b"/firmware/vp9dec.fwb", 4 << 20),
    );
    assert_eq!(got, Err(Error::NoEnt), "de naam die mist, is een NoEnt");

    // In stukken, met dezelfde buffers heen en terug (de teststream).
    let mut off = 0u64;
    let mut out = vec![0u8; 64 << 10];
    let mut path = b"/firmware/hevcdec.fwb".to_vec();
    let mut seen = Vec::new();
    loop {
        let r = KernRead { path, off, out };
        let d = drive(&mut f, &inbox, kern_read(&inbox, &reply, r));
        let (size, n) = d.result.unwrap();
        assert_eq!(size, blob.len() as u64);
        seen.extend_from_slice(&d.out[..n]);
        (path, out) = (d.buf, d.out);
        off += n as u64;
        if n == 0 {
            break;
        }
    }
    assert_eq!(seen, blob);

    // Bevroren voor de flip weigert ook de kern, en de buffers komen terug.
    on(f.freeze()).unwrap();
    let r = KernRead { path, off: 0, out };
    let d = drive(&mut f, &inbox, kern_read(&inbox, &reply, r));
    assert_eq!(d.result, Err(Error::Busy));
    assert_eq!(
        (d.buf.as_slice(), d.out.len()),
        (&b"/firmware/hevcdec.fwb"[..], 64 << 10)
    );
}

#[test]
fn sync_is_scoped_to_the_live_slot_and_reports_the_durable_generation() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 8, 1).unwrap();
    let generation = svc.current(s(2)).unwrap();
    let (fs, _) = disk(2);
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(
        2,
        generation,
        OP_WRITE,
        "db-journal",
        0,
        0,
        b"original",
    )))
    .unwrap();
    assert_eq!(
        on(f.handle(&mut fs_call(
            2,
            generation,
            OP_SYNC,
            "db-journal",
            0,
            0,
            &[]
        ))),
        Ok((1, 0))
    );
    assert_eq!(
        on(f.handle(&mut fs_call(
            2,
            generation + 1,
            OP_SYNC,
            "db-journal",
            0,
            0,
            &[]
        ))),
        Err(Error::Denied)
    );
    assert_eq!(
        on(f.handle(&mut fs_call(2, generation, OP_SYNC, "../other", 0, 0, &[]))),
        Err(Error::Denied)
    );
    assert_eq!(
        on(f.handle(&mut fs_call(2, generation, OP_SYNC, "missing", 0, 0, &[]))),
        Err(Error::NoEnt)
    );
    for (off, n, bytes) in [(1, 0, &[][..]), (0, 1, &[][..]), (0, 0, &[1][..])] {
        assert_eq!(
            on(f.handle(&mut fs_call(
                2,
                generation,
                OP_SYNC,
                "db-journal",
                off,
                n,
                bytes
            ))),
            Err(Error::Kind)
        );
    }
    on(f.handle(&mut fs_call(
        2,
        generation,
        OP_REMOVE,
        "db-journal",
        0,
        0,
        &[],
    )))
    .unwrap();
    assert_eq!(
        on(f.handle(&mut fs_call(2, generation, OP_SYNC, "/", 0, 0, &[]))),
        Ok((2, 0))
    );
    let disk = f.into_fs().into_disk();
    assert!(disk.with_dev(|r| r.flushes()) >= 4);
    let (mut restored, _) = on(Fs::mount(disk, 0, (2 << 20) / 512, 512, 1 << 20, false)).unwrap();
    assert_eq!(
        restored.stat(b"/.tasks/slot2/db-journal"),
        Err(Error::NoEnt)
    );
}

/// Een actor met de brievenbus erbij, gepolld zoals de executor het doet.
struct Bench<'s> {
    svc: &'s Servicers,
    con: &'s FakeConsole,
}

/// Stuurt een call met zijn antwoordplek.
fn send<'a>(inbox: &FsInbox<'a>, c: FsCall, reply: &'a Reply) {
    assert!(
        inbox
            .try_send(FsEnvelope {
                msg: FsMsg::Call(c),
                reply: Some(reply),
            })
            .is_ok()
    );
}

/// Pollt de actor een paar rondes.
fn spin<F: core::future::Future>(run: &mut core::pin::Pin<&mut F>) {
    let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
    for _ in 0..8 {
        let _ = run.as_mut().poll(&mut cx);
    }
}

/// De uitkomst op een antwoordplek, als die er is.
fn got(r: &Reply) -> Option<Result<(u64, usize)>> {
    r.fs.try_take().map(|d| d.result)
}

impl<'s> Bench<'s> {
    /// Slots 2 en 3 levend met een gedeeld volume `/v`; een schijf van 16 MiB.
    fn up(&self) -> (u32, u32, Fs<Disk>, Ram) {
        let mut a = actor(self.svc, self.con, Obey::Exit, 64, 4);
        for slot in [2, 3] {
            crate::slots::tests::start_with_mounts(&mut a, slot, 8, 1, vec![m("/v", "/volumes/v")])
                .unwrap();
        }
        let (g2, g3) = (
            self.svc.current(s(2)).unwrap(),
            self.svc.current(s(3)).unwrap(),
        );
        let (fs, _) = disk(16);
        let r = ram(&fs);
        // De slot-actor uit de test laat hij hier achter; de levensduren staan.
        core::mem::forget(a);
        (g2, g3, fs, r)
    }
}

/// docs/storage-sync.md: de barrière staat achter de eerdere writes van
/// dezelfde app. Een OP_SYNC die in de brievenbus achter een schrijf staat,
/// wacht tot die schrijf van het device terug is; pas dan flusht en
/// bevestigt hij, en de bevestigde generatie draagt de schrijf.
#[test]
fn sync_waits_behind_an_earlier_write_that_is_still_on_the_device() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g, _, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    // De root van de levensduur staat al: de vastgehouden schrijf is dan
    // die van de app, niet die van het klaarzetten.
    on(f.handle(&mut fs_call(2, g, OP_STAT, "/", 0, 0, &[]))).unwrap();
    let (wrote, synced) = (Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    send(
        &inbox,
        fs_call(2, g, OP_WRITE, "db", 0, 0, b"journal"),
        &wrote,
    );
    send(&inbox, fs_call(2, g, OP_SYNC, "db", 0, 0, &[]), &synced);
    r.hold(Kind::Write);
    let mut run = core::pin::pin!(f.run(&inbox));
    spin(&mut run);
    assert!(got(&wrote).is_none(), "de schrijf staat nog op het device");
    assert!(got(&synced).is_none(), "barrière vóór de schrijf");
    assert_eq!(r.flushes(), 0, "geflusht terwijl de schrijf nog liep");
    r.release(Kind::Write);
    spin(&mut run);
    assert_eq!(got(&wrote), Some(Ok((7, 0))));
    assert_eq!(got(&synced), Some(Ok((1, 0))));
    // De volgorde op het device: de schrijf terug, pas dan de eerste flush.
    let log = r.log();
    let done = log
        .iter()
        .position(|e| *e == ("done", Kind::Write, e.2))
        .unwrap();
    let flush = log.iter().position(|e| e.1 == Kind::Flush).unwrap();
    assert!(done < flush, "{log:?}");
    assert!(r.flushes() >= 2, "dataflush en metadataflush");
}

/// De wachtrij is voor de node: twee apps lezen tegelijk, allebei op het
/// device, en geen van beiden krijgt antwoord vóór zijn eigen completion.
/// Een derde call van dezelfde app als de eerste wacht op die eerste
/// (volgorde per app), ook als het een lees is.
#[test]
fn two_apps_have_their_reads_on_the_device_at_once_and_one_app_keeps_its_order() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g2, g3, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(2, g2, OP_WRITE, "a", 0, 0, &[1u8; 8192]))).unwrap();
    on(f.handle(&mut fs_call(3, g3, OP_WRITE, "b", 0, 0, &[2u8; 8192]))).unwrap();
    let (ra, rb, wa, ra2) = (Reply::new(), Reply::new(), Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    send(&inbox, fs_call(2, g2, OP_READ, "a", 0, 4096, &[]), &ra);
    send(&inbox, fs_call(3, g3, OP_READ, "b", 4096, 4096, &[]), &rb);
    // Dezelfde app (een tweede verbinding): eerst een schrijf, dan een lees.
    send(
        &inbox,
        fs_call(2, g2, OP_WRITE, "a", 0, 0, &[7u8; 4096]),
        &wa,
    );
    send(&inbox, fs_call(2, g2, OP_READ, "a", 0, 4096, &[]), &ra2);
    r.hold(Kind::Read);
    r.hold(Kind::Write);
    let mut run = core::pin::pin!(f.run(&inbox));
    spin(&mut run);
    assert_eq!(r.on_device(), 2, "de lezingen van slot 2 en 3 tegelijk");
    assert!(got(&ra).is_none() && got(&rb).is_none());
    r.release(Kind::Read);
    spin(&mut run);
    assert!(matches!(got(&ra), Some(Ok((4096, 4096)))));
    assert!(matches!(got(&rb), Some(Ok((4096, 4096)))));
    // De schrijf van slot 2 staat nu op het device; zijn lees erna niet.
    assert_eq!(r.on_device(), 1);
    assert!(got(&wa).is_none() && got(&ra2).is_none());
    r.release(Kind::Write);
    spin(&mut run);
    assert_eq!(got(&wa), Some(Ok((4096, 0))));
    let d = ra2.fs.try_take().unwrap();
    assert_eq!(d.result, Ok((4096, 4096)));
    assert!(
        d.out[HDR_LEN..HDR_LEN + 4096].iter().all(|&x| x == 7),
        "de lees zag de schrijf"
    );
    assert!(r.0.borrow().peak >= 2);
}

/// Remove geeft blokken vrij: hij wacht tot er niets meer loopt, ook niet
/// van een andere app, en de lees die liep krijgt zijn bytes heel.
#[test]
fn a_remove_waits_until_no_io_is_running() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g2, g3, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(
        3,
        g3,
        OP_WRITE,
        "/v/gedeeld",
        0,
        0,
        &[9u8; 4096],
    )))
    .unwrap();
    on(f.handle(&mut fs_call(2, g2, OP_STAT, "/", 0, 0, &[]))).unwrap();
    let (rd, rm) = (Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    send(
        &inbox,
        fs_call(3, g3, OP_READ, "/v/gedeeld", 0, 4096, &[]),
        &rd,
    );
    send(
        &inbox,
        fs_call(2, g2, OP_REMOVE, "/v/gedeeld", 0, 0, &[]),
        &rm,
    );
    r.hold(Kind::Read);
    let mut run = core::pin::pin!(f.run(&inbox));
    spin(&mut run);
    assert!(got(&rm).is_none(), "remove terwijl een lees liep");
    r.release(Kind::Read);
    spin(&mut run);
    let d = rd.fs.try_take().unwrap();
    assert_eq!(d.result, Ok((4096, 4096)));
    assert!(d.out[HDR_LEN..HDR_LEN + 4096].iter().all(|&x| x == 9));
    assert_eq!(got(&rm), Some(Ok((0, 0))));
}

/// Twee apps die in hetzelfde bestand schrijven (een gedeeld volume): de
/// tweede wacht op de eerste, en het bestand heeft geen dubbele mapping.
#[test]
fn two_writers_of_one_file_take_turns() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g2, g3, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    for (slot, g) in [(2, g2), (3, g3)] {
        on(f.handle(&mut fs_call(slot, g, OP_STAT, "/", 0, 0, &[]))).unwrap();
    }
    let (w2, w3) = (Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    send(
        &inbox,
        fs_call(2, g2, OP_WRITE, "/v/log", 0, 0, &[2u8; 4096]),
        &w2,
    );
    send(
        &inbox,
        fs_call(3, g3, OP_WRITE, "/v/log", 0, 0, &[3u8; 4096]),
        &w3,
    );
    r.hold(Kind::Write);
    {
        let mut run = core::pin::pin!(f.run(&inbox));
        spin(&mut run);
        assert_eq!(r.on_device(), 1, "één schrijver per bestand");
        r.release(Kind::Write);
        spin(&mut run);
    }
    assert_eq!(
        (got(&w2), got(&w3)),
        (Some(Ok((4096, 0))), Some(Ok((4096, 0))))
    );
    let mut out = [0u8; 4096];
    assert_eq!(
        on(crate::hopfs::path::read(
            f.tree(),
            b"/volumes/v/log",
            0,
            &mut out
        )),
        Ok(4096)
    );
    assert!(
        out.iter().all(|&x| x == 3),
        "de laatste schrijver wint, één blok"
    );
}

/// Een vastlegging loopt terwijl een andere app leest en schrijft: wat
/// tijdens de vastlegging veranderde, gaat met de volgende mee.
#[test]
fn a_commit_runs_beside_other_calls_and_misses_nothing() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g2, g3, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(2, g2, OP_WRITE, "/v/een", 0, 0, b"1"))).unwrap();
    on(f.handle(&mut fs_call(3, g3, OP_STAT, "/", 0, 0, &[]))).unwrap();
    let (sy, w3) = (Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    send(&inbox, fs_call(2, g2, OP_SYNC, "/v/een", 0, 0, &[]), &sy);
    send(
        &inbox,
        fs_call(3, g3, OP_WRITE, "/v/twee", 0, 0, b"22"),
        &w3,
    );
    r.hold(Kind::Flush);
    {
        let mut run = core::pin::pin!(f.run(&inbox));
        spin(&mut run);
        assert_eq!(
            got(&w3),
            Some(Ok((2, 0))),
            "de schrijf van slot 3 wacht niet op de flush"
        );
        assert!(got(&sy).is_none());
        r.release(Kind::Flush);
        spin(&mut run);
    }
    assert_eq!(got(&sy), Some(Ok((1, 0))));
    // Generatie 1 heeft /v/een; /v/twee kwam erna en gaat met de volgende.
    on(f.commit(CommitWhy::Periodic));
    let disk = f.into_fs().into_disk();
    let (mut g, _) = on(Fs::mount(disk, 0, (16 << 20) / 512, 512, 1 << 20, false)).unwrap();
    assert_eq!(g.generation(), 2);
    assert_eq!(g.stat(b"/volumes/v/twee").unwrap(), (2, false));
}

/// Eén vastlegging tegelijk: een OP_SYNC die binnenkomt terwijl de
/// periodieke commit nog op de flush wacht, begint pas daarna (twee
/// tegelijk zouden dezelfde plek beschrijven), en legt dan zijn eigen
/// schrijf vast.
#[test]
fn a_sync_waits_for_a_commit_in_flight() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g2, _, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(2, g2, OP_WRITE, "eerst", 0, 0, b"1"))).unwrap();
    let (w, sy) = (Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    assert!(
        inbox
            .try_send(FsEnvelope {
                msg: FsMsg::Commit(CommitWhy::Periodic),
                reply: None,
            })
            .is_ok()
    );
    send(&inbox, fs_call(2, g2, OP_WRITE, "daarna", 0, 0, b"22"), &w);
    send(&inbox, fs_call(2, g2, OP_SYNC, "daarna", 0, 0, &[]), &sy);
    r.hold(Kind::Flush);
    {
        let mut run = core::pin::pin!(f.run(&inbox));
        spin(&mut run);
        assert_eq!(
            got(&w),
            Some(Ok((2, 0))),
            "de schrijf loopt naast de commit"
        );
        let flushes = r.log().iter().filter(|e| e.1 == Kind::Flush).count();
        assert_eq!(flushes, 1, "één vastlegging tegelijk");
        assert!(got(&sy).is_none());
        r.release(Kind::Flush);
        spin(&mut run);
    }
    assert_eq!(got(&sy), Some(Ok((2, 0))), "de sync legt generatie 2 vast");
    let disk = f.into_fs().into_disk();
    let (mut g, _) = on(Fs::mount(disk, 0, (16 << 20) / 512, 512, 1 << 20, false)).unwrap();
    assert_eq!(g.stat(b"/.tasks/slot2/daarna").unwrap(), (2, false));
}

// ---------------------------------------------------------------------------
// Gebundeld lezen (OP_READ_MANY).
// ---------------------------------------------------------------------------

/// Een lijst opdrachten op de draad.
fn list(ops: &[(u64, u32)]) -> Vec<u8> {
    let mut l = vec![0u8; ops.len() * many::OP_LEN];
    for (i, &(off, len)) in ops.iter().enumerate() {
        many::put_op(&mut l, i, off, len).unwrap();
    }
    l
}

/// Een bundel zoals applib hem stuurt: `n` is het aantal opdrachten.
fn bundle(slot: usize, generation: u32, path: &str, ops: &[(u64, u32)]) -> FsCall {
    fs_call(
        slot,
        generation,
        OP_READ_MANY,
        path,
        0,
        ops.len() as u64,
        &list(ops),
    )
}

/// De uitkomsten uit het antwoord: per opdracht (bytes, status) en de
/// bytes zelf, opgeknipt.
fn outcomes(c: &FsCall, count: usize) -> Vec<(u32, u16, Vec<u8>)> {
    let body = &c.out[HDR_LEN..];
    let mut at = count * many::RESULT_LEN;
    (0..count)
        .map(|i| {
            let (k, st) = many::result(body, i).unwrap();
            let bytes = body[at..at + k as usize].to_vec();
            at += k as usize;
            (k, st, bytes)
        })
        .collect()
}

/// Een bestand van 64 KiB waarin elke byte zijn eigen plek verraadt.
fn pattern() -> Vec<u8> {
    (0..64 << 10)
        .map(|i: usize| (i / 7) as u8 ^ (i >> 12) as u8)
        .collect()
}

/// Eén call, vier lezingen: heel, half in een blok (de rand), over het
/// einde heen, en voorbij het einde. Het antwoord is de tabel en daarna de
/// bytes aaneen; voorbij het einde is geen fout maar nul bytes.
#[test]
fn a_bundle_reads_whole_blocks_edges_and_past_the_end_in_one_answer() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 8, 1).unwrap();
    let g = svc.current(s(2)).unwrap();
    let (fs, _) = disk(64);
    let mut f = FsActor::new(fs, &svc, &con);
    let file = pattern();
    on(f.handle(&mut fs_call(2, g, OP_WRITE, "f", 0, 0, &file))).unwrap();
    let ops = [
        (4096, 4096),
        (8192 + 10, 100),
        (60000, 10000),
        (1 << 40, 4096),
    ];
    let mut c = bundle(2, g, "f", &ops);
    let want = 4096 + 100 + (65536 - 60000);
    assert_eq!(
        on(f.handle(&mut c)),
        Ok((want as u64, 4 * many::RESULT_LEN + want))
    );
    let o = outcomes(&c, 4);
    assert_eq!(o[0], (4096, STATUS_OK, file[4096..8192].to_vec()));
    assert_eq!(o[1], (100, STATUS_OK, file[8202..8302].to_vec()));
    assert_eq!(o[2], (5536, STATUS_OK, file[60000..].to_vec()));
    assert_eq!(o[3], (0, STATUS_OK, Vec::new()));
}

/// De lijst als geheel: leeg, te lang, te groot, niet `n` lang, of een
/// antwoord dat niet in de buffer past, is een fout van de call vóór er
/// iets naar het device gaat. Een map of een pad dat er niet is, ook.
#[test]
fn a_bundle_that_is_empty_too_long_or_too_large_is_refused_before_the_device() {
    let svc = Servicers::new();
    let con = FakeConsole::default();
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 8, 1).unwrap();
    let g = svc.current(s(2)).unwrap();
    let (fs, _) = disk(64);
    let r = ram(&fs);
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(2, g, OP_WRITE, "d/f", 0, 0, &pattern()))).unwrap();
    let reads = |r: &Ram| r.log().iter().filter(|e| e.1 == Kind::Read).count();
    let before = reads(&r);
    let mut c = bundle(2, g, "d/f", &[]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::Corrupt { at: 0 }), "leeg");
    let mut c = bundle(2, g, "d/f", &[(0, 4096); many::MAX_OPS + 1]);
    assert_eq!(
        on(f.handle(&mut c)),
        Err(Error::TooLarge {
            len: many::MAX_OPS + 1,
            max: many::MAX_OPS
        }),
        "te lang"
    );
    let mut c = bundle(2, g, "d/f", &[(0, 300 << 10), (0, 300 << 10)]);
    assert_eq!(
        on(f.handle(&mut c)),
        Err(Error::TooLarge {
            len: 600 << 10,
            max: many::MAX_BYTES
        }),
        "te groot"
    );
    let mut c = bundle(2, g, "d/f", &[(0, 4096), (4096, 4096)]);
    c.n = 1;
    assert_eq!(on(f.handle(&mut c)), Err(Error::Corrupt { at: 0 }), "n");
    // 16 keer 4 KiB plus een blok per opdracht past niet in 64 KiB.
    let mut c = bundle(2, g, "d/f", &[(0, 4096); many::MAX_OPS]);
    assert!(matches!(on(f.handle(&mut c)), Err(Error::TooLarge { .. })));
    let mut c = bundle(2, g, "d", &[(0, 4096)]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::Kind), "een map");
    let mut c = bundle(2, g, "weg", &[(0, 4096)]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::NoEnt));
    let mut c = bundle(2, g, "../slot1/x", &[(0, 4096)]);
    assert_eq!(on(f.handle(&mut c)), Err(Error::Denied));
    assert_eq!(reads(&r), before, "niets naar het device");
}

/// Een kapot blok onder een opdracht van een bundel is het ijzer dat faalt:
/// `fatal` (§13.6), geen fout in de tabel van de app.
#[test]
#[should_panic(expected = "HOPOS_FATAL hopfs: block I/O failed at LBA")]
fn a_failing_read_in_a_bundle_is_fatal() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g, _, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    let file = pattern();
    on(f.handle(&mut fs_call(2, g, OP_WRITE, "f", 0, 0, &file))).unwrap();
    // Het blok onder de derde opdracht (offset 32 KiB) gaat kapot.
    let lba = {
        let fs = f.fs();
        let n = fs.find(b"/.tasks/slot2/f").unwrap();
        match fs.read_step(n, 32 << 10, 4096).unwrap() {
            crate::hopfs::ReadStep::Whole { lba, .. } => lba,
            other => panic!("{other:?}"),
        }
    };
    r.0.borrow_mut().bad.push(lba);
    let ops = [
        (0, 4096),
        (16 << 10, 4096),
        (32 << 10, 4096),
        (48 << 10, 4096),
    ];
    let done = Reply::new();
    let inbox: FsInbox<'_> = Mailbox::new();
    send(&inbox, bundle(2, g, "f", &ops), &done);
    let mut run = core::pin::pin!(f.run(&inbox));
    spin(&mut run);
}

/// Twee bundels van één app (twee verbindingen) staan samen op het
/// device: een lees verandert niets. Een schrijf van die app wacht op
/// beide, en een lees erna op de schrijf, en ziet hem.
#[test]
fn two_bundles_of_one_app_share_the_device_and_a_write_waits_for_both() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let (g, _, fs, r) = Bench {
        svc: &svc,
        con: &con,
    }
    .up();
    let mut f = FsActor::new(fs, &svc, &con);
    on(f.handle(&mut fs_call(2, g, OP_WRITE, "f", 0, 0, &pattern()))).unwrap();
    let (b1, b2, w, b3) = (Reply::new(), Reply::new(), Reply::new(), Reply::new());
    let inbox: FsInbox<'_> = Mailbox::new();
    send(&inbox, bundle(2, g, "f", &[(0, 4096), (4096, 4096)]), &b1);
    send(
        &inbox,
        bundle(2, g, "f", &[(8192, 4096), (12288, 4096)]),
        &b2,
    );
    send(&inbox, fs_call(2, g, OP_WRITE, "f", 0, 0, &[9u8; 4096]), &w);
    send(&inbox, bundle(2, g, "f", &[(0, 4096)]), &b3);
    r.hold(Kind::Read);
    r.hold(Kind::Write);
    let mut run = core::pin::pin!(f.run(&inbox));
    spin(&mut run);
    assert_eq!(r.on_device(), 4, "beide bundels tegelijk");
    r.release(Kind::Read);
    spin(&mut run);
    assert!(matches!(got(&b1), Some(Ok((8192, _)))));
    assert!(matches!(got(&b2), Some(Ok((8192, _)))));
    assert!(got(&w).is_none(), "de schrijf staat op het device");
    assert!(got(&b3).is_none(), "de lees wacht op de schrijf");
    r.release(Kind::Write);
    spin(&mut run);
    assert_eq!(got(&w), Some(Ok((4096, 0))));
    let d = b3.fs.try_take().unwrap();
    assert_eq!(d.result, Ok((4096, many::RESULT_LEN + 4096)));
    let at = HDR_LEN + many::RESULT_LEN;
    assert!(
        d.out[at..at + 4096].iter().all(|&x| x == 9),
        "zag de schrijf"
    );
}

/// Een klok die nooit afloopt: wat de committer dan nog doet, deed hij op
/// een gebeurtenis.
struct Frozen;

impl Timer for Frozen {
    fn now(&self) -> u64 {
        0
    }
    fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
        core::future::pending()
    }
}

// De committer hoort een stop van de servicer-tabel, niet van een ronde
// langs de tabel: met een klok die stilstaat legt hij de boom toch vast,
// meteen na de stop, en verder niets.
#[test]
fn the_committer_commits_on_the_bell_of_a_stop() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    let inbox: FsInbox<'_> = Mailbox::new();
    let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
    let mut c = core::pin::pin!(committer(&svc, &inbox, &Frozen, 4));
    start(&mut a, 2, 8, 1).unwrap();
    assert!(c.as_mut().poll(&mut cx).is_pending());
    assert!(inbox.try_recv().is_none(), "a start is no reason to commit");
    stop(&mut a, 2).unwrap();
    assert!(c.as_mut().poll(&mut cx).is_pending());
    let Some(FsEnvelope {
        msg: FsMsg::Commit(why),
        ..
    }) = inbox.try_recv()
    else {
        panic!("no commit after the stop");
    };
    assert_eq!(why, CommitWhy::Stopped(s(2)));
    assert!(c.as_mut().poll(&mut cx).is_pending());
    assert!(inbox.try_recv().is_none(), "one stop, one commit");
}
