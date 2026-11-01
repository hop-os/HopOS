//! Apple's watchdog, en de kloksnelheid van de clusters: twee dingen die
//! m1n1 stilletjes voor ons deed en die natief van ons zijn.
//!
//! DE WATCHDOG IS WAARAAN DE NODE 31-08 EEN AVOND LANG STIERF. iBoot laat er
//! meer dan één gewapend achter; m1n1's `main.c` zet ze bij zijn opstart uit
//! (`wdt_disable`), dus onder de loader merk je niets. Natief resette de
//! node op exact 1:43, elke keer, midden in een gezonde boot. De les van die
//! avond: gaat iets alleen NATIEF stuk, vraag dan als eerste wat de loader
//! voor je deed. [`quiet`] zet ELK venster van `/arm-io/wdt` stil (het
//! primaire met CTL op nul, de volgende met een nul op offset 0), ongeacht
//! `wdt-version`: een watchdog die je per ongeluk laat staan reset je node,
//! en een nul schrijven die er al stond kost niets. [`arm`], [`pet`] en
//! [`off`] nemen de primaire over voor het beleid van de kern (HOP-leven =
//! node-leven, hopos `watchdog.rs`), met dezelfde vorm als
//! `board_uefi::watchdog` en de tellerklok van 24 MHz in de regel: die staat
//! niet in de boom, en een verkeerde aanname hoort in de console te staan,
//! niet in een stille reset.
//!
//! De clusters ([`pstate_tune`], [`PStateWatch`]): iBoot laat ze laag achter en na de boot
//! regelt niemand ze meer. GEMETEN 01-09: E op p-state 1 (900 MHz), P op 4;
//! elke core die WIJ uit reset halen begint laag. Na m1n1's
//! `cpufreq_init_cluster`-recept voor de naaste familie (t8122/t6030):
//! 723,8 Msteps/s op een E-core, precies de 2172/900 = 2,41x van de tabel.
//! Het doel is bewust niet de top (E 5 = 2,17 GHz @ 785 mV, P 6 = 2,35 GHz @
//! 780 mV): zonder thermometer blind naar de 1150 mV-hoek klokken is vragen
//! om problemen.

use crate::fwinfo;
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use dev::Pa;

const WDT_COUNT: u64 = 0x10;
const WDT_ALARM: u64 = 0x14;
const WDT_CTL: u64 = 0x1c;
/// CTL bit 2: reset de machine als de teller het alarm haalt.
const WDT_CTL_RESET: u32 = 1 << 2;
/// De tellerklok; niet in de boom (Go `wdt.go`, 31-08).
pub const WDT_HZ: u32 = 24_000_000;
/// De timeout die de kern vraagt: ruim boven de aai-cadans van het beleid
/// (2 s) en boven de langste blokkerende wacht van een coprocessor
/// (RTKit `POWER_TIMEOUT_NS`, 5 s). Go: 30 s (`wdtTimeout`).
pub const WDT_TIMEOUT_MS: u64 = 30_000;

/// Een regel voor de console, zonder heap.
pub struct Line {
    buf: [u8; 160],
    len: usize,
}

impl Line {
    const fn new() -> Self {
        Self {
            buf: [0; 160],
            len: 0,
        }
    }

    /// De tekst.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.buf.get(..self.len).unwrap_or_default()).unwrap_or("?")
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        if let (Some(d), Some(src)) = (
            self.buf.get_mut(self.len..self.len + n),
            s.as_bytes().get(..n),
        ) {
            d.copy_from_slice(src);
        }
        self.len += n;
        Ok(())
    }
}

/// Zet élke watchdog van `/arm-io/wdt` stil. Geeft de consoleregel, die ook
/// in de foutgevallen moet kloppen: lukt het niet, dan reset deze node over
/// twee minuten en is dit de regel waar iemand naar zit te staren.
pub fn quiet() -> Line {
    let mut l = Line::new();
    let Some(t) = fwinfo::adt() else {
        let _ = l.write_str("firmware watchdogs NOT silenced: no device tree");
        return l;
    };
    let Some(chain) = t.trace("/arm-io/wdt") else {
        let _ = l.write_str("firmware watchdogs NOT silenced: no /arm-io/wdt");
        return l;
    };
    let _ = l.write_str("firmware watchdogs silenced:");
    let mut n = 0;
    while let Some((base, _)) = t.reg_at(&chain, n).filter(|r| r.0 != 0) {
        if n == 0 {
            dev::write32(Pa(base + WDT_CTL), 0);
        } else {
            dev::write32(Pa(base), 0);
        }
        dev::mb();
        let _ = write!(l, " reg[{n}] {base:#x}");
        n += 1;
    }
    let v = t.u32(chain.node(), "wdt-version").unwrap_or(0);
    let _ = write!(l, " ({n} window(s), wdt-version {v})");
    l
}

/// De basis van de gewapende watchdog (0 = niet gewapend): [`arm`] zet
/// hem, [`pet`] en [`off`] lezen hem. Eén schrijver, de watchdog-taak.
static ARMED: AtomicU64 = AtomicU64::new(0);

/// De gewapende watchdog, voor de consoleregel van het beleid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    /// Het registerblok (`/arm-io/wdt` reg[0]).
    pub base: u64,
    /// De timeout in milliseconden, zoals geteld op [`WDT_HZ`].
    pub timeout_ms: u64,
}

impl fmt::Display for Desc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Apple WDT at {:#x}, {} ms timeout at {} MHz (assumed, not in the ADT)",
            self.base,
            self.timeout_ms,
            WDT_HZ / 1_000_000
        )
    }
}

/// Het alarm in tellertikken voor `timeout_ms`, geklemd op wat in het
/// 32-bit ALARM past (178 s op 24 MHz) en minstens één seconde.
#[must_use]
pub const fn alarm_ticks(timeout_ms: u64) -> u32 {
    let ms = if timeout_ms < 1000 { 1000 } else { timeout_ms };
    let t = ms.saturating_mul(WDT_HZ as u64) / 1000;
    if t > u32::MAX as u64 {
        u32::MAX
    } else {
        t as u32
    }
}

/// Wapent de primaire watchdog op `timeout_ms` (Go: 30 s, ruim boven de
/// aai-cadans van het beleid). Leest terug: een gewapende watchdog die níét
/// reset is erger dan geen (de les van de Radxa).
pub fn arm(timeout_ms: u64) -> Result<Desc, &'static str> {
    let (base, _) = fwinfo::reg("/arm-io/wdt", 0).ok_or("no /arm-io/wdt in the ADT")?;
    let b = Pa(base);
    let ticks = alarm_ticks(timeout_ms);
    dev::write32(b.add(WDT_COUNT), 0);
    dev::write32(b.add(WDT_ALARM), ticks);
    dev::write32(b.add(WDT_CTL), WDT_CTL_RESET);
    dev::mb();
    if dev::read32(b.add(WDT_CTL)) & WDT_CTL_RESET == 0 {
        dev::write32(b.add(WDT_CTL), 0);
        return Err("Apple WDT refuses to arm");
    }
    ARMED.store(base, Relaxed);
    Ok(Desc {
        base,
        timeout_ms: u64::from(ticks) * 1000 / u64::from(WDT_HZ),
    })
}

/// Aait de gewapende watchdog: de teller op nul. Zonder [`arm`] niets.
pub fn pet() {
    let base = ARMED.load(Relaxed);
    if base != 0 {
        dev::write32(Pa(base + WDT_COUNT), 0);
        dev::mb();
    }
}

// ---------------------------------------------------------------------------
// De kloksnelheid.
// ---------------------------------------------------------------------------

const CL_PSTATE: u64 = 0x20020;
const CL_UNK_440F8: u64 = 0x440f8;
const PS_BUSY: u64 = 1 << 31;
const PS_SET: u64 = 1 << 25;
const PS_APSC_DIS: u64 = 1 << 23;
const PS_APSC_BUSY: u64 = 1 << 7;
const PS_PLL: u64 = 1 << 42;
const PS_DESIRED: u64 = 0x1f;
/// m1n1's defaults voor de buurchips.
pub const PS_DEFAULT: [u64; 2] = [5, 6];
const PS_SPINS: u32 = 1 << 20;

/// Het DVFM-blok van een cluster: (impl & !0xFF_FFFF) | 0xE0_0000, het
/// patroon dat m1n1 van t8103 tot t6031 hardcodet, geverifieerd tegen de
/// ADT-dump van deze machine (E: 0x2_1005_0000 → 0x2_10e0_0000).
#[must_use]
pub const fn cluster_base(impl_reg: u64) -> u64 {
    (impl_reg & !0xff_ffff) | 0xe0_0000
}

/// De frequentie van een tabel-raw in MHz (65536e9 / raw Hz: de schaal die
/// de bekende M4-kloks exact geeft).
#[must_use]
pub const fn ps_mhz(raw: u32) -> u32 {
    if raw == 0 {
        0
    } else {
        (65_536_000 / raw as u64) as u32
    }
}

fn set_pstate(base: Pa, ps: u64) -> bool {
    let v = dev::read64(base.add(CL_PSTATE));
    if v & PS_DESIRED == ps {
        // m1n1 schrijft niet als hij er al staat.
        return true;
    }
    dev::write64(
        base.add(CL_PSTATE),
        (v & !PS_DESIRED) | PS_SET | (ps & PS_DESIRED),
    );
    (0..PS_SPINS).any(|_| dev::read64(base.add(CL_PSTATE)) & PS_BUSY == 0)
}

/// De stappen van het recept per cluster, met hun naam voor de SError-toets.
const STEPS: [[&str; 4]; 2] = [
    [
        "cpufreq E: back to pstate 1",
        "cpufreq E: the APSC and PLL bits",
        "cpufreq E: the word at 0x440f8",
        "cpufreq E: the target pstate",
    ],
    [
        "cpufreq P: back to pstate 1",
        "cpufreq P: the APSC and PLL bits",
        "cpufreq P: (no word at 0x440f8)",
        "cpufreq P: the target pstate",
    ],
];

/// Het recept op beide clusters (0 = E, 1 = P): p-state 1, de APSC (de
/// hardware-governor) aan als de pmgr-node `cpu-apsc` draagt, het
/// onverklaarde woord 0x440f8 op 1 (alleen E), en dan het doel. Eén regel per
/// cluster; `off` = alleen meten.
///
/// `serror(stap)` toetst na elke schrijf of er een SError pending staat
/// (`true` = ja). Op dit silicium is een verboden schrijf stil en blijft
/// zijn SError hangen (de M4, 01-10: 131.000 slapen per seconde en een rode
/// voorproef). Dus: bij de eerste SError stopt het recept, en zegt de regel
/// bij welke stap.
pub fn pstate_tune(target: [u64; 2], off: bool, serror: &dyn Fn(&'static str) -> bool) {
    let Some(t) = fwinfo::adt() else { return };
    let Some(pm) = t.path("/arm-io/pmgr") else {
        return;
    };
    let apsc = t.u32(pm, "cpu-apsc").unwrap_or(0) != 0;
    for (cl, (name, table)) in [("E", "voltage-states1"), ("P", "voltage-states5")]
        .into_iter()
        .enumerate()
    {
        let raws = t.prop(pm, table).unwrap_or_default();
        let states = raws.len() / 8;
        let raw = |ps: u64| {
            let i = usize::try_from(ps).unwrap_or(0).checked_sub(1)?;
            let w = raws.get(8 * i..8 * i + 4)?;
            Some(ps_mhz(u32::from_le_bytes([w[0], w[1], w[2], w[3]])))
        };
        let imp = (0..fwinfo::cpus())
            .find(|&i| fwinfo::is_p_core(i) == (cl == 1))
            .and_then(fwinfo::cpu_impl)
            .filter(|&a| a != 0);
        let (Some(imp), true) = (imp, states > 0) else {
            cpu::println!(
                "cpufreq: cluster {cl} ({name}): no impl-reg or voltage-states, boot clock stays"
            );
            continue;
        };
        let base = Pa(cluster_base(imp));
        let v0 = dev::read64(base.add(CL_PSTATE));
        let cur = v0 & PS_DESIRED;
        if v0 == u64::MAX || v0 & PS_BUSY != 0 {
            cpu::println!(
                "cpufreq: cluster {cl} ({name}): CLUSTER_PSTATE reads {v0:#x}, not touching it"
            );
            continue;
        }
        if off {
            cpu::println!(
                "cpufreq: cluster {cl} ({name}): pstate {cur}/{states} ({:?} MHz), measuring only, CLUSTER_PSTATE {v0:#x}",
                raw(cur)
            );
            continue;
        }
        let steps = STEPS.get(cl).copied().unwrap_or_default();
        let want = target.get(cl).copied().unwrap_or(1).clamp(1, states as u64);
        let mut ok = true;
        for (i, step) in steps.iter().enumerate() {
            match i {
                0 => ok &= set_pstate(base, 1),
                1 => {
                    let v = dev::read64(base.add(CL_PSTATE));
                    let v = if apsc {
                        v & !PS_APSC_DIS
                    } else {
                        v | PS_APSC_DIS
                    } & !PS_PLL;
                    dev::write64(base.add(CL_PSTATE), v);
                    if apsc {
                        let _ = (0..PS_SPINS)
                            .any(|_| dev::read64(base.add(CL_PSTATE)) & PS_APSC_BUSY == 0);
                    }
                }
                // Go schreef hier ook bit 63 weg in de "throttle-registers"
                // (+0x48400, +0x48408, +0x40270, +0x40250; m1n1-gedrag voor
                // oudere chips). Op de t8132 bestaat +0x48400 niet: de
                // schrijf gaat stil mis en laat een SError achter (gemeten
                // 01-10: L2C sts=0x202 adr=0x1a82500211e48400, dat is
                // P-cluster 0x211e00000 + 0x48400, inf=0x1400100007).
                //
                // Het woord +0x440f8 (m1n1: "Unknown", voor de hele
                // M3-familie op elk cluster) neemt alleen het E-cluster aan;
                // op het P-cluster laat de schrijf een SError achter die
                // blijft hangen (gemeten 01-10, stempel M5: "SError after
                // cpufreq P: the word at 0x440f8", l2c sts=0x0, daarna
                // 46.000 slapen per seconde en een rode OS-core-zelftest).
                2 if cl == 0 => dev::write64(base.add(CL_UNK_440F8), 1),
                2 => {}
                _ => ok &= set_pstate(base, want),
            }
            if serror(step) {
                let v = dev::read64(base.add(CL_PSTATE));
                cpu::println!(
                    "cpufreq: SError after \"{step}\" (CLUSTER_PSTATE {v0:#x} -> {v:#x}); the tune stops here, the rest of the clusters stay as they are HOPOS_APPLE_PSTATE_SERROR"
                );
                return;
            }
        }
        let v = dev::read64(base.add(CL_PSTATE));
        let after = v & PS_DESIRED;
        cpu::println!(
            "cpufreq: cluster {cl} ({name}): pstate {cur} ({:?} MHz) -> {after}/{states} ({:?} MHz), apsc={apsc}, CLUSTER_PSTATE {v0:#x} -> {v:#x}{}",
            raw(cur),
            raw(after),
            if ok {
                ""
            } else {
                " (WARNING: switch still busy)"
            }
        );
    }
}

/// Het plafond uit `hopos.pstate`: "off" = alleen meten (`None`), "E,P" =
/// die twee (elk minstens 1), anders [`PS_DEFAULT`]. Een onleesbare waarde
/// is de default, niet de bodemklok (Go `pstateTargets`).
#[must_use]
pub fn pstate_targets(v: &str) -> Option<[u64; 2]> {
    if v == "off" {
        return None;
    }
    let mut it = v.split(',').map(|x| x.trim().parse::<u64>().ok());
    match (it.next(), it.next(), it.next()) {
        (Some(Some(e)), Some(Some(p)), None) if e >= 1 && p >= 1 => Some([e, p]),
        _ => Some(PS_DEFAULT),
    }
}

/// De wachter op de p-states (Go `PStateWatch`): leest per cluster
/// CLUSTER_PSTATE en zegt alleen iets als hij VERANDERT. Dat beantwoordt de
/// vraag die [`pstate_tune`] openliet: klokt de APSC (de hardware-governor
/// die wij aanzetten) uit zichzelf terug als er niets te doen is, en weer
/// omhoog onder belasting? Een node die stil op zijn plafond staat, kost
/// dit geen consoleregel.
///
/// Alleen-lezen: hij schrijft nooit een p-state. Eén eigenaar, de taak die
/// [`PStateWatch::poll`] elke paar seconden roept.
pub struct PStateWatch {
    /// Het DVFM-blok per cluster (E, P), als de boom het geeft.
    base: [Option<Pa>; 2],
    /// De laatst gemelde p-state per cluster.
    last: [Option<u64>; 2],
}

impl PStateWatch {
    /// De wachter over de clusters uit de ADT; de eerste [`Self::poll`] is
    /// de nulmeting en meldt niets.
    #[must_use]
    pub fn new() -> Self {
        let base = |cl: usize| {
            (0..fwinfo::cpus())
                .find(|&i| fwinfo::is_p_core(i) == (cl == 1))
                .and_then(fwinfo::cpu_impl)
                .filter(|&a| a != 0)
                .map(|a| Pa(cluster_base(a)))
        };
        Self {
            base: [base(0), base(1)],
            last: [None, None],
        }
    }

    /// Heeft deze node iets om te bewaken?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.base.iter().all(Option::is_none)
    }

    /// Eén ronde: per cluster de p-state, en een regel bij een sprong.
    pub fn poll(&mut self) {
        for (cl, (b, last)) in self.base.iter().zip(self.last.iter_mut()).enumerate() {
            let Some(b) = b else { continue };
            let v = dev::read64(b.add(CL_PSTATE));
            if v == u64::MAX {
                continue;
            }
            let ps = v & PS_DESIRED;
            if let Some(was) = *last
                && was != ps
            {
                cpu::println!(
                    "cpufreq: cluster {cl} ({}) {was} -> {ps} HOPOS_APPLE_PSTATE",
                    if cl == 0 { "E" } else { "P" }
                );
            }
            *last = Some(ps);
        }
    }
}

impl Default for PStateWatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_base_and_mhz() {
        assert_eq!(cluster_base(0x2_1005_0000), 0x2_10e0_0000);
        assert_eq!(cluster_base(0x2_1105_0000), 0x2_11e0_0000);
        // E-max 2,89 GHz: raw 22676 geeft 2890 MHz.
        assert_eq!(ps_mhz(22_676), 2890);
        assert_eq!(ps_mhz(0), 0);
        let mut l = Line::new();
        for _ in 0..40 {
            let _ = l.write_str("0123456789");
        }
        assert_eq!(l.as_str().len(), 160, "begrensd, geen paniek");
    }

    #[test]
    fn without_a_tree() {
        assert!(quiet().as_str().contains("NOT silenced"));
        assert!(arm(30_000).is_err());
        pet();
        pstate_tune(PS_DEFAULT, false, &|_| false);
        let mut w = PStateWatch::new();
        assert!(w.is_empty());
        w.poll();
    }

    #[test]
    fn the_alarm_fits_the_register() {
        // 30 s op 24 MHz: 720M tikken, zoals Go (31-08).
        assert_eq!(alarm_ticks(30_000), 720_000_000);
        // Te kort wordt een seconde, te lang de grens van 32 bits.
        assert_eq!(alarm_ticks(10), WDT_HZ);
        assert_eq!(alarm_ticks(1_000_000), u32::MAX);
        let d = Desc {
            base: 0x2_922b_0000,
            timeout_ms: 30_000,
        };
        let mut l = Line::new();
        let _ = write!(l, "{d}");
        assert!(l.as_str().starts_with("Apple WDT at 0x2922b0000, 30000 ms"));
    }

    #[test]
    fn the_pstate_ceiling_from_the_config() {
        assert_eq!(pstate_targets("off"), None);
        assert_eq!(pstate_targets(""), Some(PS_DEFAULT));
        assert_eq!(pstate_targets("4,7"), Some([4, 7]));
        assert_eq!(pstate_targets(" 3 , 5 "), Some([3, 5]));
        assert_eq!(pstate_targets("0,5"), Some(PS_DEFAULT));
        assert_eq!(pstate_targets("max"), Some(PS_DEFAULT));
        assert_eq!(pstate_targets("1,2,3"), Some(PS_DEFAULT));
    }
}
