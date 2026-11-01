//! De opslagtest: schrijven en lezen door het system-callpad (app-stack,
//! slot-LAN, de stack van de kern, de servicer, hopfs, de schijf).
//!
//! Een bestand in chunks van 1 MiB per call (`?kb=` kiest kleiner), terug
//! gelezen met een inhoudscontrole, dan 256 writes van 4 KiB (het patroon
//! van een database) en 200 kale `stat`-calls: de vloer van het pad zónder
//! schijfblok. Alles boven die vloer is hopfs en schijf; ligt de vloer zelf
//! hoog, dan zit de rem in het LAN-pad.
//!
//! Met `rand=N` meet hij in plaats daarvan willekeurige 4 KiB-lezingen
//! (het patroon van een database die niet in het geheugen past): N
//! lezingen op pseudo-willekeurige plekken in een bestand van `mb` MiB,
//! elk met een inhoudscontrole, als opdrachten per seconde met p50 en p99.
//! Het bestand blijft staan, zodat apps die tegelijk meten alleen lezen
//! (de eerste run schrijft het). Met `secs` loopt de lus zo lang in plaats
//! van N lezingen (N is dan het plafond), zodat apps die samen starten ook
//! samen stoppen. Met `hole=1` erbij komt het bestand er als gat (één byte
//! op het eind): elke lezing komt dan uit het RAM van de kern, zonder
//! schijfblok, en meet alleen het pad app naar OS-core en terug. Met
//! `depth=D` gaan er D lezingen per call (een bundel, `read_many`, de kern
//! zet ze samen op het device), met `bundles=2` twee bundels tegelijk over
//! twee verbindingen; p50 en p99 zijn dan die van een call.
//!
//! Een node zonder opslag geeft "niet gevonden" op elke bestandscall, ook
//! op een write; een leeg bestand maken is dus de toets of er opslag is, en
//! zonder slaat de test zichzelf over. Elke andere fout is een fout.
//!
//! Dit module bezit de system-client van één run; hij gaat met de run weg.

use crate::Shared;
use crate::report::{Report, pct, secs};
use crate::run::{Params, note};
use alloc::vec::Vec;
use applib::appnet::SystemClient;
use applib::clock;
use applib::sys;

/// Wat het rapport zegt op een node zonder opslag.
const NO_STORAGE: &str = "no storage on this node";

/// Het lege bestand dat bewijst dat er opslag is; het gaat meteen weer weg.
const PROBE_PATH: &str = "/vitals-probe";

/// Het bestand zonder `?path=`: in de eigen root.
const DEFAULT_PATH: &str = "/vitals-disk.bin";

/// De 4 KiB-writes: 256 stuks, 1 MiB.
const SMALL: usize = 4 << 10;

/// Wat een gat van 4 KiB teruggeeft.
static ZEROS: [u8; SMALL] = [0; SMALL];
const SMALL_N: usize = 256;

/// De stat-calls van de vloer.
const STAT_N: usize = 200;

/// Wat het maken van [`PROBE_PATH`] over de opslag zegt.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Storage {
    /// Er is opslag.
    Present,
    /// Niet gevonden: de node heeft geen opslag.
    Absent,
}

/// De uitkomst van het maken van [`PROBE_PATH`] als [`Storage`], of de fout.
pub(crate) fn storage_of(res: sys::Result) -> sys::Result<Storage> {
    match res {
        Ok(()) => Ok(Storage::Present),
        Err(sys::Error::NotFound { .. }) => Ok(Storage::Absent),
        Err(e) => Err(e),
    }
}

/// Microseconden sinds `t0`.
fn us_since(t0: u64) -> u32 {
    u32::try_from(clock::now_ns().saturating_sub(t0) / 1000).unwrap_or(u32::MAX)
}

/// Een buffer van `n` bytes, of `None`.
fn buffer(n: usize) -> Option<Vec<u8>> {
    let mut b = Vec::new();
    b.try_reserve_exact(n).ok()?;
    b.resize(n, 0);
    Some(b)
}

/// Een lijst voor `n` latenties, of een lege (dan geen percentielen).
fn samples(n: usize) -> Vec<u32> {
    let mut v = Vec::new();
    let _ = v.try_reserve_exact(n);
    v
}

/// Een sample erbij, zolang er plaats is.
fn record(v: &mut Vec<u32>, us: u32) {
    if v.len() < v.capacity() {
        v.push(us);
    }
}

/// De eerste plek waar `a` en `b` verschillen. Eerst de hele vergelijking
/// (`bcmp`, het snelle pad van applib), de bytelus alleen bij een
/// verschil: die lus zat in de leestijd en kostte op de M4 een halve
/// milliseconde per MiB (GEMETEN 01-10: lezen 560 MB/s door de app tegen
/// 1770 door hopfs en 1521 voor het transport alleen).
pub(crate) fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    if a == b {
        return None;
    }
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .or_else(|| (a.len() != b.len()).then(|| a.len().min(b.len())))
}

/// De maten van een run.
struct Plan<'a> {
    path: &'a str,
    total: u64,
    chunk: usize,
    hole: bool,
    /// Lezingen per call van rand (1: de gewone lees).
    depth: usize,
    /// Bundels van rand tegelijk in de lucht.
    bundles: usize,
}

/// disk: schrijven, lezen, 4 KiB-writes en de stat-vloer op één bestand,
/// dat na afloop weer weg is.
pub(crate) async fn disk(sh: &'static Shared, r: &mut Report, p: &Params) {
    let mut sys = sh.net.system_client();
    match storage_of(sys.truncate(PROBE_PATH, 0).await) {
        Ok(Storage::Present) => {
            if let Err(e) = sys.remove(PROBE_PATH).await {
                r.fail(format_args!("storage probe: remove {PROBE_PATH}: {e}"));
                return;
            }
        }
        Ok(Storage::Absent) => {
            r.skip(NO_STORAGE);
            return;
        }
        Err(e) => {
            r.fail(format_args!("storage probe: {e}"));
            return;
        }
    }
    let mb = Params::int(p.mb, 64, 1, 1024);
    let kb = Params::int(p.kb, 1024, 4, 1024);
    let plan = Plan {
        path: p.path.as_deref().unwrap_or(DEFAULT_PATH),
        total: mb << 20,
        chunk: usize::try_from(kb << 10).unwrap_or(sys::MAX_CHUNK),
        hole: p.hole,
        depth: Params::int(p.depth, 1, 1, sys::MAX_READS as u64) as usize,
        bundles: Params::int(p.bundles, 1, 1, 2) as usize,
    };
    let rand = Params::int(p.rand, 0, 0, 1 << 20);
    if rand > 0 {
        let dur = p.secs.map(|v| Params::int(Some(v), 5, 1, 600));
        rand4k(sh, &mut sys, r, &plan, rand, dur).await;
        return;
    }
    run(sh, &mut sys, r, &plan).await;
    if let Err(e) = sys.remove(plan.path).await {
        r.line(format_args!("cleanup: remove {}: {e}", plan.path));
    }
    r.add("chunk", kb as f64, "KiB");
}

/// De plek van lezing `k` (xorshift, vast zaad), in blokken van 4 KiB.
fn spot(k: u64, blocks: u64) -> u64 {
    let mut x = k.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % blocks.max(1)
}

/// Willekeurige 4 KiB-lezingen: `n` stuks over het bestand van `plan`, dat
/// er eerst komt als het er niet (heel) is.
async fn rand4k(
    sh: &Shared,
    sys: &mut SystemClient,
    r: &mut Report,
    plan: &Plan<'_>,
    n: u64,
    dur: Option<u64>,
) {
    let Some(mut wbuf) = buffer(plan.chunk) else {
        r.fail(format_args!("no heap for a {}-byte buffer", plan.chunk));
        return;
    };
    for part in wbuf.chunks_mut(sh.blob.len().max(1)) {
        let k = part.len();
        part.copy_from_slice(sh.blob.get(..k).unwrap_or_default());
    }
    if plan.hole && sys.stat(plan.path).await.ok() != Some(plan.total) {
        note(format_args!(
            "disk rand4k: a hole of {} MB",
            plan.total >> 20
        ));
        if let Err(e) = sys
            .write_at(plan.path, plan.total - 1, wbuf.get(..1).unwrap_or_default())
            .await
        {
            r.fail(format_args!("hole: {e}"));
            return;
        }
    }
    if !plan.hole && sys.stat(plan.path).await.ok() != Some(plan.total) {
        note(format_args!(
            "disk rand4k: writing {} MB first",
            plan.total >> 20
        ));
        let mut off = 0u64;
        while off < plan.total {
            let k = usize::try_from(plan.total - off)
                .unwrap_or(plan.chunk)
                .min(plan.chunk);
            if let Err(e) = sys
                .write_at(plan.path, off, wbuf.get(..k).unwrap_or_default())
                .await
            {
                r.fail(format_args!("write at {off}: {e}"));
                return;
            }
            off += k as u64;
        }
    }
    if plan.depth > 1 || plan.bundles > 1 {
        many(sh, sys, r, plan, &wbuf, n, dur).await;
        return;
    }
    let mut lat = samples(usize::try_from(n).unwrap_or(0));
    let mut b = [0u8; SMALL];
    let blocks = plan.total / SMALL as u64;
    let t0 = clock::now_ns();
    let until = dur.map(|v| t0.saturating_add(v.saturating_mul(1_000_000_000)));
    let mut done = 0u64;
    for k in 0..n {
        if until.is_some_and(|u| clock::now_ns() >= u) {
            break;
        }
        let off = spot(k, blocks) * SMALL as u64;
        let t = clock::now_ns();
        match sys.read_into(plan.path, off, &mut b).await {
            Ok(got) if got == SMALL => {}
            Ok(got) => {
                r.fail(format_args!("rand4k at {off}: {got} bytes, want {SMALL}"));
                return;
            }
            Err(e) => {
                r.fail(format_args!("rand4k at {off}: {e}"));
                return;
            }
        }
        record(&mut lat, us_since(t));
        done += 1;
        // Een gat leest nullen; het laatste blok draagt de ene byte.
        let want = if plan.hole {
            if off + SMALL as u64 >= plan.total {
                continue;
            }
            ZEROS.as_slice()
        } else {
            let at = (off % plan.chunk as u64) as usize;
            wbuf.get(at..at + SMALL).unwrap_or_default()
        };
        if let Some(i) = first_diff(&b, want) {
            r.fail(format_args!(
                "rand4k at {off}: content mismatch (first bad byte at {i})"
            ));
            return;
        }
    }
    rand_report(r, plan, &mut lat, done, clock::now_ns().saturating_sub(t0));
}

/// Wat een gat of het bestand op `off` hoort te dragen (`None`: het laatste
/// blok van een gat, met de ene byte).
fn expected<'w>(plan: &Plan<'_>, wbuf: &'w [u8], off: u64) -> Option<&'w [u8]> {
    if plan.hole {
        return (off + (SMALL as u64) < plan.total).then_some(ZEROS.as_slice());
    }
    let at = (off % plan.chunk as u64) as usize;
    Some(wbuf.get(at..at + SMALL).unwrap_or_default())
}

/// Waarom een bundel van rand niet goed terugkwam.
enum Miss {
    /// De call zelf.
    Call(sys::Error),
    /// Eén lees: de offset en wat ze kreeg.
    Read(u64, core::result::Result<usize, u16>),
}

/// Eén bundel: `buf.len() / SMALL` lezingen vanaf lezing `first`, elk in
/// haar eigen stuk van `buf`.
async fn bundle(
    sys: &mut SystemClient,
    path: &str,
    first: u64,
    blocks: u64,
    buf: &mut [u8],
) -> Result<(), Miss> {
    let mut ops: bounded::BoundedVec<sys::ReadOp<'_>, { sys::MAX_READS }> =
        bounded::BoundedVec::new();
    for (j, dst) in buf.chunks_exact_mut(SMALL).enumerate() {
        let off = spot(first + j as u64, blocks) * SMALL as u64;
        if ops.push(sys::ReadOp::new(off, dst)).is_err() {
            break;
        }
    }
    sys.read_many(path, ops.as_mut_slice())
        .await
        .map_err(Miss::Call)?;
    match ops.as_slice().iter().find(|o| o.got != Ok(SMALL)) {
        Some(o) => Err(Miss::Read(o.off, o.got)),
        None => Ok(()),
    }
}

/// Twee futures samen afwachten.
async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (core::pin::pin!(a), core::pin::pin!(b));
    let (mut x, mut y) = (None, None);
    core::future::poll_fn(|cx| {
        if x.is_none()
            && let core::task::Poll::Ready(v) = a.as_mut().poll(cx)
        {
            x = Some(v);
        }
        if y.is_none()
            && let core::task::Poll::Ready(v) = b.as_mut().poll(cx)
        {
            y = Some(v);
        }
        match (x.take(), y.take()) {
            (Some(p), Some(q)) => core::task::Poll::Ready((p, q)),
            (p, q) => {
                (x, y) = (p, q);
                core::task::Poll::Pending
            }
        }
    })
    .await
}

/// rand met bundels: `depth` lezingen per call, `bundles` calls tegelijk
/// (de tweede over een eigen verbinding), elke lees met haar
/// inhoudscontrole; p50 en p99 zijn die van een call.
async fn many(
    sh: &Shared,
    sys: &mut SystemClient,
    r: &mut Report,
    plan: &Plan<'_>,
    wbuf: &[u8],
    n: u64,
    dur: Option<u64>,
) {
    let per = plan.depth * SMALL;
    let Some(mut rbuf) = buffer(per * plan.bundles) else {
        r.fail(format_args!(
            "no heap for {} bundles of {per} bytes",
            plan.bundles
        ));
        return;
    };
    let mut second = (plan.bundles > 1).then(|| sh.net.system_client());
    let step = (plan.depth * plan.bundles) as u64;
    let mut lat = samples(usize::try_from(n / step + 1).unwrap_or(0));
    let blocks = plan.total / SMALL as u64;
    let t0 = clock::now_ns();
    let until = dur.map(|v| t0.saturating_add(v.saturating_mul(1_000_000_000)));
    let mut k = 0u64;
    while k + step <= n && until.is_none_or(|u| clock::now_ns() < u) {
        let t = clock::now_ns();
        let (a, b) = rbuf.split_at_mut(per);
        let got = match second.as_mut() {
            Some(s2) => {
                let (x, y) = both(
                    bundle(sys, plan.path, k, blocks, a),
                    bundle(s2, plan.path, k + plan.depth as u64, blocks, b),
                )
                .await;
                x.and(y)
            }
            None => bundle(sys, plan.path, k, blocks, a).await,
        };
        match got {
            Ok(()) => {}
            Err(Miss::Call(e)) => {
                r.fail(format_args!("rand4k bundle at read {k}: {e}"));
                return;
            }
            Err(Miss::Read(off, g)) => {
                r.fail(format_args!("rand4k at {off}: {g:?}, want Ok({SMALL})"));
                return;
            }
        }
        record(&mut lat, us_since(t));
        for (j, got) in rbuf.chunks_exact(SMALL).enumerate() {
            let off = spot(k + j as u64, blocks) * SMALL as u64;
            let Some(want) = expected(plan, wbuf, off) else {
                continue;
            };
            if let Some(i) = first_diff(got, want) {
                r.fail(format_args!(
                    "rand4k at {off}: content mismatch (first bad byte at {i})"
                ));
                return;
            }
        }
        k += step;
    }
    rand_report(r, plan, &mut lat, k, clock::now_ns().saturating_sub(t0));
}

/// De marker en de regel van rand: `n` lezingen in `el` ns.
fn rand_report(r: &mut Report, plan: &Plan<'_>, lat: &mut [u32], n: u64, el: u64) {
    let (p50, p99) = (pct(lat, 50), pct(lat, 99));
    r.add("rand4k", n as f64 / secs(el), "IOPS");
    r.add("rand4k_n", n as f64, "reads");
    r.add(
        "rand4k_depth",
        (plan.depth * plan.bundles) as f64,
        "in flight",
    );
    r.add("rand4k_p50", f64::from(p50), "us");
    r.add("rand4k_p99", f64::from(p99), "us");
    r.line(format_args!(
        "{n} random 4 KiB reads over {} MB of {}: {:.0}/s, p50 {p50} us, p99 {p99} us; the file stays for the next run",
        plan.total >> 20,
        plan.path,
        n as f64 / secs(el)
    ));
}

/// De run zelf; het opruimen doet [`disk`].
async fn run(sh: &Shared, sys: &mut SystemClient, r: &mut Report, plan: &Plan<'_>) {
    let (Some(mut wbuf), Some(mut rbuf)) = (buffer(plan.chunk), buffer(plan.chunk)) else {
        r.fail(format_args!("no heap for two {}-byte buffers", plan.chunk));
        return;
    };
    for part in wbuf.chunks_mut(sh.blob.len().max(1)) {
        let n = part.len();
        part.copy_from_slice(sh.blob.get(..n).unwrap_or_default());
    }
    let calls = usize::try_from(plan.total / plan.chunk as u64 + 1).unwrap_or(0);
    let (mut wlat, mut rlat) = (samples(calls), samples(calls));
    let mb = plan.total >> 20;

    // Schrijven, of (hole=1) alleen de lengte zetten met één byte op het
    // eind: de leesfase leest dan gaten, die uit het RAM van de kern komen
    // zonder één schijfblok. Dat isoleert het transport kern naar app.
    let t0 = clock::now_ns();
    if plan.hole {
        let last = plan.total - 1;
        if let Err(e) = sys
            .write_at(plan.path, last, wbuf.get(..1).unwrap_or_default())
            .await
        {
            r.fail(format_args!("hole: {e}"));
            return;
        }
    }
    let mut off = 0u64;
    while off < plan.total && !plan.hole {
        let n = usize::try_from(plan.total - off)
            .unwrap_or(plan.chunk)
            .min(plan.chunk);
        let t = clock::now_ns();
        if let Err(e) = sys
            .write_at(plan.path, off, wbuf.get(..n).unwrap_or_default())
            .await
        {
            r.fail(format_args!("write at {off}: {e}"));
            return;
        }
        record(&mut wlat, us_since(t));
        if off.is_multiple_of(8 << 20) {
            note(format_args!("disk write {}/{mb} MB", off >> 20));
        }
        off += n as u64;
    }
    let wel = clock::now_ns().saturating_sub(t0);

    // Lezen in één hergebruikte buffer, lengte én inhoud gecontroleerd: een
    // gecachte DMA-buffer zonder invalidate geeft nette lengtes met de data
    // van de vorige transfer erin (T21, 03-09), en dat zie je alleen zo.
    let t1 = clock::now_ns();
    let mut off = 0u64;
    while off < plan.total {
        let n = usize::try_from(plan.total - off)
            .unwrap_or(plan.chunk)
            .min(plan.chunk);
        let t = clock::now_ns();
        let dst = rbuf.get_mut(..n).unwrap_or_default();
        let got = match sys.read_into(plan.path, off, dst).await {
            Ok(m) => m,
            Err(e) => {
                r.fail(format_args!("read at {off}: {e}"));
                return;
            }
        };
        if got != n {
            r.fail(format_args!("read at {off}: {got} bytes, want {n}"));
            return;
        }
        if !plan.hole
            && let Some(i) = first_diff(
                rbuf.get(..n).unwrap_or_default(),
                wbuf.get(..n).unwrap_or_default(),
            )
        {
            r.fail(format_args!(
                "read at {off}: content mismatch (first bad byte at {i})"
            ));
            return;
        }
        record(&mut rlat, us_since(t));
        if off.is_multiple_of(8 << 20) {
            note(format_args!("disk read {}/{mb} MB", off >> 20));
        }
        off += n as u64;
    }
    let rel = clock::now_ns().saturating_sub(t1);

    // 4 KiB-writes over het begin van hetzelfde bestand.
    note(format_args!("disk 4 KiB writes"));
    let mut slat = samples(SMALL_N);
    let small = wbuf.get(..SMALL.min(plan.chunk)).unwrap_or_default();
    let t2 = clock::now_ns();
    for k in 0..SMALL_N {
        let t = clock::now_ns();
        if let Err(e) = sys
            .write_at(plan.path, (k * small.len()) as u64, small)
            .await
        {
            r.fail(format_args!("4 KiB write {k}: {e}"));
            return;
        }
        record(&mut slat, us_since(t));
    }
    let sel = clock::now_ns().saturating_sub(t2);

    // De vloer: stat raakt alleen de metadata van hopfs in het RAM van de
    // kern, dus dit is het system-callpad zelf zonder één schijfblok.
    note(format_args!("disk stat floor"));
    let mut flat = samples(STAT_N);
    let mut size = 0;
    for _ in 0..STAT_N {
        let t = clock::now_ns();
        match sys.stat(plan.path).await {
            Ok(s) => size = s,
            Err(e) => {
                r.fail(format_args!("stat: {e}"));
                return;
            }
        }
        record(&mut flat, us_since(t));
    }
    if size != plan.total {
        r.fail(format_args!(
            "file is {size} bytes after the run, want {}",
            plan.total
        ));
        return;
    }

    let total = plan.total as f64;
    if plan.hole {
        r.add("write", 0.0, "MB/s (skipped, hole=1)");
    } else {
        r.add("write", total / secs(wel) / 1e6, "MB/s");
    }
    r.add("read", total / secs(rel) / 1e6, "MB/s");
    r.add(
        "write_4k",
        (small.len() * SMALL_N) as f64 / secs(sel) / 1e6,
        "MB/s",
    );
    let (f50, f99) = (pct(&mut flat, 50), pct(&mut flat, 99));
    r.add("floor_p50", f64::from(f50), "us");
    r.add("floor_p99", f64::from(f99), "us");
    let ms = |us: u32| f64::from(us) / 1000.0;
    r.line(format_args!(
        "{mb} MB via {} in {} calls of {} KiB (write p50 {:.1} ms, p99 {:.1} ms; read p50 {:.1} ms, p99 {:.1} ms)",
        plan.path,
        rlat.len(),
        plan.chunk >> 10,
        ms(pct(&mut wlat, 50)),
        ms(pct(&mut wlat, 99)),
        ms(pct(&mut rlat, 50)),
        ms(pct(&mut rlat, 99))
    ));
    if plan.hole {
        r.line(format_args!(
            "hole=1: the file was never written, every read returned zeros from the kernel's RAM: the transport kernel to app alone"
        ));
    }
    r.line(format_args!(
        "4 KiB writes, the database pattern: {SMALL_N} calls, {:.0}/s, p50 {} us, p99 {} us",
        SMALL_N as f64 / secs(sel),
        pct(&mut slat, 50),
        pct(&mut slat, 99)
    ));
    r.line(format_args!(
        "system-call floor (stat, no disk block touched): p50 {f50} us, p99 {f99} us; everything above it is hopfs + disk"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_on_a_write_means_no_storage() {
        assert_eq!(storage_of(Ok(())), Ok(Storage::Present));
        assert_eq!(
            storage_of(Err(sys::Error::NotFound { op: 2 })),
            Ok(Storage::Absent)
        );
        assert_eq!(
            storage_of(Err(sys::Error::Timeout)),
            Err(sys::Error::Timeout)
        );
    }

    #[test]
    fn the_first_difference_is_found() {
        assert_eq!(first_diff(b"abcd", b"abcd"), None);
        assert_eq!(first_diff(b"abxd", b"abcd"), Some(2));
        assert_eq!(first_diff(b"ab", b"abc"), Some(2));
    }
}
