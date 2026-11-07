//! De kern-flip in de kern-binary: de haak van `PrivOp::FLIP`, de vangst
//! en de sprong (kern A), en de landing (kern B). De procedure per board,
//! de markers en de faalmodi staan in `docs/flip.md`.
//!
//! De flip is een gewone boot plus een handoff. Elk onderdeel kent alleen
//! zijn eigen stuk: kern A vangt het (`capture`), kern B neemt het over in
//! dezelfde reeks als een koude boot (`restore`). Alleen deze module weet
//! dat er een flip is.
//!
//! | Stuk | Kern A | Kern B (`main.rs` `setup`) |
//! | --- | --- | --- |
//! | klok | `clock::offset` | `clock::restore` (hier, in [`land`]) |
//! | opslag | `storage::freeze_for_flip` | `storage::start` houdt de boom |
//! | lease | `net::lease` | `net::start` met [`Landing::lease`]: de host-taak slaat DHCP over (H1) |
//! | hok | `Request::Snapshot` | `slots::start` met [`Landing::slots`]: adopteren; hun poorten gaan open als het net luistert |
//!
//! Wat niet meegaat: de conntrack en de open verbindingen van de
//! node-stack. Een verbinding die breekt, is een fout of een time-out voor
//! wie hem had, en die verbindt opnieuw (Derek, 01-11: "internet is toch
//! gemaakt om te herconnecten").
//!
//! De weg, in volgorde:
//!
//! 1. Hop haalt de bundel (`POST /flip` met URL en sha256) en stroomt hem
//!    rauw in een slot dat hij met `abi::systemapi::FLIP_BUNDLE_JOB`
//!    reserveerde; dan `PrivOp::FLIP` met het slot en de som.
//! 2. [`prepare`] (de haak, synchroon): de som toetsen, de bundel lezen
//!    (`kern::kernflip::Bundle`), de som van zijn switch-code tegen de
//!    geïnstalleerde houden, de kern plat neerleggen in de staging en
//!    relokeren naar het koude adres (`cpu::el2::chain::relocate`). Elke
//!    weigering valt hier, vóór de 202: Hop krijgt haar als antwoord op
//!    zijn FLIP, en deze kern draait gewoon door.
//! 3. De flip-taak ([`start`]) wacht even, zodat het antwoord op de FLIP
//!    Hop nog bereikt, en vangt dan, in deze volgorde: hopfs vastleggen en
//!    dicht, de USB stil, de bewoners en de lease. Dan het handoff-blob, de
//!    handoff in de feitenpagina, en de sprong (`cpu::el2::chain::chain`)
//!    met x0 = de feitenpagina. Na de 202 is er geen weg terug (§13.7): wat
//!    hier misgaat, is `HOPOS_FATAL` en de koude boot.
//! 4. De nieuwe kern komt door dezelfde ingang als bij stroom aan, en
//!    [`facts`] leest in `kmain` de feitenpagina (O6). Dan [`land`] als
//!    eerste in `setup`. Een overdracht is
//!    `HOPOS_FLIP_BOOT gen=N` en een [`Landing`]; `setup` geeft elk stuk
//!    aan zijn onderdeel. Een stuk dat niet terugkomt, is `HOPOS_FATAL`.
//!    Een kern die niet tot een luisterend net komt, vangt de ene guard van
//!    de watchdog (`kern::watchdog::GUARD_SECS`), net als bij elke boot;
//!    [`settle`] meldt het net.
//!
//! Wat een kern die stierf als laatste zei, staat in de zwarte doos (de
//! console-ring, `conport`), niet in een eigen spoor van de flip (F1,
//! 07-11): vóór de 202 is een fout een weigering, erna `HOPOS_FATAL`.
//!
//! De adressen zijn die van het board (`vboard::slots::FLIP_*`): de
//! staging, de trampoline en het blob liggen buiten de pool, de staging en
//! het blob ook buiten het kern-RAM (de trampoline op de Radxa en de M4
//! niet: de bovenste pagina ervan, buiten de heap). Het koude adres is het
//! linkadres van het board, behalve op UEFI: daar is de kern een PIE die de
//! firmware ergens neerlegde, en gaat de nieuwe kern op precies die basis
//! (`__efi_head`), binnen de maat die de firmware toen gaf.
//!
//! De flip heeft ÉÉN trigger: `PrivOp::FLIP` van Hop (`POST /flip` achter
//! dezelfde HMAC als een jobspec). Alleen warm (Derek, 01-11): een bundel
//! met een andere switch-code is een weigering vóór de 202. Beleid (wanneer,
//! en waarvandaan) is van Hop; de kern is mechanisme.
//!
//! # riscv64
//!
//! Geen flip ([`FLIPS`]): de switch-code draait daar uit het kern-image, dus
//! kern B zou hem overschrijven onder de app-harts, en er is niets wat hij
//! kan adopteren (`cage_riscv::adopt` weigert). De haak weigert vóór de 202.
//!
//! # De config
//!
//! Op elk board staat `hopos.cfg` in het kern-image (`board::cfgwin`). Een
//! bundel met een leeg venster krijgt het venster van deze kern
//! (`HOPOS_FLIP_CFG`); een bundel met een gevuld venster houdt het zijne
//! (`HOPOS_FLIP_CFG_OWN`).

use crate::{BOARD, DevMem};
use abi::layout::FLIP_HANDOFF_LEN;
use alloc::vec::Vec;
use board::Board;
use board::cfgwin::Carry;
use core::cell::Cell;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use cpu::el2::chain::{self, Jump};
use cpu::println;
use dev::Pa;
use executor::Executor;
use kern::cage::PhysMem;
use kern::kernflip::{self, Bundle, FLIP_ABI, FlipPlan, HANDOFF_TAIL, Handoff};
use kern::slots::{Reply, Request, Response, SlotState};
use kern::system::FlipBundle;
use sync::{Local, Signal};
use vboard::slots::{BOOT_SCRATCH_PA, FLIP_TRAMP_PA, STAGE_HDR_PA, STAGE_MAX, STAGE_PA};

/// Het handoff-blob: direct onder het staging-maatwoord, op elk board.
const FLIP_HANDOFF_PA: u64 = abi::layout::flip_handoff_pa(STAGE_HDR_PA);

/// De feitenpagina (O6): op de boot-scratch van elk board, en x0 van elke
/// sprong.
const FACTS_PA: u64 = abi::layout::facts_pa(BOOT_SCRATCH_PA);

// De poort van `cpu::boot::_start` (het linkcontract): een core met dit
// adres in x0 komt uit een sprong en mag door. En de trampoline: waar die
// in de kern-RAM ligt (de Radxa, de M4), toetst het linkscript dat de heap
// ervoor ophoudt.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    ".globl __hopos_facts",
    ".set __hopos_facts, {facts}",
    ".globl __hopos_flip_tramp",
    ".set __hopos_flip_tramp, {tramp}",
    facts = const FACTS_PA,
    tramp = const FLIP_TRAMP_PA,
);

// Het blob van de kern en de plek van het board zijn even groot: een
// nieuwe kern leest precies [`HANDOFF_TAIL`] bytes op [`FLIP_HANDOFF_PA`].
const _: () = assert!(FLIP_HANDOFF_LEN == HANDOFF_TAIL as u64);

/// Hoe lang de flip-taak na een geaccepteerde FLIP wacht: het antwoord moet
/// door de switch naar Hop, en Hop zijn 202 naar buiten. Op QEMU ruim
/// binnen 100 ms; de sprong is geen race die we willen winnen.
const JUMP_DELAY: Duration = Duration::from_millis(500);

/// Hoe lang de flip-taak op een actor (hopfs, de switch) wacht. Een actor
/// die in twee seconden niet antwoordt, is een kern die niet overgedragen
/// hoort te worden: `HOPOS_FATAL`.
const ACTOR_WAIT: Duration = Duration::from_secs(2);

/// Kan deze kern flippen? Op riscv64 niet: de switch-code draait daar uit
/// het kern-image, en de nieuwe kern zou hem onder de app-harts
/// overschrijven. Wat ontbreekt: een kopie van de switcher buiten het image
/// (zoals de plan-regio op arm64), met `mtvec` van elk app-hart daarop, en
/// een `adopt` in `cage_riscv`.
const FLIPS: bool = !cfg!(target_arch = "riscv64");

/// Wat de firmware bij stroom aan in x0 gaf ([`facts`]).
static FIRMWARE_X0: AtomicU64 = AtomicU64::new(0);
/// Waar de vorige kern de handoff legde, 0 = een koude boot ([`facts`]).
static HANDOFF: AtomicU64 = AtomicU64::new(0);
/// De generatie van deze kern: 1 na een koude boot, N+1 na een flip vanaf
/// generatie N (het blob draagt de generatie van de kern die landt).
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Een klaargelegde flip: de nieuwe kern ligt gerelokeerd in de staging.
#[derive(Copy, Clone, Debug)]
struct Prepared {
    /// De maat van het platte beeld (8-uitgelijnd).
    len: u64,
    /// Het fysieke entrypoint op het koude adres.
    entry: u64,
    /// Waar het beeld in de staging ligt (`kernflip::stage_slot`).
    src: u64,
}

/// Waar een flip staat: de haak legt hem klaar, de flip-taak voert hem uit.
#[derive(Copy, Clone, Debug)]
enum Phase {
    /// Geen flip: een FLIP wordt getoetst en klaargelegd.
    Idle,
    /// Aangenomen (Hop kreeg zijn `Ok`), de flip-taak heeft hem nog niet.
    Pending(Prepared),
    /// De flip-taak vangt en springt; terug komt hij niet.
    Jumping,
}

/// De flip, van de haak naar de flip-taak. Beide draaien op de executor van
/// de OS-core. Alleen in [`Phase::Idle`] neemt de haak een FLIP aan: de
/// staging is er één (F19, 24-10).
static FLIP: Local<Cell<Phase>> = Local::new(Cell::new(Phase::Idle));
/// De bel van de flip-taak.
static BELL: Signal = Signal::new();
/// De antwoordplek van de flip-taak bij de lifecycle-actor (één aanroeper).
static REPLY: Reply = Reply::new();

/// Waar de flip zijn woorden houdt.
fn plan() -> FlipPlan {
    FlipPlan {
        facts_pa: FACTS_PA,
        handoff_pa: FLIP_HANDOFF_PA,
    }
}

/// De generatie van deze kern: 1 na een koude boot.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Relaxed)
}

/// Het beeld van deze kern: waar het begint (het koude adres, en dus waar
/// de nieuwe heen gaat) en tot waar een nieuw beeld mag reiken.
///
/// Per board een eigen module (handboek §7: `cfg` op module-niveau): een
/// vast linkadres, of op UEFI de basis die de firmware koos.
#[cfg(not(any(feature = "board-uefi", feature = "board-o6n", feature = "board-altra")))]
mod image {
    use vboard::slots::{FLIP_IMAGE_END, FLIP_LINK_BASE, FLIP_PIE};

    const _: () = assert!(!FLIP_PIE && FLIP_LINK_BASE < FLIP_IMAGE_END);

    /// Het koude adres: het linkadres van het board.
    pub(super) fn base() -> u64 {
        FLIP_LINK_BASE
    }

    /// Tot waar een nieuw beeld mag reiken.
    pub(super) fn limit() -> u64 {
        FLIP_IMAGE_END
    }

    /// Het tweede veegvenster: leeg, het beeld ligt in het kern-RAM.
    pub(super) fn sweep() -> (u64, u64) {
        (0, 0)
    }
}

#[cfg(any(feature = "board-uefi", feature = "board-o6n", feature = "board-altra"))]
mod image {
    use vboard::slots::FLIP_PIE;

    const _: () = assert!(FLIP_PIE);

    unsafe extern "C" {
        /// Het begin van het PE-image (`hopos/efi.ld`, de header): de basis
        /// die de firmware koos.
        safe static __efi_head: u8;
        /// Het einde van het image met BSS en stack: `SizeOfImage`, dus
        /// precies wat de firmware toen alloceerde.
        safe static __image_end: u8;
    }

    /// Het koude adres: de basis van deze kern.
    pub(super) fn base() -> u64 {
        &raw const __efi_head as usize as u64
    }

    /// Tot waar een nieuw beeld mag reiken: het einde van wat de firmware
    /// bij de koude boot alloceerde (`SizeOfImage` van díe kern, met de
    /// speling van hopos/efi.ld; de feitenpagina draagt het over een flip
    /// heen). Niet het einde van dít beeld: een geflipte kern heeft zijn
    /// eigen maat, en erachter ligt geheugen dat de firmware niet aan ons
    /// gaf en dat nu misschien van de pool is. Zonder feit het eigen einde.
    pub(super) fn limit() -> u64 {
        match vboard::facts::image_window() {
            Some((base, size)) => base.saturating_add(size),
            None => &raw const __image_end as usize as u64,
        }
    }

    /// Het tweede veegvenster: het beeld zelf, buiten het kernvenster.
    pub(super) fn sweep() -> (u64, u64) {
        (base(), limit())
    }
}

/// De DTB van de firmware, die een geflipte kern terug moet vinden.
///
/// Op een DTB-board (virt, de Pi's, de Radxa) draagt de feitenpagina alleen
/// het adres: de firmware legde de DTB buiten de kern-RAM en de pool (de Pi:
/// het laadvenster, `device_tree_address`; de Radxa: een gat in de pool;
/// virt: de eerste 2 MB van het RAM, onder het beeld). Dus toetst de oude
/// kern vóór de sprong dat daar nog een DTB staat: een nieuwe kern zonder
/// DTB heeft geen geheugenkaart, en dat is liever een weigering dan een kern
/// die na de sprong zonder pool verder moet. UEFI zet zijn feiten zelf in
/// een pagina (`board/uefi/src/flip.rs`).
mod dtb {
    /// De magic van een FDT-kop, big-endian in het geheugen.
    const FDT_MAGIC: u32 = 0xd00d_feed;

    /// Staat de DTB van de firmware er nog? x0 = 0 is een board dat zijn
    /// DTB op een vaste plek zoekt (QEMU met een ELF-kern), en dan is er
    /// niets over te dragen. Zonder DTB in x0 (`Board::DTB_IN_X0`: UEFI
    /// geeft er de ImageHandle, Apple de boot-args van m1n1, de LicheeRV
    /// wat de FSBL in a1 liet) altijd ja.
    pub(super) fn intact(x0: u64) -> bool {
        !<crate::Machine as board::Board>::DTB_IN_X0
            || x0 == 0
            || (x0.is_multiple_of(8) && u32::from_be(dev::read32(dev::Pa(x0))) == FDT_MAGIC)
    }
}

/// De feitenpagina, in `kmain` vóór `discover`: elke ingang geeft x0 door,
/// en dat is de pagina (een sprong, met de handoff van de vorige kern) of
/// wat de firmware gaf (stroom aan: de pagina komt er opnieuw, zonder
/// handoff; `kernflip::facts`). Geeft wat de firmware bij stroom aan gaf:
/// de DTB, de boot_args van Apple, op UEFI 0. riscv64 flipt niet en heeft
/// geen pagina ([`FLIPS`]).
pub(crate) fn facts(x0: u64) -> u64 {
    if !FLIPS {
        return x0;
    }
    let (fw, handoff) = kernflip::facts(&mut DevMem, &plan(), x0);
    FIRMWARE_X0.store(fw, Relaxed);
    HANDOFF.store(handoff, Relaxed);
    fw
}

/// Wat een landing aan `setup` geeft: per onderdeel zijn stuk.
pub(crate) struct Landing {
    /// Het hok: de bewoners om te adopteren, met hun poorten.
    pub(crate) slots: Vec<SlotState>,
    /// Het kooi-geheugen van het hok: de chunks met hun bezetting.
    pub(crate) cage: Vec<kern::cageheap::Chunk>,
    /// De lease van de uplink; `None`: DHCP.
    pub(crate) lease: Option<kernflip::Lease>,
}

/// De landing, als eerste in `setup` (in `kmain` gaan alleen de heap,
/// `discover` en de verhuizing naar de OS-core ervóór): een overdracht is
/// er, of niet. De klok neemt zijn stuk hier al over; de rest gaat in
/// [`Landing`] naar `setup`.
///
/// Een koude boot is een boot zonder handoff ([`facts`]). Een onbruikbare
/// handoff ná een sprong is GEEN koude boot: er leven misschien bewoners,
/// en een koude boot zou hun regio's vrij noemen. Dan `HOPOS_FATAL`: de
/// machine komt koud terug.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn land() -> Option<Landing> {
    let handoff = HANDOFF.load(Relaxed);
    // De kale vector van de trampoline: een fault tijdens de kopie of in de
    // eerste stappen van de nieuwe kern (Go 01-09). Na de reset komt de
    // oude kern koud terug (het blob dat de sprong achterliet is dan oud);
    // hij zegt het hier.
    if let Some((esr, elr, far)) = chain::take_trap(Pa(FLIP_TRAMP_PA)) {
        println!(
            "flip: the jump faulted before the new kernel had vectors: ESR {esr:#x} ELR {elr:#x} FAR {far:#x} HOPOS_FLIP_TRAP"
        );
    }
    match kernflip::adopted(&mut DevMem, &plan(), handoff) {
        Ok(h) => h.map(landed),
        Err(e) => cpu::fatal!(
            "flip: the handoff blob does not decode: {e}; resetting cold HOPOS_FLIP_BLOB_BAD"
        ),
    }
}

/// Een gelezen overdracht in stukken.
fn landed(h: Handoff) -> Landing {
    GENERATION.store(h.generation, Relaxed);
    crate::clock::restore(h.wall_off);
    let g = h.generation;
    println!(
        "flip: landed, generation {g}, {} resident(s), lease {} HOPOS_FLIP_BOOT gen={g}",
        h.slots.len(),
        core::net::Ipv4Addr::from(h.lease.map_or(0, |l| l.ip))
    );
    Landing {
        slots: h.slots,
        cage: h.cage,
        lease: h.lease,
    }
}

/// Na een landing: zodra het net luistert (`net::NET_READY`: lease,
/// uplink, stack en de system-listener) is de flip binnen. Komt het net
/// niet, dan wacht dit voorgoed en reset de ene guard van de watchdog de
/// node (docs/sequence-hopos.md §14.1 O1); tot 01-11 had de flip daarvoor
/// een eigen guard met een eigen klok.
async fn settle() {
    crate::net::NET_READY.wait().await;
    let generation = generation();
    println!("flip: generation {generation} settled, net up HOPOS_FLIP_SETTLED");
}

/// Waarom een bundel vóór de sprong geweigerd werd: een korte reden voor
/// achter de marker (`HOPOS_FLIP_REFUSED <reden>`) en de fout voor Hop.
struct Refused {
    why: &'static str,
    err: kern::Error,
}

impl From<kern::Error> for Refused {
    fn from(err: kern::Error) -> Self {
        Refused {
            why: "bundle invalid",
            err,
        }
    }
}

fn refuse(why: &'static str, err: kern::Error) -> Refused {
    Refused { why, err }
}

/// De haak van `PrivOp::FLIP`: toetst de bundel en legt de nieuwe kern
/// klaar. Synchroon: na `Ok` geeft de system-API het slot terug.
pub(crate) fn prepare(b: &FlipBundle, sha256: &[u8; 32]) -> kern::Result {
    prepare_inner(b, sha256).map_err(|r| {
        println!(
            "flip: bundle in slot {} ({} bytes): {}; this kernel keeps running HOPOS_FLIP_REFUSED {}",
            b.slot, b.len, r.err, r.why
        );
        r.err
    })
}

fn prepare_inner(b: &FlipBundle, sha256: &[u8; 32]) -> Result<(), Refused> {
    if !FLIPS {
        return Err(refuse("no flip on riscv64", kern::Error::NoEnt));
    }
    // Eerst: een flip die al aangenomen is, houdt de staging. Deze toets
    // leest geen byte van de bundel, dus hij mag vóór de som.
    if !matches!(FLIP.get().get(), Phase::Idle) {
        println!(
            "flip: a flip is already under way (accepted, not jumped and not given up); this one is refused, the staging stays HOPOS_FLIP_BUSY"
        );
        return Err(refuse("a flip is already under way", kern::Error::Busy));
    }
    let len = usize::try_from(b.len).map_err(|_| kern::Error::TooLarge {
        len: usize::MAX,
        max: 0,
    })?;
    // SAFETY: `[base, base+len)` is de partitie van een stroom die de
    // system-API tijdens deze aanroep in handen houdt (de grant is buiten
    // de actor: er draait niets in en niemand anders schrijft erin), en de
    // lengte is wat er gestroomd is, binnen de partitie (`Placer::raw`).
    // Het geheugen is pool-RAM, Normal gemapt in de identity map. Alleen
    // lezen, en de slice leeft niet langer dan deze functie.
    let bytes = unsafe { core::slice::from_raw_parts(b.base as usize as *const u8, len) };
    let sum = abi::sha256::digest(bytes);
    if &sum != sha256 {
        return Err(refuse(
            "sha256 mismatch",
            kern::Error::Version {
                have: kernflip::sum64(&sum),
                want: kernflip::sum64(sha256),
            },
        ));
    }
    let bundle = Bundle::parse(bytes)?;
    if bundle.flip_abi != FLIP_ABI {
        return Err(refuse(
            "flip ABI mismatch",
            kern::Error::Version {
                have: u64::from(bundle.flip_abi),
                want: u64::from(FLIP_ABI),
            },
        ));
    }
    check_switch_code(&bundle)?;
    let x0 = FIRMWARE_X0.load(Relaxed);
    if !dtb::intact(x0) {
        return Err(refuse(
            "firmware DTB gone",
            kern::Error::Corrupt {
                at: usize::try_from(x0).unwrap_or(usize::MAX),
            },
        ));
    }
    let base = image::base();
    let flat = bundle.flat_size.next_multiple_of(8);
    // Het beeld moet op het koude adres onder de grens van het board
    // blijven (virt: de DMA-regio, waar de NIC schrijft tot de nieuwe kern
    // hem reset; UEFI: het einde van het oude beeld).
    let room = image::limit().saturating_sub(base);
    if flat > room {
        return Err(too_large(flat, room));
    }
    let src = stage_slot(flat)?;
    let segs = kernflip::flatten(&bundle, &mut DevMem, src)?;
    let delta = base.wrapping_sub(bundle.link_load);
    let relocs = chain::relocate(Pa(src), flat, bundle.relocs(), delta).map_err(|e| {
        println!("flip: {e} HOPOS_FLIP_RELOC");
        refuse("relocation", kern::Error::Corrupt { at: 0 })
    })?;
    // De config gaat mee: het venster in het image (`board::cfgwin`) is
    // het configbestand van de node, en de nieuwe kern gaat over dit image
    // heen. Een bundel met een gevuld venster (`CFG=` van
    // image/flip-bundle.sh, of `hop image`) houdt het zijne.
    // `view_mut` leent het gestagede beeld: de flip legde het net plat neer
    // (Normal gemapt), en tot de sprong schrijft alleen de flip-taak erin.
    let carried = usize::try_from(flat).map_or(Carry::NoWindow, |n| {
        dev::view_mut(Pa(src), n, |img| {
            board::cfgwin::carry(board::cfgwin::bytes(), img)
        })
    });
    match carried {
        Carry::Done => println!("flip: hopos.cfg carried into the new image HOPOS_FLIP_CFG"),
        Carry::Own => println!(
            "flip: the bundle carries its own hopos.cfg in its window, ours stays behind HOPOS_FLIP_CFG_OWN"
        ),
        Carry::NoWindow if !board::cfgwin::text().is_empty() => println!(
            "flip: the new image has no config window, our hopos.cfg stays behind HOPOS_FLIP_CFG_NONE"
        ),
        Carry::NoWindow | Carry::Nothing => {}
    }
    // Apple: daarnaast de plek 0xF000 (de loader, of het venster van een
    // kern van vóór `board::cfgwin`), zoals voorheen.
    if BOARD.flip_carry_config(src, flat) {
        println!("flip: the 0xF000 hopos.cfg carried into the new image HOPOS_FLIP_CFG");
    }
    let entry = bundle.entry.wrapping_add(delta);
    println!(
        "flip: bundle ok, sha256 verified: {} KiB image in {segs} segment(s) linked at {:#x}, {relocs} relocation(s) to {base:#x}, entry {entry:#x}, staged at {src:#x} HOPOS_FLIP_STAGED",
        flat >> 10,
        bundle.link_load,
    );
    let prepared = Prepared {
        len: flat,
        entry,
        src,
    };
    // De indeling van de sprong nu, vóór de 202: daarna is "nee" geen
    // antwoord meer maar `HOPOS_FATAL` (§13.7).
    jump_of(&prepared).check().map_err(|e| {
        println!("flip: {e} HOPOS_FLIP_LAYOUT");
        refuse("jump layout", kern::Error::Corrupt { at: 0 })
    })?;
    FLIP.set(Phase::Pending(prepared));
    BELL.set();
    Ok(())
}

fn too_large(flat: u64, max: u64) -> Refused {
    refuse(
        "image too large",
        kern::Error::TooLarge {
            len: usize::try_from(flat).unwrap_or(usize::MAX),
            max: usize::try_from(max).unwrap_or(0),
        },
    )
}

/// `slots::staged_hop` als (begin, maat), voor de plaats van het nieuwe
/// beeld.
fn staged_elf() -> Option<(u64, u64)> {
    crate::slots::staged_hop().map(|img| (img.as_ptr() as u64, img.len() as u64))
}

/// Waar het nieuwe beeld in de staging gaat: achter het gestagede image
/// (`kernflip::stage_slot`), zodat een herstart van Hop op kern B het nog
/// vindt. Past het er niet naast, dan luid eroverheen: Hop draait door, en
/// alleen een herstart met hetzelfde image kan dan niet meer.
fn stage_slot(flat: u64) -> Result<u64, Refused> {
    if let Some(at) = kernflip::stage_slot(STAGE_PA, STAGE_MAX, flat, staged_elf()) {
        return Ok(at);
    }
    let at = kernflip::stage_slot(STAGE_PA, STAGE_MAX, flat, None)
        .ok_or_else(|| too_large(flat, STAGE_MAX))?;
    println!(
        "flip: the staging holds the new kernel ({} KiB) but not next to the staged image; it goes over it, a restart of Hop then needs a new image HOPOS_FLIP_STAGE_SHARED",
        flat >> 10
    );
    Ok(at)
}

/// De som van de switch-code van de bundel tegen die van de geïnstalleerde
/// kopie. De bewoners draaien IN die kopie; de nieuwe kern adopteert haar
/// alleen bij een gelijke som (`cpu::el2::adopt`). Een verschil hier is dus
/// een weigering vóór de sprong, niet twee minuten later een koude herstart
/// door de guard. Zonder geïnstalleerde kopie (geen slots op dit board) is
/// er niets te adopteren en geldt elke som.
fn check_switch_code(bundle: &Bundle<'_>) -> Result<(), Refused> {
    let theirs = bundle.switch_sum;
    let Some(ours) = crate::slots::os_plan()
        .ok()
        .and_then(|p| cpu::el2::installed_hash(&p))
    else {
        return Ok(());
    };
    if theirs != ours {
        return Err(refuse(
            "switch code mismatch",
            kern::Error::Version {
                have: theirs,
                want: ours,
            },
        ));
    }
    println!("flip: switch code {ours:#018x} matches the residents HOPOS_FLIP_SWITCHCODE_OK");
    Ok(())
}

/// Spawnt de flip-taak en, na een landing, [`settle`]. Aangeroepen vóór de
/// executor draait.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor, landed: bool) {
    if let Err(e) = exec.spawn(run(exec)) {
        println!("flip: task not spawned: {e:?} HOPOS_FLIP_SPAWN");
    }
    if landed && let Err(e) = exec.spawn(settle()) {
        println!("flip: settle not spawned: {e:?} HOPOS_FLIP_SPAWN");
    }
}

/// De flip-taak: wacht op een klaargelegde flip en springt. Na de 202 is
/// er geen weg terug (§13.7): een vangst of sprong die faalt, is
/// `HOPOS_FATAL`, en de watchdog brengt de node koud terug.
async fn run(exec: &'static Executor) {
    loop {
        BELL.wait().await;
        let p = match FLIP.replace(Phase::Jumping) {
            Phase::Pending(p) => p,
            other => {
                FLIP.set(other);
                continue;
            }
        };
        exec.after(JUMP_DELAY).await;
        let e = capture_and_jump(exec, p).await;
        cpu::fatal!("flip: not jumped after the 202: {e} HOPOS_FLIP_FAIL");
    }
}

/// Waarom de sprong niet doorging.
enum JumpError {
    Kern(kern::Error),
    Chain(chain::ChainError),
}

impl core::fmt::Display for JumpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Kern(e) => write!(f, "{e}"),
            Self::Chain(e) => write!(f, "{e}"),
        }
    }
}

/// Vangen, overdragen en springen. Keert alleen terug met een fout.
async fn capture_and_jump(exec: &'static Executor, p: Prepared) -> JumpError {
    // 1. De opslag: vastleggen en dicht. Wat na deze commit geschreven zou
    //    worden, ziet de nieuwe kern niet, dus er wordt niets meer
    //    geschreven.
    if let Err(e) = crate::storage::freeze_for_flip(exec, ACTOR_WAIT).await {
        return JumpError::Kern(e);
    }
    // 2. De USB-controllers stil: DMA-masters op de RP1 die anders door de
    //    link-reset van de nieuwe kern heen zouden schrijven (30-09, de Pi
    //    5).
    crate::usb::quiesce(exec).await;
    // 3. Het laatste `.await` van deze kern: het hok.
    let (slots, cage) = match kern::slots::call(&crate::LIFECYCLE, &REPLY, Request::Snapshot).await
    {
        Ok(Response::Snapshot(s, c)) => (s, c),
        Ok(Response::Failed(e)) | Err(e) => return JumpError::Kern(e),
        Ok(_) => return JumpError::Kern(kern::Error::Busy),
    };
    // 4. De lease, zoals de keeper hem nu houdt.
    let h = Handoff {
        generation: generation() + 1,
        wall_off: crate::clock::offset(),
        lease: crate::net::lease(),
        slots,
        cage,
    };
    handoff_and_jump(p, &h)
}

/// De sprong van een klaargelegde flip: het beeld uit de staging naar het
/// koude adres, met de kern-RAM en (UEFI) het beeld als veegvensters.
fn jump_of(p: &Prepared) -> Jump {
    let ram = BOARD.plan().kern_ram;
    let (a, b) = image::sweep();
    Jump {
        dst: Pa(image::base()),
        src: Pa(p.src),
        len: p.len,
        entry: p.entry,
        x0: FACTS_PA,
        sweep: [(ram.base, ram.end()), (Pa(a), Pa(b))],
        tramp: Pa(FLIP_TRAMP_PA),
    }
}

/// Het blob, de handoff en de sprong. Synchroon: vanaf hier verandert er
/// niets meer aan de staat die overgaat.
fn handoff_and_jump(p: Prepared, h: &Handoff) -> JumpError {
    // De klok vol: de nieuwe kern landt niet op een stille klok (30-09, de
    // Pi 5: twee flips vanuit 800 MHz met een NIC die nooit meer meldde).
    crate::telemetry::clock_full_for_flip();
    // En de watchdog vol: de nieuwe kern krijgt de hele 12 s voor zijn boot.
    crate::watchdog::pet_now();
    // UEFI: de TRNG achter de firmware (`hopos.efirng=1`) is na de koude
    // boot weg, dus krijgt de nieuwe kern vers zaad uit onze DRBG, anders
    // zaait hij uit jitter (board/uefi/src/flip.rs). De Pi's en de Radxa
    // proberen hun TRNG zelf opnieuw, in `discover`.
    if BOARD.flip_carry_seed() {
        println!(
            "flip: 64 bytes of seed from the kernel DRBG (efi-rng) for the new kernel HOPOS_FLIP_SEED"
        );
    }
    // Koud of warm: het blob zegt het, de rest van de weg is dezelfde.
    let generation = h.generation;
    let (mut mem, fp) = (DevMem, plan());
    let base = image::base();
    let blob = match kernflip::encode(h, HANDOFF_TAIL) {
        Ok(b) => b,
        Err(e) => return JumpError::Kern(e),
    };
    mem.clear(FLIP_HANDOFF_PA, FLIP_HANDOFF_LEN);
    mem.copy_in(FLIP_HANDOFF_PA, &blob);
    kernflip::hand_over(&mut mem, &fp, FLIP_HANDOFF_PA);
    println!(
        "flip: {} KiB to {base:#x}, {} resident(s) and a {} B handoff, jumping to {:#x} HOPOS_FLIP_JUMP gen={generation}",
        p.len >> 10,
        h.slots.len(),
        blob.len(),
        p.entry
    );
    let j = jump_of(&p);
    // SAFETY: dit is de flip-taak op de OS-core, en na de sprong hoeft hier
    // niets meer te gebeuren. De staging draagt op `p.src` het complete
    // beeld van een bundel waarvan de som getoetst is (`prepare`),
    // gerelokeerd naar het koude adres; niemand schrijft er nog in (de
    // firmware of QEMU legde er alleen bij de boot iets neer, en dat ligt
    // ernaast, `kernflip::stage_slot`). Het blob en de feitenpagina zijn
    // net naar DRAM geveegd; hopfs is vastgelegd en dicht. De
    // app-cores draaien in hun partities en in de switch-code in de
    // plan-regio, nooit in het kern-RAM of het kern-beeld dat de trampoline
    // veegt en overschrijft.
    JumpError::Chain(unsafe { chain::chain(&j) })
}
