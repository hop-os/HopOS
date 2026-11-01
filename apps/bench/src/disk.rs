//! `BENCH=disk`: hopfs gemeten waar een app hem ziet, door de system-API.
//! Dit verving op 01-11 de schijf-bench in de kern (`hopos.nvmebench`,
//! docs/sequence-hopos.md §14.6): meten gaat op één manier, vanuit het hok.
//!
//! [`BYTES`] sequentieel schrijven in happen van 1 MiB plus één `sync`,
//! dezelfde bytes sequentieel lezen, en [`RANDOM_N`] willekeurige lezingen
//! van 4 KiB. De rauwe schijf (de Go-tabel per commandomaat) meet een app
//! niet: die is van de kern, en een app ziet alleen hopfs.

use alloc::vec::Vec;
use applib::sys::{Error, MAX_CHUNK, Result};
use applib::{App, appnet, clock, log};

/// Het bestand in de eigen root van het slot.
const PATH: &str = "/bench-disk";
/// Zoveel bytes per run: 16 MiB, de maat per commandomaat van de Go-tabel.
const BYTES: u64 = 16 << 20;
/// Zoveel willekeurige lezingen van 4 KiB (16 MiB).
const RANDOM_N: u64 = 4096;

/// Meet en zet de uitkomst op het log; de exitcode: 0 als het lukte.
pub(crate) async fn run(app: &'static App) -> u64 {
    match measure(app).await {
        Ok(()) => 0,
        Err(e) => {
            log!("bench: disk: {e} HOPOS_BENCH_FAIL role=disk");
            1
        }
    }
}

async fn measure(app: &'static App) -> Result {
    let net = appnet::up(app).map_err(|_| Error::Protocol("net startup"))?;
    let mut sys = net.system_client();
    let mut buf = Vec::new();
    buf.try_reserve_exact(MAX_CHUNK)
        .map_err(|_| Error::Protocol("no heap for 1 MiB"))?;
    buf.resize(MAX_CHUNK, 0x5a);
    let t0 = clock::now_ns();
    sys.truncate(PATH, 0).await?;
    for off in (0..BYTES).step_by(MAX_CHUNK) {
        sys.write_at(PATH, off, &buf).await?;
    }
    sys.sync(PATH).await?;
    let t1 = clock::now_ns();
    for off in (0..BYTES).step_by(MAX_CHUNK) {
        if sys.read_into(PATH, off, &mut buf).await? != MAX_CHUNK {
            return Err(Error::Protocol("short read"));
        }
    }
    let t2 = clock::now_ns();
    let (mut x, blocks) = (0x9e37_79b9_7f4a_7c15_u64, BYTES / 4096);
    for _ in 0..RANDOM_N {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let page = buf.get_mut(..4096).unwrap_or_default();
        sys.read_into(PATH, (x % blocks) * 4096, page).await?;
    }
    let t3 = clock::now_ns();
    sys.remove(PATH).await?;
    let iops = RANDOM_N * 1_000_000_000 / t3.saturating_sub(t2).max(1);
    log!(
        "bench: disk {} MiB write+sync {:.1} MB/s, read {:.1} MB/s, random 4 KiB {iops} IOPS HOPOS_BENCH_DISK",
        BYTES >> 20,
        crate::proto::mbps(BYTES, t1.saturating_sub(t0)),
        crate::proto::mbps(BYTES, t2.saturating_sub(t1)),
    );
    Ok(())
}
