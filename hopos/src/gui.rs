//! Het gui-vlak van de kern-binary: de console op het glas, de meetregels
//! naast de bunny, de toetsen en de muis van de USB naar de display-app, en
//! de framebuffer-grant aan de display-app (Go: `cmd/hopos/gui.go`,
//! `usbinput.go` en de `fb.Init`/`screenStatus` uit de main).
//!
//! Alleen met de feature `gui`; kaal staan de stubs met dezelfde
//! signatuur in off.rs (handboek §7), en linkt de binary geen regel
//! display-code. Go zei op 06-08 "geen dood gewicht": 183 KB voor de hele
//! gui-smaak op de Radxa, en een headless node draagt er niets van. De bus
//! zelf is van de bouwsteen `usb` (usb.rs), die media ook zonder gui
//! gebruikt.
//!
//! Eigendom:
//!
//! - de [`driver_fb::Console`] staat in een `LocalCell` op de OS-core. De
//!   console-haak leent hem per stuk tekst (onder het console-slot van
//!   `cpu`), de meettaak per seconde; een lening die al loopt (een
//!   noodregel uit exception-context midden in een regel) slaat het glas
//!   over, de UART krijgt alles. Een andere core tekent nooit: hij zou een
//!   `Local` van deze core aanraken.
//! - de framebuffer-grant ([`gui_fbgrant::FbGrant`]) is van de
//!   lifecycle-actor, via [`hooks::GuiGrants`] (`kern::grants::Grants`).
//! - de invoer: de USB-taak (usb.rs) bezit de zendkant van [`INPUT`]
//!   ([`UsbInput`]), de input-listener in `net.rs` (`net::input`) de
//!   ontvangkant; hij schrijft de regels aan de display-app.

use board::Board;
use core::fmt::{self, Write as _};
use core::net::Ipv4Addr;
use core::sync::atomic::{
    AtomicBool, AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release,
};
use cpu::println;
use driver_fb::{Console, Desc};
use executor::Executor;
use gui_fbgrant::FbGrant;
use gui_usbin::deliver::{Deliverer, INPUT_PORT, InputQueue, InputTx, QUEUE_DEPTH};
use sync::LocalCell;

/// De console op het glas. Alleen de OS-core raakt hem aan: de core die
/// ook de ring van de TCP-console bezit (`conport::here`, vlak vóór de
/// init), en alleen daar roept `conport::tee` [`glass`].
static GLASS: LocalCell<Console> = LocalCell::cell(Console::new());
/// Staat de console op het glas (gezet door de init)?
static GLASS_UP: AtomicBool = AtomicBool::new(false);

/// De framebuffer-grant van deze node.
static GRANT: LocalCell<FbGrant> = LocalCell::cell(FbGrant::new());

/// De kopregels: de bunny van `main`, zonder de lege scheidingsregel.
/// De meetregels komen rechts op de eerste drie.
const HEADER_ROWS: usize = 4;

/// Zet de console op het glas als het board een framebuffer geeft:
/// schone lei, de bunny als vaste kop, en vanaf nu gaat elke logregel
/// naar de UART én het scherm. Geen framebuffer is geen fout: één regel.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn init_framebuffer_console(board: &'static crate::Machine) {
    let Some(d) = board.framebuffer() else {
        println!("fb: no framebuffer on this board, console on the UART only HOPOS_FB_NONE");
        return;
    };
    normal_nc(&d);
    let r = GLASS.try_borrow_mut().map(|mut c| {
        c.init(d).map(|()| {
            bunny_header(&mut c);
            c.cells()
        })
    });
    match r {
        Ok(Ok((cols, rows))) => {
            GLASS_UP.store(true, Release);
            if let Ok(mut g) = GRANT.try_borrow_mut()
                && let Err(e) = g.offer(d)
            {
                println!("fb: not offered to a display app: {e} HOPOS_FB_ENV");
            }
            println!(
                "fb: console on {}x{} stride {} @ {:#x} ({cols}x{rows} cells) HOPOS_FB_CONSOLE",
                d.width, d.height, d.stride, d.base.0
            );
        }
        Ok(Err(e)) => println!("fb: framebuffer refused: {e} HOPOS_FB_NONE"),
        Err(_) => println!("fb: console busy at init HOPOS_FB_NONE"),
    }
}

/// Het vlak is DRAM, geen registerblok: laat het fabric de pixelstores
/// gatheren in plaats van ze als losse Device-transacties te sturen
/// (`cpu::memattr`). Weigert de map (een venster dat niet op 2 MB
/// valt), dan tekenen we zoals het board hem mapte; stil in Go, hier
/// één regel, want het is een optimalisatie en geen voorwaarde.
fn normal_nc(d: &Desc) {
    let Some(size) = d.size() else { return };
    if let Err(e) = cpu::memattr::normal_nc(d.base.0, size) {
        println!("fb: left as the board mapped it ({e})");
    }
}

/// De bunny van `main` als vaste kop, zonder de lege scheidingsregel.
fn bunny_header(c: &mut Console) {
    let mut lines = [""; HEADER_ROWS];
    for (l, b) in lines.iter_mut().zip(crate::BUNNY.iter()) {
        *l = b;
    }
    c.header(&lines);
}

/// Het glas als laatste stap van de console-tee (`conport::tee`), op de
/// OS-core.
pub(crate) fn glass(b: &[u8]) {
    if GLASS_UP.load(Acquire)
        && let Ok(mut c) = GLASS.try_borrow_mut()
    {
        c.write(b);
    }
}

/// Start de meetregels rechts naast de bunny: kern-heap als percentage,
/// datum en tijd met seconden, elke seconde. Een bevroren klok verraadt
/// zo een hangende kern meteen (Derek, 15-07).
pub(crate) fn start_screen_status(exec: &'static Executor) {
    if !GLASS_UP.load(Acquire) {
        return;
    }
    if exec.spawn(screen_status(exec)).is_err() {
        println!("fb: screen status task not spawned HOPOS_FB_STATUS_FAIL");
    }
}

async fn screen_status(exec: &'static Executor) {
    let start = exec.now();
    let mut n: u64 = 0;
    loop {
        let h = crate::HEAP.stats();
        let total = h.used.saturating_add(h.free).max(1);
        let wall = crate::clock::offset().saturating_add(exec.now()) / 1_000_000_000;
        let (date, time) = civil(wall);
        let mut mem = Line::new();
        let _ = write!(
            mem,
            "mem {}% ({}/{}MB)",
            h.used.saturating_mul(100) / total,
            h.used >> 20,
            total >> 20
        );
        if let Ok(mut c) = GLASS.try_borrow_mut() {
            c.header_status(0, mem.as_str());
            c.header_status(1, date.as_str());
            c.header_status(2, time.as_str());
        }
        n += 1;
        exec.until(start.saturating_add(n.saturating_mul(1_000_000_000)))
            .await;
    }
}

/// De houder van het glas (het bron-IP van de display-app volgt eruit),
/// voor de input-listener. `None` zonder houder, of als de grant net
/// geleend is (dan komt er niemand binnen, en de volgende regel toetst
/// opnieuw).
pub(crate) fn glass_holder() -> Option<kern::Slot> {
    GRANT.try_borrow().ok()?.holder()
}

/// De rij van de USB-taak naar de input-listener (`net::input`).
static INPUT: InputQueue = InputQueue::new();

/// Gebeurtenissen die de rij niet meer in pasten (de listener liep achter).
/// Een meting, geen fout: invoer is lossy by design.
static QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);

/// De toetsen en de muis van de USB-taak (usb.rs) naar het glas: de
/// zendkant van [`INPUT`]. Een rapport gaat als waarde naar de
/// input-listener; vol is weggooien en tellen, want de pollus mag nooit
/// wachten op een display die niet leest.
pub(crate) struct UsbInput {
    tx: InputTx<'static>,
}

impl UsbInput {
    /// Belooft het invoeradres aan de grant en spawnt de input-listener;
    /// `None` als de rij al weg is of de listener niet spawnt (één regel).
    pub(crate) fn start(exec: &'static Executor) -> Option<UsbInput> {
        let Some((tx, rx)) = INPUT.split() else {
            println!("usb: input queue already taken HOPOS_USB_FAIL");
            return None;
        };
        if let Ok(mut g) = GRANT.try_borrow_mut() {
            g.use_input(Ipv4Addr::from(abi::layout::HOST_IP4), INPUT_PORT);
        }
        let fb = GRANT
            .try_borrow()
            .ok()
            .and_then(|g| g.desc())
            .map(|d| (d.width, d.height));
        if exec
            .spawn(crate::net::input::serve(exec, Deliverer::new(rx, fb)))
            .is_err()
        {
            println!("usb: input listener not spawned HOPOS_USB_FAIL");
            Self::withdraw();
            return None;
        }
        Some(UsbInput { tx })
    }

    /// Eén gebeurtenis de rij in; vol is weggooien, één regel bij de
    /// eerste en daarna tellen.
    pub(crate) fn send(&mut self, e: usb::Event) {
        if self.tx.try_send(e).is_err() && QUEUE_DROPS.fetch_add(1, Relaxed) == 0 {
            println!(
                "usb: input queue full ({QUEUE_DEPTH} events), dropping (counted) HOPOS_USB_QUEUE_FULL"
            );
        }
    }

    /// Trekt het invoeradres in bij de grant: er komt geen invoer.
    pub(crate) fn withdraw() {
        if let Ok(mut g) = GRANT.try_borrow_mut() {
            g.stop_input();
        }
    }
}

/// Een regel van hoogstens 32 bytes op de stack: de meetregels
/// alloceren niets.
struct Line {
    buf: [u8; 32],
    len: usize,
}

impl Line {
    const fn new() -> Line {
        Line {
            buf: [0; 32],
            len: 0,
        }
    }
    fn as_str(&self) -> &str {
        self.buf
            .get(..self.len)
            .and_then(|b| core::str::from_utf8(b).ok())
            .unwrap_or("")
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        let dst = self.buf.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Datum (`dd-mm-jjjj`) en tijd (`uu:mm:ss`, UTC) uit Unix-seconden:
/// het algoritme `civil_from_days` van Howard Hinnant, zonder tabel.
fn civil(secs: u64) -> (Line, Line) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (mut date, mut time) = (Line::new(), Line::new());
    let _ = write!(date, "{d:02}-{m:02}-{y:04}");
    let _ = write!(
        time,
        "{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    (date, time)
}

/// De grant-aanbieder van de lifecycle: de framebuffer-grant, via
/// [`hooks::GuiGrants`].
pub(crate) fn slot_grants() -> hooks::GuiGrants {
    hooks::GuiGrants
}

/// De grant-haakjes voor de slot-lifecycle (`kern::grants::Grants`); de
/// lifecycle-actor (slots.rs) is hun eigenaar en roept ze.
pub(crate) mod hooks {
    use super::{GLASS, GRANT, bunny_header};
    use driver_fb::Desc;
    use gui_fbgrant::GlassConsole;
    use kern::Slot;
    use kern::cage::Console as _;
    use kern::grants::{Grants, Window, WindowMap};

    /// De console als [`GlassConsole`] voor de grant: eraf bij de toekenning,
    /// terug (schone lei, bunny) bij het vrijkomen.
    struct KernGlass;

    impl GlassConsole for KernGlass {
        fn hand_over(&mut self) {
            if let Ok(mut c) = GLASS.try_borrow_mut() {
                c.disable();
            }
        }
        fn take_back(&mut self, d: Desc) {
            if let Ok(mut c) = GLASS.try_borrow_mut()
                && c.init(d).is_ok()
            {
                bunny_header(&mut c);
            }
        }
    }

    /// Het venster in de kooi, op ijzer: het tabelblok van het slot uit het
    /// plan van deze node, en `cpu::el2::stage2::grant_window` (dezelfde
    /// bouwer als de kooi zelf, cage.rs); de toets na een flip is
    /// `has_grant_window` ernaast.
    struct CageWindows;

    impl CageWindows {
        fn block(slot: Slot) -> kern::Result<dev::Pa> {
            let bad = kern::Error::SlotRange {
                slot: slot.get(),
                max: abi::layout::SLOT_CAP,
            };
            let plan = crate::slots::os_plan().map_err(|_| bad)?;
            plan.cage_table_pa(slot).map_err(|_| bad)
        }
    }

    impl WindowMap for CageWindows {
        fn map(&mut self, slot: Slot, w: Window) -> kern::Result {
            let block = Self::block(slot)?;
            cpu::el2::stage2::grant_window(block, w.pa, w.size).map_err(|_| kern::Error::Range {
                base: w.pa,
                size: w.size,
            })
        }

        fn is_mapped(&self, slot: Slot, w: Window) -> kern::Result<bool> {
            let block = Self::block(slot)?;
            cpu::el2::stage2::has_grant_window(block, w.pa, w.size).map_err(|_| {
                kern::Error::Range {
                    base: w.pa,
                    size: w.size,
                }
            })
        }
    }

    /// De grant-haakjes van de gui-smaak voor de slot-lifecycle
    /// (`kern::grants::Grants`). De actor roept `env` na de claim en vóór
    /// de env op de control-page gaat (`Request::Env`, kern/src/slots.rs),
    /// `arm` na de kooibouw en vóór de dispatch, `adopt` bij de adoptie,
    /// en `release` na een bevestigde stop en bij een abort.
    ///
    /// De grant zelf staat in [`GRANT`], een `LocalCell` op de OS-core:
    /// de console-init biedt het venster aan vóór de actor bestaat, en
    /// daarna raakt alleen de actor hem aan, via deze haakjes.
    pub(crate) struct GuiGrants;

    impl Grants for GuiGrants {
        fn env(&mut self, slot: Slot, env: &[u8], out: &mut alloc::vec::Vec<u8>) {
            if let Ok(mut g) = GRANT.try_borrow_mut() {
                g.env(slot, env, out, &mut KernGlass, &crate::KernConsole);
            }
        }

        fn arm(&mut self, slot: Slot) -> kern::Result {
            let Ok(g) = GRANT.try_borrow() else {
                return Err(kern::Error::Busy);
            };
            if g.arm(slot, &mut CageWindows)? {
                crate::KernConsole.log(format_args!(
                    "slot {slot}: fb window mapped at ipa {:#x} HOPOS_FB_ARM",
                    abi::layout::FB_IPA
                ));
            }
            Ok(())
        }

        fn adopt(&mut self, slot: Slot) -> kern::Result {
            let Ok(mut g) = GRANT.try_borrow_mut() else {
                return Err(kern::Error::Busy);
            };
            g.adopt(slot, &CageWindows, &mut KernGlass)
        }

        fn release(&mut self, slot: Slot) {
            if let Ok(mut g) = GRANT.try_borrow_mut() {
                g.release(slot, &mut KernGlass, &crate::KernConsole);
            }
        }
    }
}
