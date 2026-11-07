//! De control-page: de 64-bit woorden die app en kern elkaar toeschrijven,
//! en de env-blob die de kern bij de start meegaf.
//!
//! Alle ABI-verkeer van de app loopt langs [`Ctrl::get`] en [`Ctrl::set`]:
//! `pull` vóór een lees, `push` na een schrijf. Een rauwe deref zou op een
//! board waar kern en app-core niet coherent zijn stil in de eigen D-cache
//! blijven staan en voor de kern niet bestaan; hier is dat één plek, geen
//! discipline per aanroeper.
//!
//! Dit module bezit de pagina niet (dat doet de kern, die veegt hem bij elke
//! start); het is een venster erop.

use crate::contract::{
    CTRL_CORES, CTRL_ENV_DATA, CTRL_ENV_LEGACY_MAX, CTRL_ENV_LEN, CTRL_ENV_MAX, CTRL_EXIT_CODE,
    CTRL_HART, CTRL_HEARTBEAT, CTRL_IDLE, CTRL_IDLE_MODE, CTRL_KILL, CTRL_MEM_SYS, CTRL_RAM_SIZE,
    CTRL_RNG_SOURCE, CTRL_RX_DOOR, CTRL_SHARED, CTRL_STATUS, CTRL_TEMP, CTRL_WAKES, CTRL_WALL_OFF,
    IDLE_YIELD, KILL_STOP, rng_source, shares_kern_hart,
};
use core::time::Duration;
use dev::Pa;

pub use abi::hopabi::AppStatus;

/// Een venster op de control-page van dit slot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ctrl {
    page: Pa,
}

impl Ctrl {
    /// Het venster op `page`, uit [`Tail::ctrl_page`](crate::Tail::ctrl_page).
    #[must_use]
    pub const fn at(page: Pa) -> Self {
        Self { page }
    }

    /// Het adres van woord `off`.
    #[must_use]
    pub const fn addr(&self, off: u64) -> Pa {
        self.page.add(off)
    }

    /// Leest woord `off`, vers uit het geheugen.
    #[must_use]
    pub fn get(&self, off: u64) -> u64 {
        let p = self.addr(off);
        dev::pull(p, 8);
        dev::read64(p)
    }

    /// Schrijft woord `off` en publiceert het.
    pub fn set(&self, off: u64, v: u64) {
        let p = self.addr(off);
        dev::write64(p, v);
        dev::push(p, 8);
    }

    /// De status die op de pagina staat (`None`: een waarde die geen
    /// status is).
    #[must_use]
    pub fn status(&self) -> Option<AppStatus> {
        AppStatus::from_raw(self.get(CTRL_STATUS))
    }

    /// Meldt de app READY: de runtime draait. Eerst de RAM-maat, het bewijs
    /// dat de patch van de kern aankwam; wie op READY wacht ziet hem dan al.
    pub fn announce_ready(&self, ram_size: u64) {
        self.set(CTRL_RAM_SIZE, ram_size);
        self.set(CTRL_STATUS, AppStatus::Ready.raw());
    }

    /// Zet exitcode en status EXITED, en zorgt dat ze zichtbaar zijn vóór
    /// de core aan de kern teruggaat.
    pub fn mark_exited(&self, code: u64) {
        self.set(CTRL_EXIT_CODE, code);
        self.set(CTRL_STATUS, AppStatus::Exited.raw());
        dev::mb();
    }

    /// De heartbeat: een oplopende teller, de hang-detectie van de kern.
    pub fn set_heartbeat(&self, beat: u64) {
        self.set(CTRL_HEARTBEAT, beat);
    }

    /// De die-temperatuur van de node in milligraden, zoals de kern hem
    /// elke seconde neerzet; 0 = geen meting (een board zonder sensor, of
    /// nog geen seconde telemetrie). Voor de heartbeat van Hop.
    #[must_use]
    pub fn temp_milli_c(&self) -> i32 {
        i32::try_from(self.get(CTRL_TEMP) as i64).unwrap_or(0)
    }

    /// De laatst geschreven hartslag.
    #[must_use]
    pub fn heartbeat(&self) -> u64 {
        self.get(CTRL_HEARTBEAT)
    }

    /// De laatst gepubliceerde geheugen-draw.
    #[must_use]
    pub fn mem_sys(&self) -> u64 {
        self.get(CTRL_MEM_SYS)
    }

    /// De RAM-maat die de app van zichzelf meldde.
    #[must_use]
    pub fn ram_size(&self) -> u64 {
        self.get(CTRL_RAM_SIZE)
    }

    /// De geslapen tikken zoals de idle ze publiceerde.
    #[must_use]
    pub fn idle_ticks(&self) -> u64 {
        self.get(CTRL_IDLE)
    }

    /// Vraagt de kern deze app te stoppen?
    #[must_use]
    pub fn kill_requested(&self) -> bool {
        self.get(CTRL_KILL) != 0
    }

    /// De termijn die de kern bij zijn stopverzoek op de vlag zette, of
    /// `None`: hij vraagt nog niets, of hij is van vóór 05-10 en schreef
    /// alleen [`KILL_STOP`].
    #[must_use]
    pub fn kill_grace(&self) -> Option<Duration> {
        match self.get(CTRL_KILL) {
            ms if ms > KILL_STOP => Some(Duration::from_millis(ms)),
            _ => None,
        }
    }

    /// De werkelijke geheugen-draw, voor het per-taak-rapport van de kern.
    pub fn set_mem_sys(&self, bytes: u64) {
        self.set(CTRL_MEM_SYS, bytes);
    }

    /// De klok-offset van de kern: wall-ns bij tellerstand nul (0 = nog
    /// niet gesynct).
    #[must_use]
    pub fn wall_offset(&self) -> u64 {
        self.get(CTRL_WALL_OFF)
    }

    /// Het aantal cores van dit slot (minstens 1).
    #[must_use]
    pub fn cores(&self) -> u64 {
        self.get(CTRL_CORES).max(1)
    }

    /// Draait dit slot op het hart van de kern (`CTRL_HART`, met magic)?
    #[must_use]
    pub fn on_kern_hart(&self) -> bool {
        shares_kern_hart(self.get(CTRL_HART))
    }

    /// Deelt dit slot zijn core? Dan yieldt de idle in plaats van te slapen.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.get(CTRL_SHARED) != 0
    }

    /// Vraagt het board om idle via een yield naar EL2 (Apple silicon)?
    #[must_use]
    pub fn is_yield_mode(&self) -> bool {
        self.get(CTRL_IDLE_MODE) & IDLE_YIELD != 0
    }

    /// Het aantal idle-rondes dat de slaper publiceerde (`CtrlWakes`): op
    /// een gedeelde core of in yield-modus is elke ronde een yield naar de
    /// switcher.
    #[must_use]
    pub fn idle_rounds(&self) -> u64 {
        self.get(CTRL_WAKES)
    }

    /// Publiceert de idle-teller (geslapen tikken) en het aantal rondes.
    pub fn publish_idle(&self, idle_ticks: u64, wakes: u64) {
        self.set(CTRL_IDLE, idle_ticks);
        self.set(CTRL_WAKES, wakes);
    }

    /// Zet de wek-drempel van de deurbel (0 = ontwapend).
    pub fn set_rx_door(&self, v: u64) {
        self.set(CTRL_RX_DOOR, v);
    }
}

/// De env-blob die de kern op de control-page schreef, één keer gekopieerd.
///
/// Vast in grootte: de pagina kan niet meer dragen, en een kopie maakt de
/// app onafhankelijk van wat de kern daarna met de pagina doet. Opzoeken is
/// een lineaire scan over `key=val\n`-regels; de blob is een paar honderd
/// bytes en wordt bij de start gelezen, niet in een heet pad.
///
/// De buffer is zo groot als de grootste env die een kern ooit schreef
/// ([`CTRL_ENV_LEGACY_MAX`]): een nieuwe app op een oude kern verliest
/// zijn env niet omdat het RNG-blok (30-09) de grens verlaagde.
pub struct Env {
    buf: [u8; CTRL_ENV_LEGACY_MAX as usize],
    len: usize,
}

impl Env {
    /// Een lege env.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            buf: [0; CTRL_ENV_LEGACY_MAX as usize],
            len: 0,
        }
    }

    /// Leest de blob van de pagina. Een lengte van 0 of voorbij het maximum
    /// geeft een lege env: de kern weigerde zo'n env al bij de start. Het
    /// maximum is [`CTRL_ENV_MAX`] op een kern met het RNG-blok (het
    /// bronwoord draagt de magic), en de oude grens op een kern zonder.
    #[must_use]
    pub fn read(ctrl: &Ctrl) -> Self {
        let mut env = Self::empty();
        let n = ctrl.get(CTRL_ENV_LEN);
        let max = match rng_source(ctrl.get(CTRL_RNG_SOURCE)) {
            Some(_) => CTRL_ENV_MAX,
            None => CTRL_ENV_LEGACY_MAX,
        };
        if n == 0 || n > max {
            return env;
        }
        let src = ctrl.addr(CTRL_ENV_DATA);
        if let Some(dst) = env.buf.get_mut(..n as usize) {
            dev::pull(src, dst.len());
            dev::copy_out(dst, src);
            env.len = dst.len();
        }
        env
    }

    /// De waarde van `key`, of `None` als hij ontbreekt of geen UTF-8 is.
    /// De `ER_PORT_*`/`ER_ATTR_*`-conventie van de kern werkt ongewijzigd.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        let blob = self.buf.get(..self.len)?;
        blob.split(|&c| c == b'\n').find_map(|line| {
            let eq = line.iter().position(|&c| c == b'=')?;
            let (k, v) = (line.get(..eq)?, line.get(eq + 1..)?);
            if eq > 0 && k == key.as_bytes() {
                core::str::from_utf8(v).ok()
            } else {
                None
            }
        })
    }

    /// Het aantal bytes in de blob.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Geen env?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use abi::layout::CTRL_STRIDE as CTRL_SIZE;

    /// Een control-page over een gewone buffer.
    pub(crate) struct Page(Vec<u64>);

    impl Page {
        pub(crate) fn new() -> Self {
            Self(vec![0u64; (CTRL_SIZE / 8) as usize])
        }
        pub(crate) fn ctrl(&self) -> Ctrl {
            Ctrl::at(Pa(self.0.as_ptr() as usize as u64))
        }
        pub(crate) fn word(&self, off: u64) -> u64 {
            self.0[(off / 8) as usize]
        }
        pub(crate) fn put(&mut self, off: u64, v: u64) {
            self.0[(off / 8) as usize] = v;
        }
    }

    #[test]
    fn ready_writes_ram_size_then_status() {
        let p = Page::new();
        let c = p.ctrl();
        assert_eq!(c.status(), Some(AppStatus::Empty));
        c.announce_ready(0x3e0_0000);
        assert_eq!(p.word(CTRL_RAM_SIZE), 0x3e0_0000);
        assert_eq!(p.word(CTRL_STATUS), AppStatus::Ready.raw());
        assert_eq!(c.status(), Some(AppStatus::Ready));
    }

    #[test]
    fn exit_leaves_code_and_status() {
        let p = Page::new();
        p.ctrl().mark_exited(7);
        assert_eq!(p.word(CTRL_EXIT_CODE), 7);
        assert_eq!(p.ctrl().status(), Some(AppStatus::Exited));
    }

    #[test]
    fn kernel_words_read_back() {
        let mut p = Page::new();
        p.put(CTRL_KILL, 1);
        p.put(CTRL_SHARED, 1);
        p.put(CTRL_IDLE_MODE, IDLE_YIELD);
        p.put(CTRL_WALL_OFF, 1_700_000_000_000_000_000);
        let c = p.ctrl();
        assert!(c.kill_requested());
        assert!(c.is_shared());
        assert!(c.is_yield_mode());
        assert_eq!(c.cores(), 1); // 0 op een geveegde pagina is één core
        assert_eq!(c.wall_offset(), 1_700_000_000_000_000_000);
    }

    #[test]
    fn env_is_read_from_the_page() {
        let mut p = Page::new();
        let blob = b"BUCKET=b1\nER_PORT_HTTP=18080\nEMPTY=\n=nokey\nROLE=spike";
        dev::copy_in(p.ctrl().addr(CTRL_ENV_DATA), blob);
        p.put(CTRL_ENV_LEN, blob.len() as u64);
        let env = Env::read(&p.ctrl());
        assert_eq!(env.len(), blob.len());
        assert_eq!(env.get("ER_PORT_HTTP"), Some("18080"));
        assert_eq!(env.get("ROLE"), Some("spike"));
        assert_eq!(env.get("EMPTY"), Some(""));
        assert_eq!(env.get("BUCKET"), Some("b1"));
        assert_eq!(env.get(""), None);
        assert_eq!(env.get("MISSING"), None);
    }

    #[test]
    fn oversized_env_length_gives_an_empty_env() {
        let mut p = Page::new();
        p.put(CTRL_ENV_LEN, CTRL_ENV_LEGACY_MAX + 1);
        assert!(Env::read(&p.ctrl()).is_empty());
        // Een kern met het RNG-blok schrijft nooit meer dan CTRL_ENV_MAX.
        p.put(
            CTRL_RNG_SOURCE,
            abi::hopabi::rng_source_word(abi::hopabi::RNG_SRC_JITTER),
        );
        p.put(CTRL_ENV_LEN, CTRL_ENV_MAX + 1);
        assert!(Env::read(&p.ctrl()).is_empty());
    }
}
