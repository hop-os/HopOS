//! De kern-flip op UEFI: de feiten van de stub voor een kern die zonder
//! firmware binnenkomt (`hopos/src/flip.rs`, `docs/flip.md`).
//!
//! Een geflipte kern begint in `_start_efi` met x0 = de feitenpagina van
//! elk board (O6, `abi::layout::facts_pa`, met de handoff): er is geen
//! SystemTable meer, de boot services zijn al bij de koude boot verlaten.
//! Alles wat de stub toen over de machine leerde ([`crate::facts`]: ACPI, de
//! memory map, de config, het image, de identity map) staat in de BSS van
//! de OUDE kern en is dus weg. Daarom schrijft de koude stub die feiten
//! één keer in een eigen pagina ([`FLIP_FACTS_PA`], in het kernvenster,
//! buiten de pool) en leest elke geflipte kern ze daar terug, vóór `kmain`
//! (de Go-generatie deed hetzelfde met zijn "firmware-facts block";
//! `OLD/docs/kernel-flip.md`, UEFI machines).
//!
//! De feiten worden nooit meer herschreven: de firmware-feiten veranderen
//! niet door een flip. Eén uitzondering staat erachter: het zaad van
//! [`carry_seed`], dat elke vertrekkende kern vers neerlegt en elke
//! landende leest en wist. Wat wél verandert, is het aantal tabellen van de
//! identity map (`map_device` voegt er tijdens het leven van een kern aan
//! toe); dat telt de nieuwe kern opnieuw door de levende map af te lopen,
//! want met de telling van de koude boot zou hij een tabel die in gebruik
//! is opnieuw uitdelen.
//!
//! Dit bezit de pagina en verder niets. De sprong zelf is van
//! `cpu::el2::chain`, het beleid van `hopos/src/flip.rs`.

use crate::facts::{self, MAX_CORES, MAX_ECAM};
use crate::slots::{FLIP_FACTS_LEN, FLIP_FACTS_PA};
use core::sync::atomic::Ordering::Relaxed;
use dev::Pa;

/// "HOPFACT1" little-endian.
pub(crate) const FACTS_MAGIC: u64 = 0x3154_4341_4650_4F48;
/// De versie van de pagina.
const FACTS_VERSION: u64 = 1;

/// De woorden van de pagina, in volgorde. De eerste vier na de kop leest de
/// assembly van de flip-ingang zelf (MMU uit, vóór er Rust draait).
const W_MAGIC: u64 = 0;
const W_VERSION: u64 = 1;
/// TTBR0_EL2, TCR_EL2, het EL en CNTHCTL_EL2 van `uefi_enter_kernel`.
pub(crate) const W_TTBR0: u64 = 2;
const W_CORES: u64 = 6;
const W_ECAM: u64 = 33;
const W_MPIDR: u64 = W_ECAM + 2 * MAX_ECAM as u64;
const W_CLASS: u64 = W_MPIDR + MAX_CORES as u64;
const W_END: u64 = W_CLASS + MAX_CORES as u64;

/// Het zaad voor de volgende kern ([`carry_seed`]): een magic en daarna
/// [`SEED_WORDS`] woorden, achter de feiten.
const W_SEED: u64 = W_END;
/// 64 bytes, de maat van het EFI-zaad ([`facts::EFI_SEED`]).
const SEED_WORDS: u64 = 8;
/// "HOPSEED1" little-endian.
const SEED_MAGIC: u64 = 0x3144_4545_5350_4F48;

const _: () = assert!((W_SEED + 1 + SEED_WORDS) * 8 <= FLIP_FACTS_LEN);
const _: () = assert!(SEED_WORDS as usize == facts::EFI_SEED.len());

fn at(w: u64) -> Pa {
    Pa(FLIP_FACTS_PA + w * 8)
}

/// De losse feiten, in de volgorde van de pagina vanaf [`W_CORES`]: één
/// lijst voor schrijven en lezen, zodat die twee het nooit oneens zijn.
fn scalars() -> [(&'static str, u64); 27] {
    let (map, image, cfg, stage) = (&facts::MAP, &facts::IMAGE, &facts::CFG, &facts::STAGE);
    [
        ("cores", facts::CORES.load(Relaxed) as u64),
        ("gicd", facts::GICD.load(Relaxed)),
        ("gic", u64::from(facts::GIC_VERSION.load(Relaxed))),
        ("gicr", facts::GICR_BASE.load(Relaxed)),
        ("gicr0", facts::GICR_RANGE[0].load(Relaxed)),
        ("gicr1", facts::GICR_RANGE[1].load(Relaxed)),
        ("its", facts::ITS.load(Relaxed)),
        ("ecams", facts::ECAMS.load(Relaxed) as u64),
        ("con", facts::CONSOLE_BASE.load(Relaxed)),
        ("contype", u64::from(facts::CONSOLE_TYPE.load(Relaxed))),
        ("conshift", u64::from(facts::CONSOLE_SHIFT.load(Relaxed))),
        ("timer", u64::from(facts::TIMER_PPI.load(Relaxed))),
        ("hyptimer", u64::from(facts::HYP_TIMER_PPI.load(Relaxed))),
        ("psci", u64::from(facts::PSCI_HVC.load(Relaxed))),
        ("oem", facts::OEM_ID.load(Relaxed)),
        ("rsdp", facts::RSDP.load(Relaxed)),
        ("map0", map[0].load(Relaxed)),
        ("map1", map[1].load(Relaxed)),
        ("map2", map[2].load(Relaxed)),
        ("image0", image[0].load(Relaxed)),
        ("image1", image[1].load(Relaxed)),
        ("cfg0", cfg[0].load(Relaxed)),
        ("cfg1", cfg[1].load(Relaxed)),
        ("stage0", stage[0].load(Relaxed)),
        ("stage1", stage[1].load(Relaxed)),
        ("el", u64::from(facts::BOOT_EL.load(Relaxed))),
        ("tables", facts::MMU_TABLES.load(Relaxed)),
    ]
}

const _: () = assert!(W_CORES + 27 == W_ECAM);

/// De koude stub, na `ExitBootServices` en vlak vóór de sprong naar de
/// kern: de feiten naar de pagina, de magic als laatste, en alles naar
/// DRAM (de flip-ingang leest de kop met de MMU uit).
pub(crate) fn publish(ttbr0: u64, tcr: u64, el: u8, cnthctl: u64) {
    dev::write64(at(W_MAGIC), 0);
    dev::write64(at(W_VERSION), FACTS_VERSION);
    for (i, v) in [ttbr0, tcr, u64::from(el), cnthctl].into_iter().enumerate() {
        dev::write64(at(W_TTBR0 + i as u64), v);
    }
    for (i, (_, v)) in scalars().into_iter().enumerate() {
        dev::write64(at(W_CORES + i as u64), v);
    }
    for (i, e) in facts::ECAM.iter().enumerate() {
        dev::write64(at(W_ECAM + 2 * i as u64), e[0].load(Relaxed));
        dev::write64(at(W_ECAM + 2 * i as u64 + 1), e[1].load(Relaxed));
    }
    for (i, (m, k)) in facts::CORE_MPIDR
        .iter()
        .zip(facts::CORE_CLASS.iter())
        .enumerate()
    {
        dev::write64(at(W_MPIDR + i as u64), m.load(Relaxed));
        dev::write64(at(W_CLASS + i as u64), u64::from(k.load(Relaxed)));
    }
    // Geen zaad van een flip die nooit landde.
    dev::write64(at(W_SEED), 0);
    dev::write64(at(W_MAGIC), FACTS_MAGIC);
    dev::push(Pa(FLIP_FACTS_PA), ((W_SEED + 1) * 8) as usize);
}

/// De vertrekkende kern, vlak vóór de sprong: 64 verse bytes uit zijn DRBG
/// als zaad voor de volgende kern, zoals Linux bij kexec een `rng-seed`
/// uit zijn eigen RNG in de DTB van de nieuwe kern legt
/// (`drivers/of/kexec.c`). Alleen als de DRBG uit het EFI_RNG_PROTOCOL
/// komt (feature `efi-rng`): die bron is na de koude boot weg, en zonder dit
/// zaaide elke geflipte O6N uit jitter (Z, 01-10). RNDR en de SMCCC TRNG
/// vindt de nieuwe kern zelf terug. Geeft terug of er zaad ligt.
pub fn carry_seed() -> bool {
    use cpu::drbg::Source;
    use cpu::trng::Kind;
    let mut seed = [0u8; (SEED_WORDS * 8) as usize];
    let ok = cpu::drbg::source() == Source::Hardware(Kind::Soc(crate::EFI_RNG))
        && cpu::drbg::read(&mut seed).is_ok();
    dev::write64(at(W_SEED), 0);
    if ok {
        for (i, w) in seed.chunks_exact(8).enumerate() {
            let w = u64::from_le_bytes(w.try_into().unwrap_or([0; 8]));
            dev::write64(at(W_SEED + 1 + i as u64), w);
        }
        dev::write64(at(W_SEED), SEED_MAGIC);
    }
    seed.fill(0);
    core::hint::black_box(&mut seed);
    dev::push(at(W_SEED), ((1 + SEED_WORDS) * 8) as usize);
    ok
}

/// De landende kern: het zaad van [`carry_seed`] naar het EFI-zaad van
/// deze kern, en van de pagina af (één kern, één keer).
fn take_seed() {
    if dev::read64(at(W_SEED)) == SEED_MAGIC {
        for (i, s) in facts::EFI_SEED.iter().enumerate() {
            let w = at(W_SEED + 1 + i as u64);
            s.store(dev::read64(w), Relaxed);
            dev::write64(w, 0);
        }
        facts::EFI_SEED_LEN.store((SEED_WORDS * 8) as usize, Relaxed);
        facts::EFI_SEED_CARRIED.store(true, Relaxed);
    }
    dev::write64(at(W_SEED), 0);
    dev::push(at(W_SEED), ((1 + SEED_WORDS) * 8) as usize);
}

/// De flip-ingang, met de MMU van de pagina aan en vóór `kmain`: de feiten
/// terug in de statics van DEZE kern. De assembly toetste de magic al.
#[unsafe(no_mangle)]
extern "C" fn hopos_efi_flip_facts() {
    if dev::read64(at(W_VERSION)) != FACTS_VERSION {
        return;
    }
    let w = |i: u64| dev::read64(at(W_CORES + i));
    let (map, image, cfg, stage) = (&facts::MAP, &facts::IMAGE, &facts::CFG, &facts::STAGE);
    facts::CORES.store(w(0) as usize, Relaxed);
    facts::GICD.store(w(1), Relaxed);
    facts::GIC_VERSION.store(w(2) as u8, Relaxed);
    facts::GICR_BASE.store(w(3), Relaxed);
    facts::GICR_RANGE[0].store(w(4), Relaxed);
    facts::GICR_RANGE[1].store(w(5), Relaxed);
    facts::ITS.store(w(6), Relaxed);
    facts::ECAMS.store(w(7) as usize, Relaxed);
    facts::CONSOLE_BASE.store(w(8), Relaxed);
    facts::CONSOLE_TYPE.store(w(9) as u8, Relaxed);
    facts::CONSOLE_SHIFT.store(w(10) as u8, Relaxed);
    facts::TIMER_PPI.store(w(11) as u32, Relaxed);
    facts::HYP_TIMER_PPI.store(w(12) as u32, Relaxed);
    facts::PSCI_HVC.store(w(13) as u8, Relaxed);
    facts::OEM_ID.store(w(14), Relaxed);
    facts::RSDP.store(w(15), Relaxed);
    for (i, s) in map.iter().enumerate() {
        s.store(w(16 + i as u64), Relaxed);
    }
    for (i, s) in image
        .iter()
        .chain(cfg.iter())
        .chain(stage.iter())
        .enumerate()
    {
        s.store(w(19 + i as u64), Relaxed);
    }
    facts::BOOT_EL.store(w(25) as u8, Relaxed);
    for (i, e) in facts::ECAM.iter().enumerate() {
        e[0].store(dev::read64(at(W_ECAM + 2 * i as u64)), Relaxed);
        e[1].store(dev::read64(at(W_ECAM + 2 * i as u64 + 1)), Relaxed);
    }
    for (i, (m, k)) in facts::CORE_MPIDR
        .iter()
        .zip(facts::CORE_CLASS.iter())
        .enumerate()
    {
        m.store(dev::read64(at(W_MPIDR + i as u64)), Relaxed);
        k.store(dev::read64(at(W_CLASS + i as u64)) as u8, Relaxed);
    }
    // De tabellen van de levende map, niet die van de koude boot (zie de
    // moduledoc): de hoogste tabel die de map aanwijst, plus één.
    let root = dev::read64(at(W_TTBR0)) & ADDR;
    let used = tables_in_use(root, 0).max(w(26));
    facts::MMU_TABLES.store(used, Relaxed);
    take_seed();
}

/// De adresbits van een tabel-descriptor (4 KB-korrel, 48 bits).
const ADDR: u64 = 0x0000_ffff_ffff_f000;

/// Het aantal tabellen vanaf het begin van de tabelpool dat de map onder
/// `table` (op `level`, 0 = de root) gebruikt: de hoogste index plus één.
/// De stub deelt tabellen oplopend uit de pool uit (`crate::mmu`), dus dat
/// is het aantal dat een volgende `map` moet overslaan.
fn tables_in_use(table: u64, level: u32) -> u64 {
    let pool = crate::TABLES;
    let index = |pa: u64| (pa.saturating_sub(pool.base.0) / 4096) + 1;
    let mut used = index(table);
    if level >= 3 || !pool.contains(Pa(table)) {
        return used;
    }
    for i in 0..512u64 {
        let d = dev::read64(Pa(table + i * 8));
        // Geldig (bit 0) en een tabel (bit 1) boven niveau 3.
        if d & 3 == 3 {
            let next = d & ADDR;
            if pool.contains(Pa(next)) {
                used = used.max(tables_in_use(next, level + 1));
            }
        }
    }
    used
}
