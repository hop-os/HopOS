//! De PSCI/SMCCC-primitieven (ARM DEN 0022): de functie-ID's, de
//! return-codes, de conduit en de dunne call-wrappers.
//!
//! Eén bron van waarheid: een verkeerde functie-ID of return-code hoort
//! maar op één plek te leven, niet per board hergedefinieerd (dat stond
//! hij, in drie stijlen; opgeruimd 18-07).
//!
//! De conduit is altijd SMC: HopOS eist een EL2-boot (stage-2-isolatie is
//! een invariant, geen optie), dus de PSCI-provider zit per definitie ónder
//! ons (TF-A op Pi/O6N, QEMU met virtualization=on). Wat hier NIET woont:
//! de vertaling core-index naar MPIDR; die is per board (de nummering
//! verschilt per cluster).

use core::fmt;

/// CPU_ON (64-bit conventie).
pub const CPU_ON: u32 = 0xC400_0003;
/// SYSTEM_RESET (SMC32).
pub const SYSTEM_RESET: u32 = 0x8400_0009;
/// PSCI_FEATURES (SMC32): kent de firmware deze functie-ID?
pub const PSCI_FEATURES: u32 = 0x8400_000A;
/// SMCCC_VERSION (DEN 0028): de versie van de call-conventie zelf; alleen
/// te vragen als PSCI_FEATURES hem kent (Linux, `psci_init_smccc`).
pub const SMCCC_VERSION: u32 = 0x8000_0000;

/// Een PSCI-fout: de code die de firmware teruggaf.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// NOT_SUPPORTED (-1).
    NotSupported,
    /// INVALID_PARAMETERS (-2).
    InvalidParams,
    /// DENIED (-3).
    Denied,
    /// ALREADY_ON (-4): de core draait al.
    AlreadyOn,
    /// ON_PENDING (-5): een eerdere CPU_ON loopt nog.
    OnPending,
    /// INTERNAL_FAILURE (-6).
    InternalFailure,
    /// Een andere code.
    Other(i64),
}

impl Error {
    /// De fout bij een PSCI-returnwaarde; `None` bij succes (0).
    #[must_use]
    pub const fn from_ret(ret: u64) -> Option<Error> {
        match ret as i64 {
            0 => None,
            -1 => Some(Self::NotSupported),
            -2 => Some(Self::InvalidParams),
            -3 => Some(Self::Denied),
            -4 => Some(Self::AlreadyOn),
            -5 => Some(Self::OnPending),
            -6 => Some(Self::InternalFailure),
            other => Some(Self::Other(other)),
        }
    }

    /// De rauwe code.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::NotSupported => -1,
            Self::InvalidParams => -2,
            Self::Denied => -3,
            Self::AlreadyOn => -4,
            Self::OnPending => -5,
            Self::InternalFailure => -6,
            Self::Other(c) => c,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PSCI error {} ({:?})", self.code(), self)
    }
}

/// (major, minor) uit een versiewoord (PSCI_VERSION, TRNG_VERSION).
#[must_use]
pub const fn split_version(v: u64) -> (u16, u16) {
    ((v >> 16) as u16, v as u16)
}

/// Start een secundaire core. `target` is het MPIDR-target (al vertaald uit
/// de core-index door het board). De core begint op `entry` (fysiek adres,
/// MMU uit) met `ctx` in x0.
pub fn cpu_on(target: u64, entry: u64, ctx: u64) -> Result<(), Error> {
    match Error::from_ret(smc(CPU_ON, target, entry, ctx)) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Een SMC #0 met vier argumenten (SMCCC: x0..x3 in, resultaat in x0).
#[must_use]
pub fn smc(func: u32, a1: u64, a2: u64, a3: u64) -> u64 {
    arch::smc4(func, a1, a2, a3)[0]
}

/// Een SMC #0 die x0..x3 teruggeeft: voor SMCCC-functies die hun resultaat
/// over x1..x3 verspreiden, met name TRNG_RND64 (DEN 0098: x0 = status,
/// x1:x2:x3 = de entropie).
#[must_use]
pub fn smc4(func: u32, a1: u64, a2: u64, a3: u64) -> [u64; 4] {
    arch::smc4(func, a1, a2, a3)
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    pub(super) fn smc4(func: u32, a1: u64, a2: u64, a3: u64) -> [u64; 4] {
        let (r0, r1, r2, r3): (u64, u64, u64, u64);
        // SAFETY: een SMCCC-call naar de firmware onder ons. SMCCC 1.1+
        // bewaart x4..x17 en alles daarboven; x0..x3 zijn resultaat. HOP
        // draait zonder HCR_EL2.TSC, dus de SMC bereikt EL3 en trapt niet.
        // Geen `nomem`: CPU_ON publiceert geheugen aan een andere core.
        unsafe {
            core::arch::asm!(
                "smc #0",
                inout("x0") u64::from(func) => r0,
                inout("x1") a1 => r1,
                inout("x2") a2 => r2,
                inout("x3") a3 => r3,
                options(nostack),
            );
        }
        [r0, r1, r2, r3]
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod arch {
    //! Host-stub: er is geen firmware; elke call is NOT_SUPPORTED.
    pub(super) fn smc4(_func: u32, _a1: u64, _a2: u64, _a3: u64) -> [u64; 4] {
        [(-1i64) as u64, 0, 0, 0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_codes() {
        assert_eq!(Error::from_ret(0), None);
        assert_eq!(Error::from_ret((-2i64) as u64), Some(Error::InvalidParams));
        assert_eq!(Error::from_ret((-4i64) as u64), Some(Error::AlreadyOn));
        assert_eq!(Error::from_ret((-9i64) as u64).map(Error::code), Some(-9));
    }

    #[test]
    fn version_split() {
        assert_eq!(split_version(0x0001_0002), (1, 2));
        assert_eq!(split_version(0x0000_0001_0001_0000), (1, 0));
    }

    #[test]
    fn host_has_no_firmware() {
        assert_eq!(cpu_on(1, 0x4000_0000, 0), Err(Error::NotSupported));
    }
}
