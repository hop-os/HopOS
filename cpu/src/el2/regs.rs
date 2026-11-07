//! De registers van een bewoner naar en uit zijn ctx-blok, één keer
//! geschreven voor de twee overgangen op EL2: de switcher van een app-core
//! (`switch.rs`) en de beurt op de OS-core (`oscore.rs`). Tot 07-11 stonden
//! ze in beide, regel voor regel gelijk (docs/sequence-hopos.md §15.2 K2).
//!
//! Elk fragment is een stuk tekst voor een `global_asm!`, zoals
//! `cache_macros!` van de riscv-switcher: binnen de assembler-macro van een
//! smaak (`\op1` is daar 0 voor nVHE, 5 voor de _EL12-encoderingen onder
//! VHE), met x1 op het ctx-blok en x2/x3 als klad. De offsets zijn de
//! operanden `gprs`, `regime`, `ctx_sp` en `resume`, die elke `global_asm!`
//! aan `abi::layout` bindt (`CTX_GPRS`, `CTX_REGIME`, `CTX_SP`,
//! `CTX_RESUME`). De save is de spiegel van het herstel: dezelfde paren in
//! omgekeerde volgorde.

use super::layout::CTX_REGIME_ARM_WORDS;

// Het EL1-regime hieronder is precies negentien woorden.
const _: () = assert!(CTX_REGIME_ARM_WORDS == 19);

/// Bewaart de hervat-PC (ELR_EL2 en SPSR ernaast), SP_EL0/SP_EL1 en het
/// EL1-regime dat de volgende bewoner NIET mag erven. Onder VHE zijn de
/// EL1-encoderingen op EL2 omgeleid naar EL2's eigen registers; de echte
/// heten dan _EL12 (op1 = 5). Niet omgeleid: TPIDR*, PAR, CSSELR.
macro_rules! save_el1 {
    () => {
        r#"
    mrs x2, elr_el2
    mrs x3, spsr_el2
    stp x2, x3, [x1, #{resume}]
    mrs x2, sp_el0
    mrs x3, sp_el1
    stp x2, x3, [x1, #{ctx_sp}]
    mrs x2, s3_\op1\()_c1_c0_0
    mrs x3, s3_\op1\()_c2_c0_2
    stp x2, x3, [x1, #({regime} + 0 * 8)]
    mrs x2, s3_\op1\()_c2_c0_0
    mrs x3, s3_\op1\()_c2_c0_1
    stp x2, x3, [x1, #({regime} + 2 * 8)]
    mrs x2, s3_\op1\()_c10_c2_0
    mrs x3, s3_\op1\()_c10_c3_0
    stp x2, x3, [x1, #({regime} + 4 * 8)]
    mrs x2, s3_\op1\()_c12_c0_0
    mrs x3, tpidr_el0
    stp x2, x3, [x1, #({regime} + 6 * 8)]
    mrs x2, tpidrro_el0
    mrs x3, tpidr_el1
    stp x2, x3, [x1, #({regime} + 8 * 8)]
    mrs x2, s3_\op1\()_c13_c0_1
    mrs x3, s3_\op1\()_c1_c0_2
    stp x2, x3, [x1, #({regime} + 10 * 8)]
    mrs x2, s3_\op1\()_c14_c1_0
    mrs x3, csselr_el1
    stp x2, x3, [x1, #({regime} + 12 * 8)]
    mrs x2, par_el1
    mrs x3, s3_\op1\()_c4_c0_1
    stp x2, x3, [x1, #({regime} + 14 * 8)]
    mrs x2, s3_\op1\()_c4_c0_0
    mrs x3, s3_\op1\()_c5_c2_0
    stp x2, x3, [x1, #({regime} + 16 * 8)]
    mrs x2, s3_\op1\()_c6_c0_0
    str x2, [x1, #({regime} + 18 * 8)]
"#
    };
}

/// Het EL1-regime, SP_EL0/SP_EL1 en de hervat-PC terug: de spiegel van
/// [`save_el1`]. De ERET synchroniseert.
macro_rules! restore_el1 {
    () => {
        r#"
    ldp x2, x3, [x1, #({regime} + 0 * 8)]
    msr s3_\op1\()_c1_c0_0, x2
    msr s3_\op1\()_c2_c0_2, x3
    ldp x2, x3, [x1, #({regime} + 2 * 8)]
    msr s3_\op1\()_c2_c0_0, x2
    msr s3_\op1\()_c2_c0_1, x3
    ldp x2, x3, [x1, #({regime} + 4 * 8)]
    msr s3_\op1\()_c10_c2_0, x2
    msr s3_\op1\()_c10_c3_0, x3
    ldp x2, x3, [x1, #({regime} + 6 * 8)]
    msr s3_\op1\()_c12_c0_0, x2
    msr tpidr_el0, x3
    ldp x2, x3, [x1, #({regime} + 8 * 8)]
    msr tpidrro_el0, x2
    msr tpidr_el1, x3
    ldp x2, x3, [x1, #({regime} + 10 * 8)]
    msr s3_\op1\()_c13_c0_1, x2
    msr s3_\op1\()_c1_c0_2, x3
    ldp x2, x3, [x1, #({regime} + 12 * 8)]
    msr s3_\op1\()_c14_c1_0, x2
    msr csselr_el1, x3
    ldp x2, x3, [x1, #({regime} + 14 * 8)]
    msr par_el1, x2
    msr s3_\op1\()_c4_c0_1, x3
    ldp x2, x3, [x1, #({regime} + 16 * 8)]
    msr s3_\op1\()_c4_c0_0, x2
    msr s3_\op1\()_c5_c2_0, x3
    ldr x2, [x1, #({regime} + 18 * 8)]
    msr s3_\op1\()_c6_c0_0, x2
    ldp x2, x3, [x1, #{ctx_sp}]
    msr sp_el0, x2
    msr sp_el1, x3
    ldp x2, x3, [x1, #{resume}]
    msr elr_el2, x2
    msr spsr_el2, x3
"#
    };
}

/// Bewaart x4..x30. x0..x3 zijn op dat moment klad of staan op de stack:
/// die bewaart de aanroeper zelf, elk op zijn eigen manier.
macro_rules! save_gprs {
    () => {
        r#"
    stp x4, x5, [x1, #({gprs} + 4 * 8)]
    stp x6, x7, [x1, #({gprs} + 6 * 8)]
    stp x8, x9, [x1, #({gprs} + 8 * 8)]
    stp x10, x11, [x1, #({gprs} + 10 * 8)]
    stp x12, x13, [x1, #({gprs} + 12 * 8)]
    stp x14, x15, [x1, #({gprs} + 14 * 8)]
    stp x16, x17, [x1, #({gprs} + 16 * 8)]
    stp x18, x19, [x1, #({gprs} + 18 * 8)]
    stp x20, x21, [x1, #({gprs} + 20 * 8)]
    stp x22, x23, [x1, #({gprs} + 22 * 8)]
    stp x24, x25, [x1, #({gprs} + 24 * 8)]
    stp x26, x27, [x1, #({gprs} + 26 * 8)]
    stp x28, x29, [x1, #({gprs} + 28 * 8)]
    str x30, [x1, #({gprs} + 30 * 8)]
"#
    };
}

/// Alle GPR's terug, x1 (de ctx-pointer zelf) helemaal aan het eind, en
/// de bewoner in: het einde van elke overgang naar EL1.
macro_rules! restore_gprs_eret {
    () => {
        r#"
    ldp x4, x5, [x1, #({gprs} + 4 * 8)]
    ldp x6, x7, [x1, #({gprs} + 6 * 8)]
    ldp x8, x9, [x1, #({gprs} + 8 * 8)]
    ldp x10, x11, [x1, #({gprs} + 10 * 8)]
    ldp x12, x13, [x1, #({gprs} + 12 * 8)]
    ldp x14, x15, [x1, #({gprs} + 14 * 8)]
    ldp x16, x17, [x1, #({gprs} + 16 * 8)]
    ldp x18, x19, [x1, #({gprs} + 18 * 8)]
    ldp x20, x21, [x1, #({gprs} + 20 * 8)]
    ldp x22, x23, [x1, #({gprs} + 22 * 8)]
    ldp x24, x25, [x1, #({gprs} + 24 * 8)]
    ldp x26, x27, [x1, #({gprs} + 26 * 8)]
    ldp x28, x29, [x1, #({gprs} + 28 * 8)]
    ldr x30, [x1, #({gprs} + 30 * 8)]
    ldp x2, x3, [x1, #({gprs} + 2 * 8)]
    ldr x0, [x1, #{gprs}]
    ldr x1, [x1, #({gprs} + 1 * 8)]
    isb
    eret
"#
    };
}
