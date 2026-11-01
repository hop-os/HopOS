//! Het zaad van de slots: 32 bytes uit de DRBG van de kern op de
//! control-page van elk slot (`CTRL_RNG_SEED`), met de generatie
//! (`CTRL_RNG_GEN`, een seqlock) en de bron (`CTRL_RNG_SOURCE`).
//!
//! Dit bezit het schrijven van dat blok: bij de bouw van een kooi en na een
//! flip-adoptie (de kooi-lijm, cage.rs) en elke seconde vers vanuit de
//! telemetrie-tik (telemetry.rs), voor elke gebouwde kooi. Niet van hier: de
//! DRBG zelf (`cpu::drbg`, gezaaid door het board in `discover`, of door
//! [`init`] als het board niets zaaide) en de lijst van pages (clock.rs
//! houdt bij welke kooien er staan). Wat de app ermee doet, staat in
//! `applib::rand`: door zijn eigen DRBG, nooit rauw.
//!
//! Eén schrijver: alles hier draait op de executor van de OS-core (de
//! lifecycle-actor en de telemetrie-taak), en de DRBG is daar een
//! `LocalCell`. De lezer is de app op zijn eigen core; het seqlock-protocol
//! van `CTRL_RNG_GEN` is de hele afspraak (handboek §1: een gedeelde pagina
//! met een monotone index), zonder slot.
//!
//! Waarom op de page en niet als system-op: de app heeft het zaad nodig
//! vóór hij een verbinding heeft (de ISS van zijn netstack, de eerste
//! TLS-handshake van Hop), en een woord op de page is de vorm die de
//! temperatuur en de wandklok al hebben. Het verversen elke seconde maakt
//! een verzoek-op overbodig: een app die herzaaien wil, ziet een nieuwe
//! generatie.

use abi::hopabi::{
    CTRL_RNG_GEN, CTRL_RNG_SEED, CTRL_RNG_SEED_LEN, CTRL_RNG_SOURCE, RNG_SRC_JITTER, RNG_SRC_RNDR,
    RNG_SRC_SMCCC, RNG_SRC_SOC, rng_source_word,
};
use cpu::drbg::Source;
use cpu::println;
use cpu::trng::Kind;

/// De teller voor een jitter-zaad: CNTPCT op arm64.
#[cfg(not(target_arch = "riscv64"))]
use cpu::idle::counter;
/// De teller voor een jitter-zaad: de TIME-CSR op riscv64.
#[cfg(target_arch = "riscv64")]
use cpu::riscv::idle::counter;

/// Zorgt dat de DRBG van de kern geseed is en zegt in één regel wat de
/// slots krijgen. Eén keer bij de boot (ook na een flip), vóór de eerste
/// kooi. Een board dat in `discover` niet zaaide (QEMU virt, de Mac mini,
/// de riscv64-boards), wordt hier gezaaid uit de CPU of uit jitter, met de
/// regel van `cpu::drbg::seed_from_cpu`.
pub(crate) fn init() {
    if !cpu::drbg::is_seeded() {
        println!("{}", cpu::drbg::seed_from_cpu(counter));
    }
    let (src, kind) = match cpu::drbg::source() {
        Source::Hardware(k) => (k.name(), "hardware"),
        Source::Jitter => ("timer jitter", "jitter"),
    };
    println!(
        "rng: every slot gets {CTRL_RNG_SEED_LEN} bytes from the kernel DRBG ({src}) on its control page at build and every second HOPOS_RNG_SLOTS source={kind}"
    );
}

/// De `RNG_SRC_*` van de bron van de DRBG.
fn source_code(src: Source) -> u8 {
    match src {
        Source::Jitter => RNG_SRC_JITTER,
        Source::Hardware(Kind::Rndr) => RNG_SRC_RNDR,
        Source::Hardware(Kind::SmcccTrng) => RNG_SRC_SMCCC,
        Source::Hardware(Kind::Soc(_)) => RNG_SRC_SOC,
    }
}

/// Zet vers zaad op de control-page op `page`, volgens het seqlock van
/// `CTRL_RNG_GEN`: oneven, zaad en bron, dan de volgende even generatie.
/// Elke `push` is een clean met een barrière erachter, dus de app (die de
/// page Normal-NC leest) ziet de stappen in deze volgorde. Geeft `false`
/// als de DRBG niets gaf (ongeseed): dan blijft de page zoals hij was.
pub(crate) fn plant(page: dev::Pa) -> bool {
    let mut seed = [0u8; CTRL_RNG_SEED_LEN];
    if cpu::drbg::read(&mut seed).is_err() {
        return false;
    }
    let gen_at = page.add(CTRL_RNG_GEN);
    dev::pull(gen_at, 8);
    // Doortellen vanaf wat er staat: zo blijft de generatie per page
    // monotoon, ook als een nieuwe kern na een flip de page overneemt.
    let even = dev::read64(gen_at) & !1;
    dev::write64(gen_at, even | 1);
    dev::push(gen_at, 8);
    for (i, w) in seed.chunks_exact(8).enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(w);
        dev::write64(
            page.add(CTRL_RNG_SEED + 8 * i as u64),
            u64::from_le_bytes(b),
        );
    }
    dev::write64(
        page.add(CTRL_RNG_SOURCE),
        rng_source_word(source_code(cpu::drbg::source())),
    );
    dev::push(page.add(CTRL_RNG_SOURCE), CTRL_RNG_SEED_LEN + 16);
    // 0 is "geen zaad": na een wrap meteen door naar 2.
    dev::write64(gen_at, even.wrapping_add(2).max(2));
    dev::push(gen_at, 8);
    seed.fill(0);
    core::hint::black_box(&mut seed);
    true
}

/// Vers zaad op de page van elke gebouwde kooi; de telemetrie-tik roept dit
/// elke seconde.
pub(crate) fn refresh() {
    crate::clock::each_page(|_, page| {
        plant(page);
    });
}
