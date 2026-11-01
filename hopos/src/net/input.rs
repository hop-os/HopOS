//! De input-listener op `10.100.0.1:7879`, de node-kant van de switch zoals
//! de system-listener (Go: `usbin.listen` en `deliver.go`). Alleen in de
//! gui-smaak; kaal bestaat hij niet (handboek §7, de `cfg` op `mod input` in
//! net.rs).
//!
//! Eigendom: de taak bezit de listen-socket, de ene verbinding naar de
//! display-app en de ontvangkant van de invoerrij (in de
//! `gui_usbin::deliver::Deliverer`); de USB-taak (usb.rs) bezit de
//! zendkant (`gui::UsbInput`). De houder van het glas leest hij per regel
//! bij de grant (`gui::glass_holder`), zonder die ooit vast te houden.

use super::{accept, close, io, poll_stack};
use abi::layout::HOST_IP4;
use core::future::{Future, poll_fn};
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use cpu::println;
use executor::Executor;
use gui_usbin::deliver::{
    self, ConnError, Deliverer, INPUT_PORT, InputConn, KEEPALIVE_NS, Lines, WRITE_DEADLINE_NS,
};
use leannet::TcpHandle;
use sync::{Either, select};

/// Regels die wegvielen omdat er geen display-app verbonden was: er
/// kijkt dan niemand, en invoer bewaren levert alleen een lawine bij het
/// aansluiten (Go deed hetzelfde). Een teller, geen fout.
pub(crate) static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Regels (keepalives niet meegeteld) die de display-app kreeg.
pub(crate) static SENT: AtomicU64 = AtomicU64::new(0);
/// Verbindingen die niet van de houder van het glas kwamen.
pub(crate) static REFUSED: AtomicU64 = AtomicU64::new(0);

/// Hoe vaak de listener het opnieuw probeert zolang de node-stack er
/// nog niet is (de USB-taak start vóór de DHCP-lease).
const STACK_RETRY: Duration = Duration::from_millis(100);

/// Weigeringen en drops die een eigen regel krijgen; daarna tellen.
const LOUD: u64 = 3;

/// De taak: luisteren zodra de node-stack er is, dan de ene verbinding
/// van de houder bedienen. Een nieuwe verbinding van de houder verdringt
/// de oude (een display die herstart, krijgt het toetsenbord terug
/// zonder dat iemand de dode verbinding hoeft op te ruimen); zonder
/// verbinding gaan de regels weg en tellen ze.
pub(crate) async fn serve(exec: &'static Executor, mut d: Deliverer<'static>) {
    let l = loop {
        match io(|st| st.tcp_listen(INPUT_PORT)) {
            Ok(l) => break l,
            Err(leannet::Error::StackClosed) => exec.after(STACK_RETRY).await,
            Err(e) => {
                println!("input: listen on {INPUT_PORT}: {e} HOPOS_INPUT_FAIL");
                return;
            }
        }
    };
    println!(
        "input: listening on {}:{INPUT_PORT} for the holder of the glass HOPOS_INPUT_UP",
        Ipv4Addr::from(HOST_IP4)
    );
    let tick = || exec.after(Duration::from_nanos(KEEPALIVE_NS));
    let mut conn: Option<Stream> = None;
    loop {
        let got = match conn.as_mut() {
            None => match select(accept(exec, l), d.next(tick())).await {
                Either::Left(r) => r,
                Either::Right(lines) => {
                    drop_lines(&lines);
                    continue;
                }
            },
            Some(c) => match select(d.serve(c, tick), accept(exec, l)).await {
                Either::Left(e) => {
                    if let Some(c) = conn.take() {
                        println!(
                            "input: {} stream ended: {e}, {} lines sent HOPOS_INPUT_GONE",
                            Ipv4Addr::from(c.remote),
                            SENT.load(Relaxed)
                        );
                        close(exec, c.h);
                    }
                    continue;
                }
                Either::Right(r) => r,
            },
        };
        let h = match got {
            Ok(h) => h,
            Err(e) => {
                println!("input: accept on {INPUT_PORT}: {e}, listener closed HOPOS_INPUT_FAIL");
                return;
            }
        };
        let remote = io(|st| st.tcp_remote(h)).map_or(0, |ep| u32::from_be_bytes(ep.ip));
        if !deliver::allowed(remote, crate::gui::glass_holder()) {
            let n = REFUSED.fetch_add(1, Relaxed).wrapping_add(1);
            if n <= LOUD {
                println!(
                    "input: {} refused, it does not hold the glass, {n} so far HOPOS_INPUT_REFUSED",
                    Ipv4Addr::from(remote)
                );
            }
            close(exec, h);
            continue;
        }
        if let Some(old) = conn.take() {
            println!(
                "input: {} replaced by {}, {} lines sent HOPOS_INPUT_GONE",
                Ipv4Addr::from(old.remote),
                Ipv4Addr::from(remote),
                SENT.load(Relaxed)
            );
            close(exec, old.h);
        }
        println!(
            "input: {} connected (holder of the glass), {} lines dropped while nobody listened HOPOS_INPUT_CONN",
            Ipv4Addr::from(remote),
            DROPPED.load(Relaxed)
        );
        conn = Some(Stream { exec, h, remote });
    }
}

/// Telt de regels die niemand kreeg; een keepalive is geen invoer.
fn drop_lines(lines: &Lines) {
    let n = lines.iter().filter(|l| *l != b"\n").count() as u64;
    if n == 0 {
        return;
    }
    let before = DROPPED.fetch_add(n, Relaxed);
    if before < LOUD {
        println!(
            "input: no display app connected, {} line(s) dropped (counted) HOPOS_INPUT_DROP",
            before.saturating_add(n)
        );
    }
}

/// De verbinding met de display-app op de node-stack: een handvat, geen
/// buffer van zichzelf.
struct Stream {
    exec: &'static Executor,
    h: TcpHandle,
    remote: u32,
}

impl InputConn for Stream {
    /// Eén regel binnen [`WRITE_DEADLINE_NS`]. Per regel opnieuw de
    /// houder: is het glas intussen van een ander slot (de display-app
    /// stopte, een nieuwe kreeg het), dan gaat de stroom dicht in plaats
    /// van andermans toetsen door te geven.
    fn write(&mut self, line: &[u8]) -> impl Future<Output = Result<(), ConnError>> {
        let (exec, h, remote) = (self.exec, self.h, self.remote);
        async move {
            if !deliver::allowed(remote, crate::gui::glass_holder()) {
                return Err(ConnError::Closed);
            }
            let deadline = exec.after(Duration::from_nanos(WRITE_DEADLINE_NS));
            match select(write_all(exec, h, line), deadline).await {
                Either::Left(Ok(())) => {
                    if line != b"\n" {
                        SENT.fetch_add(1, Relaxed);
                    }
                    Ok(())
                }
                Either::Left(Err(e)) => Err(e),
                Either::Right(()) => Err(ConnError::Timeout),
            }
        }
    }

    fn remote_ip4(&self) -> u32 {
        self.remote
    }
}

/// Schrijft `buf` helemaal; de stack neemt wat in zijn venster past.
async fn write_all(exec: &'static Executor, h: TcpHandle, mut buf: &[u8]) -> Result<(), ConnError> {
    while !buf.is_empty() {
        let n = poll_fn(|cx| {
            poll_stack(
                exec,
                cx,
                |st, now| st.tcp_write(h, buf, now),
                |st, w| st.tcp_register_write_waker(h, w),
            )
        })
        .await
        .map_err(|_| ConnError::Closed)?;
        if n == 0 {
            return Err(ConnError::Closed);
        }
        buf = buf.get(n..).unwrap_or_default();
    }
    Ok(())
}
