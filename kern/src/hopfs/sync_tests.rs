//! Stroomuitvalmodel: writes raken alleen de devicecache; uitsluitend flush maakt bytes duurzaam.
use super::*;
use std::{cell::RefCell, rc::Rc, vec};
#[derive(Clone)]
struct Disk(Rc<RefCell<State>>);
struct State {
    cache: Vec<u8>,
    stable: Vec<u8>,
    ops: usize,
    fail: Option<usize>,
    flushes: usize,
}
impl Disk {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(State {
            cache: vec![0; 2 << 20],
            stable: vec![0; 2 << 20],
            ops: 0,
            fail: None,
            flushes: 0,
        })))
    }
    fn crash(&self) {
        let mut s = self.0.borrow_mut();
        let State { cache, stable, .. } = &mut *s;
        cache.copy_from_slice(stable);
    }
    fn fail_after(&self, n: usize) {
        let mut s = self.0.borrow_mut();
        s.fail = Some(s.ops + n);
    }
}
/// Een future van hopfs afdraaien: de schijf is meteen klaar.
fn on<F: core::future::Future>(f: F) -> F::Output {
    blkdev::block_on(f)
}
impl BlockIo for &Disk {
    async fn read(&mut self, lba: u64, buf: &mut [u8]) -> blkdev::Result {
        let start = lba as usize * 512;
        buf.copy_from_slice(&self.0.borrow().cache[start..start + buf.len()]);
        Ok(())
    }
    async fn write(&mut self, lba: u64, bytes: &[u8]) -> blkdev::Result {
        let mut s = self.0.borrow_mut();
        s.ops += 1;
        if s.fail == Some(s.ops) {
            return Err(blkdev::Error::Io { lba });
        }
        let start = lba as usize * 512;
        s.cache[start..start + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    async fn flush(&mut self) -> blkdev::Result {
        let mut s = self.0.borrow_mut();
        s.ops += 1;
        s.flushes += 1;
        if s.fail == Some(s.ops) {
            return Err(blkdev::Error::Io { lba: 0 });
        }
        let State { cache, stable, .. } = &mut *s;
        stable.copy_from_slice(cache);
        Ok(())
    }
}
fn mount(d: &Disk) -> Tree<&Disk> {
    LocalCell::cell(
        on(Fs::mount(d, 0, (2 << 20) / 512, 512, 1 << 20, false))
            .unwrap()
            .0,
    )
}
fn read(fs: &Tree<&Disk>, p: &[u8]) -> Vec<u8> {
    let mut b = vec![0; fs.borrow_mut().stat(p).unwrap().0 as usize];
    let n = on(path::read(fs, p, 0, &mut b)).unwrap();
    b.truncate(n);
    b
}
fn write(fs: &Tree<&Disk>, p: &[u8], data: &[u8]) {
    on(path::write(fs, p, 0, data)).unwrap();
}
#[test]
fn barrier_survives_power_loss_in_place_overwrite_truncate_and_journal_remove() {
    let disk = Disk::new();
    let fs = mount(&disk);
    write(&fs, b"db", b"first");
    write(&fs, b"db-journal", b"rollback");
    assert_eq!(on(sync(&fs)).unwrap(), 1);
    drop(fs);
    disk.crash();
    let fs = mount(&disk);
    assert_eq!(read(&fs, b"db"), b"first");
    assert_eq!(read(&fs, b"db-journal"), b"rollback");
    write(&fs, b"db", b"other");
    assert_eq!(on(sync(&fs)).unwrap(), 2);
    drop(fs);
    disk.crash();
    let fs = mount(&disk);
    assert_eq!(read(&fs, b"db"), b"other");
    on(path::truncate(&fs, b"db", 3)).unwrap();
    fs.borrow_mut().remove(b"db-journal", false).unwrap();
    on(sync(&fs)).unwrap();
    drop(fs);
    disk.crash();
    let fs = mount(&disk);
    assert_eq!(read(&fs, b"db"), b"oth");
    assert_eq!(fs.borrow_mut().stat(b"db-journal"), Err(Error::NoEnt));
    let before = disk.0.borrow().flushes;
    on(sync(&fs)).unwrap();
    assert_eq!(
        disk.0.borrow().flushes,
        before + 1,
        "ongewijzigde boom vraagt ook een echte flush"
    );
}
/// Een commit die faalt, geeft zijn fout en laat de schijf bij de vorige
/// generatie; de aanroeper valt dan (`fatal`, §13.6), er is geen tweede
/// poging op dezelfde boom.
#[test]
fn every_commit_failure_is_returned_without_advancing_generation() {
    // commit: dataflush, metadatabody, metadatakop, metadataflush.
    for fault in 1..=4 {
        let disk = Disk::new();
        let fs = mount(&disk);
        write(&fs, b"before", b"old");
        on(sync(&fs)).unwrap();
        write(&fs, b"after", b"new");
        disk.fail_after(fault);
        assert!(
            matches!(on(sync(&fs)), Err(Error::Io { .. })),
            "fase {fault}"
        );
        assert_eq!(fs.borrow().generation(), 1);
        // De nieuwe naam mag vóór een geslaagde metadataflush niet zichtbaar zijn.
        let saved = disk.0.borrow().stable.clone();
        let reboot = Disk::new();
        {
            let mut d = reboot.0.borrow_mut();
            d.cache.copy_from_slice(&saved);
            d.stable.copy_from_slice(&saved);
        }
        let old = mount(&reboot);
        assert_eq!(read(&old, b"before"), b"old");
        assert_eq!(old.borrow_mut().stat(b"after"), Err(Error::NoEnt));
    }
}
