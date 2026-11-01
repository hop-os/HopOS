//! De willekeur van een app: een ChaCha20-DRBG, gezaaid uit het zaad dat de
//! kern op de control-page legt (`CTRL_RNG_SEED`) en gemengd met eigen
//! timer-jitter.
//!
//! Dit bezit het lezen van dat zaad (het seqlock van `CTRL_RNG_GEN`,
//! [`read_seed`]), de DRBG ([`Rng`]) en de ene luide regel per app over waar
//! de willekeur vandaan komt. Niet van hier: het zaad zelf (de kern,
//! `hopos/src/seed.rs`, uit zijn eigen DRBG: de TRNG van het board, anders
//! jitter) en wat een app met de bytes doet (TLS, de ISS van de netstack).
//!
//! Waarom een eigen DRBG en geen rauw zaad: het zaad staat op een pagina die
//! de app deelt met de kern, het wordt elke seconde vervangen, en een app
//! trekt er meer dan 32 bytes uit. Dus gaat het door een staat van 32 bytes
//! die alleen de eigenaar van de [`Rng`] ziet, samen met jitter van deze
//! core, zodat een zaad dat ooit uitlekt de stroom niet verraadt en een
//! ontbrekend zaad geen vaste stroom geeft.
//!
//! Het recept: ChaCha20 (RFC 8439) als blokfunctie. Mengen is
//! `key' = ChaCha20(key ^ blok)[0..8]` per blok van 32 bytes, met de
//! lengte en een domein in de nonce; trekken is snelle sleutelvernietiging
//! (Bernstein): elk blok geeft 32 bytes nieuwe sleutel en 32 bytes uitvoer,
//! dus een gelekte staat verraadt geen eerdere uitvoer. ChaCha20 en geen
//! SHA-256 als menger, al ligt die er (`abi::sha256`, sinds 03-10 de ene
//! kopie voor kern, `cpu::drbg` en kernflip): het trekken is toch al
//! ChaCha20, dus één primitief voor beide, en ChaCha20 is twintig regels.
//!
//! De regel, één keer per app, bij de eerste [`Rng::open`]:
//!
//! ```text
//! HOPOS_APP_RNG source=hardware kind=rndr|smccc-trng|soc   zaad uit een TRNG
//! HOPOS_APP_RNG source=jitter                              zaad uit de jitter van de kern
//! HOPOS_APP_RNG_NONE                                       geen zaad: een oudere kern
//! ```

use crate::app::App;
use crate::contract::{
    CTRL_RNG_GEN, CTRL_RNG_SEED, CTRL_RNG_SEED_LEN, CTRL_RNG_SOURCE, RNG_SRC_JITTER, RNG_SRC_RNDR,
    RNG_SRC_SMCCC, RNG_SRC_SOC, rng_source,
};
use crate::ctrl::Ctrl;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Hoeveel jitter-metingen een verse [`Rng`] doet. Elke meting geeft
/// hooguit een paar bits; met een hardware-zaad eronder is dit een extra,
/// zonder zaad is het alles wat er is. 256 houdt de start rond een
/// milliseconde op QEMU (Hop's pool meet 512 hash-rondes in ~0,3 ms, 29-09).
pub const JITTER_ROUNDS: usize = 256;

/// Hoe vaak [`read_seed`] het opnieuw probeert als de kern net schrijft.
const SEQ_TRIES: usize = 8;

/// De constanten van ChaCha20: "expand 32-byte k".
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Het domein in de nonce: mengen (met de lengte erbij) of trekken.
const DOMAIN_MIX: u32 = 0x6d69_7821;
/// Zie [`DOMAIN_MIX`].
const DOMAIN_DRAW: u32 = 0x6472_6177;

/// Is de regel van deze app al gezegd?
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// Waar het zaad van een [`Rng`] vandaan kwam.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Origin {
    /// De kern gaf zaad uit RNDR (FEAT_RNG).
    Rndr,
    /// De kern gaf zaad uit de SMCCC TRNG van de firmware.
    SmcccTrng,
    /// De kern gaf zaad uit een TRNG-blok van de SoC (RNG200, RKRNG).
    Soc,
    /// De kern gaf zaad, maar zijn eigen DRBG is uit jitter gezaaid.
    Jitter,
    /// Geen zaad op de page (een oudere kern): alleen eigen jitter.
    None,
}

impl Origin {
    /// De bron van een `RNG_SRC_*`.
    const fn of(src: u8) -> Self {
        match src {
            RNG_SRC_RNDR => Self::Rndr,
            RNG_SRC_SMCCC => Self::SmcccTrng,
            RNG_SRC_SOC => Self::Soc,
            RNG_SRC_JITTER => Self::Jitter,
            _ => Self::None,
        }
    }

    /// Kwam het zaad uit hardware-entropie?
    #[must_use]
    pub const fn is_hardware(self) -> bool {
        matches!(self, Self::Rndr | Self::SmcccTrng | Self::Soc)
    }

    /// De naam op de regel: `rndr`, `smccc-trng`, `soc`, `jitter`, `none`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rndr => "rndr",
            Self::SmcccTrng => "smccc-trng",
            Self::Soc => "soc",
            Self::Jitter => "jitter",
            Self::None => "none",
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Het zaad zoals het op de page lag.
#[derive(Clone)]
pub struct PageSeed {
    /// De 32 bytes.
    pub bytes: [u8; CTRL_RNG_SEED_LEN],
    /// De generatie (even, niet 0).
    pub generation: u64,
    /// De bron van de DRBG van de kern.
    pub origin: Origin,
}

impl fmt::Debug for PageSeed {
    /// Zonder de bytes: een zaad hoort niet in een log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageSeed")
            .field("generation", &self.generation)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// Leest het zaad van de control-page, volgens het seqlock van
/// `CTRL_RNG_GEN`: generatie, bron en zaad, en de generatie nog eens.
/// `None` als er geen zaad ligt (generatie 0, of een bronwoord zonder de
/// magic: een oude kern), of als de kern bij elke poging net schreef.
#[must_use]
pub fn read_seed(ctrl: &Ctrl) -> Option<PageSeed> {
    for _ in 0..SEQ_TRIES {
        let generation = ctrl.get(CTRL_RNG_GEN);
        if generation == 0 {
            return None;
        }
        if generation & 1 == 1 {
            core::hint::spin_loop();
            continue;
        }
        let origin = Origin::of(rng_source(ctrl.get(CTRL_RNG_SOURCE))?);
        let mut bytes = [0u8; CTRL_RNG_SEED_LEN];
        for (i, w) in bytes.chunks_exact_mut(8).enumerate() {
            w.copy_from_slice(&ctrl.get(CTRL_RNG_SEED + 8 * i as u64).to_le_bytes());
        }
        if ctrl.get(CTRL_RNG_GEN) == generation {
            return Some(PageSeed {
                bytes,
                generation,
                origin,
            });
        }
        wipe(&mut bytes);
    }
    None
}

/// Een DRBG met één eigenaar: ChaCha20 met snelle sleutelvernietiging,
/// gezaaid uit de page en uit jitter.
pub struct Rng {
    key: [u32; 8],
    draws: u64,
    generation: u64,
    origin: Origin,
    page: Option<Ctrl>,
}

impl fmt::Debug for Rng {
    /// Zonder de sleutel.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rng")
            .field("generation", &self.generation)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl Rng {
    /// Een DRBG voor `app`: het zaad van zijn control-page, het slot, de
    /// wandklok en [`JITTER_ROUNDS`] jitter-metingen op de teller van de
    /// app. De eerste in deze app zegt in één regel waar de willekeur
    /// vandaan komt (`HOPOS_APP_RNG`, of `HOPOS_APP_RNG_NONE` zonder zaad).
    ///
    /// Elke aanroep geeft een eigen staat (de jitter verschilt); geef een
    /// `Rng` liever mee dan er steeds een te openen.
    #[must_use]
    pub fn open(app: &App) -> Self {
        let mut rng = Self::with_page(app.ctrl(), crate::clock::now_ns);
        rng.stir(&app.slot().to_le_bytes());
        rng.stir(&app.wall_ns().unwrap_or(0).to_le_bytes());
        if !ANNOUNCED.swap(true, Relaxed) {
            app.log(format_args!("{}", Announce(rng.origin, rng.generation)));
        }
        rng
    }

    /// Een DRBG over de control-page `page`, met jitter uit `now` (monotone
    /// ns), zonder regel. Voor wie geen [`App`] heeft; [`Rng::open`] is de
    /// gewone weg.
    #[must_use]
    pub fn with_page(page: Ctrl, now: impl FnMut() -> u64) -> Self {
        let seed = read_seed(&page);
        let mut rng = Self::blank();
        rng.page = Some(page);
        if let Some(mut s) = seed {
            rng.absorb(&s.bytes);
            rng.generation = s.generation;
            rng.origin = s.origin;
            wipe(&mut s.bytes);
        }
        rng.harvest(now, JITTER_ROUNDS);
        rng
    }

    /// Een DRBG zonder page uit `seed` en jitter uit `now`; de bron is
    /// [`Origin::None`]. Voor tests en voor een app die zelf zaad heeft.
    #[must_use]
    pub fn from_seed(seed: &[u8], now: impl FnMut() -> u64) -> Self {
        let mut rng = Self::blank();
        rng.absorb(seed);
        rng.harvest(now, JITTER_ROUNDS);
        rng
    }

    /// Een lege staat: nul-sleutel, geen zaad.
    const fn blank() -> Self {
        Self {
            key: [0; 8],
            draws: 0,
            generation: 0,
            origin: Origin::None,
            page: None,
        }
    }

    /// Waar het zaad vandaan kwam (na een herzaaiing: het laatste zaad).
    #[must_use]
    pub const fn origin(&self) -> Origin {
        self.origin
    }

    /// De generatie van het laatst gemengde zaad (0 = nooit zaad).
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Mengt `sample` in de staat: een tijd, bytes van buiten.
    pub fn stir(&mut self, sample: &[u8]) {
        self.absorb(sample);
    }

    /// Mengt vers zaad van de page als de kern een nieuwe generatie legde.
    /// Geeft `true` als er gemengd is. [`Rng::fill`] doet dit zelf.
    pub fn reseed(&mut self) -> bool {
        let Some(page) = self.page else {
            return false;
        };
        let g = page.get(CTRL_RNG_GEN);
        if g == 0 || g == self.generation || g & 1 == 1 {
            return false;
        }
        let Some(mut s) = read_seed(&page) else {
            return false;
        };
        self.absorb(&s.bytes);
        self.generation = s.generation;
        self.origin = s.origin;
        wipe(&mut s.bytes);
        true
    }

    /// Vult `out` met willekeur; eerst vers zaad als de kern dat legde.
    pub fn fill(&mut self, out: &mut [u8]) {
        self.reseed();
        for chunk in out.chunks_mut(32) {
            self.draws = self.draws.wrapping_add(1);
            let nonce = [DOMAIN_DRAW, self.draws as u32, (self.draws >> 32) as u32];
            let b = block(&self.key, 0, nonce);
            self.key.copy_from_slice(&b[..8]);
            let mut bytes = [0u8; 32];
            for (o, w) in bytes.chunks_exact_mut(4).zip(&b[8..]) {
                o.copy_from_slice(&w.to_le_bytes());
            }
            chunk.copy_from_slice(bytes.get(..chunk.len()).unwrap_or(&bytes));
            wipe(&mut bytes);
        }
    }

    /// `N` bytes willekeur, bijvoorbeeld 96 voor een `leantls::Entropy`.
    #[must_use]
    pub fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut b = [0u8; N];
        self.fill(&mut b);
        b
    }

    /// Een willekeurig `u32`.
    #[must_use]
    pub fn next_u32(&mut self) -> u32 {
        u32::from_le_bytes(self.array())
    }

    /// Een willekeurig `u64`.
    #[must_use]
    pub fn next_u64(&mut self) -> u64 {
        u64::from_le_bytes(self.array())
    }

    /// Mengt `data` per blok van 32 bytes: `key' = ChaCha20(key ^ blok)`,
    /// met de totale lengte en de blokindex in nonce en teller, zodat
    /// `"ab"` en `"ab\0"` niet hetzelfde mengen.
    fn absorb(&mut self, data: &[u8]) {
        let len = data.len() as u64;
        for (i, chunk) in data.chunks(32).enumerate() {
            let mut words = [0u8; 32];
            if let Some(w) = words.get_mut(..chunk.len()) {
                w.copy_from_slice(chunk);
            }
            let mut k = self.key;
            for (kw, b) in k.iter_mut().zip(words.chunks_exact(4)) {
                *kw ^= u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            let nonce = [DOMAIN_MIX, len as u32, (len >> 32) as u32];
            let b = block(&k, i as u32, nonce);
            self.key.copy_from_slice(&b[..8]);
            wipe(&mut words);
        }
    }

    /// Meet `rounds` keer de duur van een ChaCha20-blok met `now` en mengt
    /// de tijden in, per vier: de jitter is de willekeur.
    fn harvest(&mut self, mut now: impl FnMut() -> u64, rounds: usize) {
        let mut buf = [0u8; 32];
        let mut prev = now();
        let mut work = [0u32; 16];
        for i in 0..rounds {
            // Werk met een schommelende duur: een blok rekenen.
            work = block(&self.key, i as u32, [work[0], prev as u32, 0]);
            let t = now();
            let at = (i % 4) * 8;
            if let Some(slot) = buf.get_mut(at..at + 8) {
                slot.copy_from_slice(&t.wrapping_sub(prev).to_le_bytes());
            }
            prev = t;
            if i % 4 == 3 || i + 1 == rounds {
                self.absorb(&buf);
            }
        }
        wipe(&mut buf);
    }
}

/// De regel van [`Rng::open`].
struct Announce(Origin, u64);

impl fmt::Display for Announce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(origin, generation) = *self;
        match origin {
            Origin::None => write!(
                f,
                "applib: WARNING no seed on the control page (an older kernel): the rng runs on {JITTER_ROUNDS} jitter samples only, no high-value secrets in this slot HOPOS_APP_RNG_NONE"
            ),
            Origin::Jitter => write!(
                f,
                "applib: rng seed from the kernel (gen {generation}) is jitter, not hardware; mixed with {JITTER_ROUNDS} own jitter samples, no high-value secrets in this slot HOPOS_APP_RNG source=jitter"
            ),
            hw => write!(
                f,
                "applib: rng seeded from the kernel ({hw}, gen {generation}) and {JITTER_ROUNDS} jitter samples HOPOS_APP_RNG source=hardware kind={hw}"
            ),
        }
    }
}

/// Eén ChaCha20-blok (RFC 8439 §2.3): de toestand na twintig rondes plus de
/// begintoestand.
fn block(key: &[u32; 8], counter: u32, nonce: [u32; 3]) -> [u32; 16] {
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&SIGMA);
    s[4..12].copy_from_slice(key);
    s[12] = counter;
    s[13..].copy_from_slice(&nonce);
    let mut x = s;
    for _ in 0..10 {
        quarter(&mut x, 0, 4, 8, 12);
        quarter(&mut x, 1, 5, 9, 13);
        quarter(&mut x, 2, 6, 10, 14);
        quarter(&mut x, 3, 7, 11, 15);
        quarter(&mut x, 0, 5, 10, 15);
        quarter(&mut x, 1, 6, 11, 12);
        quarter(&mut x, 2, 7, 8, 13);
        quarter(&mut x, 3, 4, 9, 14);
    }
    for (o, i) in x.iter_mut().zip(s) {
        *o = o.wrapping_add(i);
    }
    x
}

/// De kwartronde van ChaCha20 op de woorden `a`, `b`, `c`, `d`.
fn quarter(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}

/// Wist zaadmateriaal; `black_box` houdt de schrijf in leven.
fn wipe(b: &mut [u8]) {
    b.fill(0);
    core::hint::black_box(b);
}

#[cfg(test)]
mod tests;
