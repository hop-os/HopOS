//! De hardware-watchdog van de Radxa: de DesignWare-WDT van de RK3566
//! (`watchdog@fe600000`, `rockchip,rk3568-wdt`, `snps,dw-wdt`; de registers
//! in `driver_dwwdt`), achter `board::Watchdog` (`arm`, `pet`, `off`,
//! [`Desc`]), zodat de watchdog-taak van de kern (`hopos/src/watchdog.rs`)
//! hem aait zoals elke andere. Go: `OLD/metal/board/rk3566/wdt.go` en
//! `cmd/hopos/board_rk3566.go`.
//!
//! Dit bezit het adres van de watchdog, zijn twee klokgates en zijn twee
//! reset-lijnen; het beleid (wanneer aaien) is `kern::watchdog`.
//!
//! Wat GEMETEN is (Go, 06-08, op dit bord): de tclk is xin24m zonder deler en
//! dit silicium heeft de vaste TOP-tabel (COMP_PARAMS_1 bit 6), dus TOP n is
//! 2^(16+n) tikken van 24 MHz; TOP 15 las 89,48 s terug uit CCVR, exact
//! 2^31 / 24 MHz. En een afgelopen teller reset de HELE node: met de kortste
//! TOP gewapend en niet geaaid kwam direct `DDR V1.18`, de DDR-init uit de
//! boot-ROM, gevolgd door SPL, TF-A en U-Boot.
//!
//! De timeout is die van Go: TOP 15, ongeveer 89,5 s ([`TIMEOUT_MS`]). De
//! tabel is grof (elke stap verdubbelt) en de kern aait elke 2 s; een
//! kortere TOP koopt alleen een snellere reset van een node die toch al stil
//! staat.
//!
//! Een DW-WDT kent geen uit-knop: eenmaal ENABLE is hij aan tot een reset.
//! Linux zet hem daarom uit door zijn reset-lijn te pulsen
//! (`dw_wdt_stop`, als de DT een `resets` geeft); de rk356x-DT geeft die
//! niet, maar de CRU heeft ze wel: SRST_P_WDT_NS (421) en SRST_T_WDT_NS
//! (422), bank 26 bit 5 en 6 (rk3568-cru.h). [`off`] pulst ze en kijkt of
//! ENABLE daarna echt nul leest. NOG NIET OP IJZER GEMETEN.

use crate::soc::{CRU, hiword};
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use dev::Pa;
use driver_dwwdt::{self as dw, DwWdt, ENABLE, TOP_MAX};

/// De watchdog (rk356x-base.dtsi: `watchdog@fe600000`, `reg = <... 0x100>`).
pub const WDT: Pa = Pa(0xFE60_0000);

/// `CLKGATE_CON(26)`: bit 13 = pclk (de registers), bit 14 = tclk (de teller);
/// clk-rk3568.c `PCLK_WDT_NS`, `TCLK_WDT_NS`. Actief-laag.
const CLKGATE26: u64 = 0x300 + 26 * 4;
const GATE_PCLK: u32 = 13;
const GATE_TCLK: u32 = 14;

/// De tclk: xin24m zonder deler (clk-rk3568.c, en gemeten 06-08).
pub const TCLK_HZ: u64 = 24_000_000;
/// De timeout van Go: TOP 15, 2^31 tikken, 89 478 ms.
pub const TIMEOUT_MS: u64 = top_ms(TOP_MAX);

/// Heeft DEZE kern hem gewapend? Onderscheidt een erfenis van de vorige kern
/// (een flip) van onze eigen tweede `arm` (de boot-guard wapent eerst, de
/// taak daarna nog eens).
static ARMED: AtomicBool = AtomicBool::new(false);

/// De timeout van TOP `top` in milliseconden (de vaste tabel: 2^(16+top)
/// tikken).
#[must_use]
pub const fn top_ms(top: u32) -> u64 {
    dw::top_ms(top, TCLK_HZ)
}

/// De kleinste TOP met een timeout van minstens `timeout_ms`, hoogstens
/// [`TOP_MAX`].
#[must_use]
pub fn top_for(timeout_ms: u64) -> u32 {
    dw::top_for(timeout_ms, TCLK_HZ, 0)
}

/// Wat er gewapend is, voor de consoleregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    /// Het blok.
    pub base: u64,
    /// De TOP in TORR.
    pub top: u32,
    /// De teller direct na de kick: de werkelijke TOP in tclk-tikken (0 =
    /// niet te meten).
    pub counts: u32,
    /// Meldt het silicium de vaste TOP-tabel?
    pub fixed: bool,
    /// Was hij al gewapend (door de vorige kern)?
    pub inherited: bool,
}

impl Desc {
    /// De timeout in milliseconden: gemeten als de teller laadde, anders
    /// uit de tabel.
    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        match self.counts {
            0 => top_ms(self.top),
            n => u64::from(n) * 1000 / TCLK_HZ,
        }
    }
}

impl core::fmt::Display for Desc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ms = self.timeout_ms();
        write!(
            f,
            "DW-WDT at {:#x}, TOP {} = {}.{} s ({}, fixed-top {}){}",
            self.base,
            self.top,
            ms / 1000,
            (ms % 1000) / 100,
            if self.counts == 0 {
                "from the table, CCVR did not load"
            } else {
                "measured in CCVR at 24 MHz"
            },
            self.fixed,
            if self.inherited {
                ", already armed by the previous kernel"
            } else {
                ""
            }
        )
    }
}

/// Wapent de watchdog op `timeout_ms` (naar boven afgerond op de TOP-tabel,
/// hoogstens [`TIMEOUT_MS`]). Onomkeerbaar tot [`off`] of een reset. `Ok`
/// met de beschrijving voor de "armed"-regel, of de reden waarom niet.
pub fn arm(timeout_ms: u64) -> Result<Desc, &'static str> {
    arm_at(WDT, CRU, timeout_ms, &ARMED)
}

/// [`arm`] op het blok `wdt` met de CRU `cru`. De volgorde van
/// `dw_wdt_start`: timeout, kick, dan enable (met response mode 0: meteen
/// resetten, geen IRQ eerst). Staat hij al aan (de vorige kern), dan volstaan
/// timeout en kick.
fn arm_at(wdt: Pa, cru: Pa, timeout_ms: u64, armed: &AtomicBool) -> Result<Desc, &'static str> {
    clocks_on(cru);
    let w = DwWdt::new(wdt);
    let top = top_for(timeout_ms);
    let was_on = w.enabled();
    let inherited = was_on && !armed.load(Relaxed);
    let fixed = w.fixed_top();
    w.set_top(top);
    w.kick();
    dev::mb();
    let counts = w.counter();
    if !was_on {
        w.enable(ENABLE);
        dev::mb();
        if !w.enabled() {
            return Err("DW-WDT ENABLE does not read back, not armed");
        }
    }
    armed.store(true, Relaxed);
    Ok(Desc {
        base: wdt.0,
        top,
        counts,
        fixed,
        inherited,
    })
}

/// Opent pclk en tclk. De tclk is de teller zelf: zonder die klok loopt hij
/// niet af en is een gewapende watchdog een gewapend niets.
fn clocks_on(cru: Pa) {
    dev::write32(
        cru.add(CLKGATE26),
        hiword(0, 1, GATE_PCLK) | hiword(0, 1, GATE_TCLK),
    );
    dev::mb();
}

/// Zet de teller terug op vol: één schrijf, geen read-modify-write. Ook
/// zonder eigen `arm` (rond een flip, `hopos::watchdog::pet_now`): een kick op
/// een gewapende DW-WDT herlaadt hem, op een uitgeschakelde doet hij niets.
/// Alleen met de pclk open: een ongeklokt Rockchip-blok kan de bus
/// vasthouden (Go, 06-08), en de CRU leest altijd.
pub fn pet() {
    pet_at(WDT, CRU);
}

/// [`pet`] op het blok `wdt` met de CRU `cru`.
fn pet_at(wdt: Pa, cru: Pa) {
    if dev::read32(cru.add(CLKGATE26)) & (1 << GATE_PCLK) == 0 {
        DwWdt::new(wdt).kick();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driver_dwwdt::{CCVR, CR, CRR, KICK, PARAMS, TORR, USE_FIX_TOP};

    /// Een nep-WDT (0x100 bytes) en een nep-CRU tot en met SOFTRST_CON(26).
    fn blocks(cr: u32, ccvr: u32, params: u32) -> (Vec<u32>, Vec<u32>, Pa, Pa) {
        let mut w = vec![0u32; 0x40];
        w[(CR / 4) as usize] = cr;
        w[(CCVR / 4) as usize] = ccvr;
        w[(PARAMS / 4) as usize] = params;
        let mut c = vec![0u32; 0x480 / 4];
        // Na reset staan de gates dicht (actief-laag: 1 = uit).
        c[(CLKGATE26 / 4) as usize] = 0x6000;
        let (wp, cp) = (
            Pa(w.as_mut_ptr() as usize as u64),
            Pa(c.as_mut_ptr() as usize as u64),
        );
        (w, c, wp, cp)
    }

    #[test]
    fn the_top_table_is_the_measured_one() {
        assert_eq!(top_ms(15), 89_478);
        assert_eq!(TIMEOUT_MS, 89_478);
        assert_eq!(top_ms(0), 2);
        assert_eq!(top_for(TIMEOUT_MS), 15);
        // 12 s (de Pi's) valt tussen TOP 12 (11,2 s) en 13 (22,4 s).
        assert_eq!(top_for(12_000), 13);
        assert_eq!(top_for(0), 0);
        assert_eq!(top_for(u64::MAX), TOP_MAX);
    }

    #[test]
    fn a_fresh_arm_sets_the_timeout_kicks_then_enables() {
        let (w, c, wp, cp) = blocks(0, 1 << 31, USE_FIX_TOP);
        let armed = AtomicBool::new(false);
        let d = arm_at(wp, cp, TIMEOUT_MS, &armed).unwrap();
        assert_eq!(w[(TORR / 4) as usize], 0xFF);
        assert_eq!(w[(CRR / 4) as usize], KICK);
        assert_eq!(w[(CR / 4) as usize], ENABLE, "response mode must stay 0");
        // Beide gates open: alleen maskerbits.
        assert_eq!(c[(CLKGATE26 / 4) as usize], (1 << 29) | (1 << 30));
        assert!(!d.inherited && d.fixed);
        assert_eq!(d.timeout_ms(), 89_478);
        assert!(armed.load(Relaxed));
        let s = d.to_string();
        assert!(s.contains("TOP 15 = 89.4 s (measured"), "{s}");
    }

    #[test]
    fn an_armed_watchdog_is_an_inheritance_once() {
        let (w, _c, wp, cp) = blocks(ENABLE, 1 << 31, USE_FIX_TOP);
        let armed = AtomicBool::new(false);
        let d = arm_at(wp, cp, TIMEOUT_MS, &armed).unwrap();
        assert!(d.inherited);
        assert!(d.to_string().contains("previous kernel"));
        assert_eq!(w[(CRR / 4) as usize], KICK);
        // De tweede arm van deze kern (guard, dan taak) is geen erfenis.
        assert!(!arm_at(wp, cp, TIMEOUT_MS, &armed).unwrap().inherited);
    }

    #[test]
    fn a_counter_that_does_not_load_falls_back_to_the_table() {
        let (_w, _c, wp, cp) = blocks(0, 0, 0);
        let d = arm_at(wp, cp, TIMEOUT_MS, &AtomicBool::new(false)).unwrap();
        assert_eq!(d.timeout_ms(), 89_478);
        assert!(d.to_string().contains("from the table"));
    }

    #[test]
    fn a_pet_needs_the_register_clock() {
        let (w, mut c, wp, cp) = blocks(ENABLE, 0, 0);
        pet_at(wp, cp);
        assert_eq!(w[(CRR / 4) as usize], 0, "kicked a block without pclk");
        c[(CLKGATE26 / 4) as usize] = 0;
        pet_at(wp, cp);
        assert_eq!(w[(CRR / 4) as usize], KICK);
    }
}
