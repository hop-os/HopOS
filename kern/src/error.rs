//! De fouten van de kern: één kleine enum, met de getallen erin.

use core::fmt;

/// `Result` met de kernfout als standaard (handboek §6).
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Waarom een kernoperatie niet lukte.
///
/// Elke variant draagt de getallen die een operator nodig heeft (slot, adres,
/// maat, core); de `Display` zet ze in één Engelse regel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    missing_docs,
    reason = "de velden zijn de getallen die de variant-doc noemt"
)]
pub enum Error {
    /// Het slotnummer ligt buiten `1..=max`.
    SlotRange { slot: usize, max: usize },
    /// Het corenummer ligt buiten het app-bereik.
    CoreRange { core: usize, max: usize },
    /// Een partitiemaat die nul is, omloopt of het app-venster overschrijdt.
    PartitionSize { size: u64 },
    /// De partitie past nu niet in de pool: een CAPACITEITSTOESTAND, geen
    /// defect. Hij verdwijnt zodra een buur vrijkomt.
    NoPartition { size: u64 },
    /// Het slot bezit nog een partitie; eerst een bevestigde stop.
    StillOwned { slot: usize },
    /// Het slot bezit geen partitie.
    NotOwned { slot: usize },
    /// Het slot leeft niet (een stroom, een dode context): niet over te
    /// dragen in een flip.
    NotLive { slot: usize },
    /// Een bereik dat vrij had moeten zijn, is (deels) van een ander.
    NotFree { base: u64, size: u64 },
    /// De tabel of lijst is vol (een vaste maat uit de bron).
    Full { cap: usize },
    /// De heap kon de allocatie niet plaatsen.
    OutOfMemory { bytes: usize },
    /// Een sharegroup-spec die niet bij de bestaande groep past: geen
    /// capaciteit, dus niet "pending" behandelen.
    PoolSize { have: usize, want: usize },
    /// Geen vrije run van `cores` app-cores (van de gevraagde klasse).
    NoCores { cores: usize },
    /// Een core van een bestaande groep heeft een andere klasse dan gevraagd.
    ClassMismatch { core: usize },
    /// Een core draait nog waar hij stil had moeten staan.
    CoreBusy { core: usize },
    /// De kooi weigerde (bouwen, bereik, venster).
    Cage { slot: usize, code: u32 },
    /// Een ongeldig bereik voor een stage-2-map of grant.
    Range { base: u64, size: u64 },
    /// Een record (handoff, boom, frame) is kapot op deze byte-positie.
    Corrupt { at: usize },
    /// Een versie of magic klopt niet.
    Version { have: u64, want: u64 },
    /// Een lengte past niet in de grens.
    TooLarge { len: usize, max: usize },
    /// Het pad bestaat niet.
    NoEnt,
    /// Het pad is een directory waar een bestand hoorde (of andersom).
    Kind,
    /// De directory is niet leeg.
    NotEmpty,
    /// Een pad met `..` of een lege naam.
    BadPath,
    /// Een pad buiten het zicht van de taak: buiten de eigen root en de
    /// volumes (`errDenied` in Go).
    Denied,
    /// Het blokapparaat meldde een fout op dit LBA.
    Io { lba: u64 },
    /// Een apparaat faalde buiten HopFS; de eigenaar logt het driverdetail.
    Device { reason: &'static str },
    /// De schijf (het venster) is vol.
    DiskFull { blocks: u64 },
    /// De verbinding is weg of meldde een fout.
    Conn,
    /// Deze operatie vraagt de bevoegdheid van Hop.
    Privilege { slot: usize },
    /// De brievenbus van de actor zit vol.
    Busy,
    /// Een poort van de jobspec is op deze node al van een ander slot
    /// (`owner`); de start gaat niet door.
    PortTaken {
        slot: usize,
        port: u16,
        owner: usize,
    },
    /// De switch nam de publicatie van een poort niet aan (vol, geen switch).
    PortRefused { slot: usize, port: u16 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::SlotRange { slot, max } => write!(f, "slot {slot} out of range 1..{max}"),
            Self::CoreRange { core, max } => write!(f, "core {core} out of range 1..{max}"),
            Self::PartitionSize { size } => write!(f, "invalid partition size {size:#x}"),
            Self::NoPartition { size } => write!(
                f,
                "no partition space: {} MiB does not fit the pool",
                size >> 20
            ),
            Self::StillOwned { slot } => write!(f, "slot {slot} still owns a partition"),
            Self::NotOwned { slot } => write!(f, "slot {slot} owns no partition"),
            Self::NotLive { slot } => write!(f, "slot {slot} is not live"),
            Self::NotFree { base, size } => {
                write!(f, "range {base:#x}+{size:#x} is not free in this pool")
            }
            Self::Full { cap } => write!(f, "table full ({cap} entries)"),
            Self::OutOfMemory { bytes } => write!(f, "out of memory ({bytes} bytes)"),
            Self::PoolSize { have, want } => write!(
                f,
                "sharegroup pool size differs: group has {have} core(s), spec asks {want}"
            ),
            Self::NoCores { cores } => write!(f, "no free run of {cores} app core(s)"),
            Self::ClassMismatch { core } => write!(f, "core {core} has another class"),
            Self::CoreBusy { core } => write!(f, "core {core} still running"),
            Self::Cage { slot, code } => write!(f, "slot {slot}: cage refused (code {code})"),
            Self::Range { base, size } => write!(f, "invalid range {base:#x}+{size:#x}"),
            Self::Corrupt { at } => write!(f, "corrupt record at byte {at}"),
            Self::Version { have, want } => write!(f, "version {have:#x}, want {want:#x}"),
            Self::TooLarge { len, max } => write!(f, "length {len} exceeds {max}"),
            Self::NoEnt => f.write_str("does not exist"),
            Self::Kind => f.write_str("wrong kind (file or directory)"),
            Self::NotEmpty => f.write_str("directory not empty"),
            Self::BadPath => f.write_str("invalid path"),
            Self::Denied => f.write_str("outside the task's root and volumes"),
            Self::Io { lba } => write!(f, "block I/O failed at LBA {lba}"),
            Self::Device { reason } => write!(f, "device: {reason}"),
            Self::DiskFull { blocks } => write!(f, "disk full ({blocks} blocks)"),
            Self::Conn => f.write_str("connection closed"),
            Self::Privilege { slot } => {
                write!(f, "slot {slot}: privileged operation without privilege")
            }
            Self::Busy => f.write_str("actor mailbox full"),
            Self::PortTaken { slot, port, owner } => {
                write!(f, "slot {slot}: port {port} is taken by slot {owner}")
            }
            Self::PortRefused { slot, port } => {
                write!(f, "slot {slot}: port {port} not published by the switch")
            }
        }
    }
}

impl From<abi::Error> for Error {
    /// Een draadfout van `abi` (een framekop, een call) in de termen van de
    /// kern, met dezelfde getallen.
    fn from(e: abi::Error) -> Self {
        match e {
            abi::Error::BadMagic(m) => Self::Version {
                have: u64::from(m),
                want: u64::from(abi::systemapi::MAGIC),
            },
            abi::Error::BadVersion { got, want } => Self::Version {
                have: u64::from(got),
                want: u64::from(want),
            },
            abi::Error::PayloadTooLarge { len, max } => Self::TooLarge { len, max },
            abi::Error::Short { len, .. } => Self::Corrupt { at: len },
            _ => Self::Corrupt { at: 0 },
        }
    }
}

impl From<blkdev::Error> for Error {
    /// Een blokfout wordt de I/O-fout van de kern, mét zijn LBA; een dood
    /// device of een verzoek buiten de schijf heeft geen betere plek dan de
    /// LBA waar het misging.
    fn from(e: blkdev::Error) -> Self {
        match e {
            blkdev::Error::Io { lba } | blkdev::Error::OutOfRange { lba, .. } => Self::Io { lba },
            blkdev::Error::Dead => Self::Io { lba: 0 },
            // Een verlaten verzoek loopt nog: later opnieuw, zoals een volle
            // brievenbus.
            blkdev::Error::Busy => Self::Busy,
        }
    }
}
