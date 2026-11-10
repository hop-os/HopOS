//! De core-klassen van de O6N (small, mid, big), voor de plaatsing van Hop
//! (PORT.md beslissing 3). Drie bronnen, in deze volgorde:
//!
//! 1. de "Processor Power Efficiency Class" per GICC in de MADT: de
//!    universele vorm van "welk cluster is dit". De Cix-firmware vult hem
//!    niet (`Madt.aslc`: 0 voor elke core), dus dan:
//! 2. de HighestPerformance uit de `_CPC` per core, geclusterd op afstand:
//!    waarden binnen 25% van elkaar zijn één klasse. Op de O6N (17-09)
//!    verschillen de vier A720-paren onderling in maximum (8192/7876/7246/
//!    6931: binning per DVFS-domein) en de A520's staan op 2232; "elke
//!    waarde een klasse" gaf toen 1 big, 6 mid, 4 small, en de scheduler zag
//!    zeven cores van één type als twee soorten.
//! 3. vast per MPIDR, uit de mainline-DT (sky1.dtsi: cpu0-3 cortex-a520,
//!    cpu4-11 cortex-a720): aff1 0..3 is small, de rest big. Onbewezen tot
//!    de bootregel het op het board toont.
//!
//! Zegt geen bron iets bruikbaars, dan is het board homogeen "big": een
//! verzonnen indeling maakt jobs onplaatsbaar (de qemuvirt-les), een
//! homogene niet.

use board::CoreClass;
use bounded::BoundedVec;

/// Waar de indeling vandaan kwam, voor de bootregel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// MADT-efficiëntieklassen.
    Madt,
    /// `_CPC` HighestPerformance.
    Cpc,
    /// De vaste MPIDR-tabel.
    Mpidr,
    /// Niets: alles big.
    None,
}

/// De namen per aantal klassen: één is big, twee small/big, drie
/// small/mid/big.
fn names(n: usize) -> &'static [CoreClass] {
    match n {
        2 => &[CoreClass::Small, CoreClass::Big],
        3 => &[CoreClass::Small, CoreClass::Mid, CoreClass::Big],
        _ => &[CoreClass::Big],
    }
}

/// De klasse van `own` tussen de verschillende waarden `all` (oplopend:
/// lager is zuiniger), als dat twee of drie klassen zijn.
fn by_rank(own: u8, all: &BoundedVec<u8, 16>) -> Option<CoreClass> {
    let n = all.len();
    if !(2..=3).contains(&n) {
        return None;
    }
    let k = all.as_slice().iter().position(|&e| e == own)?;
    names(n).get(k).copied()
}

/// De verschillende efficiëntieklassen van de app-cores (de eerste core,
/// die van de kern, telt niet mee: zo deed Go het ook), oplopend.
pub fn eff_classes(effs: &[u8]) -> BoundedVec<u8, 16> {
    let mut out = BoundedVec::new();
    for &e in effs.iter().skip(1) {
        if !out.as_slice().contains(&e) && out.push(e).is_err() {
            break;
        }
    }
    out.as_mut_slice().sort_unstable();
    out
}

/// De bovengrens per `_CPC`-klasse, oplopend, hoogstens drie: een gat van
/// meer dan 25% begint een nieuwe klasse; meer dan drie clusters worden
/// samengevoegd door het kleinste relatieve gat te sluiten.
pub fn cpc_tops(highest: &[u32]) -> BoundedVec<u32, 16> {
    let mut vals: BoundedVec<u32, 64> = BoundedVec::new();
    for &v in highest.iter().filter(|&&v| v != 0) {
        if vals.push(v).is_err() {
            break;
        }
    }
    vals.as_mut_slice().sort_unstable();
    let mut tops: BoundedVec<u32, 16> = BoundedVec::new();
    let mut prev = 0u32;
    for &v in vals.as_slice() {
        let gap = u64::from(v) * 100 > u64::from(prev) * 125;
        if tops.is_empty() || gap {
            if tops.push(v).is_err() {
                break;
            }
        } else if let Some(t) = tops.as_mut_slice().last_mut() {
            *t = v;
        }
        prev = v;
    }
    while tops.len() > 3 {
        // Het kleinste relatieve gat tussen buren sluiten.
        let t = tops.as_slice();
        let mut k = 1;
        for i in 2..t.len() {
            let (a, b) = (t.get(i).copied(), t.get(k).copied());
            let (pa, pb) = (t.get(i - 1).copied(), t.get(k - 1).copied());
            if let (Some(a), Some(b), Some(pa), Some(pb)) = (a, b, pa, pb)
                && u64::from(a) * u64::from(pb) < u64::from(b) * u64::from(pa)
            {
                k = i;
            }
        }
        let _ = tops.remove(k - 1);
    }
    tops
}

/// De klasse van een core met HighestPerformance `highest` tussen de
/// klassen `tops`.
fn by_cpc(highest: u32, tops: &BoundedVec<u32, 16>) -> Option<CoreClass> {
    let n = tops.len();
    if !(2..=3).contains(&n) || highest == 0 {
        return None;
    }
    let k = tops.as_slice().iter().position(|&t| highest <= t)?;
    names(n).get(k).copied()
}

/// De vaste tabel: aff1 0..3 zijn de A520's.
fn by_mpidr(mpidr: u64) -> CoreClass {
    if (mpidr >> 8) & 0xff < 4 {
        CoreClass::Small
    } else {
        CoreClass::Big
    }
}

/// Wat de klasse-afleiding over één core weet.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreFacts {
    /// MPIDR uit de MADT.
    pub mpidr: u64,
    /// De efficiëntieklasse uit de MADT.
    pub eff: u8,
    /// HighestPerformance uit de `_CPC` (0 = geen).
    pub highest: u32,
}

/// De klasse-indeling over alle cores, één keer bij boot berekend.
pub struct Classes {
    effs: BoundedVec<u8, 16>,
    tops: BoundedVec<u32, 16>,
    /// De gebruikte bron.
    pub source: Source,
}

impl Classes {
    /// Kiest de bron: MADT als die twee of drie klassen draagt, anders de
    /// `_CPC` als die twee of drie clusters geeft, anders de MPIDR-tabel als
    /// `cix` (de firmware is die van Cix), anders niets.
    #[must_use]
    pub fn new(cores: &[CoreFacts], cix: bool) -> Self {
        let mut effs_in: BoundedVec<u8, 256> = BoundedVec::new();
        let mut highs: BoundedVec<u32, 256> = BoundedVec::new();
        for c in cores {
            let _ = effs_in.push(c.eff);
            let _ = highs.push(c.highest);
        }
        let effs = eff_classes(effs_in.as_slice());
        let tops = cpc_tops(highs.as_slice());
        let source = if (2..=3).contains(&effs.len()) {
            Source::Madt
        } else if (2..=3).contains(&tops.len()) {
            Source::Cpc
        } else if cix && cores.len() > 4 {
            Source::Mpidr
        } else {
            Source::None
        };
        Self { effs, tops, source }
    }

    /// De klasse van één core.
    #[must_use]
    pub fn of(&self, c: &CoreFacts) -> CoreClass {
        match self.source {
            Source::Madt => by_rank(c.eff, &self.effs),
            Source::Cpc => by_cpc(c.highest, &self.tops),
            Source::Mpidr => Some(by_mpidr(c.mpidr)),
            Source::None => None,
        }
        .unwrap_or(CoreClass::Big)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cores(highs: &[u32]) -> std::vec::Vec<CoreFacts> {
        highs
            .iter()
            .enumerate()
            .map(|(i, &h)| CoreFacts {
                mpidr: (i as u64) << 8,
                eff: 0,
                highest: h,
            })
            .collect()
    }

    /// De meting van 17-09: vier A520's op 2232, acht A720's in vier
    /// gebinde paren. Twee klassen, niet vier.
    #[test]
    fn the_o6n_bins_are_two_classes() {
        let highs = [
            2232, 2232, 2232, 2232, 8192, 8192, 7876, 7876, 7246, 7246, 6931, 6931,
        ];
        let cs = cores(&highs);
        let c = Classes::new(&cs, true);
        assert_eq!(c.source, Source::Cpc);
        let got: std::vec::Vec<CoreClass> = cs.iter().map(|f| c.of(f)).collect();
        assert_eq!(&got[..4], &[CoreClass::Small; 4]);
        assert_eq!(&got[4..], &[CoreClass::Big; 8]);
    }

    #[test]
    fn madt_classes_win_when_the_firmware_fills_them() {
        let mut cs = cores(&[0; 12]);
        for (i, c) in cs.iter_mut().enumerate() {
            c.eff = match i {
                0..=3 => 0,
                4..=9 => 1,
                _ => 2,
            };
        }
        let c = Classes::new(&cs, true);
        assert_eq!(c.source, Source::Madt);
        assert_eq!(c.of(&cs[1]), CoreClass::Small);
        assert_eq!(c.of(&cs[5]), CoreClass::Mid);
        assert_eq!(c.of(&cs[11]), CoreClass::Big);
    }

    #[test]
    fn without_madt_or_cpc_the_mpidr_table_holds_on_cix_only() {
        let cs = cores(&[0; 12]);
        let c = Classes::new(&cs, true);
        assert_eq!(c.source, Source::Mpidr);
        assert_eq!(c.of(&cs[3]), CoreClass::Small);
        assert_eq!(c.of(&cs[4]), CoreClass::Big);
        // Een andere UEFI-machine met dit image (de Ampere, 19-09): homogeen.
        let c = Classes::new(&cs, false);
        assert_eq!(c.source, Source::None);
        assert_eq!(c.of(&cs[3]), CoreClass::Big);
    }

    /// `hopos.system.core` op de Cix P1 zoals de firmware hem geeft (MADT overal
    /// 0, geen `_CPC` hier) en in de MADT-volgorde van de O6N (de
    /// CPU_ON-regels op de console: core 0 is 0xa00, core 1 0xb00, core 2 tot
    /// 11 zijn 0x000 tot 0x900): de klasse komt uit de MPIDR-tabel, niet uit
    /// de MADT (`board_uefi::On::os_core`).
    #[test]
    fn oscore_on_the_cix_uses_the_mpidr_table() {
        let mut cs = cores(&[0; 12]);
        for (i, c) in cs.iter_mut().enumerate() {
            c.mpidr = ((i as u64 + 10) % 12) << 8;
        }
        let c = Classes::new(&cs, true);
        let class = |i: usize| c.of(&cs[i]);
        assert_eq!(board::os_core("small", 12, class, 0), (2, None));
        assert_eq!(board::os_core("big", 12, class, 0), (0, None));
        assert_eq!(
            board::os_core("mid", 12, class, 0),
            (0, Some("no core of that class"))
        );
    }

    #[test]
    fn too_many_clusters_merge_on_the_smallest_gap() {
        let tops = cpc_tops(&[1000, 2000, 4000, 8000, 8500]);
        // 1000, 2000, 4000, 8000..8500: vier clusters, het kleinste gat
        // (2x tussen 1000/2000 of 2000/4000) sluit.
        assert_eq!(tops.len(), 3);
        assert_eq!(*tops.as_slice().last().unwrap(), 8500);
        assert_eq!(cpc_tops(&[5000, 5100, 5200]).len(), 1);
        assert_eq!(cpc_tops(&[]).len(), 0);
    }
}
