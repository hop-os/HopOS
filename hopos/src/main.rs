//! De kern-binary van HopOS v3: kiest precies één board, zet de console op,
//! toont de bunny en draait de executor.
//!
//! De bootvolgorde is die van de Go-main (`OLD/metal/cmd/hopos/main.go`)
//! voor zover de lagen eronder er zijn: console, bunny, `runtime`-regel,
//! het privilege-niveau, de firmware-regel, en dan het werk als taken op de
//! executor. Een taak per lus: de tik, de IRQ-dispatch, de opslag
//! (storage.rs: de schijf, hopfs, de hopfs-actor en de committer) en het
//! netwerkvlak (net.rs: pomp, switch, poort 0, DHCP, de system-listener).

#![no_std]
#![no_main]
#![allow(clippy::expect_used)] // boot-code: vóór de agent draait is falen parkeren (handboek §12)

mod bench; // de meetbanken achter hopos.nvmebench en hopos.idlestat (bench.rs)
mod clock;
mod codec; // het media-vlak (codec.rs); kaal een stub, feature `media`
mod config;
mod conport; // de console over TCP: de ring achter de UART (conport.rs)
mod flip; // FLIP: de kern-flip (flip.rs)
mod glue; // `DevMem`, `KernConsole` en de outbox van een slot (glue.rs)
mod gui; // het gui-vlak (gui.rs); kaal no-ops, feature `gui`
mod kooi; // de kooi als beleid, voor elke architectuur (kooi.rs)
mod load; // de meetlat per slot: idle en wekken op de console (load.rs)
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    not(feature = "board-apple")
))]
mod mem; // memcpy en memcmp met ongealigneerde ldp/stp (mem.rs)
mod net;
#[cfg(feature = "media")]
mod optical;
mod seed; // het zaad van de slots op hun control-page (seed.rs)
mod slots;
mod storage;
mod telemetry; // de thermiek op tik en heartbeat, en het klokbeleid (telemetry.rs)
mod watchdog; // de node-watchdog: kern::watchdog op de hardware (watchdog.rs)

extern crate alloc;

use alloc::boxed::Box;
use board::Board;
use board::heap::Heap;
use board::stage::StagedRole;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use cpu::println;
use executor::Executor;
use glue::{DevMem, KernConsole};
use kern::slots::{Envelope, Reply, Servicers};
use kern::system::{Hooks, LogTee, Privilege, System};
use netdev::Device;
use sync::mpsc::Mailbox;
use sync::{Local, Signal};
use vboard::slots::staged_role;

// Precies één board. Twee geven al twee keer `vboard`, geen geeft geen
// `Machine`; deze som zegt het in woorden.
const _: () = assert!(
    cfg!(feature = "board-qemuvirt") as u8
        + cfg!(feature = "board-rpi4") as u8
        + cfg!(feature = "board-rpi5") as u8
        + cfg!(feature = "board-rk3566") as u8
        + cfg!(feature = "board-uefi") as u8
        + cfg!(feature = "board-o6n") as u8
        + cfg!(feature = "board-altra") as u8
        + cfg!(feature = "board-qemuvirt-riscv") as u8
        + cfg!(feature = "board-licheerv") as u8
        + cfg!(feature = "board-apple") as u8
        == 1,
    "kies precies één board: --features board-qemuvirt, board-rpi4, board-rpi5, board-rk3566, board-uefi, board-o6n, board-altra, board-qemuvirt-riscv, board-licheerv of board-apple"
);

// Het board onder een neutrale naam: zichzelf als `Machine`, met het
// contract van `board::Board`. Daarnaast lezen de slot-, kooi- en flip-lijm
// (slots.rs, cage.rs, flip.rs) het plan van het board als `vboard::slots`
// (`plan(cores, os_core)`, `mpidr`, `core_of`, de staging, de
// flip-adressen, de zwarte doos): constanten in const-context, die elk
// board met zijn eigen getallen levert. De O6N en de Altra zijn het
// UEFI-board met hun eigen NIC, NVMe en thermometer (`board_uefi::On`); de
// riscv64-boards (docs/boards-riscv.md) bouwen op
// `--target riscv64gc-unknown-none-elf`.
#[cfg(feature = "board-altra")]
extern crate board_altra as vboard;
#[cfg(feature = "board-apple")]
extern crate board_apple as vboard;
#[cfg(feature = "board-licheerv")]
extern crate board_licheerv as vboard;
#[cfg(feature = "board-o6n")]
extern crate board_o6n as vboard;
#[cfg(feature = "board-qemuvirt")]
extern crate board_qemuvirt as vboard;
#[cfg(feature = "board-qemuvirt-riscv")]
extern crate board_qemuvirt_riscv as vboard;
#[cfg(feature = "board-rk3566")]
extern crate board_rk3566 as vboard;
#[cfg(feature = "board-rpi4")]
extern crate board_rpi4 as vboard;
#[cfg(feature = "board-rpi5")]
extern crate board_rpi5 as vboard;
#[cfg(feature = "board-uefi")]
extern crate board_uefi as vboard;

/// Het board van deze binary: één, gekozen door een feature. Wat de kern
/// van hem vraagt, staat in het contract (`board::Board` en zijn drie
/// traits); daarnaast alleen het plan in `vboard::slots`.
type Machine = vboard::Machine;

/// De schijf van het board (`Board::probe_disk`): de bench leent hem, de
/// opslag neemt hem.
type Disk = <Machine as Board>::Disk;

static BOARD: Machine = Machine::new();

/// De architectuur van deze binary, voor de runtime-regel.
const ARCH: &str = if cfg!(target_arch = "riscv64") {
    "riscv64"
} else {
    "aarch64"
};

/// De teller waarin de meetlat van de OS-core (`el2::OS_STATS.ticks`)
/// telt: CNTFRQ op arm64, de timebase van de TIME-CSR op riscv64.
const OS_HZ: fn() -> u64 = if cfg!(target_arch = "riscv64") {
    cpu::riscv::idle::hz
} else {
    cpu::idle::freq
};

/// De heap: de allocator met vrije lijsten en een plafond over de kern-RAM
/// (`heap`, docs/heap.md).
#[global_allocator]
static HEAP: Heap = Heap::new();

/// De executor van core 0. Alleen de taken op deze core raken hem aan.
static EXEC: Local<Executor> = Local::new(Executor::new());

/// Dereks bunny, het origineel (2026-07-11), letterlijk uit de Go-main:
/// oren netjes boven het snuitje. Bewust geen architectuur in de tagline
/// (ARM64 is het heden, maar AMD64-boardjes liggen al klaar). Op de UART
/// als banner; de lege regel scheidt hem van de log.
const BUNNY: [&str; 5] = [
    r#"   (\(\"#,
    r#"   ( -.-)     HopOS"#,
    r#"   o_(")(")   --------------"#,
    r#"              the Rust-only OS"#,
    "",
];

/// De ingang uit `cpu::boot`: `dtb` is x0 van de firmware, `el` het
/// exception level waarop we binnenkwamen.
#[unsafe(no_mangle)]
extern "C" fn kmain(dtb: u64, el: u64) -> ! {
    let board: &'static Machine = &BOARD;
    let uart = board.console();
    cpu::console::set_sink(uart);
    // De ring van de TCP-console achter de UART (conport.rs); het glas komt
    // er in `setup` achter, in dezelfde tee.
    conport::install(uart);

    println!();
    for line in BUNNY {
        println!("{line}");
    }
    println!();

    println!(
        "runtime {} none/{} (HopOS v{})",
        env!("HOPOS_RUSTC"),
        ARCH,
        env!("CARGO_PKG_VERSION")
    );

    // Vóór alles: het privilege-niveau. De kooi is een invariant, geen
    // optie; kunnen we hem niet zetten, dan starten we niet.
    let el = u8::try_from(el).unwrap_or(0);
    if let Err(e) = board.privilege(el) {
        println!("FAIL boot: {e}\nHOPOS_BOOT_EL");
        cpu::boot::park();
    }
    println!("{}", board.firmware());

    board.init_heap(&HEAP);
    board.discover(dtb);
    // De ring van 5555 opnieuw aan deze core: op de M4 kent `this_core`
    // de cores pas na de ADT (in `discover`), en daarvóór was dit core 0.
    // Zonder deze regel viel alles tot `conport::here` weg, de landing van
    // een flip en de zwarte doos incluis (01-10).
    conport::here();
    // De OS-core (PORT.md beslissing 2): de kern woont op de core die de
    // bootparameter kiest. Staat hij daar nog niet, dan verhuist hij nu,
    // vóór er één lijn, timer of bewoner aan deze core hangt; deze core
    // wordt dan een app-core.
    move_to_os_core(board, dtb, el);
    boot(board, dtb, el)
}

/// De `dtb` en het EL van de boot, voor de kern na zijn verhuizing.
static BOOT_ARGS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Verhuist de kern naar de OS-core als hij daar niet draait: die core komt
/// op met hetzelfde EL2-regime en een verse stack en gaat verder in
/// [`boot`], en deze core wacht tot de kooi-regio er is en parkeert dan als
/// app-core (`cpu::el2::hold`; de kooi haalt hem daar op). Keert alleen
/// terug als er niet verhuisd hoeft te worden, of als het niet kan (luid:
/// de kern blijft dan waar hij is).
fn move_to_os_core(board: &'static Machine, dtb: u64, el: u8) {
    let (os, why) = board.os_core();
    let here = board.this_core();
    if let Some(why) = why {
        println!(
            "oscore: hopos.oscore not honoured ({why}), the kern stays on core {here} HOPOS_OSCORE_FALLBACK"
        );
        return;
    }
    if os == here {
        return;
    }
    BOOT_ARGS[0].store(dtb, Relaxed);
    BOOT_ARGS[1].store(u64::from(el), Relaxed);
    let target = vboard::slots::mpidr(os);
    println!(
        "oscore: moving the kern from core {here} to core {os} (mpidr {target:#x}) HOPOS_OSCORE_MOVE"
    );
    match cpu::smp::start_one(os, target, moved) {
        Ok(()) => cpu::el2::hold(),
        Err(e) => println!("oscore: {e}, the kern stays on core {here} HOPOS_OSCORE_FALLBACK"),
    }
}

/// De kern op de OS-core na de verhuizing: verder waar `kmain` was.
fn moved(core: usize) -> ! {
    println!("oscore: the kern runs on core {core} HOPOS_OSCORE_UP");
    let el = u8::try_from(BOOT_ARGS[1].load(Relaxed)).unwrap_or(0);
    boot(&BOARD, BOOT_ARGS[0].load(Relaxed), el)
}

/// De boot vanaf de landing, op de OS-core: de executor en al zijn taken.
///
/// Twee helften, en dat is de les van 30-09: alles wat de boot opbouwt
/// (switch, pomp, actoren, de futures vóór hun `Box`) stond in één frame van
/// 125 KB (`sub sp, 0x1e9a0`), en dat frame bleef onder `exec.run` staan
/// zolang de kern leeft. Een plaatsing piekte daarop tot 186 van de 256 KB
/// stack. [`setup`] bouwt alles in een eigen frame dat weg is vóór de
/// executor draait; hier blijft alleen de slaper over.
fn boot(board: &'static Machine, dtb: u64, el: u8) -> ! {
    let exec: &'static Executor = EXEC.get();
    // De deur van de switch om de slaper heen (`net::doorbell`): zonder
    // hem wachtte elk frame van een app op de failsafe van 1 ms.
    let mut sleeper = net::doorbell(setup(board, dtb, el));
    exec.run(&mut sleeper)
}

/// Bouwt de boot op: de landing, de opslag, het net, de slots en de
/// taken. Nooit inline: zijn frame (de grote boot-locals) moet weg zijn
/// voordat [`boot`] de executor start.
///
/// En zelf houdt hij zijn helpers buiten zijn frame: elke `start` die een
/// future bouwt en spawnt (opslag, slots, flip, USB, de bank) staat op
/// `#[inline(never)]`. Ingelijnd lagen hun futures naast elkaar in dit ene
/// frame (200 KB, de USB-future alleen al 125 KB in de gui-smaak) en kwam
/// `nic_up` daar nog bovenop: op de Altra 264 KB van de 256 KB stack,
/// zonder wachtpagina op UEFI, dus stil over `.bss` heen en dood vóór het
/// net (04-10, sinds 302d257 vier KB dieper; A13b en elke kern daarna).
/// Los zijn het buren: de diepste boot is daar 156 KB, warm en koud.
#[inline(never)]
fn setup(board: &'static Machine, dtb: u64, el: u8) -> <Machine as Board>::Sleeper {
    // FLIP: de landing, als eerste in `setup`. Daarvóór gingen in `kmain`
    // alleen de heap, `discover` (het board) en de verhuizing naar de
    // OS-core. Een overdracht van een vorige kern draagt de bewoners; die
    // adopteren de slots straks.
    let landing = flip::land(dtb);
    // De watchdog van de vertrekkende kern loopt door: meteen herladen, vóór
    // het framebuffer-geduld en de rest van de boot (de Pi 5, 30-09).
    if landing.is_some() {
        watchdog::pet_now();
    }

    let exec: &'static Executor = EXEC.get();
    exec.set_clock(board.clock());

    println!(
        "boot: HopOS v{} on {}, EL{el}, {} cores ({}), {} MB DRAM HOPOS_BOOT gen={} stamp={}",
        env!("CARGO_PKG_VERSION"),
        Machine::NAME,
        board.cores(),
        board.core_class(0),
        board.mem_total() >> 20,
        flip::generation(), // FLIP: 1 koud, N+1 na een flip vanaf N
        env!("HOPOS_STAMP"),
    );

    // De ring van de TCP-console is vanaf hier van de OS-core (na een
    // verhuizing schreef de boot-core hem tot nu), dan de console op het
    // glas, op dezelfde core (gui.rs): de bunny als kop, de log eronder.
    // Zonder framebuffer of kaal gebouwd: één regel of niets.
    conport::here();
    gui::init_framebuffer_console(board);
    // De UART als lezer van de ring (conport.rs): vanaf de eerste ronde van
    // de executor wacht een regel op de OS-core niet meer op de baudrate.
    if exec.spawn(conport::drain(exec)).is_err() {
        println!(
            "console: UART pump not spawned, every line waits for the UART HOPOS_CONSOLE_WAITING"
        );
    }

    // De wandklok vóór er een bewoner is. Na een flip draagt de overdracht
    // de offset van de vorige kern (Hop had hem via SNTP gezet, en de
    // teller liep door de sprong heen door); anders een vaste waarde, luid,
    // tot Hop hem zet. Hop stempelt zijn taken ermee (clock.rs).
    if clock::offset() != 0 {
        println!(
            "clock: wall clock carried over the flip (offset {} ns) HOPOS_CLOCK_CARRIED",
            clock::offset()
        );
    } else {
        let off = clock::set(
            clock::BOOT_WALL_SECS.saturating_mul(1_000_000_000),
            exec.now(),
        );
        println!(
            "clock: no SNTP yet, wall clock fixed at 2026-09-29T00:00:00Z (unix {} s, offset {off} ns) HOPOS_CLOCK_FIXED",
            clock::BOOT_WALL_SECS
        );
    }

    // Het zaad van de slots vóór er een kooi is: de DRBG van de kern is
    // geseed (door het board, anders hier), en de regel zegt wat een slot
    // op zijn control-page krijgt (seed.rs).
    seed::init();

    // Wat QEMU stagede, en of er dus een Hop is: alleen dan bestaat het
    // token, en het hoort bij het slot waar de kern Hop plaatst, vóór de
    // system-listener de eerste verbinding ziet (PORT.md beslissing 1).
    let role = staged_role();
    // FLIP: een geadopteerde Hop draagt zijn bevoegdheid mee, ook als deze
    // kern zelf geen staging ziet. De M4 van 03-10: Hop ingebakken in de
    // kaart-kern (EMBED=), de bundel zonder, dus `staged_role` zei "app"
    // en elke store-call van de meegenomen Hop was "without privilege".
    let hop_adopted = landing
        .as_ref()
        .is_some_and(|h| h.slots.iter().any(|st| st.slot == slots::HOP_SLOT));
    // Of er een Hop is, één keer beslist: het token en de watchdog (die
    // bewaakt dan zijn hartslag) kijken naar hetzelfde (24-10, F6).
    let hop_here = role == Ok(StagedRole::Hop) || hop_adopted;
    let privilege = if hop_here {
        kern::Slot::new(slots::HOP_SLOT).and_then(Privilege::boot)
    } else {
        None
    };
    if let Some(p) = &privilege {
        println!(
            "system: privilege minted for slot {} (Hop{}) HOPOS_PRIVILEGE",
            p.slot(),
            if hop_adopted && role != Ok(StagedRole::Hop) {
                ", carried over the flip"
            } else {
                ""
            }
        );
    }
    // De interrupts vóór elk device: de schijf en de NIC bedraden hun lijn
    // in hun probe (docs/sequence-hopos.md §14.1 O3). Zonder interrupts geen
    // node (§13.3): hard, zoals een panic (FATAL: `fatal()` van agent D).
    let bell = board.start_interrupts().expect("irq HOPOS_IRQ_FAIL");
    exec.spawn(irq_dispatch(board, bell)).expect("spawn irq");

    // De opslag vóór de system-API: die krijgt de hopfs-actor alleen als
    // er een schijf is (anders weigert elke bestandscall luid). De schijf
    // wordt één keer geprobed; de meetbanken (bench.rs, alleen met
    // hopos.nvmebench of hopos.idlestat) lenen hem vóór de opslag hem mount.
    let disk = bench::start(exec, storage::probe());
    let disk = storage::start(exec, disk);
    // MEDIA: de codec-dienst ná de opslag, want de firmware-blobs staan op
    // het volume (codec.rs); zonder VPU meldt hij luid dat er geen is.
    codec::up(exec);
    let system = system(privilege, disk.is_some(), slot_count(board));

    exec.spawn(tick(exec)).expect("spawn tick");
    #[cfg(feature = "hopcost")]
    exec.spawn(hopcost(exec)).expect("spawn hopcost");
    // De node-watchdog (op een flip-boot meteen met de boot-guard) en de
    // telemetrie: thermiek en klokbeleid.
    watchdog::start(exec, hop_here);
    telemetry::start(exec);
    load::start(exec); // de meetlat per slot (docs/apps.md)
    gui::start_screen_status(exec); // de meetregels naast de bunny

    // De config van de node: het bestand en de bootargs van het board
    // (kern::nodecfg; het venster in het image wint, board::cfgwin), en
    // alleen op QEMU, dat geen bootmedium heeft, voor Hop de vaste
    // bankconfig eronder (de laatste waarde wint, dus de rest wint van de
    // bank). Hier al, vóór het net: de console over TCP (conport.rs) kiest
    // uit dezelfde config, en haar listener start zodra de lease er is, ook
    // na een flip waarin Hop niet opnieuw wordt geplaatst.
    match board::cfgwin::state() {
        board::cfgwin::State::Config(n) => println!(
            "cfg: hopos.cfg from the window in the kernel image, {n} bytes HOPOS_CFG_WINDOW"
        ),
        board::cfgwin::State::Bad => println!(
            "cfg: the window in the kernel image has a crooked head or no UTF-8, ignored HOPOS_CFG_BAD"
        ),
        board::cfgwin::State::Empty => {}
    }
    let mut node_cfg = kern::nodecfg::NodeCfg::new(board.config(), board.bootargs());
    if Machine::NO_BOOT_MEDIUM && role == Ok(StagedRole::Hop) {
        node_cfg = node_cfg.on_bank(kern::nodecfg::QEMU_CFG);
    }
    conport::enable(kern::nodecfg::console_enabled(&node_cfg));
    REPLAY_AT.store(kern::nodecfg::replay_after(&node_cfg), Relaxed);
    TICK_LOG.store(node_cfg.one("hopos.tick") == "1", Relaxed);
    OS_ASID.store(node_cfg.one("hopos.oscore.asid") != "0", Relaxed);

    // Het netwerkvlak (net.rs): de pomp op de NIC, de switch, poort 0 met
    // de node-stack, DHCP en de system-listener. Zonder NIC draait de kern
    // door zonder net; dat is een board, geen fout. Een NIC die faalt (geen
    // link), probeert [`nic_retry`] opnieuw, naast de rest van de boot.
    match board.probe_nic() {
        Ok(Some(nic)) => nic_up(exec, board, system, nic),
        Ok(None) => println!("net: no NIC on this board HOPOS_NIC_NONE"),
        Err(e) => {
            println!("net: {e} HOPOS_NIC_FAIL");
            if matches!(e, board::Error::Nic(_)) {
                NIC_RETRY.store(true, Relaxed);
                if exec.spawn(nic_retry(exec, board, system)).is_err() {
                    NIC_RETRY.store(false, Relaxed);
                    println!("net: retry task not spawned, no net until a reboot HOPOS_NIC_FAIL");
                }
            }
        }
    }
    // De USB-invoer na het netwerk (gui.rs): de stroom naar de display-app
    // loopt over de switch.
    #[cfg(feature = "media")]
    optical::start(exec);
    gui::start_usb_input(exec);

    // De slots: de kooi-lijm, de lifecycle-actor en de servicers (slots.rs).
    // FLIP: na een landing adopteren de slots de bewoners in plaats van
    // Hop opnieuw te plaatsen; de flip-taak en de guard spawnen hier.
    flip::start(exec, landing.is_some());
    slots::start(exec, role, landing.map(|h| h.slots), node_cfg, system);

    // De OS-core (PORT.md beslissing 2): de idle van de kern is de rotatie
    // over zijn bewoners (Hop). Lukt dat niet, dan houdt de kern zijn core
    // voor zichzelf, luid, en draait Hop nooit.
    let mut sleeper = board.sleeper();
    match slots::os_core() {
        Ok(os) => sleeper.host(os),
        Err(e) => println!("oscore: {e}, the kern keeps its core to itself HOPOS_OS_CORE_FAIL"),
    }
    sleeper
}

/// Het netwerkvlak op een NIC die opkwam, bij de boot of bij een
/// [`nic_retry`].
fn nic_up(
    exec: &'static Executor,
    board: &'static Machine,
    system: &'static KernSystem,
    nic: <Machine as Board>::Nic,
) {
    println!("net: nic up HOPOS_NIC_UP mac={}", nic.mac());
    let params = net::Params {
        max_slots: slot_count(board),
        now: board.clock(),
        slot_wake: slots::wake,
        resident: slots::resident,
    };
    if let Err(e) = net::start(exec, nic, params, system_api(system)) {
        println!("net: {e} HOPOS_NET_FAIL");
    }
}

/// Loopt er een [`nic_retry`]? Dan wacht de plaatsing van Hop op de lease
/// in plaats van na tien seconden zonder adres te starten (slots.rs,
/// `wait_uplink`). Eén schrijver: de boot en daarna de retry-taak.
pub(crate) static NIC_RETRY: AtomicBool = AtomicBool::new(false);

/// Geen net bij de boot is geen eindtoestand (Go, cmd/hopos/main.go, 19-09,
/// Derek: "als alles opstart zonder netwerk wil ik niet ineens 100 dode
/// nodes hebben"): een kabel die er straks in gaat, een switch die later
/// opkomt. Dus de probe opnieuw, met een pauze die zoals in Go van 5 tot
/// 30 s oploopt, een regel per poging, tot de NIC er is.
///
/// De probe spint op de linktermijn van het board (8 s; de O6N 12 s bij
/// de boot en 7 s bij een retry, onder zijn watchdog van 8,6 s) en houdt
/// zo lang deze executor vast. Daarom vlak ervoor een aai: de watchdog
/// staat nu wel gewapend (de eerste probe liep ervoor).
async fn nic_retry(exec: &'static Executor, board: &'static Machine, system: &'static KernSystem) {
    let mut wait = core::time::Duration::from_secs(5);
    let mut attempt = 1u32;
    loop {
        exec.after(wait).await;
        watchdog::pet_now();
        match board.probe_nic() {
            Ok(Some(nic)) => {
                println!("net: the NIC came up on retry {attempt} HOPOS_NIC_RETRY_OK");
                NIC_RETRY.store(false, Relaxed);
                nic_up(exec, board, system, nic);
                // FLIP: wat de adoptie zonder switch niet kon doorzetten.
                slots::republish(exec);
                return;
            }
            Err(e @ board::Error::Nic(_)) => {
                if wait.as_secs() < 30 {
                    wait += core::time::Duration::from_secs(5);
                }
                println!(
                    "net: {e}, retry {attempt}, next in {} s HOPOS_NIC_RETRY",
                    wait.as_secs()
                );
            }
            Ok(None) | Err(_) => {
                println!("net: the NIC is gone on retry {attempt}, no more retries HOPOS_NIC_FAIL");
                NIC_RETRY.store(false, Relaxed);
                return;
            }
        }
        attempt = attempt.saturating_add(1);
    }
}

/// Meetlat van de IRQ-dispatch: timer-, NIC- en onbekende interrupts.
static IRQS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// De IRQ-dispatch: de vector zette alleen een vlag en luidde de bel, en
/// hier wordt geclaimd, behandeld en afgesloten. Buiten exception-context,
/// dus gewoon code.
async fn irq_dispatch(board: &'static Machine, bell: &'static Signal) {
    loop {
        bell.wait().await;
        dispatch_once(board);
    }
}

/// Eén ronde van de dispatch van het board, geteld in [`IRQS`].
fn dispatch_once(board: &Machine) {
    let d = board.dispatch_interrupts();
    IRQS[0].fetch_add(u64::from(d.timer), Relaxed);
    IRQS[1].fetch_add(u64::from(d.nic), Relaxed);
    IRQS[2].fetch_add(u64::from(d.other), Relaxed);
}

/// Handelt af wat al bij de controller wacht, buiten de dispatch-taak om:
/// voor de zelftest van de OS-core, die in de boot draait, vóór de executor
/// de taak ooit pollt (`cpu::el2::selftest_tries`). Dezelfde context als
/// de taak (de executor-core van de kern), en dezelfde ronde, dus de
/// signalen die hij zet ziet de taak straks gewoon.
pub(crate) fn drain_interrupts() {
    dispatch_once(&BOARD);
}

/// De diepte van de lifecycle-inbox: een verzoek per system-verbinding plus
/// de boot-code, met marge.
pub(crate) const LIFECYCLE_DEPTH: usize = 8;

/// De inbox van de lifecycle-actor. De actor zelf is van het slot-spoor
/// (slots.rs); de system-listener stuurt hier zijn verzoeken heen.
pub(crate) static LIFECYCLE: Mailbox<Envelope<'static>, LIFECYCLE_DEPTH> = Mailbox::new();

/// De servicers van alle slots: de lifecycle-actor schrijft, `admit` leest.
/// Zolang er geen servicer leeft, laat de system-listener niemand toe.
pub(crate) static SERVICERS: Servicers = Servicers::new();

/// De rij van de store-ops (`kern::store`): de apps zetten er hun
/// store-calls in, Hop haalt ze op. Een `static`: acht plaatsen van
/// 10,5 KiB horen in .bss, niet op de heap van de kern.
static STORE: kern::store::StoreQueue = kern::store::StoreQueue::new();

/// De system-API van deze boot, gedeeld door alle verbindingstaken
/// (net.rs), met het token van Hop als die er is.
///
/// Geen `static`: `Privilege::boot` is geen `const` (hij slaat het token
/// precies één keer), dus de API wordt één keer bij boot gebouwd en leeft
/// daarna voor altijd. Boot-code: een heap die dit niet kan geven, is
/// parkeren.
#[inline(never)] // eigen frame, niet in dat van `setup`
fn system(privilege: Option<Privilege>, fs: bool, max_slots: usize) -> &'static KernSystem {
    let mut s = System::new(&LIFECYCLE, &SERVICERS, privilege, max_slots)
        .with_logs(&slots::LOGS)
        .with_kern_log(&conport::KERN_LOG)
        .with_store(&STORE);
    if fs {
        s = s.with_fs(&storage::FS_INBOX);
    }
    #[cfg(feature = "media")]
    {
        s = s.with_devices(&optical::INBOX);
    }
    Box::leak(Box::new(s))
}

/// De antwoordplekken van de system-API: één per verbindingstaak, zodat
/// een antwoord van de actor nooit bij een andere verbinding landt.
static SYSTEM_REPLIES: [Reply; net::SYSTEM_WORKERS] = [const { Reply::new() }; net::SYSTEM_WORKERS];

/// De system-API van de kern: één, bij boot gebouwd ([`system`]).
pub(crate) type KernSystem = System<'static, 'static, LIFECYCLE_DEPTH>;
/// De console van de system-API: de kernconsole met de logrij van de slots.
type SystemLog = LogTee<'static, KernConsole>;

/// De haken en de console van de system-API, gedeeld door alle taken.
static SYSTEM_HOOKS: BootHooks = BootHooks;
static SYSTEM_LOG: SystemLog = LogTee::new(KernConsole, &slots::LOGS);

/// De system-API voor de listener.
fn system_api(system: &'static KernSystem) -> net::SystemApi {
    net::SystemApi {
        system,
        replies: &SYSTEM_REPLIES,
        hooks: &SYSTEM_HOOKS,
        log: &SYSTEM_LOG,
    }
}

/// Het aantal slots van dit board: één per app-core plus één voor de
/// OS-core, waar Hop naast de kern woont (het plan van het board, PORT.md
/// beslissing 2): twee cores zijn twee slots. De switch en de system-API
/// kennen hetzelfde getal. Tot 01-10 kreeg de system-API het plafond uit de
/// ABI (128): een volle Pi 4 zocht dan door tot slot 5, de lifecycle
/// weigerde die met "slot 5 out of range 1..4" in plaats van "full", Hop las
/// daar geen capaciteitstekort in en bleef elke paar seconden opnieuw
/// vragen, en elke vraag was 128 statuscalls door de lifecycle-mailbox: een
/// koude flip vond hem vol ("actor mailbox full").
pub(crate) fn slot_count(board: &Machine) -> usize {
    // Kooien tellen niet als cores: het plan van het board zegt hoeveel.
    vboard::slots::plan(board.cores(), 0).map_or(2, |p| p.max_slots())
}

/// De haken van de system-API: de klok van Hop zet de wandklok van de kern
/// (en daarmee die van elke kooi); de flip toetst de bundel en legt de
/// nieuwe kern klaar (flip.rs), de sprong komt van de flip-taak; de
/// herstart van Hop toetst en neemt het image over (slots.rs), de stop en
/// de plaatsing komen van de supervisor van Hop.
struct BootHooks;

impl Hooks for BootHooks {
    fn set_clock(&self, unix_ns: u64) {
        let off = clock::set(unix_ns, (BOARD.clock())());
        println!("system: wall clock set by Hop to {unix_ns} ns (offset {off} ns) HOPOS_CLOCK_SET");
    }
    fn flip(&self, bundle: &kern::system::FlipBundle, sha256: &[u8; 32]) -> kern::Result {
        flip::prepare(bundle, sha256) // FLIP: toetsen en klaarleggen (flip.rs)
    }
    fn hop_restart(
        &self,
        image: Option<&kern::system::HopImage>,
        grace: core::time::Duration,
    ) -> Result<(), kern::system::Fail> {
        slots::hop_restart(image, grace)
    }
    fn hop_refused(&self, why: &'static str, err: &kern::system::Fail) {
        println!("system: Hop restart refused: {err} HOPOS_HOP_RESTART_REFUSED {why}");
    }
    /// Slot 0: de slaap van de executor (`slept_ns`, `sleeps`, dezelfde als
    /// `busy_ms` van de tik) en zijn klok, ná de slaap gelezen: de system-API
    /// draait in een ronde, dus elke slaap in `slept_ns` is al voorbij.
    fn kern(&self) -> kern::cage::Status {
        let exec: &Executor = EXEC.get();
        let idle_ns = exec.stats.slept_ns.load(Relaxed);
        kern::cage::Status {
            heartbeat: TICKS.load(Relaxed),
            ram_size: BOARD.plan().kern_ram.size,
            mem_sys: HEAP.stats().used as u64,
            idle_ns,
            wakes: exec.stats.sleeps.load(Relaxed),
            at_ns: exec.now(),
            ..Default::default()
        }
    }
}

/// De maat van de kern-stack: `STACK_SIZE` in elk linkscript (hopos/*.ld),
/// boven de wachtpagina (`__stack_guard`) direct boven `.bss`. De stack van
/// een verhuisde kern is even groot.
const STACK_BYTES: u64 = 0x40000;
const _: () = assert!(cpu::smp::NODE_STACK as u64 == STACK_BYTES);

/// Hoe ver onder de eigen stackpositie de meter niet wist: een IRQ-frame
/// en de vector die tijdens de meting binnenkomen, landen daar.
const STACK_METER_SLACK: u64 = 16 * 1024;

/// De diepste stack sinds de vorige meting, in bytes, en daarna het stuk
/// daaronder weer op nul (de volgende tik meet opnieuw).
///
/// De meter leest de stack van onderen tot het eerste woord dat niet nul is
/// (QEMU en elke loader geven nul-RAM; na een kern-flip is de eerste meting
/// ruis). Les van 30-09: de plaatsing van een slot (`kern::rpc::mount_table`
/// onder de lifecycle, de publicatieregel) haalde 190 KB van de 256, en een
/// overloop schreef zonder wachtpagina stil in het einde van `.bss` (de
/// switch-tabel, de deuren van de system-API, de heap). Sindsdien: het
/// boot-frame is weg vóór de executor draait ([`boot`]), de lifecycle bouwt
/// zijn actor buiten zijn future, en de boards met `cpu::boot` en de Mac
/// mini zetten een wachtpagina onder de stack (`__stack_guard`; de
/// UEFI-boards nog niet, zie `cpu::boot`). Na de boot meet de tik 16 KB
/// (dat is de meetmarge zelf), tegen 180 KB ervoor. Eén scan van 32K
/// woorden per seconde; de schrijfslag alleen over wat de vorige tik vuil
/// maakte.
///
/// Na een verhuizing (`hopos.oscore`) draait de kern op de stack van
/// `cpu::smp::start_one` in de heap, en die meet hij dan. Staat de eigen
/// stackpositie niet in de stack die hij meet, dan niets: tot 04-10 wiste
/// de meter van de verhuisde kern van de boot-stack tot aan zijn eigen sp,
/// over alles wat ertussen lag.
fn stack_high_water() -> u64 {
    unsafe extern "C" {
        safe static __bss_end: u8;
        safe static __stack_top: u8;
    }
    let (bottom, top) = match cpu::smp::moved_stack() {
        Some(s) => s,
        None => {
            let top = (&raw const __stack_top).addr() as u64;
            let bottom = top.saturating_sub(STACK_BYTES);
            if bottom < (&raw const __bss_end).addr() as u64 {
                return 0; // Een linkscript met een andere indeling: niet meten.
            }
            (bottom, top)
        }
    };
    let here = 0u64;
    let sp = (&raw const here).addr() as u64;
    if !(bottom..top).contains(&sp) {
        return 0;
    }
    let mut p = bottom;
    while p < top && dev::read64(dev::Pa(p)) == 0 {
        p += 8;
    }
    let used = top - p;
    let mut z = p;
    while z < sp.saturating_sub(STACK_METER_SLACK) {
        dev::write64(dev::Pa(z), 0);
        z += 8;
    }
    used
}

/// De hartslag: elke seconde één regel met het tiknummer en de meetlat van
/// de executor.
/// `hopos.replay=N` (kern::nodecfg): de tik waarop de kern het begin van zijn
/// console herhaalt; 0 = nooit.
static REPLAY_AT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// `hopos.tick=1`: de tikregel elke seconde op de console, niet alleen de eerste drie.
static TICK_LOG: AtomicBool = AtomicBool::new(false);
/// `hopos.oscore.asid=0`: op riscv64 een TLB-flush bij elke wissel op de
/// OS-core in plaats van een ASID per bewoner (cpu/src/riscv/oscore.rs), de
/// vergelijking op ijzer. Standaard aan; arm64 heeft zijn VMID altijd.
#[cfg_attr(not(target_arch = "riscv64"), allow(dead_code))]
static OS_ASID: AtomicBool = AtomicBool::new(true);
/// Het tiknummer: de hartslag van slot 0, de kern (SLOT_STATUS).
static TICKS: AtomicU64 = AtomicU64::new(0);

/// De meetlat van een hop tussen twee bewoners van de OS-core (feature
/// `hopcost`, cpu/src/hopcost.rs): de rondreizen op de console zodra het
/// 200 ms stil is, zodat de UART niet in de meting valt.
#[cfg(feature = "hopcost")]
async fn hopcost(exec: &'static Executor) {
    let mut seen = 0;
    loop {
        exec.after(core::time::Duration::from_millis(200)).await;
        let n = cpu::hopcost::recorded();
        if n == seen {
            cpu::hopcost::drain(&mut |a| println!("{a}"));
        }
        seen = n;
    }
}

async fn tick(exec: &'static Executor) {
    let start = exec.now();
    let mut n: u64 = 0;
    // De slaaptijd van de vorige tik (executor `slept_ns`): de rest van de
    // tik was werk op de OS-core (`busy_ms`).
    let (mut at, mut slept) = (start, 0u64);
    let (mut refused, mut dropped) = (0u64, 0u64);
    loop {
        n += 1;
        // Een late lezer (de M4 over de dockchannel) krijgt de boot alsnog:
        // elke N tikken de kop, de oudste 16 KiB van de ring en de staart,
        // zodat een lezer die pas na een kabel-herplug aanhaakt er een vangt.
        let replay_at = REPLAY_AT.load(Relaxed);
        if replay_at != 0 && n.is_multiple_of(replay_at) {
            println!(
                "console: replaying the first 16 KiB of this boot for a late reader (hopos.replay={replay_at}) HOPOS_CONSOLE_REPLAY"
            );
            let bytes = conport::replay(16 * 1024);
            println!("console: end of the replay ({bytes} bytes) HOPOS_CONSOLE_REPLAY_END");
        }
        // De diagnose van de NIC op 5 en 30 s (de RP1-keten van de Pi 5, de
        // flip-jacht van 30-09): een koude boot geeft de referentie, een
        // landing het verschil.
        if n == 5 || n == 30 {
            BOARD.nic_diag();
        }
        let due = start.saturating_add(n.saturating_mul(1_000_000_000));
        exec.until(due).await;
        TICKS.store(n, Relaxed);
        // Hoe laat deze tik kwam. Een kern die een tijd niets rondmaakte
        // (30-09, tot alpha.14: de periodieke hopfs-commit wachtte synchroon
        // op twee FLUSHes van de schijf, op macOS F_FULLFSYNC's van het
        // image, en de hele OS-core met Hop stond dan stil) haalt zijn
        // gemiste tikken daarna in één salvo in; late_ms zegt dan hoe lang
        // de stilte was.
        let late_ms = exec.now().saturating_sub(due) / 1_000_000;
        let s = &exec.stats;
        let (now, slept_now) = (exec.now(), s.slept_ns.load(Relaxed));
        let busy_ms = now
            .saturating_sub(at)
            .saturating_sub(slept_now.wrapping_sub(slept))
            / 1_000_000;
        (at, slept) = (now, slept_now);
        // De OS-core: overgangen naar een bewoner, waardoor de kern terugkwam,
        // en de tijd die de bewoners kregen. Zonder deze getallen is "Hop
        // krijgt tijd" niet te onderscheiden van "de kern spint".
        let o = &cpu::el2::OS_STATS;
        // De laatste beurt en de langste sinds de vorige tik (30-09: de
        // stilte van de hoplb-kring was niet te lezen zonder te weten wie de
        // OS-core het laatst had), en de switch: werk na de bel, volle en
        // gedropte RX, en wat de NAT weggooide.
        let last = o.last.load(Relaxed);
        let long_us = o.longest.swap(0, Relaxed) / (OS_HZ() / 1_000_000).max(1);
        let sw = &net::STATS;
        let stack_kb = stack_high_water() / 1024;
        // Een kernheap die weigert, zegt dat altijd, ook zonder
        // `hopos.tick=1`. De O6N op 30-09: de bump-heap van toen was op, elke
        // START_SLOT faalde met "out of memory", Hop las dat als
        // "unplaceable", en de console zweeg tot een koude boot.
        let h = HEAP.stats();
        if h.refused != refused {
            println!(
                "heap: {} allocation(s) refused, {} KB used, {} KB free HOPOS_HEAP_REFUSED",
                h.refused.wrapping_sub(refused),
                h.used / 1024,
                h.free / 1024
            );
            refused = h.refused;
        }
        // Een taak die geen plaats in de tabel van de executor kreeg, is weg
        // terwijl zijn `spawn` Ok gaf: net zo luid, ook zonder `hopos.tick=1`.
        let d = s.dropped.load(Relaxed);
        if d != dropped {
            println!(
                "executor: {} task(s) dropped, the task table is full HOPOS_TASK_DROPPED",
                d.wrapping_sub(dropped)
            );
            dropped = d;
        }
        // De eerste drie tikken altijd (de QEMU-toetsen lezen HOPOS_TICK 3),
        // daarna alleen met `hopos.tick=1`: een node op het LAN hoeft zijn
        // console niet elke seconde vol te zetten (Derek, 02-10).
        if n > 3 && !TICK_LOG.load(Relaxed) {
            continue;
        }
        println!(
            "HOPOS_TICK {n} late_ms={late_ms} busy_ms={busy_ms} sleeps={} polls={} irq(timer={} nic={} other={}) os(in={} irq={} ipi={} timer={} yield={} exit={} fault={} idle={} res_ms={} kicks={}) turn(last={}:{} long_us={long_us}) stack_kb={stack_kb} sw(door={} rxfull={} rxdrop={} big={} noroute={} txdrop={} xoff={} flowfull={} natin={} natmiss={}) nic(bad={} full={} held={} err={} bells={}) waker(rounds={} seen={} kicks={} rx={}) temp={}",
            s.sleeps.load(Relaxed),
            s.polls.load(Relaxed),
            IRQS[0].load(Relaxed),
            IRQS[1].load(Relaxed),
            IRQS[2].load(Relaxed),
            o.entries.load(Relaxed),
            o.irq.load(Relaxed),
            o.ipi.load(Relaxed),
            o.timer.load(Relaxed),
            o.yields.load(Relaxed),
            o.exits.load(Relaxed),
            o.faults.load(Relaxed),
            o.idle.load(Relaxed),
            o.ticks.load(Relaxed) / (OS_HZ() / 1000).max(1),
            o.kicks.load(Relaxed),
            last & 0xff,
            cpu::el2::Back::name_of((last >> 8) & 0xff),
            sw.work_by_door.load(Relaxed),
            sw.rx_full.load(Relaxed),
            sw.rx_drops.load(Relaxed),
            sw.nat_oversize.load(Relaxed),
            sw.nat_no_route.load(Relaxed),
            sw.uplink_tx_drops.load(Relaxed),
            sw.uplink_xoff.load(Relaxed),
            sw.nat_flow_full.load(Relaxed),
            sw.nat_reply_in.load(Relaxed),
            sw.nat_in_unmatched.load(Relaxed),
            sw.nic_rx_bad.load(Relaxed),
            sw.nic_tx_full.load(Relaxed),
            sw.nic_tx_held.load(Relaxed),
            sw.nic_tx_errors.load(Relaxed),
            sw.nic_doorbells.load(Relaxed),
            slots::WAKER.rounds.load(Relaxed),
            slots::WAKER.seen.load(Relaxed),
            slots::WAKER.kicks.load(Relaxed),
            slots::WAKER.rx.load(Relaxed),
            telemetry::Temp(telemetry::temp_milli_c()),
        );
    }
}

/// De panic: de reden en een marker op de console, en parkeren. Er is geen
/// herstel (handboek §6); de watchdog is de tweede lijn.
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    cpu::console::emergency(format_args!("panic: {info} HOPOS_PANIC"));
    cpu::boot::park()
}
