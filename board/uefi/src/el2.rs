//! De vorm van het EL2-regime waarin de kern zelf draait: nVHE (E2H = 0)
//! of VHE (E2H = 1), gekozen door de feature `vhe` van dit crate.
//!
//! Dit bezit alleen getallen en twee losse instructies: de waarden van
//! HCR_EL2, SCTLR_EL2, TCR_EL2, CPTR_EL2 en CNTHCTL_EL2 die de ingang
//! (`crate::entry`, `crate::boot`) schrijft, en het herstel van de
//! timertoegang van EL1 na `cpu::idle::ArmSleeper::new`. Eén uitzondering:
//! HCR, SCTLR en CPTR schrijft de nVHE-tak van `crate::entry` als
//! letterlijke waarden (byte voor byte de ingang van vóór VHE); alleen de
//! VHE-tak laadt de constanten. De nVHE-constanten hieronder lezen dus
//! alleen de toetsen, die ze gelijk houden aan die letterlijke waarden. Niet van hier: welke switcher-smaak de
//! app-cores krijgen (`hopos/src/cage.rs` `FLAVOR`); die moet wel met deze
//! vorm overeenkomen, en de binary toetst dat bij het bouwen.
//!
//! Waarom de hele kern onder E2H = 1 als de switcher VHE is: de OS-core
//! draait zijn bewoners vanuit de kern (`cpu::el2::oscore`), met de
//! `_EL12`-encoderingen voor hun EL1-regime. Die bestaan alleen onder
//! E2H = 1 (onder E2H = 0 zijn ze UNDEFINED: EC 0x0, de rode VHE-run van
//! 29-09 op QEMU met neoverse-n1, ELR in `hopos_os_vhe_enter` + 0x40). En
//! E2H omzetten rond elke beurt is geen optie: de map van de kern (TCR,
//! SCTLR) heeft onder E2H = 1 een andere lay-out, dus de kern moet in één
//! vorm leven. Afgekeken van board/apple, dat VHE-only is (E2H RES1).
//!
//! De O6N kiest `vhe` altijd (board/o6n): op de Cortex-A720 stierf een EL1
//! onder nVHE binnen een halve seconde (Go, 17-09). QEMU virt kiest nVHE,
//! en `FEATURES=vhe CPU=neoverse-n1 sh tools/qemu-uefi-test.sh` bewijst de
//! VHE-vorm op QEMU (cortex-a57 kent geen VHE).
//!
//! `cfg` staat hier op module-niveau (handboek §7): de twee vormen zijn
//! twee modules met dezelfde namen.

pub(crate) use imp::*;

/// De bits van TCR_EL2 die in beide vormen gelijk zijn en op dezelfde plek
/// staan: T0SZ = 16 (48 bits VA), IRGN0 en ORGN0 = WB-WA, SH0 = inner, TG0
/// = 4 KB (0).
const TCR_COMMON: u64 = (3 << 12) | (1 << 10) | (1 << 8) | 16;

/// Zet de EL1-teller en -timer van de bewoners weer open na
/// `cpu::idle::ArmSleeper::new`: onder E2H = 1 schrijft `msr cntkctl_el1`
/// vanaf EL2 in werkelijkheid CNTHCTL_EL2 (de vondst van de Apple-agent,
/// board/apple/src/lib.rs `sleeper`), met EL1PCTEN en EL1PTEN (10, 11) op
/// nul, en dan trapt een `mrs cntpct_el0` van elke bewoner. Onder E2H = 0
/// raakt die write alleen CNTKCTL_EL1 en is dit niets.
pub(crate) fn el1_timer_access() {
    arch::el1_timer_access();
}

#[cfg(not(feature = "vhe"))]
mod imp {
    //! nVHE: E2H = 0, de EL2-vorm van de registers. Byte voor byte wat de
    //! ingang vóór de VHE-vorm schreef.
    use super::TCR_COMMON;

    /// Draait de kern onder E2H = 1?
    pub(crate) const VHE: bool = false;

    /// HCR_EL2 van de kern: RW (een lagere EL is AArch64), en IMO, FMO en
    /// AMO: fysieke IRQ, FIQ en SError komen naar EL2.
    #[cfg_attr(
        not(all(target_arch = "aarch64", target_os = "none")),
        allow(dead_code) // de toetsen lezen hem; entry.rs schrijft hem letterlijk
    )]
    pub(crate) const HCR: u64 = (1 << 31) | (7 << 3);

    /// SCTLR_EL2 (nVHE): de RES1-set 0x30c50830, plus M, C, I en SA; A,
    /// WXN en EE uit (geen uitlijncontrole, little-endian).
    #[cfg_attr(
        not(all(target_arch = "aarch64", target_os = "none")),
        allow(dead_code) // de toetsen lezen hem; entry.rs schrijft hem letterlijk
    )]
    pub(crate) const SCTLR: u64 = 0x30c5_0830 | 1 | (1 << 2) | (1 << 3) | (1 << 12);

    /// CPTR_EL2 zonder traps (nVHE): de RES1-bits met TFP = 0.
    #[cfg_attr(
        not(all(target_arch = "aarch64", target_os = "none")),
        allow(dead_code) // de toetsen lezen hem; entry.rs schrijft hem letterlijk
    )]
    pub(crate) const CPTR: u64 = 0x33ff;

    /// CNTHCTL_EL2 van de timertoegang van EL1: EL1PCTEN en EL1PCEN
    /// (bits 0 en 1 in de nVHE-lay-out).
    pub(crate) const CNTHCTL_EL1_ACCESS: u64 = 0b11;

    /// TCR_EL2 (nVHE) voor 48 bits: de gemeenschappelijke bits, PS op 16
    /// uit ID_AA64MMFR0_EL1.PARange (tot 48 bits: 52 bits vraagt een andere
    /// korrel), en de RES1-bits 23 en 31.
    pub(crate) const fn tcr(parange: u64) -> u64 {
        let ps = if parange > 5 { 5 } else { parange };
        (1 << 31) | (1 << 23) | (ps << 16) | TCR_COMMON
    }
}

#[cfg(feature = "vhe")]
mod imp {
    //! VHE: E2H = 1, de EL2&0-vorm van de registers (die van TCR_EL1 en
    //! SCTLR_EL1). De waarden en het waarom zijn die van board/apple
    //! (`mmu::tcr`, `mmu::SCTLR`, de head-stub), dat er op de M4 mee boot.
    use super::TCR_COMMON;

    /// Draait de kern onder E2H = 1?
    pub(crate) const VHE: bool = true;

    /// HCR_EL2 van de kern: die van nVHE plus E2H (bit 34). TGE blijft 0:
    /// met TGE = 1 draait er geen EL1, en de kern geeft de OS-core aan
    /// EL1-bewoners zonder HCR tussen twee vormen te wisselen.
    #[cfg_attr(
        not(all(target_arch = "aarch64", target_os = "none")),
        allow(dead_code) // alleen de ingang (entry.rs) gebruikt hem, en de tests
    )]
    pub(crate) const HCR: u64 = (1 << 34) | (1 << 31) | (7 << 3);

    /// SCTLR_EL2 onder E2H = 1 (de vorm van SCTLR_EL1): M, C, SA, I, plus
    /// de RES1-set van die vorm (EOS, TSCXT, EIS, SPAN, nTLSMD, LSMAOE) en
    /// nTWI, nTWE. Uitlijncontrole uit, little-endian; dezelfde waarde als
    /// board/apple `mmu::SCTLR`.
    #[cfg_attr(
        not(all(target_arch = "aarch64", target_os = "none")),
        allow(dead_code) // alleen de ingang (entry.rs) gebruikt hem, en de tests
    )]
    pub(crate) const SCTLR: u64 =
        0x30d0_0800 | (1 << 16) | (1 << 18) | 1 | (1 << 2) | (1 << 3) | (1 << 12);

    /// CPTR_EL2 zonder traps onder E2H = 1: de CPACR-vorm, FPEN = 0b11
    /// (bits 21:20). De nVHE-waarde 0x33ff liet FPEN op 0 en trapte dan
    /// FP ook op EL2 zelf. Dezelfde waarde als de switcher
    /// (`CPTR_NOTRAP_VHE`) en de OS-core (`oscore::arch::prepare`).
    #[cfg_attr(
        not(all(target_arch = "aarch64", target_os = "none")),
        allow(dead_code) // alleen de ingang (entry.rs) gebruikt hem, en de tests
    )]
    pub(crate) const CPTR: u64 = 0x30_0000;

    /// CNTHCTL_EL2 van de timertoegang van EL1: EL1PCTEN en EL1PTEN (bits
    /// 10 en 11 in de VHE-lay-out, `cpu/src/el2/switch.rs` `hopos_el2_drop`).
    /// Bits 0 en 1 zijn daar de EL0-toegang van het EL2&0-regime; zonder
    /// TGE draait daar niemand.
    pub(crate) const CNTHCTL_EL1_ACCESS: u64 = 0b11 << 10;

    /// TCR_EL2 onder E2H = 1 (de vorm van TCR_EL1): de gemeenschappelijke
    /// bits, EPD1 (bit 23: geen TTBR1-walks, er is geen hoge helft), TG1 =
    /// 4 KB (bits 31:30 = 0b10, een geldige waarde ook al is hij uit), en
    /// IPS op 32 uit PARange. De nVHE-vorm zou hier T1SZ en A1 zetten
    /// (board/apple/src/mmu.rs `tcr`).
    pub(crate) const fn tcr(parange: u64) -> u64 {
        let ips = if parange > 5 { 5 } else { parange };
        (ips << 32) | (2 << 30) | (1 << 23) | TCR_COMMON
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none", feature = "vhe"))]
mod arch {
    use core::arch::asm;

    pub(super) fn report() {
        const E2H: u64 = 1 << 34;
        let hcr: u64;
        // SAFETY: HCR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, hcr_el2", out(reg) hcr, options(nomem, nostack)) };
        if hcr & E2H != 0 {
            cpu::println!("uefi: kern under E2H=1 (VHE), HCR_EL2 {hcr:#x} HOPOS_UEFI_VHE");
        } else {
            cpu::println!(
                "FAIL uefi: E2H did not stick, HCR_EL2 {hcr:#x}: a VHE kern under E2H=0 HOPOS_UEFI_VHE_FAIL"
            );
        }
    }

    pub(super) fn el1_timer_access() {
        // SAFETY: alleen de EL1-bits van CNTHCTL_EL2 van deze core erbij;
        // de event-stream-bits die `ArmSleeper::new` net zette blijven
        // staan. Geen geheugeneffect.
        unsafe {
            asm!(
                "mrs {t}, cnthctl_el2",
                "orr {t}, {t}, {bits}",
                "msr cnthctl_el2, {t}",
                "isb",
                t = out(reg) _,
                bits = in(reg) super::CNTHCTL_EL1_ACCESS,
                options(nomem, nostack),
            );
        }
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none", feature = "vhe")))]
mod arch {
    //! nVHE, en de host: niets te herstellen en niets te melden.
    pub(super) fn el1_timer_access() {}
    pub(super) fn report() {}
}

/// Eén regel op de console met de vorm die de core werkelijk aannam (HCR_EL2
/// teruggelezen), alleen in de VHE-build: op de O6N is dat de eerste vraag
/// als er iets misgaat. Onder nVHE niets (de kale boot-console blijft zoals
/// hij was).
pub(crate) fn report() {
    arch::report();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcr_has_the_form_of_its_regime() {
        let t = tcr(5);
        // T0SZ 16, 4 KB-korrel, walks WB-WA inner shareable: in beide vormen.
        assert_eq!(t & 0xffff, 0x3510);
        if VHE {
            // IPS op 32, EPD1, TG1 = 4 KB; geen PS op 16 en geen RES1 op 31.
            assert_eq!(t, 0x5_8080_3510);
        } else {
            // PS op 16, de RES1-bits 23 en 31: de waarde van vóór VHE.
            assert_eq!(t, 0x8085_3510);
        }
        // PARange boven 48 bits wordt 48 bits.
        assert_eq!(tcr(6), tcr(5));
    }

    #[test]
    fn sctlr_and_cptr_have_the_form_of_their_regime() {
        // M, C, SA en I staan in beide vormen op dezelfde plek.
        assert_eq!(SCTLR & 0x100d, 0x100d);
        if VHE {
            assert_eq!(SCTLR, 0x30d5_180d);
            assert_eq!(CPTR, 0x30_0000);
        } else {
            assert_eq!(SCTLR, 0x30c5_183d);
            assert_eq!(CPTR, 0x33ff);
        }
    }

    #[test]
    fn hcr_carries_e2h_only_under_vhe() {
        assert_eq!(HCR & (1 << 34) != 0, VHE);
        assert_eq!(HCR & !(1 << 34), 0x8000_0038);
    }
}
