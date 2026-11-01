//! De CLINT: `msip` (de kick) en `mtimecmp` (de wekker) per hart, in de
//! SiFive-indeling die zowel QEMU virt als de c900-CLINT van de SG2002 volgt.
//!
//! Wat hier NIET is: `mtime`. De c900-CLINT heeft dat register niet (gemeten
//! 30-07: een lees op 0xbff8 is een bus-fout, mcause 5); de tijd komt uit de
//! TIME-CSR ([`super::csr::rdtime`]), op elk hart en op elk board.
//!
//! 32-BIT TOEGANG, ALTIJD. Dezelfde CLINT weigert 64-bit MMIO, dus
//! `mtimecmp` gaat als twee woorden in de volgorde van de privileged spec
//! (lo = alles-één, hi, lo): zo staat er tijdens het schrijven nooit een
//! tussenwaarde in het verleden die per ongeluk vuurt. QEMU accepteert
//! beide breedtes, dus één pad.

use dev::Pa;

/// De offset van `msip[0]`: 4 bytes per hart.
pub const MSIP_OFF: u64 = 0x0000;
/// De offset van `mtimecmp[0]`: 8 bytes per hart.
pub const MTIMECMP_OFF: u64 = 0x4000;
/// "Nooit": de ontwapende stand van een wekker.
pub const NEVER: u64 = u64::MAX;

/// Een CLINT-blok op een vaste basis.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Clint {
    base: Pa,
}

impl Clint {
    /// De CLINT op `base`.
    ///
    /// # Safety
    ///
    /// `base` is een CLINT in de SiFive-indeling, en blijft bereikbaar
    /// zolang het programma draait (machine mode: geen vertaling).
    #[must_use]
    pub const unsafe fn new(base: Pa) -> Self {
        Self { base }
    }

    /// De PA van `msip` van `hart`: het IPI-kanaal waarmee de kern een
    /// slapend hart wekt (de switcher kent hem via `SCHED_MSIP_PA`).
    #[must_use]
    pub const fn msip(&self, hart: usize) -> Pa {
        self.base.add(MSIP_OFF + 4 * hart as u64)
    }

    /// De PA van `mtimecmp` van `hart` (de switcher kent hem via
    /// `SCHED_CLINT_PA`).
    #[must_use]
    pub const fn mtimecmp(&self, hart: usize) -> Pa {
        self.base.add(MTIMECMP_OFF + 8 * hart as u64)
    }

    /// Zet de wekker van `hart` op tik `v`, in de spec-volgorde.
    pub fn set_timecmp(&self, hart: usize, v: u64) {
        let a = self.mtimecmp(hart);
        dev::write32(a, u32::MAX);
        dev::write32(a.add(4), (v >> 32) as u32);
        dev::write32(a, v as u32);
    }

    /// Leest de wekker van `hart` terug.
    #[must_use]
    pub fn timecmp(&self, hart: usize) -> u64 {
        let a = self.mtimecmp(hart);
        (u64::from(dev::read32(a.add(4))) << 32) | u64::from(dev::read32(a))
    }

    /// Zet of wist de `msip` van `hart`.
    pub fn set_msip(&self, hart: usize, on: bool) {
        dev::write32(self.msip(hart), u32::from(on));
    }

    /// De probe van het eigen hart, zoals de Go-kern hem deed (`ProbeCLINT`):
    /// houdt `mtimecmp` een waarde vast en leest hij hem terug? Het vuren
    /// zelf bewijst de eerste slaap (op de LicheeRV met de grens van
    /// [`super::idle::WFI_CAP_NS`], dus een wekker die niet vuurt is daar
    /// een trage node en geen dode).
    ///
    /// WAAROM EEN PROBE: op 30-07 bleek de helft van de SiFive-indeling op dit
    /// silicium niet te bestaan. Dat `mtimecmp` er wél is, was toen een
    /// aanname; een hart dat op een niet-bestaande wekker `wfi` doet, wordt
    /// nooit meer wakker, en dat faalt stil.
    pub fn probe(&self, hart: usize, now: u64) -> Result<(), ProbeError> {
        self.set_timecmp(hart, NEVER);
        let got = self.timecmp(hart);
        if got != NEVER {
            return Err(ProbeError::NotWritable { got });
        }
        let want = now.wrapping_add(1 << 40);
        self.set_timecmp(hart, want);
        let got = self.timecmp(hart);
        self.set_timecmp(hart, NEVER);
        if got != want {
            return Err(ProbeError::Readback { want, got });
        }
        Ok(())
    }
}

/// Waarom de CLINT-probe faalde.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// Alles-één geschreven, iets anders gelezen: geen register.
    NotWritable {
        /// Wat er terugkwam.
        got: u64,
    },
    /// Een waarde geschreven, een andere gelezen.
    Readback {
        /// Wat er geschreven werd.
        want: u64,
        /// Wat er terugkwam.
        got: u64,
    },
}

impl core::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::NotWritable { got } => {
                write!(f, "mtimecmp not writable (wrote all-ones, read {got:#x})")
            }
            Self::Readback { want, got } => {
                write!(f, "mtimecmp readback {got:#x}, wrote {want:#x}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_follow_the_sifive_layout() {
        // SAFETY: de host-test raakt het blok alleen via de adresrekening.
        let c = unsafe { Clint::new(Pa(0x7400_0000)) };
        assert_eq!(c.msip(1), Pa(0x7400_0004));
        assert_eq!(c.mtimecmp(1), Pa(0x7400_4008));
    }

    #[test]
    fn timecmp_is_written_in_spec_order_and_probes() {
        let mut block = vec![0u32; 0x5000 / 4];
        let base = Pa(block.as_mut_ptr() as u64);
        // SAFETY: `block` is een nep-CLINT in host-geheugen.
        let c = unsafe { Clint::new(base) };
        c.set_timecmp(1, 0x1234_5678_9abc_def0);
        assert_eq!(c.timecmp(1), 0x1234_5678_9abc_def0);
        assert!(c.probe(0, 5).is_ok());
        assert_eq!(c.timecmp(0), NEVER);
        c.set_msip(2, true);
        assert_eq!(block[2], 1);
    }
}
