//! De kern-flip in de kern-binary: de haak van `PrivOp::FLIP`, de sprong,
//! de landing en de flip-boot-guard (`OLD/metal/kern/kernflip/flip.go`,
//! `adopted.go`, en de gratie van `cmd/hopos/watchdog.go`). De procedure per
//! board, de markers en de faalmodi staan in `docs/flip.md`.
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
//!    weigering valt hier, vóór de sprong: Hop krijgt haar als antwoord op
//!    zijn FLIP, en deze kern draait gewoon door.
//! 3. De flip-taak ([`start`]) wacht even, zodat het antwoord op de FLIP
//!    Hop nog bereikt, en legt dan vast, in deze volgorde: hopfs bevriezen
//!    en committen (`storage::freeze_for_flip`), de conntrack van de
//!    switch-actor (`Command::SnapshotNat`, die ook de masquerade
//!    dichtzet), en de bewoners van de lifecycle-actor. Dan het
//!    handoff-blob, het pointer/magic-paar, de recorder op "springen", en
//!    de sprong (`cpu::el2::chain::chain`). Na de bewoners staat er geen
//!    `.await` meer: niemand kan er nog iets aan veranderen. Gaat er
//!    onderweg iets mis, dan ontdooien hopfs en de NAT weer.
//! 4. De nieuwe kern: [`land`] als eerste in `setup`, na de heap,
//!    `discover` en de verhuizing naar de OS-core. Een overdracht is
//!    `HOPOS_FLIP_BOOT gen=N`; de NAT houdt de node-poorten van de oude
//!    flows vast vóór de switch zijn eerste ronde draait, de slots
//!    adopteren hun bewoners (`slots::start`, `HOPOS_FLIP_ADOPT`), daarna
//!    komt de conntrack terug (`HOPOS_FLIP_NAT`), en de guard ([`guard`])
//!    eist binnen [`GRACE`] adoptie en net, anders een koude herstart.
//!
//! De adressen zijn die van het board (`vboard::slots::FLIP_*`): de
//! staging, de recorder, de trampoline en het blob liggen buiten het
//! kern-RAM en buiten de pool. Het koude adres is het linkadres van het
//! board, behalve op UEFI: daar is de kern een PIE die de firmware ergens
//! neerlegde, en gaat de nieuwe kern op precies die basis (`__efi_head`),
//! binnen de maat die de firmware toen gaf.
//!
//! De flip heeft ÉÉN trigger: `PrivOp::FLIP` van Hop (`POST /flip` achter
//! dezelfde HMAC als een jobspec). De Go-kern had er ook een
//! console-commando en een `hopos.flip=<url>` bij boot voor, en die zijn
//! gesloopt: één weg, geen opties (Derek, 01-09). Beleid (wanneer, en
//! waarvandaan) is van Hop; de kern is mechanisme. Daarom bestaat er ook
//! geen `hopos.flip.cold`-sleutel in `hopos.cfg`.
//!
//! # De koude flip
//!
//! Met de vlag `cold` op dezelfde trigger (`abi::systemapi::FLIP_COLD`,
//! `POST /flip {"cold":true}`) springt de kern zonder bewoners over te
//! dragen: de weg voor een bundel met een andere switch-code, die de warme
//! flip weigert. Hop stopt eerst zijn eigen taken; de flip-taak stopt de
//! rest (elke bewoner die niet op de OS-core woont), zet de app-cores uit
//! (`cpu::el2::chain::send_off`, PSCI CPU_OFF), legt hopfs vast en springt
//! met een blob dat alleen "koud", de generatie en de som draagt. De nieuwe
//! kern boot dan als een koude kern (`HOPOS_FLIP_COLD_BOOT`): eigen
//! switch-code, CPU_ON voor de app-cores, en Hop koud uit de staging, precies
//! zoals bij een koude boot. Daarom legt [`prepare`] het nieuwe beeld
//! ACHTER het gestagede image van Hop (`kernflip::stage_slot`) en niet
//! eroverheen: zo is er geen extra kopie van Hop nodig, en blijft hij ook
//! na een warme flip liggen voor een latere koude (29-09).
//!
//! # De zwarte doos
//!
//! Elke consoleregel (de tee van `conport.rs`) gaat ook naar een ring van
//! 16 KiB op een vaste plek naast de recorder ([`black_box`], de vorm in
//! `kern::kernflip::box_open`). Een koude boot leest hem vóór hij er zelf in
//! schrijft en drukt de staart af na `HOPOS_FLIP_LAST`
//! (`HOPOS_FLIP_BLACKBOX`); een landende kern begint een verse doos met zijn
//! generatie. Zo zegt de volgende koude boot niet alleen wáár een geflipte
//! kern stierf, maar ook wat hij daarvoor zei: de Pi 5 zonder UART (30-09),
//! waar de TCP-console de enige was en zijn ring met de kern verdween.
//!
//! # riscv64
//!
//! Alleen koud: de switch-code draait daar uit het kern-image, dus er is
//! niets wat een nieuwe kern kan adopteren (`cage_riscv::adopt` weigert), en een
//! warme flip weigert vóór de sprong ([`WARM`]). De koude weg is dezelfde
//! als op arm64, met twee riscv-stappen: het app-hart gaat uit het image
//! ([`cores_off`]: het resetblok van de C906L, of de uit-stub van de
//! switcher op QEMU) en de sprong is de M-mode-trampoline van
//! `cpu::el2::chain`. De LicheeRV draagt Hop in het image: het nieuwe beeld
//! start zijn eigen Hop.
//!
//! # De config
//!
//! Op elk board staat `hopos.cfg` in het kern-image (`board::cfgwin`). Een
//! bundel met een leeg venster krijgt het venster van deze kern
//! (`HOPOS_FLIP_CFG`); een bundel met een gevuld venster houdt het zijne
//! (`HOPOS_FLIP_CFG_OWN`): zo neemt een flip de config mee, of brengt hij
//! bewust een andere.
//!
//! Wat hier bewust NIET gebeurt: een hardware-watchdog op QEMU (die is er
//! niet).

use crate::{BOARD, DevMem, Machine};
use abi::layout::{FLIP_HANDOFF_LEN, HANDOFF_MAGIC_OFF, HANDOFF_PTR_OFF};
use alloc::boxed::Box;
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
use cpu::psci;
use dev::Pa;
use executor::Executor;
use kern::cage::PhysMem;
use kern::kernflip::{
    self, BOX_SHOW, Boot, Bundle, FLIP_ABI, FlipPlan, HAND_MAGIC, HANDOFF_TAIL, Handoff, Stage,
};
use kern::nodecfg::ColdFlip;
use kern::slots::{Reply, Request, Response, SlotState};
use kern::system::FlipBundle;
use net::nat::{self as nat, MAX_ADOPT};
use net::switch::{Ack, Command, NatReply};
use sync::{Either, Local, Signal, Stop, select};
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

/// Hoe lang de flip-taak op een actor (hopfs, de switch) wacht voordat hij
/// de flip opgeeft. Een actor die in twee seconden niet antwoordt, is een
/// kern die niet overgedragen hoort te worden.
const ACTOR_WAIT: Duration = Duration::from_secs(2);

/// De gratie van een geflipte kern: binnen deze tijd moeten de bewoners
/// geadopteerd en het net op zijn, anders komt de node koud terug. Twee
/// minuten, de les van de Go-kern ("een geflipte kern verliest na twee
/// minuten zijn gratie", watchdog.go): langer blind petten verbergt een
/// kern die hangt, korter verliest een trage DHCP.
pub(crate) const GRACE: Duration = Duration::from_secs(120);

/// De coöperatieve kans van een bewoner die de koude flip stopt: Hop
/// stopte zijn eigen taken al; wat nu nog leeft, krijgt een seconde en
/// dan de kill van de lifecycle.
const COLD_STOP: Duration = Duration::from_secs(1);

/// Hoe lang de koude flip wacht tot een app-core uit is (AFFINITY_INFO
/// OFF). Op QEMU een paar microseconden; een core die in een seconde niet
/// uit is, staat niet in de stub.
const CORES_OFF_WAIT: Duration = Duration::from_secs(1);

/// Keert PSCI CPU_OFF op dit board terug (`Board::CPU_OFF_RETURNS`)? Zo
/// niet (de Pi 5), dan weigert de koude flip zodra een app-core ooit
/// draaide; een core die nooit startte, is al uit en telt niet.
const CPU_OFF_RETURNS: bool = Machine::CPU_OFF_RETURNS;

/// Antwoordt er een PSCI op een SMC (`Board::PSCI`)? Op Apple niet: daar
/// is geen EL3, en een SMC op EL2 is een UNDEF (de core parkeert in de
/// vectoren); Go deed er geen enkele PSCI-call (hop/board.go, 02-09). Op
/// riscv64 is er geen PSCI. Bewust board-kennis en geen
/// `cpu::trng::has_monitor`: QEMU virt zonder `secure=on` heeft ook geen
/// EL3 (ID_AA64PFR0_EL1.EL3 = 0), maar emuleert PSCI over SMC, en daar
/// draaien de koude flip en de reset.
const PSCI: bool = Machine::PSCI;

/// Kan dit board koud flippen? Eén waarde uit [`PSCI`] en
/// [`CPU_OFF_RETURNS`] voor twee lezers: [`prepare`] (en [`no_way_back`])
/// weigeren ermee, en Hop krijgt hem als `HOPOS_COLD_FLIP` in zijn env
/// (`kern::nodecfg::ColdFlip`), zodat hij vooraf beslist en geen taak
/// stopt voor een weigering. Op riscv64 haalt de koude flip de harts uit
/// het image, zonder PSCI.
pub(crate) const COLD_FLIP: ColdFlip = if cfg!(target_arch = "riscv64") {
    ColdFlip::Yes
} else if !PSCI {
    ColdFlip::No
} else if !CPU_OFF_RETURNS {
    ColdFlip::Fresh
} else {
    ColdFlip::Yes
};

/// Kan deze kern bewoners over de sprong heen dragen (de warme flip)? Op
/// riscv64 niet: de switch-code draait daar uit het kern-image, en de
/// nieuwe kern adopteert niemand (`cage_riscv::adopt`). Een warme flip zou de
/// bewoners dan pas na de gratie verliezen (`HOPOS_FLIP_ADOPT_FAIL`, de
/// guard); daarom weigert hij hier vóór de sprong, en is de koude de weg.
const WARM: bool = !cfg!(target_arch = "riscv64");

/// Wat de firmware deze kern in x0 gaf: de nieuwe krijgt hetzelfde.
static FIRMWARE_X0: AtomicU64 = AtomicU64::new(0);
/// De generatie van deze kern: 1 na een koude boot, N+1 na een flip vanaf
/// generatie N (het blob draagt de generatie van de kern die landt).
static GENERATION: AtomicU64 = AtomicU64::new(1);
/// De som van de bundel waaruit deze kern kwam (0 = koud).
static SUM: AtomicU64 = AtomicU64::new(0);
/// Geluid door de slots na een geslaagde adoptie: de guard leest hem, het
/// NAT-herstel wacht erop.
static ADOPTED: Stop = Stop::new();
/// Deze kern landde uit een KOUDE flip: niets te adopteren, wel de guard.
static COLD_LANDED: AtomicBool = AtomicBool::new(false);
/// Deze kern landde uit een flip (warm of koud): er was een overdracht. De
/// opslag mount dan de generatie die de vorige kern vastlegde, ook als de
/// config stateless zegt. Een firmware-boot, ook met een oud paar
/// (`HOPOS_FLIP_STALE`), is geen landing.
static LANDED: AtomicBool = AtomicBool::new(false);

/// Landde deze kern uit een flip? Anders dan [`jumped`] zegt dit op elke
/// architectuur alleen ja bij een gevonden overdracht.
pub(crate) fn landed() -> bool {
    LANDED.load(Relaxed)
}
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
    /// De som van de bundel ([`kernflip::sum64`]).
    sum: u64,
    /// Waar het beeld in de staging ligt (`kernflip::stage_slot`).
    src: u64,
    /// Een koude flip: bewoners stoppen, cores uit, niets overdragen.
    cold: bool,
}

/// De conntrack van de switch als eigendom: de snapshot vóór de sprong, het
/// blob erna (`abi::NatState`, dezelfde vorm aan beide kanten).
type Nat = abi::NatState<Vec<abi::FlowState>>;

/// Waar een flip staat: de haak legt hem klaar, de flip-taak voert hem uit.
#[derive(Copy, Clone, Debug)]
enum Phase {
    /// Geen flip: een FLIP wordt getoetst en klaargelegd.
    Idle,
    /// Aangenomen (Hop kreeg zijn `Ok`), de flip-taak heeft hem nog niet.
    Pending(Prepared),
    /// De flip-taak wacht [`JUMP_DELAY`], bevriest, vangt of springt.
    Jumping,
}

/// De flip, van de haak naar de flip-taak. Beide draaien op de executor van
/// de OS-core. Alleen in [`Phase::Idle`] neemt de haak een FLIP aan: de
/// staging is er één, en tot 24-10 legde een tweede FLIP tijdens de
/// [`JUMP_DELAY`] of het vangen opnieuw plat in dezelfde staging terwijl de
/// flip-taak de eerste nog ging uitvoeren. De flip-taak zet hem terug op
/// `Idle` na een sprong die niet doorging (na [`Frozen::thaw`]).
static FLIP: Local<Cell<Phase>> = Local::new(Cell::new(Phase::Idle));
/// De conntrack uit het blob, van de landing naar de herstel-taak.
static LANDED_NAT: Local<Cell<Option<Nat>>> = Local::new(Cell::new(None));
/// De bel van de flip-taak.
static BELL: Signal = Signal::new();
/// De antwoordplek van de flip-taak bij de lifecycle-actor (één aanroeper).
static REPLY: Reply = Reply::new();
/// De antwoordplek van de flip-taak bij de switch-actor (de snapshot).
static NAT_REPLY: NatReply = NatReply::new();
/// De bevestigingen van de NAT-commando's rond de flip (één aanroeper per
/// stuk: de flip-taak vóór de sprong, de herstel-taak erna).
static NAT_ACK: Ack = Ack::new();
static HOLD_ACK: Ack = Ack::new();

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

/// De landing, als eerste in `setup` (in `kmain` gaan alleen de heap,
/// `discover` en de verhuizing naar de OS-core ervóór): een overdracht is
/// er, of niet.
///
/// Alleen na een sprong ([`jumped`]): een firmware-boot die nog een paar
/// vindt, wist het en boot koud (`HOPOS_FLIP_STALE`). Na een harde reset
/// draaien de bewoners niet meer, en het blob is van een kern die al landde
/// of al dood is.
///
/// Een onbruikbaar blob met een geldig paar ná een sprong is GEEN koude
/// boot: er leven misschien bewoners, en een koude boot zou hun regio's vrij
/// noemen. Op ijzer wacht de kern dan op de watchdog; QEMU heeft er geen,
/// dus een PSCI-reset: de machine komt koud terug, zoals de watchdog dat
/// ook deed.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn land(x0: u64) -> Option<Handoff> {
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
        Ok(Boot::Adopted(h)) if h.cold => {
            GENERATION.store(h.generation, Relaxed);
            SUM.store(h.bundle_sum, Relaxed);
            COLD_LANDED.store(true, Relaxed);
            LANDED.store(true, Relaxed);
            // De doos van de kern die sprong gaat weg: die leeft niet meer,
            // maar stierf ook niet. Vanaf hier is hij van ons.
            open_box(h.generation);
            crate::clock::restore(h.wall_off);
            println!(
                "flip: landed cold, generation {} from a {} MB kernel at {:#x}: no residents to adopt, this kernel installs its own switch code and starts Hop from the staging HOPOS_FLIP_BOOT gen={} HOPOS_FLIP_COLD_BOOT",
                h.generation,
                h.old_size >> 20,
                h.old_base,
                h.generation
            );
            // Geen overdracht voor `main`: de slots booten koud (eigen
            // switch-code, Hop uit de staging), zoals na een koude boot.
            None
        }
        Ok(Boot::Adopted(mut h)) => {
            GENERATION.store(h.generation, Relaxed);
            LANDED.store(true, Relaxed);
            SUM.store(h.bundle_sum, Relaxed);
            open_box(h.generation);
            crate::clock::restore(h.wall_off);
            println!(
                "flip: landed, generation {} from a {} MB kernel at {:#x}, {} resident(s), {} NAT flow(s) HOPOS_FLIP_BOOT gen={}",
                h.generation,
                h.old_size >> 20,
                h.old_base,
                h.slots.len(),
                h.nat.flows.len(),
                h.generation
            );
            // De conntrack is van de herstel-taak ([`start`]); de slots
            // krijgen de rest.
            LANDED_NAT.set(Some(core::mem::take(&mut h.nat)));
            Some(h)
        }
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
        Err(e) => {
            // De doos blijft dicht: wat erin staat (de kern die sprong) is
            // het spoor voor de koude boot hierna.
            println!(
                "flip: the handoff blob does not decode: {e}; resetting cold HOPOS_FLIP_BLOB_BAD"
            );
            reset()
        }
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

/// De koude weg terug: PSCI SYSTEM_RESET. Keert niet terug. Zonder PSCI
/// (Apple, riscv64), of als de reset terugkeert, de watchdog op zijn
/// kortst en dan parkeren: een geparkeerde core aait niet meer (Go op
/// Apple: de pets inhouden, cmd/hopos/watchdog.go). Zonder watchdog blijft
/// het bij parkeren, met een regel die dat zegt.
fn reset() -> ! {
    if PSCI {
        let _ = psci::smc(psci::SYSTEM_RESET, 0, 0, 0);
    }
    println!("flip: resetting through the watchdog at its shortest alarm HOPOS_FLIP_RESET_WDT");
    if !crate::watchdog::fire() {
        println!(
            "flip: no PSCI reset and no watchdog on this board: this core parks and the node stays down until a power cycle HOPOS_FLIP_RESET_PARK"
        );
    }
    cpu::boot::park()
}

/// De slots meldden een geslaagde adoptie.
pub(crate) fn adopted_ok() {
    ADOPTED.set();
}

/// De flip-boot-guard: een geflipte kern moet binnen [`GRACE`] zijn
/// bewoners geadopteerd en zijn net op hebben. Zo niet, dan PSCI-reset: een
/// halve kern met levende bewoners is erger dan een koude boot. Lukt het,
/// dan gaat de recorder leeg (een latere koude boot is geen mislukte flip).
///
/// De les van 06-09 (de M4): een guard die alleen "de kern draait" toetst,
/// liet een kern zonder net twee minuten petten en dan toch vallen; de
/// adoptie én het net zijn de voorwaarde, niet de executor.
///
/// Wat hij NIET dekt: een kern die hangt vóór de executor draait. Dat is
/// de hardware-watchdog, en QEMU virt heeft er geen.
///
/// Na een KOUDE flip is er niets te adopteren: dan is het net de
/// voorwaarde. Een koude landing zonder net binnen de gratie gaat ook
/// terug naar een koude boot: dan komt het bootmedium weer aan het woord,
/// en dat is de kern die er vóór de flip stond.
pub(crate) async fn guard(exec: &'static Executor) {
    let deadline = exec.now().saturating_add(GRACE.as_nanos() as u64);
    let generation = generation();
    let cold = COLD_LANDED.load(Relaxed);
    loop {
        if (cold || ADOPTED.is_set()) && crate::net::uplink_ip().is_some() {
            let (mut mem, p) = (DevMem, plan());
            kernflip::stage(&mut mem, &p, Stage::NetUp, generation);
            let _ = kernflip::take_last_flip(&mut mem, &p);
            let what = if cold {
                "cold, nothing to adopt,"
            } else {
                "residents adopted and"
            };
            println!("flip: generation {generation} settled, {what} net up HOPOS_FLIP_SETTLED");
            return;
        }
        if exec.now() >= deadline {
            println!(
                "flip: generation {generation} not settled within {} s (adopted={}), resetting cold HOPOS_FLIP_GUARD",
                GRACE.as_secs(),
                ADOPTED.is_set()
            );
            reset();
        }
        exec.after(Duration::from_millis(250)).await;
    }
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
    let sum64 = kernflip::sum64(&sum);
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
    let cold = b.cold;
    if !cold && !WARM {
        return Err(refuse(
            "warm flip not on riscv64, ask cold",
            kern::Error::Version {
                have: 0,
                want: abi::systemapi::FLIP_COLD,
            },
        ));
    }
    if cold && COLD_FLIP == ColdFlip::No {
        // De koude flip zet de app-cores uit met PSCI CPU_OFF en wacht op
        // AFFINITY_INFO: zonder PSCI een UNDEF ná het bevriezen van de
        // opslag. Dus hier weigeren, vóór er iets onherroepelijks gebeurt.
        println!(
            "flip: cold flip refused: this board has no PSCI (no EL3, an SMC is UNDEF), so its app cores cannot be powered off; ask warm HOPOS_FLIP_COLD_NO_PSCI"
        );
        return Err(refuse(
            "cold flip needs PSCI, ask warm",
            kern::Error::Version {
                have: abi::systemapi::FLIP_COLD,
                want: 0,
            },
        ));
    }
    if cold && let Some(c) = no_way_back() {
        println!(
            "flip: cold flip refused: CPU_OFF has no way back on this board and app core {c} ran; ask warm HOPOS_FLIP_COLD_NO_WAY_BACK"
        );
        return Err(refuse(
            "cold flip: CPU_OFF has no way back, ask warm",
            kern::Error::Version {
                have: abi::systemapi::FLIP_COLD,
                want: 0,
            },
        ));
    }
    if cold {
        // Koud: de nieuwe kern installeert zijn eigen switch-code en
        // adopteert niemand, dus de som doet er niet toe. Dit is precies de
        // weg voor een bundel die warm geweigerd wordt.
        println!(
            "flip: cold flip asked, the switch code sum of the bundle is not held against the residents HOPOS_FLIP_COLD_ASKED"
        );
    } else {
        check_switch_code(&bundle)?;
    }
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
    let src = stage_slot(flat, cold)?;
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
        "flip: bundle ok, sha256 verified: {} KiB image in {segs} segment(s) linked at {:#x}, {relocs} relocation(s) to {base:#x}, entry {entry:#x}, staged at {src:#x}{} HOPOS_FLIP_STAGED",
        flat >> 10,
        bundle.link_load,
        if cold { ", cold" } else { "" }
    );
    FLIP.set(Phase::Pending(Prepared {
        len: flat,
        entry,
        sum: sum64,
        src,
        cold,
    }));
    BELL.set();
    Ok(())
}

/// De app-core die een koude flip op dit board onmogelijk maakt: CPU_OFF
/// keert hier niet terug ([`COLD_FLIP`] is `Fresh`), en elke core die ooit
/// draaide (geparkeerd, of nog bezet en straks geparkeerd) moet uit.
/// Dezelfde toets als in [`cores_off`], maar in de haak: Hop krijgt de
/// weigering op zijn FLIP, vóór hopfs bevriest en vóór de flip-taak een
/// bewoner stopt (de Pi 5, 03-10: een 202, dan `HOPOS_FLIP_FAIL`).
#[cfg(not(target_arch = "riscv64"))]
fn no_way_back() -> Option<usize> {
    use cpu::el2::CoreState;
    if COLD_FLIP != ColdFlip::Fresh {
        return None;
    }
    let plan = crate::slots::os_plan().ok()?;
    (1..=plan.app_cores()).find(|&c| {
        abi::layout::Core::new(c)
            .is_some_and(|core| !matches!(cpu::el2::core_state(&plan, core), Ok(CoreState::Cold)))
    })
}

/// Op riscv64 haalt de koude flip elk app-hart uit het image en zet de
/// nieuwe kern het terug ([`cores_off`]): er is geen deur zonder terugweg.
#[cfg(target_arch = "riscv64")]
fn no_way_back() -> Option<usize> {
    None
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
/// (`kernflip::stage_slot`). Een koude flip eist dat image, want de nieuwe
/// kern start het; een warme flip die er niet naast past, legt het beeld
/// er luid overheen (Hop draait door en heeft zijn image niet meer nodig,
/// alleen een latere koude flip weigert dan).
fn stage_slot(flat: u64, cold: bool) -> Result<u64, Refused> {
    let staged = staged_elf();
    if cold && staged.is_none() {
        return Err(refuse(
            "cold flip without a staged image",
            kern::Error::NoEnt,
        ));
    }
    if let Some(at) = kernflip::stage_slot(STAGE_PA, STAGE_MAX, flat, staged) {
        return Ok(at);
    }
    let alone = kernflip::stage_slot(STAGE_PA, STAGE_MAX, flat, None);
    match (alone, cold) {
        (Some(at), false) => {
            println!(
                "flip: the staging holds the new kernel ({} KiB) but not next to the staged image; it goes over it, a later cold flip will be refused HOPOS_FLIP_STAGE_SHARED",
                flat >> 10
            );
            Ok(at)
        }
        _ => Err(too_large(flat, STAGE_MAX)),
    }
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

/// Spawnt de flip-taak en, na een landing, de guard en het herstel van de
/// conntrack. Aangeroepen vóór de executor draait: de claim op de
/// node-poorten ligt dan in de brievenbus van de switch vóór zijn eerste
/// ronde, dus geen antwoord van een oude peer valt bij de node-stack (die
/// zou een levende verbinding met een RST doden).
///
/// Een koude landing geeft `main` geen overdracht (`landed` is dan
/// onwaar), maar krijgt wel de guard: ook een koude kern moet binnen de
/// gratie net hebben.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor, landed: bool) {
    if let Err(e) = exec.spawn(run(exec)) {
        println!("flip: task not spawned: {e:?} HOPOS_FLIP_SPAWN");
    }
    if COLD_LANDED.load(Relaxed) {
        if let Err(e) = exec.spawn(guard(exec)) {
            println!("flip: guard not spawned: {e:?}, no grace check HOPOS_FLIP_SPAWN");
        }
        return;
    }
    if !landed {
        return;
    }
    if let Err(e) = exec.spawn(guard(exec)) {
        println!("flip: guard not spawned: {e:?}, no grace check HOPOS_FLIP_SPAWN");
    }
    let Some(state) = LANDED_NAT.take() else {
        return;
    };
    hold_ports(&state);
    if let Err(e) = exec.spawn(restore_nat(exec, state)) {
        println!("flip: NAT restore not spawned: {e:?}, the conntrack is lost HOPOS_FLIP_SPAWN");
    }
}

/// Houdt de node-poorten van de overgedragen flows vast tot het herstel.
/// Meer dan [`MAX_ADOPT`] poorten: de eerste gaan vast, de rest niet, luid
/// (die flows komen wel terug, maar een antwoord vóór het herstel kan bij
/// de node-stack vallen).
fn hold_ports(state: &Nat) {
    let mut ports: Vec<u16> = Vec::new();
    let n = state.flows.len().min(MAX_ADOPT);
    if ports.try_reserve_exact(n).is_err() {
        println!("flip: out of memory for the held NAT ports HOPOS_FLIP_NAT_HOLD");
        return;
    }
    ports.extend(state.flows.iter().take(n).map(|f| f.node_port));
    if state.flows.len() > n {
        println!(
            "flip: {} NAT flow(s), only the first {n} node ports held until the restore HOPOS_FLIP_NAT_HOLD",
            state.flows.len()
        );
    }
    // 'static voor de brievenbus: het commando draagt een `&'static`-slice.
    // Hij lekt bewust, één keer per boot en hooguit [`MAX_ADOPT`] poorten.
    let ports: &'static [u16] = Box::leak(ports.into_boxed_slice());
    let cmd = Command::HoldAdoption {
        ports,
        ack: &HOLD_ACK,
    };
    if crate::net::COMMANDS.try_send(cmd).is_err() {
        println!("flip: switch mailbox full, NAT ports not held HOPOS_FLIP_NAT_HOLD");
    }
}

/// Het herstel van de conntrack na de landing: wachten tot de slots hun
/// bewoners adopteerden (hun `Attach` staat dan vóór ons in de brievenbus,
/// en `restore` houdt alleen flows van aangesloten slots), dan
/// `RestoreNat`, dan de claim op de poorten los.
async fn restore_nat(exec: &'static Executor, state: Nat) {
    let total = state.flows.len();
    let _ = select(ADOPTED.wait(), exec.after(GRACE)).await;
    let restored = if ADOPTED.is_set() {
        // 'static voor de brievenbus, zoals de poorten.
        let st = nat::NatState {
            flows: &*Vec::leak(state.flows),
            masq_next: state.masq_next,
        };
        let send = || {
            let cmd = Command::RestoreNat {
                state: st,
                ack: &NAT_ACK,
            };
            crate::net::COMMANDS.try_send(cmd).is_ok()
        };
        match send_and_wait(exec, send, &NAT_ACK).await {
            Some(Ok(n)) => Some(n),
            Some(Err(e)) => {
                println!("flip: NAT restore refused: {e} HOPOS_FLIP_NAT_FAIL");
                None
            }
            None => None,
        }
    } else {
        println!("flip: residents not adopted, the conntrack is dropped HOPOS_FLIP_NAT_FAIL");
        None
    };
    // De claim los, ook na een mislukt herstel: anders krijgt geen enkele
    // bewoner ooit nog een nieuwe uitgaande verbinding.
    let finish = || {
        let cmd = Command::FinishAdoption { ack: &NAT_ACK };
        crate::net::COMMANDS.try_send(cmd).is_ok()
    };
    if send_and_wait(exec, finish, &NAT_ACK).await.is_none() {
        println!(
            "flip: NAT adoption not finished, outbound mapping stays closed HOPOS_FLIP_NAT_FAIL"
        );
    }
    if let Some(n) = restored {
        println!(
            "flip: conntrack restored, {n} of {total} NAT flow(s) carried over HOPOS_FLIP_NAT restored={n} of={total}"
        );
    }
}

/// Een commando naar de switch en wachten op zijn bevestiging, hooguit
/// [`ACTOR_WAIT`] (een switch die er niet is, antwoordt nooit). `send`
/// bouwt het commando en probeert het af te leveren (de ring-typen van de
/// brievenbus zijn van `net.rs`); een volle brievenbus probeert het even
/// opnieuw.
async fn send_and_wait(
    exec: &'static Executor,
    send: impl Fn() -> bool,
    ack: &'static Ack,
) -> Option<net::Result<u32>> {
    let _ = ack.try_take();
    let deadline = exec.now().saturating_add(ACTOR_WAIT.as_nanos() as u64);
    while !send() {
        if exec.now() >= deadline {
            println!("flip: switch mailbox full HOPOS_FLIP_NAT_FAIL");
            return None;
        }
        exec.after(Duration::from_millis(5)).await;
    }
    match select(ack.recv(), exec.after(ACTOR_WAIT)).await {
        Either::Left(r) => Some(r),
        Either::Right(()) => {
            println!(
                "flip: the switch did not answer within {} s HOPOS_FLIP_NAT_FAIL",
                ACTOR_WAIT.as_secs()
            );
            None
        }
    }
}

/// De flip-taak: wacht op een klaargelegde flip en springt.
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
        let e = jump(exec, p).await;
        println!("flip: not jumped: {e}; this kernel keeps running HOPOS_FLIP_FAIL");
        // Niet gesprongen is geen mislukte flip voor een latere boot: de
        // recorder gaat leeg (Go: `stageClear`).
        let _ = kernflip::take_last_flip(&mut DevMem, &plan());
        // Ontdooid en opgegeven: een volgende FLIP mag weer.
        FLIP.set(Phase::Idle);
    }
}

/// Waarom de sprong niet doorging.
enum JumpError {
    Kern(kern::Error),
    Nat(&'static str),
    Chain(chain::ChainError),
    /// De koude flip kreeg een bewoner of een app-core niet stil.
    Cold(&'static str, usize),
}

impl core::fmt::Display for JumpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Kern(e) => write!(f, "{e}"),
            Self::Nat(why) => write!(f, "conntrack: {why}"),
            Self::Chain(e) => write!(f, "{e}"),
            Self::Cold(why, n) => write!(f, "cold flip: {why} ({n})"),
        }
    }
}

/// Wat vóór de sprong bevroren is, en dus ontdooid moet worden als hij
/// niet doorgaat.
struct Frozen {
    fs: bool,
    nat: bool,
}

impl Frozen {
    /// De flip gaat niet door: hopfs en de NAT weer open, luid als het
    /// bericht niet aankomt.
    fn thaw(&self) {
        if self.fs && !crate::storage::thaw_after_flip() {
            println!("flip: hopfs mailbox full, the tree stays frozen HOPOS_FLIP_FAIL");
        }
        if self.nat {
            let _ = NAT_ACK.try_take();
            let cmd = Command::FinishAdoption { ack: &NAT_ACK };
            if crate::net::COMMANDS.try_send(cmd).is_err() {
                println!(
                    "flip: switch mailbox full, outbound mapping stays closed HOPOS_FLIP_FAIL"
                );
            }
        }
    }
}

/// Vastleggen, overdragen en springen. Keert alleen terug met een fout; het
/// pointer/magic-paar is dan weer weg, en hopfs en de NAT zijn ontdooid.
async fn jump(exec: &'static Executor, p: Prepared) -> JumpError {
    let mut frozen = Frozen {
        fs: false,
        nat: false,
    };
    let e = capture_and_jump(exec, p, &mut frozen).await;
    frozen.thaw();
    e
}

async fn capture_and_jump(exec: &'static Executor, p: Prepared, frozen: &mut Frozen) -> JumpError {
    // 1. De opslag: vastleggen en dicht. Wat na deze commit geschreven zou
    //    worden, ziet de nieuwe kern niet, dus er wordt niets meer
    //    geschreven.
    match crate::storage::freeze_for_flip(exec, ACTOR_WAIT).await {
        Ok(_) => frozen.fs = crate::storage::is_up(),
        Err(e) => return JumpError::Kern(e),
    }
    if p.cold {
        return cold_jump(exec, p).await;
    }
    // 2. De conntrack, als waarde terug van de switch-actor.
    let snap = match snapshot_nat(exec).await {
        Ok(s) => {
            frozen.nat = s.is_some();
            s.unwrap_or_default()
        }
        Err(why) => return JumpError::Nat(why),
    };
    // 2b. De USB-controllers stil: DMA-masters op de RP1 die anders door de
    // link-reset van de nieuwe kern heen zouden schrijven (30-09, de Pi 5:
    // vijf flips vanuit de koud gebootte gui-kern, en de RP1 kreeg daarna
    // niets meer naar de host, geen descriptor en geen MSI).
    crate::gui::quiesce_usb(exec).await;
    // 3. Het laatste `.await` van deze kern: de bewoners.
    let slots: Vec<SlotState> =
        match kern::slots::call(&crate::LIFECYCLE, &REPLY, Request::Snapshot).await {
            Ok(Response::Snapshot(s)) => s,
            Ok(Response::Failed(e)) | Err(e) => return JumpError::Kern(e),
            Ok(_) => return JumpError::Kern(kern::Error::Busy),
        };
    handoff_and_jump(p, slots, snap)
}

/// De koude weg na de bevriezing: elke bewoner die niet op de OS-core
/// woont stoppen, de app-cores uit, de USB stil, en springen met een blob
/// zonder bewoners. Wat hier misgaat, laat een kern achter die gewoon
/// doordraait: gestopte bewoners blijven gestopt (Hop plaatst ze opnieuw),
/// en een uitgezette core staat op "koud", dus de volgende dispatch is weer
/// een PSCI CPU_ON.
async fn cold_jump(exec: &'static Executor, p: Prepared) -> JumpError {
    let stopped = match stop_residents(exec).await {
        Ok(n) => n,
        Err(e) => return e,
    };
    let off = match cores_off(exec).await {
        Ok(n) => n,
        Err(e) => return e,
    };
    // De USB-controllers stil, zoals op de warme weg (stap 2b in
    // `capture_and_jump`): ook kern B na een koude flip doet de link-reset,
    // en de xHCI's zijn DMA-masters op de RP1. Op de Pi 5 is koud toegestaan
    // zolang er geen app-core draaide, en tot 24-10 sprong deze weg met
    // draaiende controllers: het geval van fc66084. Als laatste `.await`,
    // na de bewoners en de cores: een bewoner die stopt mag zijn laatste
    // invoer of optische lezing nog krijgen (de display-app heeft alleen
    // het glas nodig, de USB is zijn invoer), en een stop of CPU_OFF die
    // faalt laat een kern achter die doordraait mét toetsenbord; een
    // gestopte USB-taak komt in deze kern niet meer terug.
    crate::gui::quiesce_usb(exec).await;
    println!(
        "flip: cold flip, {stopped} resident(s) stopped and {off} app core(s) powered off, nothing to hand over HOPOS_FLIP_COLD stopped={stopped} cores_off={off}"
    );
    let e = handoff_and_jump(p, Vec::new(), Nat::default());
    cores_back();
    e
}

/// Vraagt de lifecycle-actor iets, hooguit [`ACTOR_WAIT`] plus `extra`.
async fn ask(
    exec: &'static Executor,
    req: Request,
    extra: Duration,
) -> Result<Response, JumpError> {
    let call = kern::slots::call(&crate::LIFECYCLE, &REPLY, req);
    match select(call, exec.after(ACTOR_WAIT.saturating_add(extra))).await {
        Either::Left(Ok(Response::Failed(e)) | Err(e)) => Err(JumpError::Kern(e)),
        Either::Left(Ok(r)) => Ok(r),
        Either::Right(()) => Err(JumpError::Kern(kern::Error::Busy)),
    }
}

/// Stopt elke bewoner met een app-core. Wie op de OS-core woont (Hop, in
/// de idle van de kern), stopt vanzelf met de sprong: die kreeg zijn
/// antwoord op de FLIP al, en de nieuwe kern start hem koud. Geeft het
/// aantal gestopte bewoners.
async fn stop_residents(exec: &'static Executor) -> Result<usize, JumpError> {
    let slots = match ask(exec, Request::Snapshot, Duration::ZERO).await? {
        Response::Snapshot(s) => s,
        _ => return Err(JumpError::Kern(kern::Error::Busy)),
    };
    let mut n = 0;
    for st in slots.iter().filter(|st| st.core != 0) {
        let Some(slot) = kern::Slot::new(st.slot) else {
            continue;
        };
        let req = Request::Stop {
            slot,
            timeout: COLD_STOP,
        };
        match ask(exec, req, COLD_STOP).await {
            Ok(_) => {
                println!(
                    "flip: slot {} on core {} stopped for the cold flip HOPOS_FLIP_COLD_STOP slot={}",
                    st.slot, st.core, st.slot
                );
                n += 1;
            }
            Err(JumpError::Kern(e)) => {
                println!(
                    "flip: slot {} would not stop: {e} HOPOS_FLIP_COLD_STOP_FAIL",
                    st.slot
                );
                return Err(JumpError::Cold("a resident would not stop, slot", st.slot));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Zet elke geparkeerde app-core uit (PSCI CPU_OFF via de uit-stub van
/// `cpu::el2::chain`) en wacht tot AFFINITY_INFO voor elke app-core "uit"
/// zegt. Een core die nooit startte, is al uit. Geeft het aantal
/// uitgezette cores.
#[cfg(not(target_arch = "riscv64"))]
async fn cores_off(exec: &'static Executor) -> Result<usize, JumpError> {
    use cpu::el2::CoreState;
    if !PSCI {
        // `prepare` weigerde al; dit is de tweede lijn vóór de eerste SMC.
        return Err(JumpError::Cold("no PSCI on this board, app cores", 0));
    }
    let Ok(plan) = crate::slots::os_plan() else {
        // Geen slots op dit board: geen app-cores om uit te zetten.
        return Ok(0);
    };
    let stub = Pa(FLIP_TRAMP_PA);
    let mut sent = 0usize;
    for c in 1..=plan.app_cores() {
        let Some(core) = abi::layout::Core::new(c) else {
            continue;
        };
        match cpu::el2::core_state(&plan, core) {
            Ok(CoreState::Cold) => {}
            Ok(CoreState::Parked) if !CPU_OFF_RETURNS => {
                // `prepare` weigerde al ([`no_way_back`]); de tweede lijn.
                return Err(JumpError::Cold(
                    "CPU_OFF has no way back on this board, parked app core",
                    c,
                ));
            }
            Ok(CoreState::Parked) => {
                if sent == 0 {
                    chain::place_off_stub(stub).map_err(JumpError::Chain)?;
                }
                chain::send_off(&plan, core, stub).map_err(JumpError::Chain)?;
                sent += 1;
            }
            Ok(CoreState::Running(_)) | Err(_) => {
                return Err(JumpError::Cold("app core still runs", c));
            }
        }
    }
    let deadline = exec.now().saturating_add(CORES_OFF_WAIT.as_nanos() as u64);
    for c in 1..=plan.app_cores() {
        let Some(core) = abi::layout::Core::new(c) else {
            continue;
        };
        let target = vboard::slots::mpidr(plan.phys_core(core));
        loop {
            match psci::affinity_info(target) {
                psci::Affinity::Off => break,
                // Een firmware zonder AFFINITY_INFO: dan zegt de mailbox
                // het (de stub schreef "koud" vlak voor zijn CPU_OFF), plus
                // een tik voor de CPU_OFF zelf.
                psci::Affinity::Err(psci::Error::NotSupported)
                    if matches!(cpu::el2::core_state(&plan, core), Ok(CoreState::Cold)) =>
                {
                    exec.after(Duration::from_millis(10)).await;
                    break;
                }
                _ => {}
            }
            if exec.now() >= deadline {
                println!(
                    "flip: app core {c} (mpidr {target:#x}) not off after {} ms, affinity {:?}, mailbox {:?} HOPOS_FLIP_COLD_CORE",
                    CORES_OFF_WAIT.as_millis(),
                    psci::affinity_info(target),
                    cpu::el2::core_state(&plan, core)
                );
                return Err(JumpError::Cold("app core did not power off", c));
            }
            exec.after(Duration::from_millis(1)).await;
        }
    }
    Ok(sent)
}

/// Na een koude sprong die niet doorging: niets op arm64, de volgende
/// dispatch is daar weer een PSCI CPU_ON (de mailbox staat op koud).
#[cfg(not(target_arch = "riscv64"))]
fn cores_back() {}

/// De app-harts uit het kern-image voor de koude flip (riscv64): elk hart
/// dat in de switcher staat, gaat in reset (de C906L) of naar de uit-stub
/// op `FLIP_PARK_PA` (QEMU), en de taak wacht tot de stub bevestigt. Een
/// hart dat nooit startte, telt niet. Faalt er één, dan komen de andere
/// terug in de switcher ([`cores_back`]). Geeft het aantal harts buiten het
/// image.
#[cfg(target_arch = "riscv64")]
async fn cores_off(exec: &'static Executor) -> Result<usize, JumpError> {
    use crate::slots::{self as rv, Off};
    let Ok(plan) = crate::slots::os_plan() else {
        return Ok(0);
    };
    let stub = Pa(vboard::slots::FLIP_PARK_PA);
    let mut n = 0usize;
    for c in 1..=plan.app_cores() {
        let Some(core) = abi::layout::Core::new(c) else {
            continue;
        };
        match rv::park_for_flip(&plan, core, stub) {
            Ok(Off::Cold) => continue,
            Ok(Off::Reset) => {
                n += 1;
                continue;
            }
            Ok(Off::Sent) => {}
            Err(why) => {
                println!("flip: app core {c}: {why} HOPOS_FLIP_COLD_CORE");
                cores_back();
                return Err(JumpError::Cold(why, c));
            }
        }
        let deadline = exec.now().saturating_add(CORES_OFF_WAIT.as_nanos() as u64);
        while !rv::is_off(&plan, core, stub) {
            if exec.now() >= deadline {
                println!(
                    "flip: app core {c} did not reach the off stub at {:#x} within {} ms HOPOS_FLIP_COLD_CORE",
                    stub.0,
                    CORES_OFF_WAIT.as_millis()
                );
                cores_back();
                return Err(JumpError::Cold("app core did not leave the image", c));
            }
            exec.after(Duration::from_millis(1)).await;
        }
        n += 1;
    }
    Ok(n)
}

/// Na een koude sprong die niet doorging (riscv64): elk app-hart dat
/// [`cores_off`] uit het image haalde, terug de switcher in.
#[cfg(target_arch = "riscv64")]
fn cores_back() {
    if let Ok(plan) = crate::slots::os_plan() {
        crate::slots::unpark_after_flip(&plan);
    }
}

/// Vraagt de switch-actor om de conntrack (en zet daarmee de masquerade
/// dicht). `Ok(None)`: er is geen switch op deze node, dus ook niets om
/// mee te nemen.
async fn snapshot_nat(exec: &'static Executor) -> Result<Option<Nat>, &'static str> {
    if !crate::net::switch_up() {
        return Ok(None);
    }
    let mut buf: Vec<nat::FlowState> = Vec::new();
    if buf.try_reserve_exact(net::MAX_FLOWS).is_err() {
        return Err("out of memory for the snapshot buffer");
    }
    buf.resize(net::MAX_FLOWS, nat::FlowState::default());
    let cmd = Command::SnapshotNat {
        buf,
        reply: &NAT_REPLY,
    };
    let ask = sync::oneshot::call(&crate::net::COMMANDS, &NAT_REPLY, cmd);
    match select(ask, exec.after(ACTOR_WAIT)).await {
        Either::Left(Err(_)) => Err("switch mailbox full"),
        Either::Left(Ok(s)) => {
            println!(
                "flip: conntrack captured, {} NAT flow(s), outbound mapping closed until the jump HOPOS_FLIP_NAT_CAPTURED flows={}",
                s.flows.len(),
                s.flows.len()
            );
            Ok(Some(s))
        }
        Either::Right(()) => Err("the switch did not answer"),
    }
}

/// Het blob, het paar en de sprong. Synchroon: vanaf hier verandert er
/// niets meer aan de staat die overgaat.
fn handoff_and_jump(p: Prepared, slots: Vec<SlotState>, nat: Nat) -> JumpError {
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
    let generation = generation() + 1;
    let (mut mem, fp) = (DevMem, plan());
    kernflip::stage(&mut mem, &fp, Stage::Captured, generation);
    let ram = BOARD.plan().kern_ram;
    let base = image::base();
    let h = Handoff {
        old_base: ram.base.0,
        old_size: ram.size,
        window: base,
        total: p.len,
        generation,
        bundle_sum: p.sum,
        slots,
        nat,
        cold: p.cold,
        wall_off: crate::clock::offset(),
    };
    let blob = match kernflip::encode(&h, HANDOFF_TAIL) {
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
    let (a, b) = image::sweep();
    let j = Jump {
        dst: Pa(base),
        src: Pa(p.src),
        len: p.len,
        entry: p.entry,
        x0: FIRMWARE_X0.load(Relaxed),
        sweep: [(ram.base, ram.end()), (Pa(a), Pa(b))],
        tramp: Pa(FLIP_TRAMP_PA),
    };
    // SAFETY: dit is de flip-taak op de OS-core, en na de sprong hoeft hier
    // niets meer te gebeuren. De staging draagt op `p.src` het complete
    // beeld van een bundel waarvan de som getoetst is (`prepare`),
    // gerelokeerd naar het koude adres; niemand schrijft er nog in (de
    // firmware of QEMU legde er alleen bij de boot iets neer, en dat ligt
    // ernaast, `kernflip::stage_slot`). Het blob, het paar en de recorder
    // zijn net naar DRAM geveegd; hopfs is vastgelegd en dicht, de
    // masquerade dicht. De app-cores draaien in hun partities en in de
    // switch-code in de plan-regio, nooit in het kern-RAM of het kern-beeld
    // dat de trampoline veegt en overschrijft; bij een koude flip zijn ze
    // uit (`cores_off`: AFFINITY_INFO zei OFF), dus voert ook niemand de
    // uit-stub nog uit die de trampoline nu overschrijft.
    let e = unsafe { chain::chain(&j) };
    mem.write64(pair, 0);
    mem.write64(BOOT_SCRATCH_PA + HANDOFF_MAGIC_OFF, 0);
    dev::push(Pa(pair), 16);
    JumpError::Chain(e)
}
