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
//! | net | `net::capture`: lease en conntrack | `net::start` met [`Landing::net`]: de switch begint met de uplink, de poorten en de flows, de host-taak slaat DHCP over (H1) |
//! | hok | `Request::Snapshot` | `slots::start` met [`Landing::slots`]: adopteren, of leeg beginnen |
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
//!    dicht, de lease en de conntrack (de masquerade gaat dicht), de USB
//!    stil, en de bewoners. Dan het handoff-blob, het pointer/magic-paar, de recorder op
//!    "springen", en de sprong (`cpu::el2::chain::chain`). Na de 202 is er
//!    geen weg terug (§13.7): wat hier misgaat, is `HOPOS_FATAL` en de
//!    koude boot.
//! 4. De nieuwe kern: [`land`] als eerste in `setup`. Een overdracht is
//!    `HOPOS_FLIP_BOOT gen=N` en een [`Landing`]; `setup` geeft elk stuk
//!    aan zijn onderdeel. Een stuk dat niet terugkomt, is `HOPOS_FATAL`. De
//!    guard ([`guard`]) eist binnen [`GRACE`] een luisterend net.
//!
//! De adressen zijn die van het board (`vboard::slots::FLIP_*`): de
//! staging, de recorder, de trampoline en het blob liggen buiten het
//! kern-RAM en buiten de pool. Het koude adres is het linkadres van het
//! board, behalve op UEFI: daar is de kern een PIE die de firmware ergens
//! neerlegde, en gaat de nieuwe kern op precies die basis (`__efi_head`),
//! binnen de maat die de firmware toen gaf.
//!
//! De flip heeft ÉÉN trigger: `PrivOp::FLIP` van Hop (`POST /flip` achter
//! dezelfde HMAC als een jobspec). Alleen warm (Derek, 01-11): een bundel
//! met een andere switch-code is een weigering vóór de 202. Beleid (wanneer,
//! en waarvandaan) is van Hop; de kern is mechanisme.
//!
//! # De zwarte doos
//!
//! Elke consoleregel (de tee van `conport.rs`) gaat ook naar een ring van
//! 16 KiB op een vaste plek naast de recorder ([`black_box`], de vorm in
//! `kern::kernflip::box_open`). Een koude boot leest hem vóór hij er zelf in
//! schrijft en drukt de staart af na `HOPOS_FLIP_LAST`
//! (`HOPOS_FLIP_BLACKBOX`); een landende kern begint een verse doos met zijn
//! generatie.
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
use abi::layout::{FLIP_HANDOFF_LEN, HANDOFF_MAGIC_OFF, HANDOFF_PTR_OFF};
use alloc::vec::Vec;
use board::Board;
use board::cfgwin::Carry;
use core::cell::Cell;
use core::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use core::time::Duration;
use cpu::el2::chain::{self, Jump};
use cpu::println;
use dev::Pa;
use executor::Executor;
use kern::cage::PhysMem;
use kern::kernflip::{
    self, BOX_SHOW, Boot, Bundle, FLIP_ABI, FlipPlan, HAND_MAGIC, HANDOFF_TAIL, Handoff, Stage,
};
use kern::slots::{Reply, Request, Response, SlotState};
use kern::system::FlipBundle;
use sync::{Either, Local, Signal, select};
use vboard::slots::{
    BOOT_SCRATCH_PA, FLIP_RECORDER_PA, FLIP_TRAMP_PA, STAGE_HDR_PA, STAGE_MAX, STAGE_PA,
};

/// Het handoff-blob: direct onder het staging-maatwoord, op elk board.
const FLIP_HANDOFF_PA: u64 = abi::layout::flip_handoff_pa(STAGE_HDR_PA);

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

/// De gratie van een geflipte kern: binnen deze tijd moet zijn net
/// luisteren, anders komt de node koud terug. Twee minuten, de les van de
/// Go-kern. Tijdelijk: de ene boot-guard van de watchdog (O1) neemt dit over.
pub(crate) const GRACE: Duration = Duration::from_secs(120);

/// Kan deze kern flippen? Op riscv64 niet: de switch-code draait daar uit
/// het kern-image, en de nieuwe kern zou hem onder de app-harts
/// overschrijven. Wat ontbreekt: een kopie van de switcher buiten het image
/// (zoals de plan-regio op arm64), met `mtvec` van elk app-hart daarop, en
/// een `adopt` in `cage_riscv`.
const FLIPS: bool = !cfg!(target_arch = "riscv64");

/// Wat de firmware deze kern in x0 gaf: de nieuwe krijgt hetzelfde.
static FIRMWARE_X0: AtomicU64 = AtomicU64::new(0);
/// De generatie van deze kern: 1 na een koude boot, N+1 na een flip vanaf
/// generatie N (het blob draagt de generatie van de kern die landt).
static GENERATION: AtomicU64 = AtomicU64::new(1);
/// De zwarte doos staat open: vanaf nu gaat elke consoleregel erin. Pas na
/// de landing (zie [`land`]): wat er nog in staat, is van een vorige boot.
static BOX_OPEN: AtomicBool = AtomicBool::new(false);

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
        handoff_ptr_pa: BOOT_SCRATCH_PA + HANDOFF_PTR_OFF,
        stage_pa: FLIP_RECORDER_PA,
        own_ram_end: FLIP_HANDOFF_PA,
        black_box_pa: black_box::PA,
    }
}

/// De zwarte doos van de console: de plek van het board
/// (`vboard::slots::BLACK_BOX`), naast de recorder en met dezelfde
/// levensduur: buiten het kernimage, de staging, de DTB, de pool en de DMA,
/// en nooit iets wat de firmware of de lader bij een verse boot beschrijft.
/// Op virt, de Pi's en UEFI de 32 KiB direct onder het handoff-blob; op de
/// Radxa en de M4 de doos van hun plan (van de Go-kern overgenomen, net als
/// hun recorder buiten de boot-scratch: op de M4 legt iBoot het bootobject
/// bij elke boot terug over de scratch, 01-09). Device-gemapt is het vegen
/// overbodig maar onschadelijk. Maat 0 (riscv64): geen doos.
mod black_box {
    use kern::kernflip::BOX_LEN;
    use vboard::slots::BLACK_BOX;

    /// Het begin van de doos; 0 = geen.
    pub(super) const PA: u64 = if BLACK_BOX.size == 0 {
        0
    } else {
        BLACK_BOX.base
    };

    // De doos past (de maat min BOX_LEN bestaat) en ligt op 64 bytes.
    const _: () = assert!(
        BLACK_BOX.size == 0
            || (BLACK_BOX.size.checked_sub(BOX_LEN).is_some() && PA.is_multiple_of(64))
    );
}

/// De tee van de console (`conport::tee`): de bytes ook in de zwarte doos,
/// zodra die open is. Onder het console-slot, dus één schrijver tegelijk;
/// een noodregel na de grens van dat slot kan een byte verminken, en een
/// verminkte byte in een post-mortem is beter dan geen post-mortem (Go).
pub(crate) fn black_box(b: &[u8]) {
    if BOX_OPEN.load(Acquire) {
        kernflip::box_write(&mut DevMem, &plan(), b);
    }
}

/// Begint een verse doos voor deze kern en zet de tee erop.
fn open_box(generation: u64) {
    let p = plan();
    if p.black_box_pa == 0 {
        return;
    }
    kernflip::box_open(&mut DevMem, &p, generation);
    BOX_OPEN.store(true, Release);
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

/// De feiten van de firmware die een geflipte kern terug moet vinden.
///
/// Op een DTB-board (virt, de Pi's, de Radxa) zijn dat er geen andere dan
/// de DTB zelf: de firmware legde hem buiten de kern-RAM en de pool (de Pi:
/// het laadvenster, `device_tree_address`; de Radxa: een gat in de pool;
/// virt: de eerste 2 MB van het RAM, onder het beeld), en de flip geeft
/// dezelfde x0 door. Dus toetst de oude kern vóór de sprong dat daar nog
/// een DTB staat: een nieuwe kern zonder DTB heeft geen geheugenkaart, en
/// dat is liever een weigering dan een kern die na de sprong zonder pool
/// verder moet. UEFI heeft een eigen feitenpagina (`board/uefi/src/flip.rs`);
/// daar en op Apple toetst de ingang zelf.
mod facts {
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

/// Kwam deze kern uit een sprong? Op arm64 zegt de ingang het
/// (`cpu::boot::FLIP_ENTERED`: x3 van de trampoline, op UEFI de
/// flip-ingang). Op riscv64 is er geen merkteken (de trampoline geeft a0 = 0,
/// en de firmware a0 = het hart, op hart 0 ook 0): daar blijft het paar het
/// bewijs, en het wissen ervan. Daar bestaat ook alleen de koude flip, en een
/// oud koud blob boot hoe dan ook koud.
pub(crate) fn jumped() -> bool {
    cfg!(target_arch = "riscv64") || cpu::boot::FLIP_ENTERED.load(Relaxed)
}

/// Wat een landing aan `setup` geeft: per onderdeel zijn stuk.
pub(crate) struct Landing {
    /// Het hok: de bewoners om te adopteren.
    pub(crate) slots: Vec<SlotState>,
    /// Het net: de lease, de poorten van de bewoners en hun conntrack.
    pub(crate) net: crate::net::Carried,
}

/// De landing, als eerste in `setup` (in `kmain` gaan alleen de heap,
/// `discover` en de verhuizing naar de OS-core ervóór): een overdracht is
/// er, of niet. De klok neemt zijn stuk hier al over; de rest gaat in
/// [`Landing`] naar `setup`.
///
/// Alleen na een sprong ([`jumped`]): een firmware-boot die nog een paar
/// vindt, wist het en boot koud (`HOPOS_FLIP_STALE`). Na een harde reset
/// draaien de bewoners niet meer, en het blob is van een kern die al landde
/// of al dood is.
///
/// Een onbruikbaar blob met een geldig paar ná een sprong is GEEN koude
/// boot: er leven misschien bewoners, en een koude boot zou hun regio's vrij
/// noemen. Dan `HOPOS_FATAL`: de machine komt koud terug.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn land(x0: u64) -> Option<Landing> {
    FIRMWARE_X0.store(x0, Relaxed);
    let (mut mem, p) = (DevMem, plan());
    let jumped = jumped();
    if jumped {
        kernflip::mark_early_boot(&mut mem, &p);
    }
    // De kale vector van de trampoline: een fault tijdens de kopie of in de
    // eerste stappen van de nieuwe kern (Go 01-09). Na de reset komt de
    // oude kern koud terug (het blob dat de sprong achterliet is dan oud);
    // hij zegt het hier.
    if let Some((esr, elr, far)) = chain::take_trap(Pa(FLIP_TRAMP_PA)) {
        println!(
            "flip: the jump faulted before the new kernel had vectors: ESR {esr:#x} ELR {elr:#x} FAR {far:#x} HOPOS_FLIP_TRAP"
        );
    }
    match kernflip::adopted(&mut mem, &p, jumped) {
        Ok(Boot::Adopted(h)) => Some(landed(h)),
        Ok(b @ (Boot::Cold | Boot::Stale)) => {
            if b == Boot::Stale {
                println!(
                    "flip: a handoff lay in memory, but this kernel came from the firmware, not from a jump: wiped, not adopted, cold boot HOPOS_FLIP_STALE"
                );
            }
            report_cold(&mut mem, &p);
            // Pas na het lezen: anders drukt een koude kern zijn eigen
            // regels af als die van een dode.
            open_box(generation());
            None
        }
        // De doos blijft dicht: wat erin staat (de kern die sprong) is het
        // spoor voor de koude boot hierna.
        Err(e) => crate::fatal(format_args!(
            "flip: the handoff blob does not decode: {e}; resetting cold HOPOS_FLIP_BLOB_BAD"
        )),
    }
}

/// Een gelezen overdracht in stukken.
fn landed(h: Handoff) -> Landing {
    GENERATION.store(h.generation, Relaxed);
    // De doos van de kern die sprong gaat weg: die leeft niet meer, maar
    // stierf ook niet. Vanaf hier is hij van ons.
    open_box(h.generation);
    crate::clock::restore(h.wall_off);
    let g = h.generation;
    let lease = h.lease.map_or(0, |l| l.ip);
    println!(
        "flip: landed, generation {g}, {} resident(s), {} NAT flow(s), lease {} HOPOS_FLIP_BOOT gen={g}",
        h.slots.len(),
        h.nat.flows.len(),
        core::net::Ipv4Addr::from(lease)
    );
    let mut ports = Vec::new();
    for st in &h.slots {
        for &port in &st.ports {
            if ports.try_reserve(1).is_err() {
                crate::fatal(format_args!("flip: out of memory for the carried ports"));
            }
            ports.push((st.slot, port));
        }
    }
    Landing {
        slots: h.slots,
        net: crate::net::Carried {
            lease: h.lease,
            ports,
            nat: h.nat,
        },
    }
}

/// Een koude boot vertelt wat een eerdere flip achterliet: de laatste
/// poging die niet landde, en de lopende stand als de machine midden in
/// een flip omviel.
fn report_cold(mem: &mut DevMem, p: &FlipPlan) {
    if let Some((s, g)) = kernflip::take_archived(mem, p) {
        println!(
            "flip: an earlier flip (generation {g}) stopped at: {} HOPOS_FLIP_ARCHIVED",
            s.describe()
        );
    }
    if let Some((s, g)) = kernflip::take_last_flip(mem, p) {
        println!(
            "flip: the last flip (generation {g}) ended cold at: {} HOPOS_FLIP_LAST",
            s.describe()
        );
    }
    report_black_box(mem, p);
}

/// De zwarte doos van de vorige boot: de staart (hoogstens [`BOX_SHOW`]
/// bytes), regel voor regel met `  | ` ervoor, tussen een kop- en een
/// slotregel, en daarna leeg. Is er niets, dan zegt één regel dat (Go:
/// "black box: 0 bytes carried over"): anders is "geen post-mortem" niet te
/// onderscheiden van "de doos is onderweg gewist".
fn report_black_box(mem: &mut DevMem, p: &FlipPlan) {
    if p.black_box_pa == 0 {
        println!("flip: this board has no black box HOPOS_FLIP_BLACKBOX_NONE");
        return;
    }
    let mut buf = [0u8; BOX_SHOW];
    let Some(t) = kernflip::box_take(mem, p, &mut buf) else {
        println!(
            "flip: the black box at {:#x} carried nothing over HOPOS_FLIP_BLACKBOX_EMPTY",
            p.black_box_pa
        );
        return;
    };
    let g = t.generation;
    println!(
        "flip: the console of the dead kernel (generation {g}), last {} bytes HOPOS_FLIP_BLACKBOX",
        t.len
    );
    let tail = buf.get_mut(..t.len).unwrap_or_default();
    // Alleen afdrukbaar ASCII: de console van een stervende kern kan alles
    // bevatten, de regels hier niet. Een `\r` wordt een spatie, die valt
    // aan het eind van de regel weg.
    for c in tail.iter_mut() {
        if *c == b'\r' {
            *c = b' ';
        } else if !(c.is_ascii_graphic() || *c == b' ' || *c == b'\n') {
            *c = b'.';
        }
    }
    let mut tail: &[u8] = tail;
    // Een afgekapte staart begint midden in een regel: die halve regel weg.
    if t.written > t.len as u64
        && let Some(i) = tail.iter().position(|c| *c == b'\n')
    {
        tail = tail.get(i + 1..).unwrap_or_default();
    }
    let tail = tail.strip_suffix(b"\n").unwrap_or(tail);
    for line in tail.split(|c| *c == b'\n') {
        if let Ok(text) = core::str::from_utf8(line.trim_ascii_end()) {
            println!("  | {text}");
        }
    }
    println!(
        "flip: end of the console of the dead kernel (generation {g}, {} bytes written) HOPOS_FLIP_BLACKBOX_END",
        t.written
    );
}

/// De flip-boot-guard: een geflipte kern moet binnen [`GRACE`] een
/// luisterend net hebben (`net::NET_READY`: lease, uplink, stack en de
/// system-listener). Zo niet, dan `HOPOS_FATAL`: een halve kern met levende
/// bewoners is erger dan een koude boot. Lukt het, dan gaat de recorder
/// leeg (een latere koude boot is geen mislukte flip). Een adoptie die
/// faalt, wacht niet op de guard: die is meteen `HOPOS_FATAL`.
///
/// Tijdelijk: de ene boot-guard van 120 s in de watchdog (O1) neemt dit
/// over, voor elke boot.
async fn guard(exec: &'static Executor) {
    let generation = generation();
    if let Either::Right(()) = select(crate::net::NET_READY.wait(), exec.after(GRACE)).await {
        crate::fatal(format_args!(
            "flip: generation {generation} has no listening net within {} s, resetting cold HOPOS_FLIP_GUARD",
            GRACE.as_secs()
        ));
    }
    let (mut mem, p) = (DevMem, plan());
    kernflip::stage(&mut mem, &p, Stage::NetUp, generation);
    let _ = kernflip::take_last_flip(&mut mem, &p);
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
    if !facts::intact(x0) {
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
    let generation = generation() + 1;
    let (mut mem, p) = (DevMem, plan());
    kernflip::archive_stage(&mut mem, &p, generation);
    kernflip::stage(&mut mem, &p, Stage::Fetched, generation);
    kernflip::stage(&mut mem, &p, Stage::BundleOk, generation);
    let segs = kernflip::flatten(&bundle, &mut mem, src)?;
    kernflip::stage(&mut mem, &p, Stage::Placed, generation);
    let delta = base.wrapping_sub(bundle.link_load);
    let relocs = chain::relocate(Pa(src), flat, bundle.relocs(), delta).map_err(|e| {
        println!("flip: {e} HOPOS_FLIP_RELOC");
        refuse("relocation", kern::Error::Corrupt { at: 0 })
    })?;
    kernflip::stage(&mut mem, &p, Stage::Rebased, generation);
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

/// Het gestagede image van Hop zolang het nog een ELF is: na een warme
/// flip die er zelf overheen moest (`HOPOS_FLIP_STAGE_SHARED`) is het dat
/// niet meer, en is er geen Hop om koud te starten of te herstarten. De
/// supervisor van Hop (`slots`) vraagt het bij elke plaatsing opnieuw, want
/// een flip die klaargelegd werd en niet sprong, kan het al overschreven
/// hebben.
pub(crate) fn staged_hop() -> Option<&'static [u8]> {
    let img = vboard::slots::staged_image()?;
    (img.get(..4) == Some(b"\x7fELF".as_slice())).then_some(img)
}

/// [`staged_hop`] als (begin, maat), voor de plaats van het nieuwe beeld.
fn staged_elf() -> Option<(u64, u64)> {
    staged_hop().map(|img| (img.as_ptr() as u64, img.len() as u64))
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
    let Some(theirs) = bundle.switch_sum else {
        return Err(refuse(
            "bundle carries no switch code sum",
            kern::Error::Version { have: 0, want: 2 },
        ));
    };
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

/// Spawnt de flip-taak en, na een landing, de guard. Aangeroepen vóór de
/// executor draait.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor, landed: bool) {
    if let Err(e) = exec.spawn(run(exec)) {
        println!("flip: task not spawned: {e:?} HOPOS_FLIP_SPAWN");
    }
    if landed && let Err(e) = exec.spawn(guard(exec)) {
        println!("flip: guard not spawned: {e:?}, no grace check HOPOS_FLIP_SPAWN");
    }
}

/// De flip-taak: wacht op een klaargelegde flip en springt. Na de 202 is
/// er geen weg terug (§13.7): een vangst of sprong die faalt, is
/// `HOPOS_FATAL`, en de koude boot meldt de stap (`HOPOS_FLIP_LAST`).
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
        crate::fatal(format_args!(
            "flip: not jumped after the 202: {e} HOPOS_FLIP_FAIL"
        ));
    }
}

/// Waarom de sprong niet doorging.
enum JumpError {
    Kern(kern::Error),
    Net(&'static str),
    Chain(chain::ChainError),
}

impl core::fmt::Display for JumpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Kern(e) => write!(f, "{e}"),
            Self::Net(why) => write!(f, "net: {why}"),
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
    // 2. Het net: de lease en de conntrack, als waarde terug van de
    //    switch-actor (die zet de masquerade dicht).
    let (lease, mut nat) = match crate::net::capture(exec, ACTOR_WAIT).await {
        Ok(c) => c,
        Err(why) => return JumpError::Net(why),
    };
    // 3. De USB-controllers stil: DMA-masters op de RP1 die anders door de
    //    link-reset van de nieuwe kern heen zouden schrijven (30-09, de Pi
    //    5).
    crate::gui::quiesce_usb(exec).await;
    // 4. Het laatste `.await` van deze kern: het hok.
    let slots = match kern::slots::call(&crate::LIFECYCLE, &REPLY, Request::Snapshot).await {
        Ok(Response::Snapshot(s)) => s,
        Ok(Response::Failed(e)) | Err(e) => return JumpError::Kern(e),
        Ok(_) => return JumpError::Kern(kern::Error::Busy),
    };
    // Alleen de flows van een bewoner die meegaat: de nieuwe switch neemt
    // de rest dan niet over.
    let resident = |slot: u8| slots.iter().any(|st| st.slot == usize::from(slot));
    nat.flows.retain(|f| resident(f.slot));
    println!(
        "flip: conntrack captured, {} NAT flow(s), outbound mapping closed until the jump HOPOS_FLIP_NAT_CAPTURED flows={}",
        nat.flows.len(),
        nat.flows.len()
    );
    let h = Handoff {
        generation: generation() + 1,
        wall_off: crate::clock::offset(),
        lease,
        slots,
        nat,
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
        x0: FIRMWARE_X0.load(Relaxed),
        sweep: [(ram.base, ram.end()), (Pa(a), Pa(b))],
        tramp: Pa(FLIP_TRAMP_PA),
    }
}

/// Het blob, het paar en de sprong. Synchroon: vanaf hier verandert er
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
    kernflip::stage(&mut mem, &fp, Stage::Captured, generation);
    let base = image::base();
    let blob = match kernflip::encode(h, HANDOFF_TAIL) {
        Ok(b) => b,
        Err(e) => return JumpError::Kern(e),
    };
    mem.clear(FLIP_HANDOFF_PA, FLIP_HANDOFF_LEN);
    mem.copy_in(FLIP_HANDOFF_PA, &blob);
    // Het paar als laatste, en de regel zelf naar DRAM: hij deelt de
    // boot-scratch met woorden die straks met de MMU uit geschreven worden.
    let pair = BOOT_SCRATCH_PA + HANDOFF_PTR_OFF;
    mem.write64(pair, FLIP_HANDOFF_PA);
    mem.write64(BOOT_SCRATCH_PA + HANDOFF_MAGIC_OFF, HAND_MAGIC);
    dev::push(Pa(pair), 16);
    kernflip::stage(&mut mem, &fp, Stage::Handoff, generation);
    println!(
        "flip: {} KiB to {base:#x}, {} resident(s), {} NAT flow(s) and a {} B handoff, jumping to {:#x} HOPOS_FLIP_JUMP gen={generation}",
        p.len >> 10,
        h.slots.len(),
        h.nat.flows.len(),
        blob.len(),
        p.entry
    );
    kernflip::stage(&mut mem, &fp, Stage::Jumping, generation);
    // Wat de pomp van de UART nog had, gaat er nu wachtend uit: na de
    // sprong is die pomp weg (de zwarte doos houdt het toch).
    crate::conport::flush();
    let j = jump_of(&p);
    // SAFETY: dit is de flip-taak op de OS-core, en na de sprong hoeft hier
    // niets meer te gebeuren. De staging draagt op `p.src` het complete
    // beeld van een bundel waarvan de som getoetst is (`prepare`),
    // gerelokeerd naar het koude adres; niemand schrijft er nog in (de
    // firmware of QEMU legde er alleen bij de boot iets neer, en dat ligt
    // ernaast, `kernflip::stage_slot`). Het blob, het paar en de recorder
    // zijn net naar DRAM geveegd; hopfs is vastgelegd en dicht, de
    // masquerade dicht. De app-cores draaien in hun partities en in de
    // switch-code in de plan-regio, nooit in het kern-RAM of het kern-beeld
    // dat de trampoline veegt en overschrijft.
    let e = unsafe { chain::chain(&j) };
    mem.write64(pair, 0);
    mem.write64(BOOT_SCRATCH_PA + HANDOFF_MAGIC_OFF, 0);
    dev::push(Pa(pair), 16);
    JumpError::Chain(e)
}
