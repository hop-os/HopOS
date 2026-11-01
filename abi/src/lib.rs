//! Het contract tussen kern en app: wat beide kanten in hetzelfde geheugen
//! lezen en schrijven.
//!
//! Dit is ABI-versie 1 van HopOS v3. Het idee is dat van de Go-generatie
//! (slot-ABI 7 in `OLD/metal/abi`): een app ziet één regio, zijn eigen
//! partitie, met onderin zijn RAM en bovenin een staart met control-page,
//! outbox-ring en frame-ringen; alles wat hij kent rekent hij uit twee
//! waarden die al in zijn image staan. De nummering begint opnieuw omdat
//! de generatie opnieuw begint; de resten (de mailbox-RPC, recordtypes 3
//! en 4, de inbox-ring) gaan niet mee.
//!
//! De modules:
//!
//! - [`layout`]: de slot-ABI (de staart van de partitie) en het PA-plan
//!   ([`layout::Plan`]): waar de kern zijn eigen structuren fysiek legt.
//! - [`ring`]: de SPSC-ring over device-geheugen, met [`ring::Writer`] en
//!   [`ring::Reader`] als de twee kanten.
//! - [`hopabi`]: de control-page en de payload van een system-call.
//! - [`systemapi`]: het frame-protocol over TCP en de bevoegde operaties.
//! - `glass` (feature `gui`): het glas en de invoer van de display-app (de
//!   `FB_*`-env, de pixelregel, de invoerstroom op poort 7879).
//! - [`checksum`]: de content-som die kern en app over hetzelfde bestand
//!   rekenen.
//! - [`place`]: de plaatsingstoets van een app-image.
//! - [`sha256`]: SHA-256, voor de DRBG, hopfs en de flip-bundel.
//!
//! Twee dingen hier hebben geen app-gebruiker: [`sha256`] en [`checksum`].
//! Ze staan hier omdat kern en hopos ze allebei nodig hebben en `abi` de
//! laagste crate is die beide kennen; een ontwerpkeuze
//! (docs/description.md 8.3), geen lek in het contract.
//!
//! Wat hier NIET staat: toegang tot device-geheugen buiten de ring. Wie een
//! control-page-veld leest, doet `dev::read64(tail.ctrl_page().add(..))` met
//! een offset uit deze crate; het vertrouwen zit in de layout (handboek §5).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![forbid(unsafe_code)]

pub mod checksum;
// Het contract van het glas (feature `gui`, docs/gui.md): headless kent het
// niet.
#[cfg(feature = "gui")]
pub mod glass;
pub mod hopabi;
pub mod layout;
pub mod place;
pub mod ring;
pub mod sha256;
pub mod systemapi;

use core::fmt;

/// De ABI-versie die kern en app moeten delen. Een image met een ander
/// nummer wordt bij plaatsing geweigerd ([`place::build`]).
///
/// Verhogen bij élke wijziging die de app-kant raakt: de indeling van de
/// staart, de control-page-offsets, de ringgeometrie, of de betekenis van
/// een woord op een bestaand adres. Dat laatste is de les van Go-versie 5
/// (03-09): `CtrlIdleMode` hergebruikte het adres en de waarde van
/// `CtrlCorePrep` met een ander bevel, en een oude app op een nieuwe kern
/// voerde elke idle-ronde een IMP-DEF-write uit. Een woord hergebruiken kost
/// een versie, ook als het adres blijft staan.
///
/// Versie 1 is byte voor byte de Go-ABI 10 (`OLD/metal/abi/layout`): de
/// staart, de control-page-offsets, de ringgeometrie, de HVC-nummers en het
/// systemapi-frame. Daarom laat [`place::build`] ook de Go-stempel
/// ([`place::GO_ABI_VERSION`]) toe, en draait een tamago-image uit `OLD/`
/// ongewijzigd (`docs/go-apps.md`). Wie hier 2 van maakt, haalt die alias
/// weg of hertaalt de Go-applib mee.
pub const ABI_VERSION: u32 = 1;

/// Een regio fysiek geheugen: basis en maat in bytes.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Region {
    /// Het eerste byte.
    pub base: u64,
    /// De maat in bytes.
    pub size: u64,
}

impl Region {
    /// Een regio van `size` bytes vanaf `base`.
    #[must_use]
    pub const fn new(base: u64, size: u64) -> Region {
        Region { base, size }
    }

    /// Het eerste byte voorbij de regio, of `None` als dat omloopt.
    #[must_use]
    pub const fn end(self) -> Option<u64> {
        self.base.checked_add(self.size)
    }

    /// Delen twee niet-lege regio's minstens één byte?
    ///
    /// Beide regio's moeten door [`Region::end`] komen; wie dat niet
    /// toetste, krijgt `true` (een omlopende regio overlapt alles).
    #[must_use]
    pub const fn overlaps(self, other: Region) -> bool {
        if self.size == 0 || other.size == 0 {
            return false;
        }
        match (self.end(), other.end()) {
            (Some(a), Some(b)) => self.base < b && other.base < a,
            _ => true,
        }
    }
}

/// Waarom een ABI-operatie weigerde; elke variant draagt de getallen.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// Een regio loopt voorbij het einde van de adresruimte.
    RegionOverflow {
        /// Basis van de regio.
        base: u64,
        /// Maat van de regio.
        size: u64,
    },
    /// Een adres of maat heeft niet de uitlijning die het ijzer eist.
    Misaligned {
        /// Welk veld.
        what: &'static str,
        /// Het adres of de maat.
        value: u64,
        /// De vereiste uitlijning.
        align: u64,
    },
    /// Een verplicht veld van het plan staat op nul.
    Missing(&'static str),
    /// Het plan heeft geen partitie-geheugen.
    EmptyPool,
    /// Twee regio's die niet mogen overlappen, overlappen.
    Overlap {
        /// De eerste regio.
        a: Region,
        /// De tweede regio.
        b: Region,
    },
    /// Een begrensde lijst regio's of segmenten is vol.
    TooMany {
        /// Wat er vol zit.
        what: &'static str,
        /// De capaciteit.
        cap: usize,
    },
    /// Een index valt buiten wat het plan reserveerde.
    OutOfPlan {
        /// De index (slot of core).
        index: usize,
        /// De hoogste index die het plan draagt.
        max: usize,
    },
    /// Een ring-backing die niet klopt: te klein, scheef of omlopend.
    RingBacking {
        /// De basis.
        base: u64,
        /// De datacapaciteit.
        size: u64,
    },
    /// Een record past nooit in deze ring (groter dan de halve capaciteit).
    RecordTooLarge {
        /// Wat het record nodig heeft, kop en opvulling inbegrepen.
        need: u64,
        /// De grens: de halve capaciteit.
        max: u64,
    },
    /// De ring is (nu) vol; na een drain past het record wel.
    RingFull {
        /// Wat het record nodig heeft.
        need: u64,
        /// Wat er vrij is.
        free: u64,
    },
    /// De indexen van een ring zijn onmogelijk (een malafide tegenpartij).
    RingIndices {
        /// De producer-index.
        head: u64,
        /// De consumer-index.
        tail: u64,
    },
    /// Een frame of payload is korter dan zijn kop.
    Short {
        /// De lengte die er is.
        len: usize,
        /// De lengte die nodig is.
        need: usize,
    },
    /// Een frame draagt een verkeerd magisch getal.
    BadMagic(u32),
    /// Een frame of payload spreekt een andere versie.
    BadVersion {
        /// De versie op de draad.
        got: u8,
        /// De versie van deze kant.
        want: u8,
    },
    /// Een frame draagt een onbekende soort.
    BadKind(u8),
    /// Een payload is groter dan het protocol toestaat.
    PayloadTooLarge {
        /// De lengte.
        len: usize,
        /// De grens.
        max: usize,
    },
    /// Het plaatsingsvenster is onzin.
    PlaceWindow {
        /// De linkbasis.
        base: u64,
        /// De app-RAM-maat.
        size: u64,
        /// De ondergrens (offset).
        lo: u64,
        /// De bovengrens (offset).
        top: u64,
    },
    /// De image-maat is onzin.
    ImageSize(u64),
    /// De entry van het image ligt buiten het linkvenster.
    Entry {
        /// De entry.
        entry: u64,
        /// De linkbasis.
        base: u64,
        /// De app-RAM-maat.
        size: u64,
    },
    /// Een segment valt buiten het linkvenster, het image of onder de staging.
    Segment {
        /// Het doeladres.
        paddr: u64,
        /// De maat in het geheugen.
        memsz: u64,
        /// De maat in het bestand.
        filesz: u64,
        /// De bestandsoffset.
        off: u64,
    },
    /// Het image heeft geen enkel PT_LOAD-segment.
    NoSegments,
    /// Een verplicht symbool ontbreekt of ligt buiten het venster.
    Symbol {
        /// De symboolnaam.
        name: &'static str,
        /// Het adres (0 als het ontbreekt).
        addr: u64,
    },
    /// Het image draagt geen ABI-stempel.
    NoAbiStamp {
        /// De versie die deze kern spreekt.
        want: u32,
    },
    /// Het image spreekt een andere ABI-versie.
    AbiMismatch {
        /// De versie in het image.
        image: u64,
        /// De versie die deze kern spreekt.
        want: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::RegionOverflow { base, size } => {
                write!(f, "memory region {base:#x}+{size:#x} overflows")
            }
            Self::Misaligned { what, value, align } => {
                write!(f, "{what} {value:#x} not aligned to {align:#x}")
            }
            Self::Missing(what) => write!(f, "plan field {what} missing"),
            Self::EmptyPool => f.write_str("plan pool is empty"),
            Self::Overlap { a, b } => write!(
                f,
                "region {:#x}+{:#x} overlaps {:#x}+{:#x}",
                a.base, a.size, b.base, b.size
            ),
            Self::TooMany { what, cap } => write!(f, "more than {cap} {what}"),
            Self::OutOfPlan { index, max } => {
                write!(f, "index {index} outside plan (max {max})")
            }
            Self::RingBacking { base, size } => {
                write!(f, "invalid ring backing {base:#x}+{size:#x}")
            }
            Self::RecordTooLarge { need, max } => {
                write!(f, "record of {need} bytes exceeds ring limit {max}")
            }
            Self::RingFull { need, free } => {
                write!(f, "ring full: need {need}, free {free}")
            }
            Self::RingIndices { head, tail } => {
                write!(f, "impossible ring indices head={head:#x} tail={tail:#x}")
            }
            Self::Short { len, need } => write!(f, "frame of {len} bytes, need {need}"),
            Self::BadMagic(m) => write!(f, "bad magic {m:#x}"),
            Self::BadVersion { got, want } => write!(f, "version {got}, want {want}"),
            Self::BadKind(k) => write!(f, "unknown frame kind {k}"),
            Self::PayloadTooLarge { len, max } => write!(f, "payload {len} > {max}"),
            Self::PlaceWindow {
                base,
                size,
                lo,
                top,
            } => write!(
                f,
                "invalid placement window {base:#x}+{size:#x} [{lo:#x},{top:#x})"
            ),
            Self::ImageSize(n) => write!(f, "invalid image size {n}"),
            Self::Entry { entry, base, size } => {
                write!(
                    f,
                    "entry {entry:#x} outside link window {base:#x}+{size:#x}"
                )
            }
            Self::Segment {
                paddr,
                memsz,
                filesz,
                off,
            } => write!(
                f,
                "segment {paddr:#x}+{memsz:#x} (file {off:#x}+{filesz:#x}) outside bounds"
            ),
            Self::NoSegments => f.write_str("no PT_LOAD segments"),
            Self::Symbol { name, addr } => {
                write!(f, "symbol {name} ({addr:#x}) missing or outside link range")
            }
            Self::NoAbiStamp { want } => write!(
                f,
                "image predates the versioned slot ABI, rebuild it against ABI {want}"
            ),
            Self::AbiMismatch { image, want } => {
                write!(f, "image speaks slot ABI {image}, this HopOS speaks {want}")
            }
        }
    }
}

/// Het resultaat van een ABI-operatie.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;
