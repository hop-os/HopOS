//! Het media-vlak van de kern-binary: de codec-dienst van de system-API (Go:
//! `OLD/metal/cmd/hopos/codec.go` en `codec_off.go`). De meting door de
//! decoder doet `apps/decode`, als app.
//!
//! Dit is compute, geen beeld: de node decodeert een stream naar frames in
//! het geheugen van een app, en er komt geen scherm aan te pas. Alleen met
//! de feature `media`; zonder staat de stub in off.rs en antwoorden de
//! codec-ops "onbekende op" (de kern kent ze dan niet).
//!
//! # Eigendom
//!
//! De dienst ([`kern::codecabi::CodecCell`]) staat in een `static` en bezit
//! de engine: de VPU-driver van `media-mve` met zijn arena, zijn sessies en
//! zijn firmware-cache. De system-API leent hem per call één synchrone
//! beurt; de opruimtaak hier leent hem eens per seconde om de sessies van
//! gestopte bewoners te sluiten.
//!
//! # De bring-up
//!
//! [`up`] meldt de dienst aan en spawnt één taak die de drie haken van
//! `docs/media.md` na elkaar loopt, elk als bericht aan zijn eigenaar:
//!
//! 1. De arena: een blok buiten de partitie-pool (`hopos.codec=<MB>`, 0 is
//!    uit) van de lifecycle-actor (`Request::ReserveDevice`, de regel
//!    `HOPOS_POOL_DEVICE`). Op een board zonder VPU gaat hij na
//!    `HOPOS_CODEC_NONE` meteen terug; zo bewijst QEMU de weg.
//! 2. De firmware: de zestien blobs van het volume, door de hopfs-actor voor
//!    de kern gelezen (`kern::rpc::read_file`), ná de mount en vóór de VPU.
//!    Wat mist, staat bij naam in `HOPOS_CODEC_NOFW`.
//! 3. Het ijzer: stroom, klokken, reset, de driver, `HOPOS_CODEC_UP`; faalt
//!    dat, dan gaat de arena terug (Go: `ReleaseDevice`).
//!
//! De taak draait op de executor, dus pas als de actoren draaien: de
//! lifecycle-actor handelt de reservering af ná een eventuele adoptie
//! (kern-flip), en vóór de eerste plaatsing die daarna in de rij staat.

use crate::KernConsole;
use alloc::vec::Vec;
use bounded::BoundedVec;
use core::time::Duration;
use cpu::println;
use driver_codec::Firmware;
use executor::Executor;
use kern::codecabi::{CacheMaint, CodecCell, Lives};
use kern::slots::{Reply, Request, Response};
use kern::{Region, Slot};
use media_mve::Device;
use sync::{Either, select};

/// De engine van deze binary: de Linlon V8 met de firmware-cache.
pub(crate) type Vpu = Device<Blobs>;

/// De codec-dienst van de node.
pub(crate) static CODEC: CodecCell<Vpu, NodeLives, DevCache, KernConsole> =
    CodecCell::new(NodeLives, DevCache, KernConsole);

/// Hoeveel firmware-blobs de cache houdt: de O6N levert er zestien (elf
/// decoders, vijf encoders).
pub(crate) const MAX_BLOBS: usize = media_mve::FIRMWARE.len();

/// De grootste blob die we aannemen; de echte zijn zo'n 300 KB (Go: de
/// grens van `codecFirmware.Load`).
pub(crate) const MAX_BLOB: usize = 4 << 20;

/// De antwoordplek van de bring-up en het meetinstrument, bij de
/// lifecycle-actor en bij de hopfs-actor. Eén aanroeper tegelijk: de
/// bring-up spawnt het meetinstrument pas als hij zelf klaar is.
static REPLY: Reply = Reply::new();

/// Hoe lang de bring-up op de eigenaar van de pool wacht. Op een board
/// waar de slots niet opkomen, leest niemand de brievenbus; dan liever
/// een luide regel dan een taak die voor altijd hangt.
const POOL_WAIT: Duration = Duration::from_secs(10);

/// Hoe lang één lezing van de hopfs-actor mag duren (een blob van 300
/// KB is een paar milliseconden van de NVMe).
#[cfg(feature = "board-o6n")]
pub(crate) const FS_WAIT: Duration = Duration::from_secs(5);

/// De levende bewoners en hun partities, uit de servicer-tabel.
pub(crate) struct NodeLives;

impl Lives for NodeLives {
    fn current(&self, slot: Slot) -> Option<u32> {
        crate::SERVICERS.current(slot)
    }

    fn partition(&self, slot: Slot) -> Option<Region> {
        // De basis en maat die de lifecycle-actor bij de start in de
        // servicer-besturing zette; `None` zodra de servicer weg is, en
        // dan weigert de grant met STATUS_DENIED "slot is being
        // released": veilig dicht.
        crate::SERVICERS.partition(slot)
    }
}

/// Het cache-onderhoud via `dev`: `dc cvac` en `dc civac`.
pub(crate) struct DevCache;

impl CacheMaint for DevCache {
    fn clean(&mut self, pa: u64, len: u64) {
        dev::push(dev::Pa(pa), usize::try_from(len).unwrap_or(0));
    }
    fn clean_inv(&mut self, pa: u64, len: u64) {
        dev::pull(dev::Pa(pa), usize::try_from(len).unwrap_or(0));
    }
}

/// De firmwarecache: bring-up vult wat aanwezig is; een latere codec-open
/// laat de system-API ontbrekende blobs bijlezen vóór de synchrone driverbeurt.
#[derive(Default)]
pub(crate) struct Blobs {
    blobs: BoundedVec<(&'static str, Vec<u8>), MAX_BLOBS>,
}

impl Blobs {
    /// Legt een blob in de cache (de lezer van het volume); vol of te
    /// groot wordt geweigerd.
    pub(crate) fn put(&mut self, name: &'static str, bin: Vec<u8>) -> bool {
        bin.len() <= MAX_BLOB
            && media_mve::FIRMWARE.contains(&name)
            && media_mve::validate_firmware(&bin).is_ok()
            && self.blobs.push((name, bin)).is_ok()
    }

    /// Hoeveel blobs de cache houdt.
    #[cfg_attr(
        not(feature = "board-o6n"),
        expect(dead_code, reason = "alleen de O6N laadt firmware")
    )]
    pub(crate) fn len(&self) -> usize {
        self.blobs.len()
    }
}

impl Firmware for Blobs {
    fn install(&mut self, name: &'static str, bytes: Vec<u8>) -> driver_codec::Result {
        if !media_mve::FIRMWARE.contains(&name) || bytes.is_empty() || bytes.len() > MAX_BLOB {
            return Err(driver_codec::Error::Unsupported);
        }
        if self.blobs.iter().any(|(n, _)| *n == name) {
            return Ok(());
        }
        if self.put(name, bytes) {
            Ok(())
        } else {
            Err(driver_codec::Error::Busy)
        }
    }
    fn load(&mut self, name: &str) -> Option<&[u8]> {
        self.blobs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b.as_slice())
    }
}

/// Zet de dienst op: altijd aangemeld bij de system-API (zonder ijzer
/// weigert hij luid), en de bring-up als taak (arena, firmware, ijzer).
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn up(exec: &'static Executor) {
    if !kern::codecabi::install(&CODEC) {
        println!("codec: service installed twice HOPOS_CODEC_FAIL");
        return;
    }
    if exec.spawn(bring_up(exec)).is_err() {
        println!("codec: bring-up not spawned, video codec stays off HOPOS_CODEC_FAIL");
    }
}

/// De maat van de arena: `hopos.codec=<MB>`, of die van het board.
fn arena_mb() -> u64 {
    match board::Board::boot_param(&crate::BOARD, "hopos.codec") {
        "" => hw::DEFAULT_ARENA_MB,
        v => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                println!(
                    "codec: ignoring hopos.codec={v:?}, using {} MB",
                    hw::DEFAULT_ARENA_MB
                );
                hw::DEFAULT_ARENA_MB
            }
        },
    }
}

/// Wat de bring-up straks uit de partitie-pool haalt (de arena), in
/// bytes: de plaatsing van Hop trekt dat af van de poolgrootte die hij
/// aan Hop meldt, anders overschat Hop de node met de arena (768 MB op
/// de O6N) en plaatst hij een job die niet past.
pub(crate) fn planned_bytes() -> u64 {
    arena_mb() << 20
}

/// De bring-up: arena, dan het ijzer van het board; wat niet opkomt,
/// geeft zijn blok terug.
async fn bring_up(exec: &'static Executor) {
    let mb = arena_mb();
    if mb == 0 {
        println!("codec: off (hopos.codec=0), codec calls refused HOPOS_CODEC_OFF");
        return;
    }
    let what = "the codec arena";
    // Een onzinnig getal in de config is een weigering, geen omloop.
    let arena = match mb.checked_mul(1 << 20) {
        Some(size) => reserve(exec, size, what).await,
        None => None,
    };
    let Some(arena) = arena else {
        println!(
            "codec: no {mb} MB arena outside the partition pool, video codec stays off HOPOS_CODEC_OFF"
        );
        return;
    };
    if !hw::up(exec, arena).await {
        release(exec, arena, what).await;
    }
}

/// Vraagt de eigenaar van de pool om een blok buiten de partities. De
/// actor zegt het zelf luid (`HOPOS_POOL_DEVICE` of `_FAIL`); hier
/// alleen wat hij niet kan zeggen: dat hij nooit antwoordde.
async fn reserve(exec: &'static Executor, size: u64, what: &'static str) -> Option<Region> {
    let call = kern::slots::call(
        &crate::LIFECYCLE,
        &REPLY,
        Request::ReserveDevice { size, what },
    );
    match select(call, exec.after(POOL_WAIT)).await {
        Either::Left(Ok(Response::Device(r))) => Some(r),
        Either::Left(Ok(Response::Failed(_))) => None,
        Either::Left(Ok(r)) => {
            println!("codec: the pool owner answered {r:?} to a device reservation");
            None
        }
        Either::Left(Err(e)) => {
            println!("codec: device reservation not sent: {e}");
            None
        }
        Either::Right(()) => {
            // Staat het verzoek al in de rij, dan geeft de actor het blok
            // straks toch: dat lekt (veilig dicht), en dit zegt waarom.
            println!(
                "codec: the pool owner did not answer within {} s (no slots on this board?)",
                POOL_WAIT.as_secs()
            );
            None
        }
    }
}

/// Geeft het blok terug: het telt af van wat apps kunnen krijgen, en
/// honderden MB kwijt zijn omdat het ijzer niet opkwam is zonde (Go).
async fn release(exec: &'static Executor, region: Region, what: &'static str) {
    let call = kern::slots::call(
        &crate::LIFECYCLE,
        &REPLY,
        Request::ReleaseDevice { region, what },
    );
    match select(call, exec.after(POOL_WAIT)).await {
        Either::Left(Ok(Response::Done)) => {}
        Either::Left(Ok(r)) => println!(
            "codec: {} MB at {:#x} not back in the pool: {r:?}",
            region.size >> 20,
            region.base
        ),
        Either::Left(Err(e)) => println!(
            "codec: {} MB at {:#x} not back in the pool: {e}",
            region.size >> 20,
            region.base
        ),
        Either::Right(()) => println!(
            "codec: the pool owner did not take {} MB at {:#x} back within {} s",
            region.size >> 20,
            region.base,
            POOL_WAIT.as_secs()
        ),
    }
}

/// Een board zonder videocodec: de arena gaat terug, de dienst weigert.
#[cfg(not(feature = "board-o6n"))]
mod hw {
    use cpu::println;
    use executor::Executor;
    use kern::Region;

    /// De arena als niets anders gezegd wordt: klein, want hij gaat meteen
    /// terug. Hij bestaat om de weg van de reservering op QEMU te bewijzen
    /// (`HOPOS_POOL_DEVICE` en `_RELEASE` rond `HOPOS_CODEC_NONE`).
    pub(super) const DEFAULT_ARENA_MB: u64 = 64;

    /// Geen ijzer: `false`, en de aanroeper geeft het blok terug.
    pub(super) async fn up(_exec: &'static Executor, _arena: Region) -> bool {
        println!("codec: no video codec on this board, codec calls refused HOPOS_CODEC_NONE");
        false
    }
}

/// De VPU van de O6N: firmware, stroom, driver, opruimtaak.
#[cfg(feature = "board-o6n")]
mod hw {
    use super::{Blobs, CODEC, FS_WAIT, MAX_BLOB, REPLY, Vpu};
    use alloc::vec::Vec;
    use bounded::BoundedVec;
    use core::fmt;
    use core::time::Duration;
    use cpu::println;
    use driver_codec::{Engine, Graveyard, Show};
    use executor::Executor;
    use kern::Region;
    use media_mve::{Device, FIRMWARE};
    use sync::{Either, select};

    /// De bak van de gevallen sessie-handvatten van de VPU.
    static GRAVES: Graveyard = Graveyard::new();

    /// De arena als niets anders gezegd wordt. Een 4K HEVC-decoder
    /// houdt zestien referentieframes vast; in 10 bit is dat 24 MB per
    /// frame. Met 256 MB liep de arena leeg en eindigde dat in een MMU
    /// ABORT (22-09); 768 MB draagt één 4K10-stream met werkruimte en is
    /// op 16 GB nog geen twintigste. Alleen 8 bit: `hopos.codec=256`.
    pub(super) const DEFAULT_ARENA_MB: u64 = 768;

    /// Waar de blobs op het volume staan, in deze volgorde. `/firmware`
    /// is die van de Go-kern (`codecFirmwareDir`); `/codec-firmware` is
    /// waar Lumen ze neerzet: zijn jobspec mount `/firmware` op dat
    /// volume (jobs/hopos-media-o6n.cfg), en een volume is een
    /// gewoon hopfs-pad.
    const FW_DIRS: [&str; 2] = ["/firmware", "/codec-firmware"];

    /// Hoe vaak de opruimtaak de sessies van gestopte bewoners sluit.
    const REAP_EVERY: Duration = Duration::from_secs(1);

    /// De opruimtaak, als er een engine is.
    fn spawn_reaper(exec: &'static Executor) {
        let reaper = async move {
            loop {
                exec.after(REAP_EVERY).await;
                let _ = CODEC.with(|s, _| s.reap());
            }
        };
        if exec.spawn(reaper).is_err() {
            println!(
                "codec: reaper not spawned, sessions of stopped tasks stay open HOPOS_CODEC_FAIL"
            );
        }
    }

    /// De O6N: firmware, stroom, driver. `false` als het ijzer niet
    /// opkwam; de aanroeper geeft de arena dan terug.
    pub(super) async fn up(exec: &'static Executor, arena: Region) -> bool {
        let blobs = firmware(exec).await;
        let w = match vboard::codec::power_vpu(arena.base, arena.size) {
            Ok(w) => w,
            Err(e) => {
                println!("codec: {e}, video codec stays off HOPOS_CODEC_OFF");
                return false;
            }
        };
        let a = match media_mve::Arena::new(arena.base, arena.size) {
            Ok(a) => a,
            Err(e) => {
                println!("codec: {e}, video codec stays off HOPOS_CODEC_OFF");
                return false;
            }
        };
        // SAFETY: `w.base` komt uit de DSDT van dit board (of de SoC-constante)
        // en is door `power_vpu` gemapt, van stroom, klok en reset voorzien;
        // alleen deze driver raakt het blok aan. De arena komt van de
        // eigenaar van de pool (`Request::ReserveDevice`), ligt dus buiten
        // elke partitie, en is door `power_vpu` ongecached gemapt (`_CCA = 0`).
        let probed = unsafe { Device::probe(w.base, a, blobs, &GRAVES, cpu::idle::now) };
        let dev = match probed {
            Ok(d) => d,
            Err(e) => {
                println!("codec: {e}, video codec stays off HOPOS_CODEC_OFF");
                return false;
            }
        };
        println!(
            "codec: {} HOPOS_CODEC_UP",
            Show(&dev, |d: &Vpu, f| d.describe(f))
        );
        if CODEC.with(|s, _| s.install(dev)).is_none() {
            // De engine viel hier: zijn arena is van niemand meer, maar
            // het blok blijft gemapt voor het ijzer. Dicht houden.
            println!("codec: service busy at boot HOPOS_CODEC_FAIL");
            return true;
        }
        spawn_reaper(exec);
        true
    }

    /// Namen met een komma ertussen, voor één regel.
    struct Names<'a>(&'a [&'static str]);

    impl fmt::Display for Names<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for (i, n) in self.0.iter().enumerate() {
                if i > 0 {
                    f.write_str(",")?;
                }
                f.write_str(n)?;
            }
            Ok(())
        }
    }

    /// De zestien blobs van het volume, door de hopfs-actor voor de kern
    /// gelezen. Wat mist, staat bij naam in één regel: een open van die
    /// codec weigert straks met "no firmware for this codec".
    async fn firmware(exec: &'static Executor) -> Blobs {
        let mut blobs = Blobs::default();
        if !crate::storage::is_up() {
            println!(
                "codec: no volume to read /firmware from, every open will be refused HOPOS_CODEC_NOFW missing=all"
            );
            return blobs;
        }
        let mut missing: BoundedVec<&'static str, { FIRMWARE.len() }> = BoundedVec::new();
        let mut bytes = 0usize;
        for name in FIRMWARE {
            match read_blob(exec, name).await {
                Ok(bin) => {
                    let n = bin.len();
                    if blobs.put(name, bin) {
                        bytes += n;
                    } else {
                        let _ = missing.push(name);
                    }
                }
                Err(kern::Error::Busy) => {
                    // Geen antwoord binnen de termijn: het verzoek staat
                    // nog in de rij en zijn antwoord komt op dezelfde
                    // plek. Een volgende lezing zou dat antwoord voor het
                    // hare houden; dus stoppen, en de rest mist.
                    println!(
                        "codec: the hopfs actor did not answer (within {} s), firmware reading stopped at {name}",
                        FS_WAIT.as_secs()
                    );
                    for rest in FIRMWARE.iter().skip_while(|n| **n != name) {
                        let _ = missing.push(rest);
                    }
                    break;
                }
                Err(e) => {
                    if e != kern::Error::NoEnt {
                        println!("codec: firmware {name}: {e}");
                    }
                    let _ = missing.push(name);
                }
            }
        }
        println!(
            "codec: {} of {} firmware blobs read from the volume ({} KB)",
            blobs.len(),
            FIRMWARE.len(),
            bytes >> 10
        );
        if !missing.is_empty() {
            println!(
                "codec: no firmware for {} in {} or {}, those codecs refuse to open HOPOS_CODEC_NOFW missing={}",
                Names(missing.as_slice()),
                FW_DIRS[0],
                FW_DIRS[1],
                Names(missing.as_slice())
            );
        }
        blobs
    }

    /// Eén blob: `<dir>/<naam>.fwb` in elk van [`FW_DIRS`], de eerste die
    /// er is. Een lege is geen firmware.
    async fn read_blob(exec: &'static Executor, name: &str) -> kern::Result<Vec<u8>> {
        let mut last = kern::Error::NoEnt;
        for dir in FW_DIRS {
            let mut path = [0u8; 64];
            let n = join(&mut path, &[dir.as_bytes(), b"/", name.as_bytes(), b".fwb"]);
            let p = path.get(..n).unwrap_or(&[]);
            let read = kern::rpc::read_file(&crate::storage::FS_INBOX, &REPLY, p, MAX_BLOB);
            match select(read, exec.after(FS_WAIT)).await {
                Either::Left(Ok(b)) if b.is_empty() => last = kern::Error::Corrupt { at: 0 },
                Either::Left(Ok(b)) => return Ok(b),
                // Een volle brievenbus (of een bevroren actor) is ook
                // `Busy`: dan staat er niets van ons in de rij, maar een
                // actor die nu niet leest, leest de volgende ook niet.
                Either::Left(Err(kern::Error::Busy)) | Either::Right(()) => {
                    return Err(kern::Error::Busy);
                }
                Either::Left(Err(e)) => last = e,
            }
        }
        Err(last)
    }

    /// Plakt `parts` in `out`; geeft de lengte (afgekapt op de buffer).
    fn join(out: &mut [u8], parts: &[&[u8]]) -> usize {
        let mut n = 0;
        for p in parts {
            for b in *p {
                if let Some(d) = out.get_mut(n) {
                    *d = *b;
                    n += 1;
                }
            }
        }
        n
    }
}
