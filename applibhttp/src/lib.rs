//! De HTTP-server van een app, opt-in: een acceptor, een park voor stille
//! keep-alive-verbindingen, en een vaste pool werkers rond
//! `leanhttp::serve`.
//!
//! ```text
//! listener ─▶ acceptor ─(Doors)─▶ werker 0..W: één verzoek (leanhttp::serve)
//!                ▲                      │
//!                └──── park ◀── Kept ───┘   (keep-alive: terug naar het park)
//! ```
//!
//! Waarom: een werker hoort bij één verzoek, nooit bij een stille
//! verbinding. Met keep-alive in de werker zelf legden 16 stille
//! verbindingen 16 werkers stil (gemeten 06-11-2026; tot dan kapte elke app
//! de stilte zelf af op 5 s, `with_read_cap`). Hier wacht een stille
//! verbinding in het park van de acceptor: geen taak en geen timer per
//! verbinding (handboek §2, docs/apps.md "Pools, geen taak per
//! verbinding"; een app-core heeft `rt::TIMERS` = 32). Het park wacht op
//! `TcpConn::readable`, dat niets verbruikt, en houdt één timer voor de
//! vroegste stilte.
//!
//! Taken: de acceptor en `W` werkers. Timers: één voor het park, plus de
//! termijnen van leanhttp in een werker (kop, body, schrijven), hooguit
//! twee tegelijk per werker.
//!
//! Heeft de handler meer nodig dan het verzoek, dan geeft
//! [`Http::run_with`] hem per verzoek een [`Ctx`]: de client (gelezen bij
//! `accept`, mee door het park) en het nummer van de werker, voor staat per
//! werker (Meet: de rate-limit per adres en een `Io` per werker).
//!
//! Opt-in: applib weet hier niets van. Een app die het anders wil (een
//! eigen lus, `hijack` voor WebSocket), gebruikt deze crate niet; een
//! overgenomen verbinding sluit hier.
//!
//! ```ignore
//! static HTTP: applibhttp::Http<8, 512> = applibhttp::Http::new();
//! let handler: &'static _ = Box::leak(Box::new(async |ex: &mut Exchange<'_, TcpConn>| {
//!     ex.write(b"ok").await.map(|_| ())
//! }));
//! HTTP.run(exec, listener, handler).await;
//! ```

#![no_std]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::cell::Cell;
use core::future::{Future, poll_fn};
use core::pin::{Pin, pin};
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use applib::appnet::TcpListener;
use applib::rt::Exec;
use applib::tcp::TcpConn;
use leanhttp::{Exchange, Kept, Outcome, serve};
use sync::Doors;
use sync::Local;
use sync::mpsc::Mailbox;

/// Hoe lang een verbinding standaard stil in het park mag staan; daarna
/// sluit de acceptor haar ([`Http::with_idle`] zet het anders). Een stille verbinding kost hier geen werker, dus dit mag
/// ruim zijn (de 60 s die leanhttp tot 06-11-2026 zelf hield).
pub const IDLE: Duration = Duration::from_secs(60);

/// Rust na een mislukte `accept`: een stack die even geen verbinding kan
/// geven (zonder termijn tolt de core rond op een blijvende fout).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Hoe lang een verse verbinding in het park mag zwijgen voor haar eerste
/// byte; gelijk aan leanhttp's termijn voor de kop. Een browser opent
/// verbindingen vooruit (preconnect): zonder park hield zo'n stille
/// verbinding een werker die tijd vast (Stulp, 06-11-2026).
pub const FIRST: Duration = Duration::from_secs(15);

/// Het adres van de client, gelezen bij `accept` en mee door het park: voor
/// een rate-limit per adres en de logregel (Meet, 06-11-2026).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Peer {
    /// Het IPv4-adres; `[0; 4]` als de stack het niet meer wist.
    pub ip: [u8; 4],
    /// De poort.
    pub port: u16,
}

/// Wat een handler van [`Http::run_with`] naast het verzoek krijgt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ctx {
    /// De client van deze verbinding.
    pub peer: Peer,
    /// De werker die dit verzoek bedient (`0..W`): voor staat per werker,
    /// zoals een eigen client of een eigen buffer.
    pub worker: usize,
}

/// Wat de acceptor aan een werker geeft.
enum Job {
    /// Een verse verbinding.
    New(TcpConn, Peer),
    /// Een verbinding uit het park waar het volgende verzoek binnenkomt.
    Again(Kept<TcpConn>, Peer),
}

/// De wekker van het park: de future van `Exec::until`, met zijn tijdstip.
type Alarm = (u64, Pin<Box<dyn Future<Output = ()>>>);

/// Een stille verbinding in het park.
struct Parked {
    conn: Waiting,
    peer: Peer,
    /// Tot wanneer ze stil mag zijn, in de klok van de executor.
    until: u64,
    /// De bel van déze verbinding: de stack wekt haar, en de acceptor pollt
    /// alleen verbindingen waarvan de bel ging.
    bell: Arc<Bell>,
    waker: Waker,
}

impl Parked {
    /// Een verbinding voor het park; de bel staat aan, zodat de eerste ronde
    /// haar één keer pollt en zo de waker bij de stack legt.
    fn new(conn: Waiting, peer: Peer, until: u64, task: &Waker) -> Self {
        let bell = Arc::new(Bell {
            rang: AtomicBool::new(true),
            task: task.clone(),
        });
        let waker = Waker::from(bell.clone());
        Self {
            conn,
            peer,
            until,
            bell,
            waker,
        }
    }
}

/// Een waker per geparkeerde verbinding: hij zet haar vlag en wekt de
/// acceptor. Zonder hem pollde de acceptor bij elke wek álle geparkeerde
/// verbindingen; met 176 in het park en ~100k wekken per seconde viel de
/// doorvoer van 124k terug naar 91k req/s (M4, 06-11-2026).
struct Bell {
    rang: AtomicBool,
    task: Waker,
}

impl Wake for Bell {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.rang.store(true, Ordering::Release);
        self.task.wake_by_ref();
    }
}

/// Een verbinding in het park: vers (nog geen verzoek) of tussen twee
/// verzoeken.
enum Waiting {
    Fresh(TcpConn),
    Kept(Kept<TcpConn>),
}

impl Waiting {
    fn conn_mut(&mut self) -> &mut TcpConn {
        match self {
            Self::Fresh(c) => c,
            Self::Kept(k) => k.conn_mut(),
        }
    }

    fn job(self, peer: Peer) -> Job {
        match self {
            Self::Fresh(c) => Job::New(c, peer),
            Self::Kept(k) => Job::Again(k, peer),
        }
    }

    async fn close(self) {
        match self {
            Self::Fresh(mut c) => {
                let _ = leanhttp::close(&mut c).await;
            }
            Self::Kept(k) => k.close().await,
        }
    }
}

/// Tellers voor de logregel van de app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Verbindingen in het park.
    pub parked: usize,
    /// Verbindingen die het park sloot omdat ze te lang stil waren (vers na
    /// [`FIRST`], tussen verzoeken na [`IDLE`]).
    pub idle_closed: u64,
    /// Verbindingen die dichtgingen omdat het park vol was (de stilste
    /// eerst).
    pub park_full: u64,
    /// Werkers die opkwamen (hoogstens `W`; minder als de takenlijst vol was).
    pub workers: usize,
    /// Mislukte `accept`s (de acceptor rust dan 100 ms): de opvolger van de
    /// `*_ACCEPT`-regels die elke app zelf logde.
    pub accept_failed: u64,
    /// Aanroepen van `leanhttp::serve`: een verzoek, of een verbinding die
    /// sloot toen het volgende verzoek moest komen.
    pub calls: u64,
}

/// De server van een app: `W` werkers, hoogstens `P` verbindingen in het
/// park. Voor in een `static`, op de executor van één core: alles staat in
/// een [`Local`], want een `TcpConn` met zijn wekkers hoort bij die ene
/// executor (handboek §1.1).
pub struct Http<const W: usize, const P: usize> {
    doors: Local<Doors<Job, W>>,
    back: Local<Mailbox<(Kept<TcpConn>, Peer), W>>,
    stats: Local<Cell<Stats>>,
    /// Kap op elke leestermijn van leanhttp (kop, body); `None` laat ze.
    read_cap: Option<Duration>,
    /// Hoe lang een verbinding tussen twee verzoeken in het park mag staan.
    idle: Duration,
}

impl<const W: usize, const P: usize> Default for Http<W, P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const W: usize, const P: usize> Http<W, P> {
    /// Een server zonder werkers; [`Http::run`] start ze.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            doors: Local::new(Doors::new()),
            back: Local::new(Mailbox::new()),
            stats: Local::new(Cell::new(Stats {
                parked: 0,
                idle_closed: 0,
                park_full: 0,
                workers: 0,
                accept_failed: 0,
                calls: 0,
            })),
            read_cap: None,
            idle: IDLE,
        }
    }

    /// Hoe lang een verbinding tussen twee verzoeken stil in het park mag
    /// staan; standaard [`IDLE`]. Hop houdt bewust 2 s: een stille verbinding
    /// kost geen werker meer, maar wel 20 KiB netstack.
    #[must_use]
    pub const fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// Kapt elke leestermijn die leanhttp zet (15 s voor de kop, 5 s voor
    /// de body) af op `cap`: een client die zijn verzoek langzaam druppelt,
    /// houdt een werker dan hooguit `cap` vast (Meet: 5 s). De stilte tussen
    /// twee verzoeken valt hier niet onder; die is van het park.
    #[must_use]
    pub const fn with_read_cap(mut self, cap: Duration) -> Self {
        self.read_cap = Some(cap);
        self
    }

    /// De tellers nu.
    pub fn stats(&self) -> Stats {
        let cell: &Cell<Stats> = &self.stats;
        cell.get()
    }

    fn count(&self, f: impl FnOnce(&mut Stats)) {
        let cell: &Cell<Stats> = &self.stats;
        let mut s = cell.get();
        f(&mut s);
        cell.set(s);
    }

    /// Start de werkers op `exec` en is daarna zelf de acceptor, voor
    /// altijd. Lukt het spawnen van een werker niet (de vaste takenlijst is
    /// vol), dan draait de server met minder; zonder één werker keert dit
    /// meteen terug met het aantal dat er wel kwam (0).
    pub async fn run<H>(
        &'static self,
        exec: &'static Exec,
        listener: TcpListener,
        handler: &'static H,
    ) -> usize
    where
        H: AsyncFn(&mut Exchange<'_, TcpConn>) -> leanhttp::Result + 'static,
    {
        let with: &'static _ = Box::leak(Box::new(
            async move |ex: &mut Exchange<'_, TcpConn>, _: &Ctx| handler(ex).await,
        ));
        self.run_with(exec, listener, with).await
    }

    /// Als [`Http::run`], met een handler die ook de [`Ctx`] van het verzoek
    /// krijgt: de client en de werker.
    pub async fn run_with<H>(
        &'static self,
        exec: &'static Exec,
        listener: TcpListener,
        handler: &'static H,
    ) -> usize
    where
        H: AsyncFn(&mut Exchange<'_, TcpConn>, &Ctx) -> leanhttp::Result + 'static,
    {
        let mut staffed = 0;
        for i in 0..W {
            if exec.spawn(self.worker(i, handler)).is_err() {
                break;
            }
            staffed += 1;
        }
        self.doors.staffed(staffed);
        self.count(|s| s.workers = staffed);
        if staffed == 0 {
            return 0;
        }
        self.accept(exec, listener).await;
        staffed
    }

    /// Werker `i`: neemt een verbinding, bedient één verzoek (of meer als de
    /// client pijplijnde), en geeft haar terug aan het park of sluit haar.
    async fn worker<H>(&'static self, i: usize, handler: &'static H)
    where
        H: AsyncFn(&mut Exchange<'_, TcpConn>, &Ctx) -> leanhttp::Result + 'static,
    {
        loop {
            let (job, peer) = match self.doors.take(i).await {
                Job::New(conn, peer) => (Ok(conn), peer),
                Job::Again(kept, peer) => (Err(kept), peer),
            };
            let ctx = Ctx { peer, worker: i };
            let h = async |ex: &mut Exchange<'_, TcpConn>| handler(ex, &ctx).await;
            let mut out = match job {
                Ok(conn) => serve(conn, &h).await,
                Err(kept) => kept.serve(&h).await,
            };
            loop {
                self.count(|s| s.calls += 1);
                match out {
                    // Al gepijplijnd: wachten op leesbaar zou nooit eindigen.
                    Ok(Outcome::Kept(k)) if k.has_buffered() => out = k.serve(&h).await,
                    Ok(Outcome::Kept(k)) => {
                        if let Err(full) = self.back.try_send((k, peer)) {
                            full.0.0.close().await;
                        }
                        break;
                    }
                    Ok(Outcome::Hijacked(h)) => {
                        let mut conn = h.conn;
                        let _ = leanhttp::close(&mut conn).await;
                        break;
                    }
                    Ok(Outcome::Closed) | Err(_) => break,
                }
            }
            self.doors.free(i);
        }
    }

    /// Zet `p` in het park. Is het vol, dan gaat de verbinding met de
    /// vroegste deadline dicht, niet de nieuwe: anders houden `P` stille
    /// sockets elke nieuwe client buiten tot hun stilte op is (haasbase,
    /// 06-11-2026).
    fn park(&self, park: &mut Vec<Parked>, gone: &mut Vec<Waiting>, p: Parked) {
        if park.len() >= P {
            let oldest = park
                .iter()
                .enumerate()
                .min_by_key(|(_, q)| q.until)
                .map(|(i, _)| i);
            match oldest {
                Some(i) => {
                    self.count(|s| s.park_full += 1);
                    gone.push(park.swap_remove(i).conn);
                }
                // P = 0: niets te parkeren; de verbinding zelf gaat dicht.
                None => {
                    self.count(|s| s.park_full += 1);
                    gone.push(p.conn);
                    return;
                }
            }
        }
        park.push(p);
    }

    /// De acceptor met het park: een verse verbinding en een verbinding
    /// tussen twee verzoeken wachten hier tot er bytes komen, en gaan dan naar
    /// een vrije werker; stilte sluit ze na [`FIRST`] of [`IDLE`].
    async fn accept(&'static self, exec: &'static Exec, listener: TcpListener) {
        let mut park: Vec<Parked> = Vec::new();
        let mut alarm: Option<Alarm> = None;
        let mut due: Vec<Job> = Vec::new();
        let mut gone: Vec<Waiting> = Vec::new();
        loop {
            let mut failed = false;
            poll_fn(|cx| {
                let mut moved = false;
                // Terug van de werkers: het park in.
                while let Poll::Ready((kept, peer)) = pin!(self.back.recv()).poll(cx) {
                    moved = true;
                    let until = exec.now().saturating_add(nanos(self.idle));
                    self.park(
                        &mut park,
                        &mut gone,
                        Parked::new(Waiting::Kept(kept), peer, until, cx.waker()),
                    );
                }
                // Nieuwe verbindingen.
                loop {
                    match pin!(listener.accept()).poll(cx) {
                        Poll::Ready(Ok(s)) => {
                            moved = true;
                            let peer = s
                                .remote()
                                .map(|e| Peer {
                                    ip: e.ip,
                                    port: e.port,
                                })
                                .unwrap_or_default();
                            let conn = match self.read_cap {
                                Some(cap) => TcpConn::new(s, exec).with_read_cap(cap),
                                None => TcpConn::new(s, exec),
                            };
                            let until = exec.now().saturating_add(nanos(FIRST));
                            self.park(
                                &mut park,
                                &mut gone,
                                Parked::new(Waiting::Fresh(conn), peer, until, cx.waker()),
                            );
                        }
                        Poll::Ready(Err(_)) => {
                            moved = true;
                            failed = true;
                            self.count(|s| s.accept_failed += 1);
                            break;
                        }
                        Poll::Pending => break,
                    }
                }
                // Het park: leesbaar gaat naar een werker, te lang stil gaat
                // dicht. Alleen een verbinding waarvan de bel ging, wordt
                // gepolld, met haar eigen waker; een `readable` die wegvalt,
                // kost niets, de stack houdt die waker.
                let now = exec.now();
                let mut i = 0;
                while i < park.len() {
                    let ready = match park.get_mut(i) {
                        Some(p) if p.until <= now => Some(false),
                        Some(p) if p.bell.rang.swap(false, Ordering::AcqRel) => {
                            let mut own = Context::from_waker(&p.waker);
                            pin!(p.conn.conn_mut().readable())
                                .poll(&mut own)
                                .is_ready()
                                .then_some(true)
                        }
                        Some(_) => None,
                        None => None,
                    };
                    match ready {
                        Some(again) => {
                            moved = true;
                            let p = park.swap_remove(i);
                            if again {
                                due.push(p.conn.job(p.peer));
                            } else {
                                self.count(|s| s.idle_closed += 1);
                                gone.push(p.conn);
                            }
                        }
                        None => i += 1,
                    }
                }
                // Eén timer: de vroegste stilte.
                match park.iter().map(|p| p.until).min() {
                    None => alarm = None,
                    Some(at) => {
                        if alarm.as_ref().is_none_or(|(t, _)| *t != at) {
                            alarm = Some((at, Box::pin(exec.until(at))));
                        }
                        if let Some((_, a)) = alarm.as_mut()
                            && a.as_mut().poll(cx).is_ready()
                        {
                            alarm = None;
                            moved = true;
                        }
                    }
                }
                let parked = park.len();
                self.count(|s| s.parked = parked);
                if moved {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            for conn in gone.drain(..) {
                conn.close().await;
            }
            // Alle werkers bezet: wachten op de eerste die vrijkomt (een bel,
            // geen klok). Het park en de listener wachten zolang in de stack,
            // maar wat werkers intussen teruggeven, gaat meteen het park in:
            // anders loopt de rij van `W` vol en sluit een werker een goede
            // verbinding (M4, 06-11-2026: tientallen per fase bij 192
            // verbindingen).
            for job in due.drain(..) {
                let mut placing = pin!(self.doors.place(job));
                poll_fn(|cx| {
                    while let Poll::Ready((kept, peer)) = pin!(self.back.recv()).poll(cx) {
                        let until = exec.now().saturating_add(nanos(self.idle));
                        let p = Parked::new(Waiting::Kept(kept), peer, until, cx.waker());
                        self.park(&mut park, &mut gone, p);
                    }
                    placing.as_mut().poll(cx)
                })
                .await;
            }
            if failed {
                exec.after(ACCEPT_BACKOFF).await;
            }
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}
