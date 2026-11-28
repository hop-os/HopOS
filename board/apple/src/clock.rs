//! De klokknop van de Mac mini: de p-state per cluster in het DVFM-blok,
//! als [`driver_dvfs::Knob`] voor het klokbeleid van de kern
//! (`hopos/src/telemetry.rs`), zoals de mailbox-knop van de Pi's
//! (`board/raspi/src/clock.rs`).
//!
//! Afgekeken van Linux (`drivers/cpufreq/apple-soc-cpufreq.c`,
//! `apple_soc_cpufreq_set_target`): wacht tot BUSY (bit 31) van het
//! commando-register (CLUSTER_PSTATE, +0x20020) nul is, hoogstens 400 µs;
//! vervang PS1 (bits 4:0) en zet SET (bit 25); schrijf, en wacht daarna
//! niet. Elke cluster is een eigen klok. Er is geen "plafond waaronder de
//! APSC governt": tot 28-11 dacht de code dat, maar wat `pstate_tune` bij de
//! boot schrijft (m1n1's default 5/6) is de gevraagde p-state, en daar
//! bleef de node op staan (E 2172, P 2352 MHz).
//!
//! Twee standen voor beide clusters samen: vol is per cluster de hoogste
//! p-state uit `voltage-states` (begrensd door `hopos.mhz`), stil is de
//! laagste. Het commentaar van Linux: het register negeert "boost
//! limitations", dus de hardware kan lager leveren dan de topstate vraagt.
//!
//! Wat de knop niet weet: de temperatuur (de SMC meet hier alleen bij de
//! boot, `lib.rs`), en de throttle-registers van de M4-familie (die van
//! m1n1 bestaan op de t8132 niet, `wdt.rs`). `hopos.mhz` is daarom de rem.

use crate::fwinfo;
use crate::wdt::{cluster_base, ps_mhz};
use core::fmt;
use dev::Pa;
use driver_dvfs::{Knob, Level};

/// Zoveel p-states per cluster houden we bij (PS1 is vijf bits).
pub const MAX_STATES: usize = 32;

const CL_PSTATE: u64 = 0x20020;
// Linux leest de huidige p-state uit `APPLE_DVFS_STATUS` (+0x20050). Op de
// t8132 NIET doen: één lees van dat woord legde kern B na een flip om,
// vóór het net en zonder SError-regel (de M4, 28-11, bisect: met de lees
// een koude reset, zonder geland en vol op 4464 MHz). Wat de chip echt
// draait, zegt de rekenmeting (vitals cpu), niet een register.
const PS_BUSY: u64 = 1 << 31;
const PS_SET: u64 = 1 << 25;
const PS1: u64 = 0x1f;
/// Linux `APPLE_DVFS_TRANSITION_TIMEOUT`: 400 µs.
const BUSY_NS: u64 = 400_000;

/// De tabellen van de clusters in de pmgr-node: E, dan P (GEMETEN op de M4,
/// 01-10; zie `wdt::pstate_tune`).
const TABLES: [&str; 2] = ["voltage-states1", "voltage-states5"];
const NAMES: [&str; 2] = ["E", "P"];

/// De twee standen per cluster.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Vol: de p-state en zijn MHz, per cluster (E, P).
    pub full: [(u64, u32); 2],
    /// Stil: idem.
    pub quiet: [(u64, u32); 2],
    /// Het aantal p-states per cluster.
    pub states: [usize; 2],
}

/// Waarom er geen knop is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Een chip waar de clusterblokken niet gemeten zijn (de M6), zonder
    /// `hopos.pstate` die het toch vraagt.
    NotMeasured {
        /// De SoC.
        soc: &'static str,
    },
    /// Geen ADT, pmgr-node, impl-reg of `voltage-states` voor een cluster.
    NoTable {
        /// De cluster (0 = E, 1 = P).
        cluster: usize,
    },
    /// Een cluster met één p-state: stil en vol vallen samen.
    OneState {
        /// De cluster.
        cluster: usize,
    },
    /// `hopos.mhz` ligt op of onder de stil-stand van een cluster.
    CapBelowQuiet {
        /// De cap in MHz.
        cap_mhz: u32,
        /// De cluster.
        cluster: usize,
        /// Zijn stil-stand in MHz.
        quiet_mhz: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = |c: &usize| NAMES.get(*c).copied().unwrap_or("?");
        match self {
            Self::NotMeasured { soc } => write!(
                f,
                "the cluster blocks of {soc} are not measured (hopos.pstate=E,P tries the M4 recipe)"
            ),
            Self::NoTable { cluster } => write!(
                f,
                "cluster {cluster} ({}) has no impl-reg or voltage-states in the ADT",
                name(cluster)
            ),
            Self::OneState { cluster } => write!(
                f,
                "cluster {cluster} ({}) has one p-state, nothing to turn",
                name(cluster)
            ),
            Self::CapBelowQuiet {
                cap_mhz,
                cluster,
                quiet_mhz,
            } => write!(
                f,
                "hopos.mhz={cap_mhz} is not above the quiet clock of cluster {cluster} ({}), {quiet_mhz} MHz",
                name(cluster)
            ),
        }
    }
}

/// De standen uit de MHz per p-state (index 0 = p-state 1) en `hopos.mhz`.
/// Vol is de snelste p-state binnen de cap (bij gelijke MHz de hoogste),
/// stil de traagste: zo doet de volgorde van de tabel er niet toe.
pub fn plan(mhz: [&[u32]; 2], cap_mhz: Option<u32>) -> Result<Plan, Error> {
    let mut p = Plan {
        full: [(0, 0); 2],
        quiet: [(0, 0); 2],
        states: [0; 2],
    };
    for (cluster, t) in mhz.iter().enumerate() {
        let n = t.len().min(MAX_STATES);
        let ps = |i: usize| i as u64 + 1;
        let known = t.iter().take(n).enumerate().filter(|(_, m)| **m > 0);
        let quiet = known
            .clone()
            .min_by_key(|(i, m)| (**m, *i))
            .map(|(i, m)| (ps(i), *m))
            .ok_or(Error::NoTable { cluster })?;
        let top = known
            .clone()
            .max_by_key(|(i, m)| (**m, *i))
            .map(|(i, m)| (ps(i), *m))
            .ok_or(Error::NoTable { cluster })?;
        if top.1 == quiet.1 {
            return Err(Error::OneState { cluster });
        }
        let full = match cap_mhz {
            Some(c) if c <= quiet.1 => {
                return Err(Error::CapBelowQuiet {
                    cap_mhz: c,
                    cluster,
                    quiet_mhz: quiet.1,
                });
            }
            Some(c) => known
                .filter(|(_, m)| **m <= c)
                .max_by_key(|(i, m)| (**m, *i))
                .map_or(quiet, |(i, m)| (ps(i), *m)),
            None => top,
        };
        if let (Some(f), Some(q), Some(s)) = (
            p.full.get_mut(cluster),
            p.quiet.get_mut(cluster),
            p.states.get_mut(cluster),
        ) {
            *f = full;
            *q = quiet;
            *s = n;
        }
    }
    Ok(p)
}

/// De knop: de standen en het DVFM-blok per cluster.
pub struct PstateKnob {
    plan: Plan,
    base: [Pa; 2],
}

impl PstateKnob {
    /// Zet beide clusters op hun stand; `None` als een cluster BUSY bleef.
    /// De Level is die van de P-cluster (de snelste klok van de node).
    fn set(&mut self, to: [(u64, u32); 2]) -> Option<Level> {
        let mut ok = true;
        for (b, (ps, _)) in self.base.iter().zip(to) {
            ok &= switch(*b, ps);
        }
        ok.then(|| Level {
            value: to[1].1,
            unit: "MHz",
        })
    }
}

/// Het recept van Linux op één cluster: BUSY af, PS1 en SET, schrijven.
fn switch(base: Pa, ps: u64) -> bool {
    let a = base.add(CL_PSTATE);
    if !dev::poll_until(cpu::idle::now, BUSY_NS, || dev::read64(a) & PS_BUSY == 0) {
        return false;
    }
    let v = dev::read64(a);
    if v & PS1 == ps {
        return true;
    }
    dev::write64(a, (v & !PS1) | PS_SET | (ps & PS1));
    dev::mb();
    true
}

/// De knop voor de regel `HOPOS_CLOCK_UP`.
impl fmt::Display for PstateKnob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.plan;
        f.write_str("p-states per cluster")?;
        for (c, name) in NAMES.iter().enumerate() {
            let (Some(full), Some(quiet), Some(n)) =
                (p.full.get(c), p.quiet.get(c), p.states.get(c))
            else {
                continue;
            };
            write!(
                f,
                ", {name} full {}/{n} ({} MHz) quiet {} ({} MHz)",
                full.0, full.1, quiet.0, quiet.1
            )?;
        }
        Ok(())
    }
}

impl Knob for PstateKnob {
    fn full(&mut self) -> Option<Level> {
        self.set(self.plan.full)
    }
    fn quiet(&mut self) -> Option<Level> {
        self.set(self.plan.quiet)
    }
}

/// De MHz per p-state van een `voltage-states`-tabel (8 bytes per state,
/// het eerste woord de raw, `wdt::ps_mhz`).
fn table(raw: &[u8], out: &mut [u32; MAX_STATES]) -> usize {
    let mut n = 0;
    for (o, w) in out.iter_mut().zip(raw.chunks_exact(8)) {
        *o = ps_mhz(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
        n += 1;
    }
    n
}

/// De knop van deze boot: de tabellen en de clusterblokken uit de ADT.
/// `asked` is of `hopos.pstate` gezet is (op een niet gemeten chip de
/// enige weg naar een knop).
pub(crate) fn knob(cap_mhz: Option<u32>, asked: bool) -> Result<PstateKnob, Error> {
    let chip = &crate::chip::CHIP;
    if !chip.measured && !asked {
        return Err(Error::NotMeasured { soc: chip.soc });
    }
    let t = fwinfo::adt().ok_or(Error::NoTable { cluster: 0 })?;
    let pm = t
        .path(fwinfo::pmgr_path())
        .ok_or(Error::NoTable { cluster: 0 })?;
    let mut mhz = [[0u32; MAX_STATES]; 2];
    let mut n = [0usize; 2];
    let mut base = [Pa(0); 2];
    for c in 0..2 {
        let (Some(m), Some(len), Some(b), Some(name)) =
            (mhz.get_mut(c), n.get_mut(c), base.get_mut(c), TABLES.get(c))
        else {
            continue;
        };
        *len = table(t.prop(pm, name).unwrap_or_default(), m);
        let imp = (0..fwinfo::cpus())
            .find(|&i| fwinfo::is_p_core(i) == (c == 1))
            .and_then(fwinfo::cpu_impl)
            .filter(|&a| a != 0)
            .ok_or(Error::NoTable { cluster: c })?;
        *b = Pa(cluster_base(imp));
    }
    let plan = plan(
        [
            mhz[0].get(..n[0]).unwrap_or_default(),
            mhz[1].get(..n[1]).unwrap_or_default(),
        ],
        cap_mhz,
    )?;
    Ok(PstateKnob { plan, base })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// De M4: 8 E-states en 20 P-states; de getallen van 01-10 (E 5 = 2172,
    /// P 6 = 2352) op hun plek, de rest oplopend.
    const E: [u32; 8] = [912, 1284, 1752, 2004, 2172, 2460, 2724, 2892];
    const P: [u32; 20] = [
        702, 948, 1188, 1452, 1704, 2352, 2532, 2736, 2952, 3132, 3312, 3480, 3624, 3756, 3864,
        3972, 4080, 4212, 4320, 4440,
    ];

    #[test]
    fn full_is_the_top_state_quiet_is_the_bottom() {
        let p = plan([&E, &P], None).unwrap();
        assert_eq!(p.full, [(8, 2892), (20, 4440)]);
        assert_eq!(p.quiet, [(1, 912), (1, 702)]);
        assert_eq!(p.states, [8, 20]);
    }

    #[test]
    fn the_cap_picks_the_fastest_state_under_it() {
        let p = plan([&E, &P], Some(3000)).unwrap();
        assert_eq!(p.full, [(8, 2892), (9, 2952)]);
        // Een cap onder de E-top maar boven zijn bodem: E zakt mee.
        let p = plan([&E, &P], Some(2000)).unwrap();
        assert_eq!(p.full, [(3, 1752), (5, 1704)]);
        assert_eq!(
            plan([&E, &P], Some(800)),
            Err(Error::CapBelowQuiet {
                cap_mhz: 800,
                cluster: 0,
                quiet_mhz: 912
            })
        );
    }

    #[test]
    fn order_and_holes_do_not_matter() {
        // Een tabel die aflopend staat, met een lege raw ertussen.
        let down = [2892, 0, 1752, 912];
        let p = plan([&down, &P], None).unwrap();
        assert_eq!((p.full[0], p.quiet[0]), ((1, 2892), (4, 912)));
    }

    #[test]
    fn refusals_are_named() {
        assert_eq!(plan([&[], &P], None), Err(Error::NoTable { cluster: 0 }));
        assert_eq!(
            plan([&E, &[1500, 1500]], None),
            Err(Error::OneState { cluster: 1 })
        );
        let e = Error::NotMeasured { soc: "t8152" };
        assert!(format!("{e}").contains("t8152"));
    }

    #[test]
    fn a_raw_table_becomes_mhz() {
        // 22676 is 2890 MHz (wdt::ps_mhz), het tweede woord is de spanning.
        let raw = [0x94, 0x58, 0, 0, 0xaa, 0xbb, 0, 0];
        let mut out = [0; MAX_STATES];
        assert_eq!(table(&raw, &mut out), 1);
        assert_eq!(out[0], 2890);
    }
}
