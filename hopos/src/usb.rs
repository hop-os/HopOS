//! De bouwsteen `usb` van de kern-binary: de USB-taak (Go: `usbin.Start` en
//! `Manager.Run`). Geen keuze op zich: de gui-smaak trekt hem mee voor het
//! toetsenbord en de muis, de media-smaak voor de optische drive, en kaal
//! staat hier niets (off.rs; docs/sequence-hopos.md §14.3).
//!
//! Eigendom: de taak bezit de `usb::Manager` (alle controllers) als `&mut`
//! en is de enige die een xHCI aanraakt. Wat hij leest, gaat als waarde
//! weg: een toets naar het glas (`gui::UsbInput`, een SPSC-rij naar de
//! input-listener in `net::input`), een bulk-antwoord naar de optische
//! eigenaar (`optical::Bridge`). Zonder het ene of het andere vlak is die
//! kant een stub.

use crate::clock::ExecTimer;
use crate::gui::UsbInput;
use crate::optical;
use board::{Board, UsbHost, UsbHosts, UsbKind};
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering::Acquire, Ordering::Release};
use core::time::Duration;
use cpu::println;
use driver_xhci::Hc;
use executor::Executor;
use usb::register::{HostSpec, PrepareError, Registry};
use usb::storage::{BulkError, BulkId, BulkInfo, BulkReq};
use usb::{Manager, Sink};

/// De flip vraagt de USB-taak te stoppen ([`quiesce`]).
static FLIP_STOP: sync::Signal = sync::Signal::new();
/// De USB-taak meldt dat de controllers stilstaan.
static USB_QUIET: sync::Signal = sync::Signal::new();
/// Draait de USB-taak met levende controllers?
static USB_LIVE: AtomicBool = AtomicBool::new(false);

/// De sink van de manager: invoer naar het glas, opslag naar de optische
/// eigenaar, logregels naar de console.
struct UsbSink {
    #[cfg_attr(
        not(feature = "gui"),
        expect(dead_code, reason = "zonder gui gaan de toetsen nergens heen")
    )]
    input: UsbInput,
    optical: optical::Bridge,
}

impl Sink for UsbSink {
    #[cfg(feature = "gui")]
    fn input(&mut self, e: usb::Event) {
        self.input.send(e);
    }
    fn log(&mut self, args: fmt::Arguments<'_>) {
        println!("{args}");
    }
    fn storage_attached(&mut self, info: &BulkInfo) {
        self.optical.attached(info);
    }
    fn storage_gone(&mut self, id: BulkId) {
        self.optical.gone(id);
    }
    fn bulk_out(&mut self, req: &BulkReq) -> &[u8] {
        self.optical.output(req)
    }
    fn bulk_in(&mut self, req: &BulkReq) -> &mut [u8] {
        self.optical.input(req)
    }
    fn bulk_done(&mut self, req: &BulkReq, r: Result<usize, BulkError>) {
        self.optical.done(req, r);
    }
}

/// De USB na het netwerk: de invoer naar het glas loopt over de switch.
/// Vraagt het board zijn controllers (`Board::usb_hosts`: PCIe en firmware
/// zijn dan al gedaan) en spawnt de USB-taak. De invoerkant (`UsbInput`)
/// belooft zijn adres aan de grant vóór de taak (en dus vóór elke
/// plaatsing, die `main` pas hierna start): de bring-up zelf slaapt op de
/// executor, en een display-app die intussen het glas kreeg, moet het
/// adres al in zijn env hebben. Komt geen controller op, dan trekt de taak
/// het in.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor) {
    let hosts = crate::BOARD.usb_hosts();
    if hosts.is_empty() {
        println!("usb: no host controllers on this board HOPOS_USB_NONE");
        return;
    }
    let Some(input) = UsbInput::start(exec) else {
        return;
    };
    let sink = UsbSink {
        input,
        optical: optical::Bridge::default(),
    };
    if exec.spawn(run(exec, hosts, sink)).is_err() {
        println!("usb: task not spawned HOPOS_USB_FAIL");
        UsbInput::withdraw();
    }
}

/// Vóór de sprong van een flip: de USB-controllers halteren (ze zijn
/// DMA-masters op de bus; de Pi 5, 30-09) en hoogstens een seconde wachten
/// tot de USB-taak dat meldt. Zonder levende USB-taak niets.
pub(crate) async fn quiesce(exec: &'static Executor) {
    if !USB_LIVE.load(Acquire) {
        return;
    }
    FLIP_STOP.set();
    let _ = sync::select(USB_QUIET.wait(), exec.after(Duration::from_secs(1))).await;
}

/// De taak: controllers op, dan de ronde van de manager tot het einde van
/// de node (Go's `Run`).
async fn run(exec: &'static Executor, hosts: UsbHosts, mut sink: UsbSink) {
    let mut reg = Registry::new();
    for h in hosts.iter() {
        let spec = HostSpec {
            name: h.name,
            base: h.regs.base,
            bus_off: h.bus_off,
            dma: h.dma.base,
            dma_size: h.dma.size,
        };
        if reg.register(spec).is_err() {
            println!(
                "usb: {}: more controllers than the manager takes, skipped",
                h.name
            );
        }
    }
    // Het timerwiel van de executor als klok en slaap van de driver: een
    // poortreset of een commando slaapt hierop in plaats van de core vast
    // te houden.
    let t = ExecTimer(exec);
    let mut mgr = Manager::new(t);
    let live = reg
        .bring_up(&mut mgr, &mut sink, async |spec: &HostSpec| {
            make(&hosts, spec, &t).await
        })
        .await;
    if live == 0 {
        UsbInput::withdraw();
        println!("usb: no controller came up, INPUT_ADDR withdrawn HOPOS_USB_NONE");
        return;
    }
    println!(
        "usb: {live} controller(s) up, polling every {} ms HOPOS_USB_UP",
        usb::POLL_INTERVAL_NS / 1_000_000
    );
    USB_LIVE.store(true, Release);
    loop {
        if let Some(req) = sink.optical.next() {
            mgr.enqueue(req, &mut sink);
        }
        // Een flip komt (FLIP_STOP): alle controllers stil, en zeggen dat
        // het zo is. De taak eindigt; komt de sprong er niet, dan is de
        // USB weg tot de volgende boot. Het signaal telt in de stap én in
        // de slaap erna: de slaap kan een seconde zijn, en de flip wacht
        // hoogstens één (30-09: de eerste proef zag de stop nooit).
        let flip = match sync::select(mgr.step(&mut sink), FLIP_STOP.wait()).await {
            sync::Either::Left(wait) => {
                let nap =
                    sync::select(exec.after(Duration::from_nanos(wait)), optical::WAKE.wait());
                let slept = sync::select(nap, FLIP_STOP.wait()).await;
                matches!(slept, sync::Either::Right(()))
            }
            sync::Either::Right(()) => true,
        };
        if flip {
            mgr.stop_all().await;
            println!("usb: {live} controller(s) halted for the flip HOPOS_USB_HALTED");
            USB_QUIET.set();
            return;
        }
    }
}

/// Maakt de driver voor één aangeboden controller: een DWC3-core eerst in
/// hostmodus, dan de xHCI op hetzelfde venster.
async fn make(hosts: &UsbHosts, spec: &HostSpec, t: &ExecTimer) -> Result<Hc, PrepareError> {
    let Some(h) = hosts.iter().find(|h| h.name == spec.name) else {
        return Err(PrepareError {
            what: "controller not offered by the board",
            value: spec.base.0,
        });
    };
    if h.kind == UsbKind::Dwc3 {
        dwc3_host_mode(h, t).await?;
    }
    // SAFETY: het venster komt van `Board::usb_hosts`: het capability-blok
    // van een xHCI (of een DWC3 die nu in hostmodus staat), Device gemapt
    // voor altijd door het board, en `h.dma` is DMA-geheugen dat het board
    // voor deze controller alleen plande.
    Ok(unsafe { Hc::new(spec.base, spec.name, spec.bus_off) })
}

/// De DWC3-core van `h` in hostmodus (de RK3566), met de globale registers
/// in één regel: op dat silicium is de vraag niet "werkt de driver" maar
/// "staat de klok en de PHY aan" (Go, 06-08).
async fn dwc3_host_mode(h: &UsbHost, t: &ExecTimer) -> Result<(), PrepareError> {
    // SAFETY: het board noemt dit venster als DWC3-core: de globale
    // registers liggen op +0xC100 binnen `h.regs`, Device gemapt voor
    // altijd.
    let core = unsafe { driver_dwc3::Core::new(h.regs.base) };
    let r = core.host_mode(t).await;
    let g = core.regs();
    println!(
        "usb: {} dwc3 id={:08x} gctl={:08x} usb2={:08x} usb3={:08x} gsts={:08x}",
        h.name, g.id, g.ctl, g.usb2_phy, g.usb3_pipe, g.sts
    );
    r.map_err(|e| PrepareError {
        what: e.what(),
        value: e.value(),
    })
}
