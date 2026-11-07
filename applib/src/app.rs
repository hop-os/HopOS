//! [`App`]: het handvat van een app op zijn slot.
//!
//! Het App bezit wat de app met de kern deelt en wat alleen de app-core
//! aanraakt: de outbox-producer, het venster op de control-page, de env. De
//! main-schil maakt er precies één (in een [`Local`](sync::Local)) en geeft
//! de app een `&'static App`; wie iets met de kern wil, gaat daarlangs.

use crate::arch;
use crate::clock;
use crate::contract::RING_DATA_CAP;
use crate::ctrl::{Ctrl, Env};
use crate::log;
use crate::ring::Writer;
use crate::tail::{Tail, TailError, tail_of};
use core::cell::RefCell;
use core::fmt;
use core::time::Duration;
use sync::Stop;

/// Waarom een app niet op zijn slot past.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AppError {
    /// De RAM-declaratie geeft geen geldige staart.
    Tail(TailError),
    /// De outbox ligt niet waar hij hoort.
    Ring(abi::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tail(e) => write!(f, "tail: {e}"),
            Self::Ring(e) => write!(f, "outbox: {e}"),
        }
    }
}

/// Wat een hartslag vond.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Beat {
    /// Doorgaan.
    Alive,
    /// De kern vraagt de app te stoppen (`CtrlKill`).
    Kill,
}

/// Elke hoeveelste hartslag de geheugen-draw meegaat: 40 × 50 ms = 2 s,
/// het ritme van de Go-watch.
pub const MEM_EVERY: u64 = 40;

/// Hoe lang [`App::shutdown`] hooguit op het net-afscheid wacht. Het
/// slot-LAN is geheugen en de kern bevestigt in microseconden; wat na
/// 200 ms nog openstaat, wacht op een peer die er niet meer is.
pub const SHUTDOWN_LIMIT: Duration = Duration::from_millis(200);

/// Zoveel taken tegelijk kunnen op de stopbel wachten zonder zichzelf per
/// ronde te wekken: de hoofdlus en een paar werkers. Meer blijft goed en
/// telt in [`Stop::overflows`].
pub const STOP_WAITERS: usize = 8;

/// De termijn als de kern er geen op de vlag zet (een kern van vóór 05-10,
/// `KILL_STOP`): het venster van Hop, `HOP_STOP_TIMEOUT_MS` in de hop-repo,
/// want zo lang wachtte die kern toch al vóór zijn intrekking.
pub const STOP_GRACE_DEFAULT: Duration = Duration::from_secs(3);

/// Wat van de termijn niet aan de app is: één hartslag om de vlag te zien
/// ([`crate::rt::WATCH_PERIOD`], 50 ms), het net-afscheid ([`SHUTDOWN_LIMIT`],
/// 200 ms), de poll van de kern (10 ms), en de rest voor een gedeelde core
/// die zijn beurt moet krijgen. Zo is de app weg vóór de kern intrekt.
pub const STOP_MARGIN: Duration = Duration::from_millis(500);

/// Het handvat van een app.
pub struct App {
    slot: u64,
    ram_start: u64,
    ram_size: u64,
    tail: Tail,
    ctrl: Ctrl,
    /// De outbox-producer. Een `RefCell` omdat `log!` hem vanaf elke taak
    /// leent; de lening duurt één bericht en loopt nooit over een `.await`.
    outbox: RefCell<Writer>,
    env: Env,
    /// De stopbel: luidt als de kern vraagt te stoppen (zie [`App::stop`]).
    stop: Stop<STOP_WAITERS>,
}

impl App {
    /// Koppelt aan het slot met RAM-declaratie `ram_start`, `ram_size` en
    /// slotnummer `slot` (0 = onbekend: een image buiten de kern om). Leest
    /// de env; meldt nog niets aan de kern (dat doet [`App::announce`]).
    pub fn new(ram_start: u64, ram_size: u64, slot: u64) -> Result<Self, AppError> {
        let tail = tail_of(ram_start, ram_size).map_err(AppError::Tail)?;
        let outbox = Writer::open(tail.outbox(), RING_DATA_CAP).map_err(AppError::Ring)?;
        let ctrl = Ctrl::at(tail.ctrl_page());
        let env = Env::read(&ctrl);
        Ok(Self {
            slot,
            ram_start,
            ram_size,
            tail,
            ctrl,
            outbox: RefCell::new(outbox),
            env,
            stop: Stop::new(),
        })
    }

    /// Het slotnummer (= de core-index van de kern).
    #[must_use]
    pub const fn slot(&self) -> u64 {
        self.slot
    }

    /// De eigen partitiebasis zoals de app hem ziet (het linkadres).
    #[must_use]
    pub const fn ram_start(&self) -> u64 {
        self.ram_start
    }

    /// De eigen RAM-declaratie (door de kern gepatcht).
    #[must_use]
    pub const fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// De ABI-staart.
    #[must_use]
    pub const fn tail(&self) -> Tail {
        self.tail
    }

    /// Het venster op de control-page.
    #[must_use]
    pub const fn ctrl(&self) -> Ctrl {
        self.ctrl
    }

    /// Schrijft `args` als logregel(s) naar de outbox; het werk achter
    /// [`log!`](crate::log!) en de paniek. Is de outbox al geleend (een
    /// `Display` die zelf logt, een paniek midden in een regel), dan wordt
    /// gedropt en geteld.
    pub fn log(&self, args: fmt::Arguments<'_>) {
        match self.outbox.try_borrow_mut() {
            Ok(mut w) => log::emit_to(Some(&mut w), args),
            Err(_) => log::emit_to(None, args),
        }
    }

    /// Een door de kern meegegeven omgevingsvariabele.
    #[must_use]
    pub fn env(&self, key: &str) -> Option<&str> {
        self.env.get(key)
    }

    /// Meldt READY: de runtime draait. Wie op READY wacht, ziet dan ook de
    /// RAM-maat die de app van zichzelf kent.
    pub fn announce(&self) {
        self.ctrl.announce_ready(self.ram_size);
    }

    /// De wandklok in nanoseconden, als de kern hem al synct.
    #[must_use]
    pub fn wall_ns(&self) -> Option<u64> {
        match self.ctrl.wall_offset() {
            0 => None,
            off => Some(off.wrapping_add(clock::now_ns())),
        }
    }

    /// De stopbel: luidt zodra de kern vraagt te stoppen, en gaat nooit meer
    /// uit (`close(stop)`, `context.WithCancel`). Voor een `select` in de
    /// hoofdlus; [`App::stopped`] is dezelfde bel als future. Wie hem
    /// hoort, rondt af (een database dicht, een sessie beëindigd, een
    /// laatste regel) en keert terug uit `main`: de main-schil doet dan het
    /// net-afscheid en de exit. Wie hem negeert, krijgt na
    /// [`App::stop_grace`] hetzelfde van de heartbeat.
    #[must_use]
    pub const fn stop(&self) -> &Stop<STOP_WAITERS> {
        &self.stop
    }

    /// Wacht tot de kern vraagt te stoppen.
    pub async fn stopped(&self) {
        self.stop.wait().await;
    }

    /// Hoeveel tijd de app na de bel heeft om af te ronden: de termijn van de
    /// kern (of [`STOP_GRACE_DEFAULT`]) min [`STOP_MARGIN`].
    #[must_use]
    pub fn stop_grace(&self) -> Duration {
        self.ctrl
            .kill_grace()
            .unwrap_or(STOP_GRACE_DEFAULT)
            .saturating_sub(STOP_MARGIN)
    }

    /// Het stopverzoek van de kern: één regel, de bel, en de gratie die
    /// erbij hoort. De heartbeat roept dit één keer, bij [`Beat::Kill`].
    pub fn request_stop(&self) -> Duration {
        let grace = self.stop_grace();
        self.log(format_args!(
            "applib: stop requested by the kernel, {} ms to finish HOPOS_APP_STOP grace_ms={}",
            grace.as_millis(),
            grace.as_millis()
        ));
        self.stop.set();
        grace
    }

    /// Eén hartslag: de teller op de pagina, de kill-vlag gelezen, en om de
    /// [`MEM_EVERY`] slagen de geheugen-draw `mem`.
    pub fn beat(&self, beat: u64, mem: u64) -> Beat {
        self.ctrl.set_heartbeat(beat);
        if self.ctrl.kill_requested() {
            return Beat::Kill;
        }
        if beat % MEM_EVERY == 1 {
            self.ctrl.set_mem_sys(mem);
        }
        Beat::Alive
    }

    /// Stopt de app netjes met exitcode `code`: eerst het net-afscheid
    /// ([`Net::shutdown`](crate::appnet::Net::shutdown): elk open handvat
    /// dicht en de pomp door tot elke FIN bevestigd is, hooguit
    /// [`SHUTDOWN_LIMIT`]), één regel met de uitkomst, en dan
    /// [`App::exit`]. Keert nooit terug.
    ///
    /// Waarom: `exit` parkeert de core, en wat dan nog in een TCP-zendbuffer
    /// staat, bereikt niemand meer. Gemeten 29-09: de laatste logregel van
    /// appspike over de system-verbinding stond na `log_us=320` in de
    /// buffer, en de kern zag hem nooit.
    pub async fn shutdown(&self, code: u64) {
        if let Some(net) = crate::appnet::net() {
            let d = net.shutdown(SHUTDOWN_LIMIT).await;
            crate::log!(
                "applib: shutdown code={code} closed={} drained={} waited_us={} untracked={} HOPOS_APP_SHUTDOWN",
                d.closed,
                d.drained,
                d.waited_us,
                d.untracked
            );
        }
        self.exit(code)
    }

    /// Meldt de exitcode en geeft de core aan de kern terug. Keert nooit
    /// terug, en doet niets meer dan dat: geen taken, geen timers. Dus ook
    /// veilig vanuit de paniek, waar de executor niet meer draait.
    ///
    /// Geen net-afscheid: dat doet [`App::shutdown`], en dit is de weg voor
    /// wie niet mag wachten (de paniek, een kill van de kern). Een peer
    /// merkt de dood dan via zijn eigen deadline, en dat moet hij toch
    /// kunnen, want een switch kan een verbinding op elk moment stil
    /// doodmaken.
    pub fn exit(&self, code: u64) -> ! {
        self.ctrl.mark_exited(code);
        arch::park_exit()
    }
}

/// De poort uit een env-waarde (`ER_PORT_HTTP`), of `default` zonder of bij
/// onzin. Onzin is luid: een jobspec die iets anders bedoelde, moet dat
/// kunnen zien.
#[must_use]
pub fn port_of(env: Option<&str>, default: u16) -> u16 {
    match env.map(str::parse::<u16>) {
        None => default,
        Some(Ok(p)) if p != 0 => p,
        Some(_) => {
            crate::log!("app: port {env:?} is not a port, using {default} HOPOS_APP_PORT");
            default
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::contract::{
        ABI_TAIL, CTRL_HEARTBEAT, CTRL_KILL, CTRL_MEM_SYS, CTRL_RAM_SIZE, CTRL_STATUS,
    };
    use crate::ring;
    use core::task::{Context, Poll, Waker};

    /// Een hele partitie over een buffer: een RAM-declaratie van één pagina
    /// en de staart erboven. Pagina-gealigneerd via een grotere buffer.
    pub(crate) struct Partition {
        words: Vec<u64>,
        pub(crate) start: u64,
    }

    impl Partition {
        pub(crate) fn new() -> Self {
            let words = vec![0u64; ((0x1000 + ABI_TAIL + 0x1000) / 8) as usize];
            let raw = words.as_ptr() as usize as u64;
            let start = (raw + 0xfff) & !0xfff;
            let p = Self { words, start };
            let tail = tail_of(p.start, 0x1000).unwrap();
            ring::init(tail.outbox(), RING_DATA_CAP).unwrap();
            p
        }
        pub(crate) fn app(&self) -> App {
            assert!(!self.words.is_empty());
            App::new(self.start, 0x1000, 3).unwrap()
        }
    }

    #[test]
    fn announce_puts_ready_on_the_page() {
        let part = Partition::new();
        let app = part.app();
        assert_eq!(app.tail().base().0, part.start + 0x1000);
        app.announce();
        assert_eq!(
            app.ctrl().get(CTRL_STATUS),
            crate::ctrl::AppStatus::Ready.raw()
        );
        assert_eq!(app.ctrl().get(CTRL_RAM_SIZE), 0x1000);
        assert_eq!(app.wall_ns(), None);
    }

    #[test]
    fn heartbeat_counts_reports_memory_and_obeys_kill() {
        let part = Partition::new();
        let app = part.app();
        assert_eq!(app.beat(1, 4096), Beat::Alive);
        assert_eq!(app.ctrl().get(CTRL_HEARTBEAT), 1);
        assert_eq!(app.ctrl().get(CTRL_MEM_SYS), 4096);
        assert_eq!(app.beat(2, 8192), Beat::Alive);
        assert_eq!(app.ctrl().get(CTRL_MEM_SYS), 4096); // pas weer bij slag 41
        assert_eq!(app.beat(41, 8192), Beat::Alive);
        assert_eq!(app.ctrl().get(CTRL_MEM_SYS), 8192);
        app.ctrl().set(CTRL_KILL, 1);
        assert_eq!(app.beat(42, 0), Beat::Kill);
        assert_eq!(app.ctrl().get(CTRL_HEARTBEAT), 42);
    }

    /// Eén regel uit de outbox van `app`.
    fn next_line(app: &App) -> String {
        let mut r = ring::Reader::open(app.tail().outbox(), RING_DATA_CAP).unwrap();
        let mut buf = [0u8; 128];
        let rec = r.read_into(&mut buf).unwrap();
        String::from_utf8_lossy(rec.payload).into_owned()
    }

    #[test]
    fn the_stop_bell_rings_with_the_kernels_grace_less_the_margin() {
        let part = Partition::new();
        let app = part.app();
        let mut cx = Context::from_waker(Waker::noop());
        let mut waiting = core::pin::pin!(app.stopped());
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(app.stop_grace(), STOP_GRACE_DEFAULT - STOP_MARGIN);
        // De kern van na 05-10: de termijn in ms op de vlag.
        app.ctrl().set(CTRL_KILL, 3_000);
        assert_eq!(app.beat(1, 0), Beat::Kill);
        assert_eq!(app.request_stop(), Duration::from_millis(2_500));
        assert!(app.stop().is_set());
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(
            next_line(&app),
            "applib: stop requested by the kernel, 2500 ms to finish HOPOS_APP_STOP grace_ms=2500"
        );
        // Een termijn korter dan de marge: meteen het afscheid, niet negatief.
        app.ctrl().set(CTRL_KILL, 100);
        assert_eq!(app.stop_grace(), Duration::ZERO);
    }

    #[test]
    fn an_old_kernel_writes_one_and_gets_the_default_grace() {
        let part = Partition::new();
        let app = part.app();
        app.ctrl().set(CTRL_KILL, 1);
        assert_eq!(app.beat(1, 0), Beat::Kill);
        assert_eq!(app.stop_grace(), Duration::from_millis(2_500));
    }

    #[test]
    fn log_lands_on_this_apps_outbox() {
        let part = Partition::new();
        let app = part.app();
        app.log(format_args!("slot {} up", app.slot()));
        let mut r = ring::Reader::open(app.tail().outbox(), RING_DATA_CAP).unwrap();
        let mut buf = [0u8; 64];
        let rec = r.read_into(&mut buf).unwrap();
        assert_eq!(rec.payload, b"slot 3 up");
    }

    #[test]
    fn a_bad_declaration_is_refused() {
        assert!(matches!(
            App::new(0x5000_0000, 0, 1),
            Err(AppError::Tail(TailError::Empty))
        ));
    }

    #[test]
    fn the_port_comes_from_the_env_or_is_the_default() {
        assert_eq!(port_of(Some("8081"), 80), 8081);
        assert_eq!(port_of(None, 80), 80);
        assert_eq!(port_of(Some("0"), 9000), 9000);
        assert_eq!(port_of(Some("http"), 8080), 8080);
    }
}
