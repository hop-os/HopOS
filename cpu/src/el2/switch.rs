//! De EL2-assembly van de app-cores: de switcher, de twee trampolines en de
//! parkeerlus, als `global_asm!`.
//!
//! Dit bestand is de vertaling van `OLD/metal/cpu/el2` (switch_body.h,
//! el2_body.h, smp_body.h, drop.h, hygiene.h, sysreg.h) en van de parkeerlus
//! die `kern/stage2.InitVectors` als hexwoorden schreef. De code draait
//! nooit uit het kern-image: [`super::install_switch_code`] kopieert de drie
//! blobs naar de plan-regio (`SWITCH_CODE_OFF`) en de parkeerlus naar
//! `PARK_CODE_OFF`, zodat een kern-flip zijn eigen venster kan verlaten
//! terwijl geyielde en geparkeerde cores doordraaien (docs/kern-flip.md).
//! Alles is daarom positie-onafhankelijk: relatieve sprongen binnen een
//! blob, geen literal pools, en elk adres komt uit een register, het
//! sched-blok (via SP) of TPIDR_EL2.
//!
//! Elk getal dat een offset is, komt als `const`-operand uit `layout` of
//! `hopabi`; er staat hier geen literal offset. Architecturale waarden
//! (VTCR, HCR-bits, SCTLR-reset) staan als benoemde constante erboven.
//!
//! Drie smaken uit één bron, zoals `el2_nvhe.s`, `el2_o6n.s` en
//! `el2_apple.s` in Go (Derek, 20-09: "het board bepaalt wat erin wordt
//! gesteld"): de assembler-macro `hopos_el2_flavor` wordt drie keer
//! geïnstantieerd, en het board kiest bij de install welke kopie de
//! plan-regio in gaat ([`Flavor`]). Waar Go `-D VHE` en `-D APPLE_IPI`
//! had, heeft de macro de parameters `vhe`, `op1` en `apple`.

use super::Error;
use super::dispatch::{
    Flavor, HVC_DOOR_ACK, HVC_EXIT, HVC_KICK_OS, HVC_WAKE, MAX_BLOB, SCRATCH_X2, VEC_FIQ_LOWER,
    VEC_SYNC_LOWER,
};
use super::layout::{
    CAGE_STRIDE, CTX_BOOT_ARG, CTX_BOOT_PC, CTX_CTRL_PA, CTX_GPRS, CTX_KICK_PENDING,
    CTX_KICK_TARGET, CTX_NEXT_PA, CTX_OFF, CTX_REGIME, CTX_REGIME_ARM_WORDS, CTX_RESUME,
    CTX_RING_HEAD_PA, CTX_SLEEPS, CTX_SP, CTX_STATE, CTX_UNIT_SLOT, CTX_WAKE, CTX_WAKE_NO_PEEK,
    CTX_WAKES, CtxState, PARK_CODE_OFF, PARK_MBOX_OFF, PARK_PARKED, SCHED_COUNT, SCHED_CURRENT,
    SCHED_CURSOR, SCHED_LIST, SCHED_MBOX_CTX, SCHED_MBOX_PC, SCHED_S2_PA, SCHED_SCRATCH, SLOT_CAP,
    SMP_CTX_OFF,
};
use super::oscore::{
    SCHED_OS_KICK, SCHED_OS_KICK_PA, SCTLR_EL1_CLEAN, SPSR_EL1H_MASKED, apple_kick_target,
};
use abi::hopabi::{
    CTRL_DOOR_IRQ, CTRL_ENTRY, CTRL_FAULT_ESR, CTRL_FAULT_FAR, CTRL_FAULT_VEC, CTRL_MBOX_PA,
    CTRL_RX_DOOR, CTRL_S2_TABLE, CTRL_SLOT, CTRL_SMP_FN, CTRL_SMP_G0, CTRL_SMP_MAIR, CTRL_SMP_MBOX,
    CTRL_SMP_MP, CTRL_SMP_SP, CTRL_SMP_STUB, CTRL_SMP_TCR, CTRL_SMP_TTBR0, CTRL_SMP_VBAR,
    CTRL_VEC_PA, RX_DOOR_ARMED,
};
use core::arch::{asm, global_asm};

// ---------------------------------------------------------------------------
// De getallen die de assembly krijgt.
// ---------------------------------------------------------------------------

/// SP_EL2 wijst op de scratch van het sched-blok (mailbox + SCHED_SCRATCH,
/// door de trampolines gezet), dus elk sched-veld is SP-relatief op zijn
/// offset min de scratch. Dezelfde getallen als de literals in switch.s
/// (208, 32, 64, 72, 80); de asserties houden ze gelijk.
const SP_CURRENT: u64 = SCHED_CURRENT - SCHED_SCRATCH;
const SP_S2_PA: u64 = SCHED_S2_PA - SCHED_SCRATCH;
const SP_CURSOR: u64 = SCHED_CURSOR - SCHED_SCRATCH;
const SP_COUNT: u64 = SCHED_COUNT - SCHED_SCRATCH;
const SP_LIST: u64 = SCHED_LIST - SCHED_SCRATCH;
const _: () = assert!(SP_CURRENT == 32 && SP_S2_PA == 208);
const _: () = assert!(SP_CURSOR == 64 && SP_COUNT == 72 && SP_LIST == 80);

// De scratch zelf: +0 x0, +8 x1 (el2entry), +16 x2, +24 x3 (de thunk). Vier
// woorden, en die passen vóór SCHED_CURRENT.
const _: () = assert!(SCRATCH_X2 + 16 <= SCHED_CURRENT - SCHED_SCRATCH);

/// Kooi-blokken liggen op een macht van twee: de context-index schuift erin.
const CAGE_SHIFT: u32 = CAGE_STRIDE.trailing_zeros();
const _: () = assert!(1u64 << CAGE_SHIFT == CAGE_STRIDE);

/// De VMID staat in VTTBR_EL2 op bit 48.
const VMID_SHIFT: u32 = 48;

/// VTCR_EL2 zonder PS-veld: 4 KB-granule, 39-bit IPA (T0SZ=25, SL0=1),
/// inner/outer write-back, inner shareable, RES1 op bit 31.
const VTCR_NO_PS: u64 = 0x8000_3559;
/// PS-veld van VTCR_EL2.
const VTCR_PS_SHIFT: u32 = 16;
/// De klem op PARange: 4 = 44 bit (16 TB).
const PARANGE_44: u64 = 4;

/// HCR_EL2.RW (EL1 is AArch64), E2H (VHE), TSC (trap SMC), FMO (FIQ naar
/// EL2), VM (stage-2 aan), VF (virtuele FIQ pending).
const HCR_RW: u64 = 1 << 31;
const HCR_E2H: u64 = 1 << 34;
const HCR_TSC: u64 = 1 << 19;
const HCR_FMO: u64 = 1 << 3;
const HCR_VM: u64 = 1 << 0;
const HCR_VF: u64 = 1 << 6;

// SCTLR_EL1 bij de drop (`SCTLR_EL1_CLEAN`: de RES1-bits, M/C/I/A/WXN uit,
// NOOIT erven) en de SPSR ervan (`SPSR_EL1H_MASKED`: EL1h, DAIF dicht)
// komen uit `oscore`, dat een eerste beurt op de OS-core net zo zet.
/// CPTR_EL2 zonder FP-trap. nVHE: de RES1-bits met TFP=0. VHE: de
/// CPACR-vorm, FPEN=0b11.
const CPTR_NOTRAP_NVHE: u64 = 0x33FF;
const CPTR_NOTRAP_VHE: u64 = 0x30_0000;
/// Het kick-woord van de OS-core, gezien vanaf de kooi-basis: sched-blok 0
/// (logische core 0 is de OS-core), veld [`SCHED_OS_KICK`].
const OS_KICK_OFF: u64 = PARK_MBOX_OFF + SCHED_OS_KICK;
/// Het adres van GICD_SGIR ernaast (GICv2), veld [`SCHED_OS_KICK_PA`].
const OS_KICK_PA_OFF: u64 = PARK_MBOX_OFF + SCHED_OS_KICK_PA;
const _: () = assert!(OS_KICK_OFF.is_multiple_of(8) && OS_KICK_OFF < 32760);
const _: () = assert!(OS_KICK_PA_OFF.is_multiple_of(8) && OS_KICK_PA_OFF < 32760);

/// Het masker dat van het Apple-kick-woord in sched-blok 0 het
/// IPI_RR-doel maakt (het scherp-bit eraf, `oscore::apple_kick_word`).
const APPLE_KICK_MASK: u64 = apple_kick_target(u64::MAX);
const _: () = assert!(APPLE_KICK_MASK == !(1 << 63));

/// De MPIDR-affiniteit (aff0..aff2) waarmee een sibling een core aanwijst.
const MPIDR_AFF: u64 = 0xFF_FFFF;

/// Bit 63 van de wektijd en van de doorbell.
const WAKE_NO_PEEK_BIT: u32 = CTX_WAKE_NO_PEEK.trailing_zeros();
const RX_ARMED_BIT: u32 = RX_DOOR_ARMED.trailing_zeros();
const _: () = assert!(WAKE_NO_PEEK_BIT == 63 && RX_ARMED_BIT == 63);

// De exit-HVC wordt met een `cbz` herkend.
const _: () = assert!(HVC_EXIT == 0);
// De regime-save hieronder schrijft precies negentien woorden.
const _: () = assert!(CTX_REGIME_ARM_WORDS == 19);

// ---------------------------------------------------------------------------
// De assembly.
// ---------------------------------------------------------------------------

global_asm!(
    r#"
    .pushsection .text.hopos_el2, "ax"

// ===========================================================================
// hopos_el2_ctx_of: het ctx-blok van een context-id (was vijf keer inline in
// switch.s). Id's 1..SLOT_CAP zijn kooi-contexten op CTX_OFF van hun blok;
// id's erboven zijn secundaire cores (id - (SLOT_CAP-1)) op SMP_CTX_OFF. In:
// id in het eerste register, de kooi-basis in het tweede. Uit: het tweede
// register is het ctx-blok, het eerste is geklobberd.
// ===========================================================================
    .macro hopos_el2_ctx_of id, pa
    cmp \id, #{slot_cap}
    b.ls 8f
    sub \id, \id, #({slot_cap} - 1)
    add \pa, \pa, #({smp_ctx_off} - {ctx_off})
8:
    add \pa, \pa, \id, lsl #{cage_shift}
    add \pa, \pa, #{ctx_off}
    .endm

// ===========================================================================
// hopos_el2_i_hygiene (hygiene.h): de I-cache van DEZE core leeg vóór een
// sprong in vers geschreven code. Een PIPT-I$ houdt de instructies van de
// vorige huurder van dezelfde PA's vast; de D-veeg van de plaatser raakt de
// I-zijde nooit. Twee keer op ijzer betaald: Altra 15-07 (elke warme
// herdispatch dood bij boot) en de M4-kern-flip 01-09 (vier flashes lang
// dood vóór de eerste instructie). QEMU-TCG heeft geen I$-model en verhult
// dit. Lokaal (IALLU): de gebruiker is de core die er zelf in springt.
// ===========================================================================
    .macro hopos_el2_i_hygiene
    ic iallu
    dsb sy
    isb
    .endm

// ===========================================================================
// hopos_el2_drop (drop.h, DROP_TO_EL1): de ENE drop van EL2 naar EL1. Acht
// plekken deden dit elk anders en groeiden uit elkaar op precies de punten
// die pijn deden: SCTLR_EL1 op een bekende waarde (de Pi 5-les van 10-07:
// een warme CPU_ON erfde EL1 van de vorige huurder; de flip-fix van 02-09),
// CNTHCTL in BEIDE lay-outs (bits 0/1 onder E2H=0, 10/11 onder E2H=1),
// CPTR_EL2 zonder FP-trap. HCR_EL2 is van de aanroeper; x0..x7 overleven de
// ERET (zo geeft de SMP-trampoline zijn context door); klad is x16.
// ===========================================================================
    .macro hopos_el2_drop entry, vhe, op1
    movz x16, #({sctlr_clean} & 0xffff)
    movk x16, #({sctlr_clean} >> 16), lsl #16
    msr s3_\op1\()_c1_c0_0, x16
    .if \vhe
    mov x16, #{cptr_vhe}
    .else
    mov x16, #{cptr_nvhe}
    .endif
    msr cptr_el2, x16
    mrs x16, cnthctl_el2
    orr x16, x16, #0x3
    orr x16, x16, #(0x3 << 10)
    msr cnthctl_el2, x16
    mov x16, #0
    msr cntvoff_el2, x16
    mov x16, #{spsr_el1h}
    msr spsr_el2, x16
    msr elr_el2, \entry
    isb
    eret
    .endm

// VTCR_EL2 (el2.s): 4 KB-granule, 39-bit IPA, PS = min(PARange, 44 bit). De
// pool ligt op servers ver boven de oude 40-bit-aanname (Altra 15-07: met
// PS=40 stierf elke loader op een address-size-fault bij zijn eerste
// instructie uit een hoge partitie). Klemmen is verplicht: PS boven PARange
// is constrained unpredictable (de A72/A76 van de Pi's melden 40 bit).
    .macro hopos_el2_vtcr
    mrs x5, id_aa64mmfr0_el1
    and x5, x5, #0xf
    cmp x5, #{parange_44}
    b.lt 7f
    mov x5, #{parange_44}
7:
    movz x4, #({vtcr} & 0xffff)
    movk x4, #({vtcr} >> 16), lsl #16
    orr x4, x4, x5, lsl #{vtcr_ps}
    msr vtcr_el2, x4
    .endm

// HCR_EL2 van een gekooide core: RW (+E2H onder VHE) | TSC | FMO | VM. TSC:
// een app-SMC bestaat niet, dus elke SMC is een ontsnappingspoging en landt
// als fault. GEEN TWE: de wissel is een expliciete HVC-yield, want een
// getrapte WFE is op QEMU-TCG een no-op (onbewijsbaar) en op ijzer een
// heisenbug. GEEN IMO: de hard-kill is stage-2-intrekking, geen IRQ. FMO:
// een FIQ landt op EL2, nooit in de app; op Apple is dat de kick.
    .macro hopos_el2_hcr_cage vhe
    mov x4, #{hcr_rw}
    .if \vhe
    orr x4, x4, #{hcr_e2h}
    .endif
    orr x4, x4, #{hcr_tsc}
    orr x4, x4, #{hcr_fmo}
    orr x4, x4, #{hcr_vm}
    msr hcr_el2, x4
    .endm

// hopos_el2_kick_os: de bel naar de OS-core (PORT.md beslissing 2). Staat in
// sched-blok 0 een ICC_SGI1R-waarde, dan hoort de kern op dit moment geen
// SEV (hij draait een bewoner, of slaapt in WFI) en krijgt hij een SGI. Nul
// = niets doen, en dan raakt deze code ook geen GIC-register (Apple, of een
// board zonder kick). Staat er ook een GICD_SGIR-adres (GICv2, de GIC-400
// van de Pi's), dan is de kick een 32-bit MMIO-schrijf daarheen; met de
// MMU uit is dat Device-geheugen. Anders ICC_SGI1R (GICv3), met SRE op EL2
// eerst: na PSCI CPU_ON is ICC_SRE_EL2 van de firmware, en zonder SRE is
// ICC_SGI1R ongedefinieerd (QEMU: RAO). Klad: x2, x3.
    .macro hopos_el2_kick_os
    ldr x2, [sp, #{sp_s2pa}]
    ldr x3, [x2, #{os_kick}]
    cbz x3, 6f
    ldr x2, [x2, #{os_kick_pa}]
    cbz x2, 5f
    str w3, [x2]
    dsb sy
    b 6f
5:
    mrs x2, s3_4_c12_c9_5
    orr x2, x2, #1
    msr s3_4_c12_c9_5, x2
    isb
    msr s3_0_c12_c11_5, x3
    isb
6:
    .endm

// hopos_el2_kick_os_apple: dezelfde bel op Apple, waar geen GIC is: de fast
// IPI (IPI_RR_GLOBAL_EL1) naar de OS-core. Het woord in sched-blok 0 is het
// doel (core | cluster << 16) met bit 63 als "scherp" (het doel van E-core
// 0 is 0); het masker haalt dat bit eraf (oscore `apple_kick_word`). De
// kern slaapt in WFI of draait een bewoner met FMO: de FIQ haalt hem in
// beide gevallen terug, en hij ackt hem zelf (les 04-09). Klad: x2, x3.
    .macro hopos_el2_kick_os_apple
    ldr x2, [sp, #{sp_s2pa}]
    ldr x3, [x2, #{os_kick}]
    cbz x3, 6f
    and x3, x3, #{apple_kick_mask}
    msr s3_5_c15_c0_1, x3
    isb
6:
    .endm

// hopos_el2_mmu_off: een core die m1n1 uit zijn spin-table loslaat, komt
// binnen als FUNCTIE, op EL2 met m1n1's MMU en caches nog aan (29-08, de
// hop-avond). De trampolines hieronder schrijven VBAR, VTTBR en HCR en
// lezen de control-page als fysiek adres: dat mag alleen met de MMU uit.
// Dus eerst maskers dicht en M, C en I uit, zonder cache-onderhoud op
// set/way (de machine reset erop terwijl een andere core loopt, 29-08). Op
// een core uit de parkeerlus of de brievenbus staat alles al uit, en is dit
// niets. Klad: x9.
    .macro hopos_el2_mmu_off
    msr daifset, #0xf
    mrs x9, sctlr_el2
    bic x9, x9, #(1 << 0)
    bic x9, x9, #(1 << 2)
    bic x9, x9, #(1 << 12)
    dsb sy
    msr sctlr_el2, x9
    isb
    .endm

// ===========================================================================
// De drie blobs van één smaak.
// ===========================================================================
    .macro hopos_el2_flavor p, vhe, op1, apple

// ---------------------------------------------------------------------------
// el2entry (switch_body.h): de coöperatieve core-deling. De thunk op de
// vectortabel zette x2/x3 op de scratch, x2 = vectorindex, x3 = geklobberd.
// Hier eerst x0/x1 erbij; daarna zijn x0..x3 werkregisters.
// ---------------------------------------------------------------------------
    .balign 64
    .global \p\()_entry
\p\()_entry:
    stp x0, x1, [sp]
    .if \apple
    // Idx 10 (FIQ uit EL1): op Apple HOP's kick, de fast IPI. Acken en terug;
    // de app ziet alleen zijn WFI terugkeren.
    cmp x2, #{vec_fiq}
    b.eq .L\p\()_fiq
    .endif
    // Alleen idx 8 (synchroon uit EL1) draagt een bruikbare ESR; elke andere
    // vector is per definitie een fault-rapport.
    cmp x2, #{vec_sync}
    b.ne .L\p\()_fault
    mrs x0, esr_el2
    lsr x1, x0, #26
    cmp x1, #{ec_hvc}
    b.ne .L\p\()_fault
    // De HVC-immediate kiest: 0 exit, 4 wek een sibling, 5 doorbell-ack
    // (Apple), 6 bel de OS-core; al het andere is de idle-yield (1).
    and x3, x0, #0xffff
    cbz x3, .L\p\()_exited
    cmp x3, #{hvc_wake}
    b.eq .L\p\()_wake
    cmp x3, #{hvc_kick_os}
    b.eq .L\p\()_kickos
    .if \apple
    cmp x3, #{hvc_door_ack}
    b.eq .L\p\()_doorack
    .endif

// yield: HVC 1. De bewoner staat in SCHED_CURRENT, NIET in de VMID: een
// secundaire core deelt de VMID van zijn primaire en schreef zo in diens
// ctx-blok, waarna zijn eigen blok op Running bleef en de core parkeerde
// (QEMU + M4, 03-09).
.L\p\()_yield:
    ldr x0, [sp, #{sp_current}]
    ldr x1, [sp, #{sp_s2pa}]
    hopos_el2_ctx_of x0, x1
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
    ldp x2, x3, [sp]
    stp x2, x3, [x1, #{gprs}]
    // x1 van de bewoner is de WEKTIJD: vóór deze CNTVCT-stand hoeft de
    // rotatie hem niet te hervatten (0 = nu).
    str x3, [x1, #{wake}]
    // Kwam er een sibling-wek (HVC 4) vóór of tijdens deze yield, dan is de
    // wektijd nu; anders overschreef de yield die wek (lost wakeup, 04-09).
    // DSB: de eigen wektijd vóór de lees van de latch, zodat we óf de latch
    // zien, óf de 0 van de wekker ná onze T landt.
    dsb sy
    ldr x2, [x1, #{kick_pending}]
    cbz x2, .L\p\()_nokick
    str xzr, [x1, #{kick_pending}]
    str xzr, [x1, #{wake}]
.L\p\()_nokick:
    ldp x2, x3, [sp, #{scratch_x2}]
    stp x2, x3, [x1, #({gprs} + 2 * 8)]
    // Wie hier slaapt: de affiniteit van deze core, zodat een sibling hem
    // met HVC 4 kan aanwijzen.
    mrs x2, mpidr_el1
    and x2, x2, #{mpidr_aff}
    str x2, [x1, #{kick_target}]
    // Hervat-PC = ELR_EL2 (wijst bij een HVC al voorbij de hvc), SPSR ernaast.
    mrs x2, elr_el2
    mrs x3, spsr_el2
    stp x2, x3, [x1, #{resume}]
    mrs x2, sp_el0
    mrs x3, sp_el1
    stp x2, x3, [x1, #{ctx_sp}]
    // Het EL1-regime dat de volgende bewoner NIET mag erven. Onder VHE zijn
    // de EL1-encoderingen op EL2 omgeleid naar EL2's eigen registers; de
    // echte heten dan _EL12 (op1 = 5). Niet omgeleid: TPIDR*, PAR, CSSELR.
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
    // GEEN FP: EL2 draait met de MMU uit, dus alles is Device-nGnRnE, en een
    // SIMD-store naar Device is op ijzer een alignment-fault (QEMU verhult
    // het). De yield-aanroeper bewaart zijn eigen callee-saved FP-staat.
    // Staat Saved; DSB want HOP polt dit woord.
    mov x2, #{st_saved}
    str x2, [x1, #{state}]
    dsb sy
    // Een app die idle gaat, wacht meestal op de kern (een antwoord op wat
    // hij net publiceerde): bel de OS-core als die een SEV niet hoort.
    .if \apple
    hopos_el2_kick_os_apple
    .else
    hopos_el2_kick_os
    .endif
    b .L\p\()_sleep

// exited: HVC 0, de coöperatieve exit. Dead en meteen roteren, zonder slaap:
// er is net een plek vrij en een boot-pending buur mag direct. De kick zegt
// het de kern: zijn servicer leest de laatste woorden en ziet de dode
// context zonder te pollen (docs/sequence-hopos.md §13.3).
.L\p\()_exited:
    ldr x0, [sp, #{sp_current}]
    ldr x1, [sp, #{sp_s2pa}]
    hopos_el2_ctx_of x0, x1
    mov x2, #{st_dead}
    str x2, [x1, #{state}]
    dsb sy
    .if \apple
    hopos_el2_kick_os_apple
    .else
    hopos_el2_kick_os
    .endif
    b .L\p\()_rotate

// fault: elke andere vector, een stage-2-fault, een getrapte SMC, of HOP's
// revoke. Het rapport op de eigen control-page (vec+1, ESR, FAR); de dode
// context houdt zijn fault-PC in zijn resume-woord. Daarna draait de rest
// van de core gewoon door. x2 is nog de vectorindex.
.L\p\()_fault:
    ldr x0, [sp, #{sp_current}]
    ldr x1, [sp, #{sp_s2pa}]
    hopos_el2_ctx_of x0, x1
    mrs x0, elr_el2
    str x0, [x1, #{resume}]
    ldr x3, [x1, #{ctrl_pa}]
    add x2, x2, #1
    str x2, [x3, #{c_fault_vec}]
    mrs x2, esr_el2
    str x2, [x3, #{c_fault_esr}]
    mrs x2, far_el2
    str x2, [x3, #{c_fault_far}]
    mov x2, #{st_dead}
    str x2, [x1, #{state}]
    dsb sy
    .if \apple
    hopos_el2_kick_os_apple
    .else
    hopos_el2_kick_os
    .endif
    b .L\p\()_rotate

// sleep: de idle-slaap van de core, één WFE op EL2. De event stream van de
// geyielde app loopt door, dus dit wekt ~elke ms, en een al gearriveerd
// event valt er meteen doorheen: geen verloren wek. Ook als iedereen leeft
// maar niemand due is komt de rotatie hier: de event stream is dan de
// wekker. Tellen op de bewoner in x1: tientallen per seconde is slaap,
// miljoenen is spin.
.L\p\()_sleep:
    ldr x0, [x1, #{sleeps}]
    add x0, x0, #1
    str x0, [x1, #{sleeps}]
    .if \apple
    // Apple: WFE slaapt hier niet (CYC_OVRD, buiten bereik op de M4) en de
    // event stream wekt niets. m1n1's recept: wfi, en de fast IPI van HOP's
    // wekker. De pending IPI wissen NA de wek, anders keert de volgende WFI
    // meteen terug.
    wfi
    mrs x0, s3_5_c15_c1_1
    cbz x0, .L\p\()_rotate
    msr s3_5_c15_c1_1, x0
    .else
    wfe
    .endif

// rotate: round-robin vanaf cursor+1 over de bewonerslijst: de eerste met
// BootPending, of Saved en aan de beurt. x11 onthoudt of er iemand leefde
// die alleen nog niet due was.
.L\p\()_rotate:
    mov x11, #0
    ldr x4, [sp, #{sp_cursor}]
    ldr x5, [sp, #{sp_count}]
    cbz x5, .L\p\()_park
    mov x6, x5
.L\p\()_next:
    add x4, x4, #1
    cmp x4, x5
    b.lt .L\p\()_scan
    mov x4, #0
.L\p\()_scan:
    add x7, sp, #{sp_list}
    ldrb w8, [x7, x4]
    cbz x8, .L\p\()_skip
    ldr x1, [sp, #{sp_s2pa}]
    mov x9, x8
    hopos_el2_ctx_of x9, x1
    ldr x9, [x1, #{state}]
    // De hercontrole (30-09, sharegroups op app-cores): staat de byte er na
    // de staat nog? De kern haalt een slot eerst uit elke lijst en schrijft
    // pas daarna zijn nieuwe staat (`dispatch::forget`, `join`); wie hier de
    // nieuwe staat ziet, ziet dus ook dat de byte weg is. Zonder dit startte
    // een oude core een slot dat intussen op een andere core boot-pending
    // stond, als hij tussen byte en staat werd opgehouden.
    dmb sy
    ldrb w10, [x7, x4]
    cmp x10, x8
    b.ne .L\p\()_skip
    cmp x9, #{st_boot_pending}
    b.eq .L\p\()_boot
    cmp x9, #{st_saved}
    b.ne .L\p\()_skip
    // Saved, maar al aan de beurt? Vóór zijn wektijd hervatten is nooit fout
    // (hij kijkt en yieldt opnieuw); erna laten liggen wel.
    ldr x9, [x1, #{wake}]
    cbz x9, .L\p\()_resume
    // Bit 63: een wachter zonder P, alleen zijn wektijd telt, de doorbell
    // niet (QEMU 03-09: anders keerde een semafoor-wachter meteen terug).
    tbz x9, #{no_peek_bit}, .L\p\()_timed
    and x9, x9, #0x7fffffffffffffff
    cbz x9, .L\p\()_resume
    mrs x10, cntvct_el0
    cmp x10, x9
    b.hs .L\p\()_resume
    b .L\p\()_notdue
.L\p\()_timed:
    mrs x10, cntvct_el0
    cmp x10, x9
    b.hs .L\p\()_resume
    // De RX-peek: de bewoner wapent zijn doorbell vlak vóór zijn slaap
    // (gezien-kop | bit 63). Is het live kopwoord van zijn RX-ring voorbij
    // die drempel, dan kwam er verkeer. Ongewapend peekt er niets: een kooi
    // die zijn ring nooit leest blijft zo uit de resume-lus (ARP-floods).
    ldr x12, [x1, #{ctrl_pa}]
    ldr x12, [x12, #{c_rx_door}]
    tbz x12, #{rx_armed_bit}, .L\p\()_notdue
    ldr x13, [x1, #{ring_head}]
    cbz x13, .L\p\()_notdue
    ldr x13, [x13]
    and x12, x12, #0x7fffffffffffffff
    // Voorbij is groter-dan, niet ongelijk: de kop uit DRAM mag achterlopen
    // op wat de bewoner in zijn cache zag (T30/T31, 04-09: ongelijk-is-due
    // wekte spookachtig en liet schrijven van 690 naar 30 MB/s zakken). De
    // kop is monotoon, dus unsigned.
    cmp x13, x12
    b.hi .L\p\()_resume
.L\p\()_notdue:
    mov x11, #1
.L\p\()_skip:
    subs x6, x6, #1
    b.ne .L\p\()_next
    cbz x11, .L\p\()_park
    b .L\p\()_sleep

// boot: koude start van een boot-pending bewoner, exact het mailbox-pad maar
// EL2 naar EL2. Running vóór de sprong (HOP's poll leest het).
.L\p\()_boot:
    str x4, [sp, #{sp_cursor}]
    str x8, [sp, #{sp_current}]
    mov x9, #{st_running}
    str x9, [x1, #{state}]
    dsb sy
    ldr x0, [x1, #{boot_arg}]
    ldr x2, [x1, #{boot_pc}]
    br x2

// resume: de volgende bewoner terug. VTTBR naar zijn EENHEID (CTX_UNIT_SLOT):
// een secundaire deelt tabel en VMID met zijn primaire. GEEN TLBI: de
// entries zijn VMID-getagd, de vertalingen van beide bewoners bestaan naast
// elkaar, en dat maakt de wissel goedkoop.
.L\p\()_resume:
    str x4, [sp, #{sp_cursor}]
    str x8, [sp, #{sp_current}]
    mov x9, #{st_running}
    str x9, [x1, #{state}]
    str xzr, [x1, #{kick_pending}]
    dsb sy
    ldr x9, [x1, #{unit_slot}]
    ldr x2, [sp, #{sp_s2pa}]
    add x2, x2, x9, lsl #{cage_shift}
    orr x2, x2, x9, lsl #{vmid_shift}
    msr vttbr_el2, x2
    // Het EL1-regime terug, spiegel van de save; de ERET synchroniseert.
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
    // GPR's als laatste, x1 (de ctx-pointer zelf) helemaal aan het eind.
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

// park: niemand meer te draaien. De parkeerlus (kooi-basis + PARK_CODE_OFF)
// meldt zich via TPIDR_EL2 als geparkeerd en wacht op de mailbox.
.L\p\()_park:
    ldr x2, [sp, #{sp_s2pa}]
    add x2, x2, #{park_code}
    br x2

    .if \apple
// fiq: Apple's fast IPI (m1n1 smp.c): pending in IPI_SR_EL1 bit 0, wissen
// door terug te schrijven. Geen IPI = een onbekende FIQ, dus een rapport.
// Heeft de app zich voor de doorbell als interrupt aangemeld
// (CTRL_DOOR_IRQ), dan een virtuele FIQ (HCR_EL2.VF): ook als de core druk
// is met GC komt de pomp dan aan de beurt (gemeten 04-09: anders 1 ms per
// call op de poll-timer).
.L\p\()_fiq:
    mrs x0, s3_5_c15_c1_1
    cbz x0, .L\p\()_fault
    msr s3_5_c15_c1_1, x0
    ldr x1, [sp, #{sp_current}]
    ldr x2, [sp, #{sp_s2pa}]
    hopos_el2_ctx_of x1, x2
    ldr x3, [x2, #{ctrl_pa}]
    cbz x3, .L\p\()_fiqdone
    ldr x3, [x3, #{c_door_irq}]
    cbz x3, .L\p\()_fiqdone
    mrs x0, hcr_el2
    orr x0, x0, #{hcr_vf}
    msr hcr_el2, x0
.L\p\()_fiqdone:
    ldp x0, x1, [sp]
    ldp x2, x3, [sp, #{scratch_x2}]
    isb
    eret

// doorack: HVC 5, de app handelde zijn doorbell af; de virtuele FIQ weer weg
// (level: anders vuurt hij opnieuw zodra EL1 F opent).
.L\p\()_doorack:
    mrs x0, hcr_el2
    bic x0, x0, #{hcr_vf}
    msr hcr_el2, x0
    ldp x0, x1, [sp]
    ldp x2, x3, [sp, #{scratch_x2}]
    isb
    eret
    .endif

// kickos: HVC 6, de expliciete bel naar de OS-core; meteen terug.
.L\p\()_kickos:
    .if \apple
    hopos_el2_kick_os_apple
    .else
    hopos_el2_kick_os
    .endif
    ldp x0, x1, [sp]
    ldp x2, x3, [sp, #{scratch_x2}]
    isb
    eret

// wake: HVC 4, de reschedule-IPI van Linux in HopOS-vorm. x0 = de affiniteit
// van de sibling. Zoek langs de vertrouwde circulaire keten (CTX_NEXT_PA,
// door de kern gezet) de context met dezelfde control-page (de eenheid, niet
// iets wat de app opgeeft) en dit wekdoel. Eerst de latch, dan de wektijd,
// met een DSB ertussen (review 05-09): de yield ziet óf de latch, óf onze 0
// landt na zijn T. Alleen x0..x3 zijn klad; x4/x5 gaan even in het GPR-vak
// van de eigen ctx, dat dood is tot de volgende yield.
.L\p\()_wake:
    ldr x1, [sp, #{sp_current}]
    ldr x2, [sp, #{sp_s2pa}]
    hopos_el2_ctx_of x1, x2
    stp x4, x5, [x2, #({gprs} + 4 * 8)]
    ldr x0, [sp]
    ldr x1, [x2, #{ctrl_pa}]
    mov x4, x2
.L\p\()_wakescan:
    ldr x5, [x4, #{ctrl_pa}]
    cmp x5, x1
    b.ne .L\p\()_wakenext
    ldr x5, [x4, #{kick_target}]
    cmp x5, x0
    b.ne .L\p\()_wakenext
    mov x5, #1
    str x5, [x4, #{kick_pending}]
    dsb sy
    str xzr, [x4, #{wake}]
    ldr x5, [x4, #{wakes}]
    add x5, x5, #1
    str x5, [x4, #{wakes}]
    dsb sy
    .if \apple
    // Op Apple ook meteen de fast IPI: core | cluster << 16 uit aff0/aff1.
    and x5, x0, #0xff
    lsr x4, x0, #8
    and x4, x4, #0xff
    orr x5, x5, x4, lsl #16
    msr s3_5_c15_c0_1, x5
    .endif
    b .L\p\()_wakedone
.L\p\()_wakenext:
    ldr x4, [x4, #{next_pa}]
    cbz x4, .L\p\()_wakedone
    cmp x4, x2
    b.ne .L\p\()_wakescan
.L\p\()_wakedone:
    ldp x4, x5, [x2, #({gprs} + 4 * 8)]
    ldp x0, x1, [sp]
    ldp x2, x3, [sp, #{scratch_x2}]
    isb
    eret
    .global \p\()_entry_end
\p\()_entry_end:

// ---------------------------------------------------------------------------
// s2tramp (el2_body.h): de ingang van een app-core onder stage-2. x0 = de
// fysieke control-page; alles is data-gedreven, geen adres-defines. De app
// draait hierdoor nooit op EL2.
// ---------------------------------------------------------------------------
    .balign 64
    .global \p\()_tramp
\p\()_tramp:
    .if \apple
    hopos_el2_mmu_off
    .endif
    // Vectoren eerst: elke exception hierna rapporteert.
    ldr x1, [x0, #{c_vec_pa}]
    msr vbar_el2, x1
    ldr x2, [x0, #{c_s2_table}]
    ldr x3, [x0, #{c_entry}]
    ldr x6, [x0, #{c_slot}]
    // TPIDR_EL2 = de eigen mailbox: de parkeerlus vindt hem zonder MPIDR.
    ldr x7, [x0, #{c_mbox_pa}]
    msr tpidr_el2, x7
    // SP_EL2 = de sched-scratch: thunks en switcher parkeren daar registers.
    add x8, x7, #{sched_scratch}
    mov sp, x8
    // VPIDR/VMPIDR: bij EL2-entry UNKNOWN (op de M4 las een app 0 voor zijn
    // tweede core, 03-09); zo zegt een app die zijn core meldt de waarheid.
    mrs x4, midr_el1
    msr vpidr_el2, x4
    mrs x4, mpidr_el1
    msr vmpidr_el2, x4
    hopos_el2_vtcr
    // VTTBR = L1 | VMID(slot) << 48; ISB vóór de TLBI zodat hij deze VMID
    // raakt.
    lsl x5, x6, #{vmid_shift}
    orr x5, x5, x2
    msr vttbr_el2, x5
    isb
    tlbi vmalls12e1
    dsb sy
    hopos_el2_hcr_cage \vhe
    // Warme herdispatch: de I$ houdt nog de code van de vorige huurder op
    // exact deze canonieke adressen (Altra 15-07).
    hopos_el2_i_hygiene
    hopos_el2_drop x3, \vhe, \op1
    .global \p\()_tramp_end
\p\()_tramp_end:

// ---------------------------------------------------------------------------
// smpEL2Tramp (smp_body.h): een extra core in een AL draaiende app-runtime,
// of een node-core van de kern zelf. x0 = een node-owned handoff
// (prepare_smp): app-geheugen wordt hier niet gelezen. De context voor de
// EL1-stub van de app gaat in x0..x7 door de ERET heen.
// ---------------------------------------------------------------------------
    .balign 64
    .global \p\()_smp
\p\()_smp:
    .if \apple
    hopos_el2_mmu_off
    .endif
    mov x1, x0
    ldr x10, [x1, #{c_smp_sp}]
    ldr x11, [x1, #{c_smp_mp}]
    ldr x12, [x1, #{c_smp_g0}]
    ldr x13, [x1, #{c_smp_fn}]
    ldr x14, [x1, #{c_smp_ttbr0}]
    ldr x15, [x1, #{c_smp_stub}]
    ldr x2, [x1, #{c_s2_table}]
    ldr x6, [x1, #{c_slot}]
    ldr x7, [x1, #{c_smp_mbox}]
    msr tpidr_el2, x7
    add x8, x7, #{sched_scratch}
    mov sp, x8
    ldr x3, [x1, #{c_vec_pa}]
    msr vbar_el2, x3
    mrs x4, midr_el1
    msr vpidr_el2, x4
    mrs x4, mpidr_el1
    msr vmpidr_el2, x4
    // Het kooi-profiel op de vertrouwde tabel: 0 = node-core (geen kooi),
    // anders een app-core met stage-2 (Derek: "bijna hergebruiken = een
    // gedeelde functie").
    cmp x2, #0
    b.eq .L\p\()_s2none
    hopos_el2_vtcr
    lsl x5, x6, #{vmid_shift}
    orr x5, x5, x2
    msr vttbr_el2, x5
    isb
    tlbi vmalls12e1
    dsb sy
    // FMO ook hier: zonder stond hij alleen op de eerste core van een app, en
    // kreeg de tweede de fast IPI als EL1-exception (M4, 02-09).
    hopos_el2_hcr_cage \vhe
    b .L\p\()_s2done
.L\p\()_s2none:
    // Node-core: VM=0, SMC niet getrapt. VTTBR = 0, want ook bij VM=0 tagt
    // het silicium TLB-entries op VTTBR.VMID en een geërfde VMID liet deze
    // core stale vertalingen houden na een map-wijziging van de node.
    mov x4, #0
    msr vttbr_el2, x4
    isb
    tlbi vmalls12e1
    dsb sy
    mov x4, #{hcr_rw}
    .if \vhe
    orr x4, x4, #{hcr_e2h}
    .endif
    msr hcr_el2, x4
.L\p\()_s2done:
    hopos_el2_i_hygiene
    // Het geërfde EL1-regime van de dispatchende primaire, voor de stub.
    ldr x5, [x1, #{c_smp_mair}]
    ldr x6, [x1, #{c_smp_tcr}]
    ldr x7, [x1, #{c_smp_vbar}]
    mov x0, x10
    mov x1, x11
    mov x2, x12
    mov x3, x13
    mov x4, x14
    hopos_el2_drop x15, \vhe, \op1
    .global \p\()_smp_end
\p\()_smp_end:
    .endm

// ===========================================================================
// De drie smaken. nVHE: QEMU, de Pi's, RK3566, Altra. VHE: de O6N (17-09:
// nVHE-EL1 stierf er binnen 0,5 s). VHE met de fast IPI: Apple (E2H staat
// vast op 1, gemeten M4 28-08).
// ===========================================================================
    hopos_el2_flavor hopos_el2_nvhe, 0, 0, 0
    hopos_el2_flavor hopos_el2_vhe, 1, 5, 0
    hopos_el2_flavor hopos_el2_apple, 1, 5, 1

// ===========================================================================
// De parkeerlus (stage2.InitVectors, daar als hexwoorden). TPIDR_EL2 wijst op
// de eigen mailbox. Meld "geparkeerd", wek HOP, WFE tot HOP een ctx schrijft,
// en spring dan de trampoline in (die is idempotent). Board-neutraal: geen
// MPIDR-decodering. Het protocol is stabiel over een flip: een core kan hier
// staan zonder bewoners, dus een lege flip laat deze bytes staan.
// ===========================================================================
    .balign 64
    .global hopos_el2_park
hopos_el2_park:
    mrs x8, tpidr_el2
    mov x9, #{park_parked}
    str x9, [x8, #{mbox_ctx}]
    dsb sy
    sev
1:
    wfe
    ldr x0, [x8, #{mbox_ctx}]
    cmp x0, #{park_parked}
    b.eq 1b
    dmb sy
    ldr x1, [x8, #{mbox_pc}]
    br x1
    .global hopos_el2_park_end
hopos_el2_park_end:

    .popsection
"#,
    slot_cap = const SLOT_CAP,
    ctx_off = const CTX_OFF,
    smp_ctx_off = const SMP_CTX_OFF,
    cage_shift = const CAGE_SHIFT,
    vmid_shift = const VMID_SHIFT,
    sctlr_clean = const SCTLR_EL1_CLEAN,
    cptr_vhe = const CPTR_NOTRAP_VHE,
    cptr_nvhe = const CPTR_NOTRAP_NVHE,
    spsr_el1h = const SPSR_EL1H_MASKED,
    parange_44 = const PARANGE_44,
    vtcr = const VTCR_NO_PS,
    vtcr_ps = const VTCR_PS_SHIFT,
    hcr_rw = const HCR_RW,
    hcr_e2h = const HCR_E2H,
    hcr_tsc = const HCR_TSC,
    hcr_fmo = const HCR_FMO,
    hcr_vm = const HCR_VM,
    hcr_vf = const HCR_VF,
    vec_fiq = const VEC_FIQ_LOWER,
    vec_sync = const VEC_SYNC_LOWER,
    ec_hvc = const crate::vectors::EC_HVC64,
    hvc_wake = const HVC_WAKE,
    hvc_kick_os = const HVC_KICK_OS,
    os_kick = const OS_KICK_OFF,
    os_kick_pa = const OS_KICK_PA_OFF,
    apple_kick_mask = const APPLE_KICK_MASK,
    hvc_door_ack = const HVC_DOOR_ACK,
    sp_current = const SP_CURRENT,
    sp_s2pa = const SP_S2_PA,
    sp_cursor = const SP_CURSOR,
    sp_count = const SP_COUNT,
    sp_list = const SP_LIST,
    scratch_x2 = const SCRATCH_X2,
    sched_scratch = const SCHED_SCRATCH,
    gprs = const CTX_GPRS,
    regime = const CTX_REGIME,
    wake = const CTX_WAKE,
    kick_pending = const CTX_KICK_PENDING,
    kick_target = const CTX_KICK_TARGET,
    mpidr_aff = const MPIDR_AFF,
    resume = const CTX_RESUME,
    ctx_sp = const CTX_SP,
    state = const CTX_STATE,
    ctrl_pa = const CTX_CTRL_PA,
    sleeps = const CTX_SLEEPS,
    ring_head = const CTX_RING_HEAD_PA,
    unit_slot = const CTX_UNIT_SLOT,
    boot_arg = const CTX_BOOT_ARG,
    boot_pc = const CTX_BOOT_PC,
    wakes = const CTX_WAKES,
    next_pa = const CTX_NEXT_PA,
    no_peek_bit = const WAKE_NO_PEEK_BIT,
    rx_armed_bit = const RX_ARMED_BIT,
    st_saved = const CtxState::Saved.raw(),
    st_dead = const CtxState::Dead.raw(),
    st_running = const CtxState::Running.raw(),
    st_boot_pending = const CtxState::BootPending.raw(),
    park_code = const PARK_CODE_OFF,
    park_parked = const PARK_PARKED,
    mbox_ctx = const SCHED_MBOX_CTX,
    mbox_pc = const SCHED_MBOX_PC,
    c_fault_vec = const CTRL_FAULT_VEC,
    c_fault_esr = const CTRL_FAULT_ESR,
    c_fault_far = const CTRL_FAULT_FAR,
    c_rx_door = const CTRL_RX_DOOR,
    c_door_irq = const CTRL_DOOR_IRQ,
    c_vec_pa = const CTRL_VEC_PA,
    c_s2_table = const CTRL_S2_TABLE,
    c_entry = const CTRL_ENTRY,
    c_slot = const CTRL_SLOT,
    c_mbox_pa = const CTRL_MBOX_PA,
    c_smp_sp = const CTRL_SMP_SP,
    c_smp_mp = const CTRL_SMP_MP,
    c_smp_g0 = const CTRL_SMP_G0,
    c_smp_fn = const CTRL_SMP_FN,
    c_smp_ttbr0 = const CTRL_SMP_TTBR0,
    c_smp_stub = const CTRL_SMP_STUB,
    c_smp_mbox = const CTRL_SMP_MBOX,
    c_smp_mair = const CTRL_SMP_MAIR,
    c_smp_tcr = const CTRL_SMP_TCR,
    c_smp_vbar = const CTRL_SMP_VBAR,
);

// ---------------------------------------------------------------------------
// De Rust-kant: de blobs als byte-reeksen, en drie losse instructies.
// ---------------------------------------------------------------------------

/// Eén blob tussen zijn begin- en eindmarker. Een onmogelijke maat betekent
/// dat de linker ze uit elkaar trok of een marker verschoof, en dat hoort
/// hard te vallen, niet stil megabytes te kopiëren (blobs.go
/// `MaxBlobSize`).
fn span(index: usize, start: *const u8, end: *const u8) -> Result<&'static [u8], Error> {
    let len = (end as usize).wrapping_sub(start as usize);
    if len == 0 || len > MAX_BLOB {
        return Err(Error::Blob { index, len });
    }
    // SAFETY: `start` en `end` zijn twee labels in dezelfde `global_asm!`
    // hierboven, in één sectie, met `end` erachter (net getoetst): de bytes
    // ertussen zijn code in de eigen `.text`, leesbaar, `'static` en nooit
    // beschreven.
    Ok(unsafe { core::slice::from_raw_parts(start, len) })
}

macro_rules! blobs {
    ($($i:literal: $start:ident .. $end:ident),+) => {{
        unsafe extern "C" {
            $(
                safe static $start: u8;
                safe static $end: u8;
            )+
        }
        [$(span($i, &raw const $start, &raw const $end)),+]
    }};
}

/// De drie blobs van een smaak, in kopieervolgorde (blobs.go
/// `BlobSymbols`): el2entry, s2tramp, smpEL2Tramp. De install, de som en de
/// adoptie lopen hier allemaal over; twee keer extraheren is twee kansen om
/// het net anders te doen.
pub(super) fn blobs(flavor: Flavor) -> Result<[&'static [u8]; 3], Error> {
    let [e, t, s] = match flavor {
        Flavor::Nvhe => blobs!(
            0: hopos_el2_nvhe_entry..hopos_el2_nvhe_entry_end,
            1: hopos_el2_nvhe_tramp..hopos_el2_nvhe_tramp_end,
            2: hopos_el2_nvhe_smp..hopos_el2_nvhe_smp_end
        ),
        Flavor::Vhe => blobs!(
            0: hopos_el2_vhe_entry..hopos_el2_vhe_entry_end,
            1: hopos_el2_vhe_tramp..hopos_el2_vhe_tramp_end,
            2: hopos_el2_vhe_smp..hopos_el2_vhe_smp_end
        ),
        Flavor::AppleVhe => blobs!(
            0: hopos_el2_apple_entry..hopos_el2_apple_entry_end,
            1: hopos_el2_apple_tramp..hopos_el2_apple_tramp_end,
            2: hopos_el2_apple_smp..hopos_el2_apple_smp_end
        ),
    };
    Ok([e?, t?, s?])
}

/// De parkeerlus als byte-reeks.
pub(super) fn park_code() -> Result<&'static [u8], Error> {
    let [p] = blobs!(3: hopos_el2_park..hopos_el2_park_end);
    p
}

/// Maakt vers gekopieerde code zichtbaar voor elke core: I-cache-invalidatie
/// over het inner-shareable domein (publish_arm64.s). De aanroeper veegde de
/// D-kant al naar PoC. Een trampoline die tijdens een lege flip verhuist,
/// kan zichzelf niet beschermen met zijn eigen lokale invalidatie: wat al
/// gefetcht was vóór die instructie blijft staan.
pub(super) fn publish_code() {
    // SAFETY: cache-onderhoud en barrières; geen geheugeneffect buiten de
    // I-cache.
    unsafe {
        asm!(
            "ic ialluis",
            "dsb sy",
            "isb",
            options(nostack, preserves_flags)
        )
    };
}

/// De fast IPI van Apple (IPI_RR_GLOBAL_EL1): `v` = core | cluster << 16. In
/// Go was dit HVC #3 naar de revoke-handler; de kern van v3 staat zelf op
/// EL2 en schrijft het register ter plekke.
pub(super) fn apple_ipi(v: u64) {
    // SAFETY: het register stuurt alleen een IPI; op een Apple-core bestaat
    // het (de aanroeper kiest dit pad alleen voor `Flavor::AppleVhe`).
    unsafe { asm!("msr s3_5_c15_c0_1, {}", "isb", in(reg) v, options(nostack, preserves_flags)) };
}
