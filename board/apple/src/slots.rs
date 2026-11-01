//! Het slot-plan van de Mac mini: waar de kern zijn kooien, control-pages
//! en partities fysiek legt. Dezelfde vorm als de andere boards
//! (`board_rk3566::slots`, `board_uefi::slots`), zodat de kern-binary er
//! met één `use` op staat.
//!
//! De pool komt uit iBoot's contract, niet uit een constante (de mini
//! bestaat in 16, 24 en 32 GB): `[phys_base, phys_base + mem_size)` is van
//! ons, en `top_of_kernel_data` is waar de spullen die de firmware er zelf
//! in legde (kernel-image, ADT, trust cache, m1n1 met zijn spin-table)
//! ophouden. Daar gaan twee gaten uit: de firmware, en ons eigen venster.
//! Er blijven twee regio's over, onder en boven het venster, en dat is met
//! opzet (GEMETEN 29-08: van 19,7 naar 23,7 GB, en een app aantoonbaar in
//! het lage stuk geplaatst, gestreamd en gestart).
//!
//! De staging: de m1n1-loader (`image/apple/load-probe.py`, `STAGE=`) legt
//! het image van Hop op [`STAGE_PA`], de maat op [`STAGE_HDR_PA`] en
//! [`STAGE_MAGIC`] als laatste woord: zonder magic is de loader-regio gewoon
//! DRAM van een vorige boot, en dat is geen image.

use crate::{ADMIN, LOADER, RAM_BASE, WINDOW_END, fwinfo};
use abi::Region;
use abi::layout::{Plan, PlanSpec, Pool, carve_pool};
use board::stage;
use core::sync::atomic::{AtomicU8, Ordering::Relaxed};
use cpu::el2::{Back, Bell, OsCore};
use cpu::println;
use dev::Pa;

/// De control-pages van de eigen cores van de kern: het begin van het
/// kooi-venster.
pub const NODE_CTRL_PA: u64 = ADMIN.base.0;
/// De kooi-regio: 1 MB verder, blok 0 en de ctx-blokken van de cores
/// (`abi::layout::cage_len`). Wat per slot is, komt uit de pool.
pub const CAGE_PA: u64 = ADMIN.base.0 + 0x10_0000;
/// De vluchtrecorder van de kern-flip: buiten het image, want iBoot legt
/// het bootobject bij élke boot terug over het begin van het venster
/// (GEMETEN 01-09: een spoor op de boot-scratch wiste zichzelf).
pub const FLIP_SCRATCH_PA: u64 = ADMIN.base.0 + 0xc0_0000;
/// De console-zwarte-doos, 32 KB, naast de recorder en BUITEN de
/// kooi-regio (Go 06-09: een doos die 256 bytes de kooi in liep, schreef
/// consoletekst over de vectoren van de app-cores).
pub const BLACK_BOX: Region = Region::new(ADMIN.base.0 + 0xc0_8000, 32 << 10);
/// Het Device-venster van de kooi.
pub const DEVICE_WINDOW: Region = Region::new(ADMIN.base.0, ADMIN.size);
/// De boot-scratch: het begin van de loader-regio.
pub const BOOT_SCRATCH_PA: u64 = LOADER.base.0;
/// Het woord met de maat van het gestagede image.
pub const STAGE_HDR_PA: u64 = LOADER.base.0 + 0x10_0000;
/// Het magic-woord: alleen dan is er een staging.
pub const STAGE_MAGIC_PA: u64 = STAGE_HDR_PA + 16;
/// "HOPSTAGE", little-endian (pariteit met de loader).
pub const STAGE_MAGIC: u64 = 0x4547_4154_5350_4f48;
/// Waar het image staat.
pub const STAGE_PA: u64 = LOADER.base.0 + 0x20_0000;
/// Het grootste gestagede image.
pub const STAGE_MAX: u64 = LOADER.base.0 + LOADER.size - STAGE_PA;
/// De pool als boot_args ontbreekt: 1 GB boven het venster, LUID
/// (`HOPOS_POOL_FALLBACK`). Elke mini heeft daar RAM.
pub const POOL_FALLBACK: Region = Region::new(WINDOW_END, 1 << 30);

const _: () = {
    assert!(BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN <= STAGE_HDR_PA);
    assert!(STAGE_MAGIC_PA + 8 <= STAGE_PA);
    let cores = abi::layout::CORES_MAX - 1;
    assert!(NODE_CTRL_PA + abi::layout::node_ctrl_len(cores) <= CAGE_PA);
    assert!(CAGE_PA + abi::layout::cage_len(cores) <= FLIP_SCRATCH_PA);
    assert!(FLIP_SCRATCH_PA + 8 <= BLACK_BOX.base);
    assert!(BLACK_BOX.base + BLACK_BOX.size <= ADMIN.base.0 + ADMIN.size);
    assert!(CAGE_PA.is_multiple_of(2048));
};

// Van het flip-spoor: alleen getallen, zoals op de andere boards. Op ijzer
// nog niet geflipt (in Go wel, 01-09: zes ijzer-flips, en de les dat iBoot
// het bootobject bij élke boot terug over het begin van het venster legt).

/// Waar de nieuwe kern heen gaat: het linkadres (`hopos/link-apple.ld`).
pub const FLIP_LINK_BASE: u64 = RAM_BASE;
/// Geen PIE.
pub const FLIP_PIE: bool = false;
/// De trampoline: de bovenste pagina van de kern-RAM.
pub const FLIP_TRAMP_PA: u64 = crate::KERN_RAM.base.0 + crate::KERN_RAM.size - 0x1000;
/// Het beeld blijft onder de trampoline.
pub const FLIP_IMAGE_END: u64 = FLIP_TRAMP_PA;
/// De vluchtrecorder.
pub const FLIP_RECORDER_PA: u64 = FLIP_SCRATCH_PA;
const _: () = assert!(
    abi::layout::flip_handoff_pa(STAGE_HDR_PA) >= BOOT_SCRATCH_PA + abi::layout::BOOT_SCRATCH_LEN
);

/// De pool uit het RAM-contract van boot_args, met de gaten eruit. Leeg of
/// mislukt = de luide terugval (`false`).
#[must_use]
pub fn pool() -> (Pool, bool) {
    let (phys, size, top, _) = fwinfo::ram();
    pool_of(phys, size, top)
}

fn pool_of(phys: u64, size: u64, top: u64) -> (Pool, bool) {
    let carved = (phys != 0 && size != 0 && top > phys)
        .then(|| {
            let banks = [Region::new(phys, size)];
            let holes = [
                Region::new(phys, top - phys),
                Region::new(RAM_BASE, WINDOW_END - RAM_BASE),
            ];
            carve_pool(&banks, &holes, 2 << 20).ok()
        })
        .flatten()
        .filter(|p| !p.is_empty());
    match carved {
        Some(p) => (p, true),
        None => {
            let mut p = Pool::new();
            // Eén regio past altijd.
            let _ = p.push(POOL_FALLBACK);
            (p, false)
        }
    }
}

/// Wat `hopos.cages` vraagt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Cages {
    /// Geen waarde: kooien als de voorproef slaagt.
    Auto,
    /// `on`: kooien, ook zonder voorproef (de rem van vóór 29-09 eraf).
    On,
    /// `off`: geen kooien, luid.
    Off,
}

fn cages(v: &str) -> Cages {
    match v {
        "on" => Cages::On,
        "off" => Cages::Off,
        _ => Cages::Auto,
    }
}

/// De uitkomst van de voorproef: 0 = nog niet gedaan, 1 = geslaagd, 2 =
/// gezakt. Eén keer per boot, op de OS-core (de eerste `plan`).
static PREFLIGHT: AtomicU8 = AtomicU8::new(0);

/// De voorproef van de OS-core-rotatie: dezelfde drie beurten als de
/// zelftest van de kern (hopos `slots::os_core`: de CNTHP, een yield, de
/// fast IPI naar zichzelf), maar vóór er één kooi, partitie of bewoner
/// bestaat. Waarom hier en niet pas in de kern: op dit silicium is elke
/// stap nieuw (niets van de Rust-kern draaide hier ooit), en een kern die
/// zijn kooien opbouwt terwijl de CNTHP deze core niet bereikt, geeft Hop
/// een core die nooit terugkomt. Een spinner die de CNTHP niet ziet, yieldt
/// na twee termijnen zelf (`OsCore::selftest`): rood, geen hang.
///
/// Waar het kan misgaan (docs/boards-apple.md, stap 11): een OS-core die de
/// kern zelf opbracht (`hopos.oscore=small`) krijgt op t8132 geen timer-FIQ
/// (29-08), en dan zakt de timer-beurt.
fn preflight(plan: &Plan) -> bool {
    match PREFLIGHT.load(Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let here = cpu::mpidr();
    // Het EL2-regime zoals iBoot of m1n1 het achterliet en de kern het
    // zette, teruggelezen zoals Go's el2Apple (30-09: onder kmutil gaf de
    // voorproef drie keer Fault, en zonder deze regel is dat blind).
    println!(
        "slots: el2 regime hcr={:#x} cnthctl={:#x} cptr={:#x} sctlr_el2={:#x} HOPOS_APPLE_EL2",
        crate::arch::hcr(),
        crate::arch::cnthctl(),
        crate::arch::cptr(),
        crate::arch::sctlr_el2()
    );
    crate::serror_check("everything before the preflight");
    let ok = match OsCore::new(plan, crate::FLAVOR, Some(Bell::apple(here))) {
        Ok(mut os) => {
            let ms = cpu::idle::freq() / 1000;
            let t = os.selftest(false, ms, &|| {});
            let y = os.selftest(true, 100 * ms, &|| {});
            let k = os.selftest(false, 100 * ms, &|| crate::cores::kick(here));
            let back = |r: Option<cpu::el2::Probe>| r.map(|p| p.back);
            let ok = back(t) == Some(Back::Timer)
                && back(y) == Some(Back::Yield)
                && back(k) == Some(Back::Ipi);
            println!(
                "slots: OS-core preflight on mpidr {here:#x}: timer={:?} yield={:?} kick={:?} {}",
                back(t),
                back(y),
                back(k),
                if ok {
                    "HOPOS_APPLE_PREFLIGHT ok"
                } else {
                    "HOPOS_APPLE_PREFLIGHT_FAIL"
                }
            );
            if [back(t), back(y), back(k)].contains(&Some(Back::Fault)) {
                let (esr, far, pc, vec) = cpu::el2::last_fault();
                println!(
                    "slots: preflight fault: esr={esr:#x} far={far:#x} pc={pc:#x} vec={vec} HOPOS_APPLE_PREFLIGHT_FAULT"
                );
            }
            ok
        }
        Err(e) => {
            println!("slots: OS-core preflight: {e} HOPOS_APPLE_PREFLIGHT_FAIL");
            false
        }
    };
    PREFLIGHT.store(if ok { 1 } else { 2 }, Relaxed);
    ok
}

/// Het PA-plan voor een node met `cores` cores en de kern op fysieke core
/// `os_core`: elke andere core is een app-core met één kooi, en de OS-core
/// draagt er één bij.
///
/// Sinds 29-09 staat het plan standaard AAN, mits de voorproef van de
/// OS-core slaagt ([`preflight`]): de kooi-lijm installeert `AppleVhe`, de
/// rotatie kent de fast IPI, en een koude core start via
/// [`crate::cores::cpu_on_mpidr`] (m1n1's spin-table of PMGR) in plaats van
/// PSCI. Zakt de voorproef, dan weigert het plan luid (`HOPOS_SLOT_PLAN`)
/// en draait de node zonder kooien door, zoals vóór 29-09. `hopos.cages=on`
/// slaat de voorproef over (de zelftest van de kern meldt het dan nog
/// steeds), `hopos.cages=off` weigert altijd.
pub fn plan(cores: usize, os_core: usize) -> abi::Result<Plan> {
    let want = cages(board::Board::boot_param(
        &crate::Apple::new(),
        "hopos.cages",
    ));
    if want == Cages::Off {
        return Err(abi::Error::Missing("cages: hopos.cages=off"));
    }
    let app_cores = cores.saturating_sub(1).max(1);
    let plan = Plan::new(PlanSpec {
        node_ctrl_pa: NODE_CTRL_PA,
        cage_pa: CAGE_PA,
        boot_scratch_pa: BOOT_SCRATCH_PA,
        flip_scratch_pa: FLIP_SCRATCH_PA,
        black_box: BLACK_BOX,
        net_dma_pa: crate::NET_DMA.base.0,
        ram_base: crate::DRAM_BASE,
        pool: pool().0,
        app_cores,
        os_core,
        ..PlanSpec::default()
    })?;
    if want == Cages::Auto && !preflight(&plan) {
        return Err(abi::Error::Missing(
            "cages: the OS-core preflight failed (HOPOS_APPLE_PREFLIGHT; hopos.cages=on forces them)",
        ));
    }
    Ok(plan)
}

/// Het MPIDR van core `core`: 0x8000_0000, aff2 op de P-cores, en aff1:aff0
/// uit het reg-woord van de ADT (GEMETEN 28-08: cpu0 0x80000000, cpu6
/// 0x80010100, cpu7..9 0x80010101..3).
#[must_use]
pub fn mpidr(core: usize) -> u64 {
    let reg = u64::from(fwinfo::cpu_reg(core).unwrap_or(0));
    let aff2 = if fwinfo::is_p_core(core) { 1 << 16 } else { 0 };
    0x8000_0000 | aff2 | reg
}

/// De fysieke core bij een MPIDR (0 als hij onbekend is).
#[must_use]
pub fn core_of(mpidr: u64) -> usize {
    fwinfo::core_of(mpidr).unwrap_or(0)
}

fn staged() -> bool {
    dev::read64(Pa(STAGE_MAGIC_PA)) == STAGE_MAGIC
}

/// De ingebakken stage (build.rs, `HOPOS_EMBED`): Hop in het kernimage
/// zelf, voor een boot zonder loader (kmutil). Leeg = niets ingebakken.
static EMBED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embed.elf"));

/// De ingebakken stage, of `None`.
fn embedded() -> Option<&'static [u8]> {
    if EMBED.is_empty() { None } else { Some(EMBED) }
}

/// Het image dat de loader neerlegde, zonder loader de ingebakken stage
/// (image/apple-m4.sh `EMBED=`), of `None`. Alleen de maat wordt hier
/// getoetst; de inhoud is onvertrouwd en gaat door de ELF-lezer.
#[must_use]
pub fn staged_image() -> Option<&'static [u8]> {
    if !staged() {
        return embedded();
    }
    stage::staged_at(STAGE_HDR_PA, STAGE_PA, STAGE_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cages_knob() {
        assert_eq!(cages(""), Cages::Auto);
        assert_eq!(cages("on"), Cages::On);
        assert_eq!(cages("off"), Cages::Off);
        assert_eq!(cages("yes"), Cages::Auto);
    }

    /// Op de host keert elke beurt terug op een IRQ: de voorproef zakt, en
    /// dat onthoudt hij (één keer per boot).
    #[test]
    fn a_red_preflight_refuses_and_stays_red() {
        let plan = Plan::new(PlanSpec {
            node_ctrl_pa: NODE_CTRL_PA,
            cage_pa: CAGE_PA,
            boot_scratch_pa: BOOT_SCRATCH_PA,
            flip_scratch_pa: FLIP_SCRATCH_PA,
            black_box: BLACK_BOX,
            net_dma_pa: crate::NET_DMA.base.0,
            ram_base: crate::DRAM_BASE,
            pool: pool_of(0, 0, 0).0,
            app_cores: 1,
            os_core: 0,
            ..PlanSpec::default()
        });
        // De plan-regio ligt op zijn ijzeradres; `OsCore::new` schrijft
        // daar, dus op de host alleen de weigering zonder plan.
        assert!(plan.is_ok());
        PREFLIGHT.store(2, Relaxed);
        assert!(!preflight(&plan.unwrap()));
        PREFLIGHT.store(0, Relaxed);
    }

    /// Het RAM-contract van de M4 (GEMETEN 29-08).
    #[test]
    fn the_pool_is_carved_from_the_contract() {
        let phys = 0x100_0137_4000;
        let size = 0x5_df56_c000;
        let top = 0x100_0435_0000;
        let (p, ok) = pool_of(phys, size, top);
        assert!(ok);
        assert_eq!(p.len(), 2, "onder en boven het venster");
        assert!(p[0].base >= top && p[0].base + p[0].size <= RAM_BASE);
        assert!(p[1].base >= WINDOW_END && p[1].base + p[1].size <= phys + size);
        let total: u64 = p.iter().map(|r| r.size).sum();
        // 23,7 GB, zoals in Go na 29-08.
        assert!(total >> 20 > 23_000, "{} MB", total >> 20);
    }

    #[test]
    fn without_boot_args_the_fallback() {
        let (p, ok) = pool_of(0, 0, 0);
        assert!(!ok);
        assert_eq!(p[0], POOL_FALLBACK);
    }
}
