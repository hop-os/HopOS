//! De M-mode-switcher op een app-hart: de RISC-V-tegenhanger van
//! `cpu::el2::switch`, en bedoeld om ernaast gelezen te worden (Go:
//! `cpu/mmode/switch.s`, 810 regels, hier de kern ervan).
//!
//! Een app-hart draait deze code in machine mode met `mtvec` = [`entry_pc`]
//! en `mscratch` = zijn sched-blok (kern-eigen geheugen, buiten elke kooi).
//! Drie ingangen, één sched-blok:
//!
//! - **parkenter** (`a1` = sched-blok): de boot-intrek van een geparkeerd hart
//!   ([`super::boot::start_hart`]); daarna draait de rotatie hier voorgoed.
//! - **mentry**: elke trap van een bewoner. `csrrw sp, mscratch, sp` is het
//!   enige dat aan een bruikbare pointer komt (RISC-V heeft geen vrij
//!   register bij de trap, ARM heeft SP_EL2).
//! - **rotate**: round-robin over de bewonerslijst vanaf de rotor, de eerste
//!   die boot-pending is of saved en aan de beurt (zijn wektijd verstreken).
//!   Niemand: parkeren met `wfi` op de eigen `mtimecmp` (de vroegste wektijd,
//!   geklemd op `SCHED_SLEEP_CAP`), gewekt door de kick (`msip`).
//!
//! Het wisselmoment is een EXPLICIETE yield, net als op ARM: `ecall` uit
//! S-mode met a7 = 0 en de wektijd in a0 ("hervat me niet vóór deze tik";
//! zonder dat getal pingpongen twee lege apps op volle snelheid, gemeten
//! 31-07: allebei 36% van het hart). a7 = 1 is exit. a7 = 2 is de kick (de
//! tegenhanger van HVC #6, sinds 02-10): een 1 op de bel van de OS-core
//! (`SCHED_OS_BELL`, 0 = geen) en meteen terug, zonder beurtwissel, zodat
//! de kern een frame op de TX-ring nu leest en niet op zijn failsafe van
//! 1 ms. Al het andere is een fault: mcause en mtval naar de control-page,
//! de bewoner dood.
//!
//! Drie dingen die anders zijn dan op ARM, alle drie architectuur:
//!
//! 1. `mepc + 4` bij een yield: mepc wijst naar de `ecall` zelf.
//! 2. De kooi is `satp` plus `pmpcfg0`/`pmpaddr0..7` (het regime,
//!    `CTX_REGIME`); bij een koude boot schrijft de switcher ze uit het
//!    ctx-blok dat de kern bouwde en leest `pmpcfg0` terug: een kooi die niet
//!    aantoonbaar staat, wordt niet betreden (de bewoner gaat dood).
//! 3. Cache-onderhoud tussen de kern en dit hart: op de C906 zijn de harts
//!    niet coherent, dus de regels die de kern schrijft (lijst, staat) worden
//!    vóór het lezen geïnvalideerd, en wat de kern leest (staat) na het
//!    schrijven weggeveegd (`th.dcache.cipa`, met de feature `thead`). Op
//!    QEMU zijn de macro's leeg.
//!
//! Vier dingen van de Go-switcher die er sinds 29-09 ook hier zijn:
//!
//! - **De kill-tick** (`SCHED_TICK_TICKS`, 0 = geen): vóór elke sprong naar
//!   een bewoner de eigen comparator op nu + de periode en alleen MTIE aan.
//!   Een tick kijkt naar één woord (`CTX_REVOKE`) en gaat bij niet-nul naar
//!   de teardown. Met één bewoner in de lijst gaat hij terug dezelfde
//!   bewoner in; met meer is de tick ook de **tijdschijf** (sinds 02-10):
//!   de context gaat weg zoals bij een yield, met wektijd 0, en de rotatie
//!   beslist wie nu mag. Een bewoner die nooit yieldt, houdt het hart dus
//!   hooguit één tick (10 ms) van zijn buren af. Dit is de enige plek in
//!   HopOS met preemptie, en alleen op een gedeeld hart; de LicheeRV droeg
//!   vier apps op zijn ene app-hart, en een CASE of TLS-handshake van
//!   seconden hield de rest zonder beurt.
//! - **De intrekking van een slaper**: een bewoner die geyield is of nog
//!   BootPending staat en ingetrokken wordt, gaat bij de volgende ronde
//!   dood zonder nog één instructie te draaien.
//! - **De kick als wek** (`msip`): wie door de kick wakker werd, geeft elke
//!   geyielde bewoner één beurt, en **de deurbel** (`CTRL_RX_DOOR` tegen de
//!   kop van de RX-ring, `CTX_RING_HEAD_PA`) maakt een slaper met RX meteen
//!   due, zoals `el2::rx_due` op ARM.
//! - **De hercontrole van de lijst** bij een koude boot, na het lezen van
//!   de staat: een slot dat naar een ander hart verhuisde, start niet op
//!   twee.
//!
//! De FP-registers (f0..f31 en `fcsr`, `CTX_FPRS` achter het ctx-blok) gaan
//! mee bij elke yield en elke tijdschijf en komen terug bij het hervatten;
//! een koude boot begint met nullen, zodat een bewoner niets van zijn
//! voorganger in de f-registers vindt.

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
use abi::layout::{
    CTX_BOOT_ARG, CTX_BOOT_PC, CTX_CTRL_PA, CTX_FPRS, CTX_GPRS, CTX_OFF, CTX_REGIME, CTX_RESUME,
    CTX_REVOKE, CTX_RING_HEAD_PA, CTX_STATE, CTX_WAKE, CTX_WAKE_NO_PEEK, CtxState, SCHED_CLINT_PA,
    SCHED_COUNT, SCHED_CURRENT, SCHED_LIST, SCHED_MSIP_PA, SCHED_OS_BELL, SCHED_ROTOR, SCHED_S2_PA,
    SCHED_SCRATCH, SCHED_SLEEP_CAP, SCHED_TICK_TICKS,
};

/// De verschuiving van slot naar kooi-blok (`CAGE_STRIDE` = 64 KB).
pub const CAGE_SHIFT: u32 = 16;
const _: () = assert!(1 << CAGE_SHIFT == abi::layout::CAGE_STRIDE);

/// De regime-woorden in `CTX_REGIME`, in de volgorde van de switcher:
/// `satp`, `stvec`, `sscratch`, `pmpcfg0`, `pmpaddr0..7`.
pub const REGIME_SATP: u64 = 0;
/// `stvec`.
pub const REGIME_STVEC: u64 = 8;
/// `sscratch`.
pub const REGIME_SSCRATCH: u64 = 16;
/// `pmpcfg0`.
pub const REGIME_PMPCFG0: u64 = 24;
/// `pmpaddr0`; `pmpaddr k` op `+ 8k`.
pub const REGIME_PMPADDR0: u64 = 32;
const _: () = assert!(REGIME_PMPADDR0 + 8 * 8 == 8 * abi::layout::CTX_REGIME_RV_WORDS);

/// Waarde voor [`abi::hopabi::CTRL_FAULT_VEC`] bij een kooi die niet stond
/// (`pmpcfg0` las anders terug dan de kern schreef): de bewoner is nooit
/// binnengelaten.
pub const FAULT_CAGE_VERIFY: u64 = 0xFA11;

/// Het fysieke adres van de trap-ingang (`mentry`).
#[must_use]
pub fn entry_pc() -> u64 {
    imp::entry()
}

/// Het fysieke adres van `parkenter`: de ingang voor
/// [`super::boot::start_hart`], met het sched-blok als argument.
#[must_use]
pub fn park_pc() -> u64 {
    imp::park()
}

/// De periode van de kill-tick: 10 ms, het getal van de Go-kern
/// (`killTickTicks`, board/licheerv/hop/hart.go). De tick zelf is een
/// handvol instructies; de periode begrenst hoe lang een intrekking van een
/// bewoner die niet yieldt onderweg is.
pub const KILL_TICK_NS: u64 = 10_000_000;

/// Wat de kooi per app-hart van het board weet: de wekker, de bel, de
/// slaap- en tickperiode, de attributen van de CPU en of het hart een
/// resetblok heeft. Het board vult hem (`app_hart`); de kooi-lijm
/// (`hopos/src/cage_riscv.rs`) schrijft hem in het sched-blok.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AppHart {
    /// `mtimecmp` van het hart zoals HET hart hem adresseert (0 = geen
    /// wekker: de switcher spint en er is geen kill-tick). Op de SG2002 is
    /// de CLINT per core: elk hart ziet zijn eigen comparator op index 0.
    pub mtimecmp: dev::Pa,
    /// `msip` van het hart zoals de KERN hem adresseert (0 = geen bel: de
    /// kern kan het hart niet wekken, en een dispatch wacht op de volgende
    /// ronde van de switcher).
    pub msip: dev::Pa,
    /// De bel naar de OS-core zoals HET hart hem adresseert: waar de kick
    /// van een bewoner (`ecall`, a7 = 2) een 1 schrijft (0 = geen bel: de
    /// app kickt dan niet, `abi::hopabi::IDLE_KICK`).
    pub kick: dev::Pa,
    /// De langste slaap in tikken (0 = niet slapen: spinnen).
    pub sleep_cap: u64,
    /// De kill-tick in tikken (0 = geen).
    pub tick: u64,
    /// De PTE-attributen voor normaal RAM.
    pub attrs: super::sv39::Attrs,
    /// De PMP van de CPU.
    pub pmp: super::pmp::Profile,
    /// Heeft het hart een resetblok (de harde intrekking)?
    pub resettable: bool,
}

/// Het bereik `[begin, einde)` van de switch-code in het image: wat een
/// app-hart van de kern uitvoert.
#[must_use]
pub fn code_range() -> (u64, u64) {
    imp::range()
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
mod imp {
    pub(super) fn range() -> (u64, u64) {
        unsafe extern "C" {
            /// Het einde van de switch-code (hieronder).
            static __hopos_mmode_end: u8;
        }
        (park(), (&raw const __hopos_mmode_end) as u64)
    }
    pub(super) fn entry() -> u64 {
        unsafe extern "C" {
            /// De trap-ingang (hieronder).
            static __hopos_mentry: u8;
        }
        (&raw const __hopos_mentry) as u64
    }
    pub(super) fn park() -> u64 {
        unsafe extern "C" {
            /// De parkeer-ingang (hieronder).
            static __hopos_parkenter: u8;
        }
        (&raw const __hopos_parkenter) as u64
    }
}

#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
mod imp {
    //! Host-stub: er is geen switcher.
    pub(super) fn range() -> (u64, u64) {
        (0, 0)
    }
    pub(super) fn entry() -> u64 {
        0
    }
    pub(super) fn park() -> u64 {
        0
    }
}

// Het cache-onderhoud van de switcher als assembler-macro's: met `thead`
// `th.dcache.cipa` (clean + invalidate op PA, rs2 = 11 in het custom-0-veld)
// en `th.sync.is`; zonder (QEMU, coherent) niets.
#[cfg(all(feature = "thead", target_arch = "riscv64", target_os = "none"))]
macro_rules! cache_macros {
    () => {
        r#"
    .macro HOPOS_RV_CIPA reg
    .insn r 0x0b, 0, 1, x0, \reg, x11
    .endm
    .macro HOPOS_RV_CPA reg
    .insn r 0x0b, 0, 1, x0, \reg, x9
    .endm
    .macro HOPOS_RV_SYNC
    .4byte 0x01b0000b
    .endm
    .macro HOPOS_RV_CIALL
    .4byte 0x0030000b
    .4byte 0x01b0000b
    .endm
"#
    };
}

#[cfg(all(not(feature = "thead"), target_arch = "riscv64", target_os = "none"))]
macro_rules! cache_macros {
    () => {
        r#"
    .macro HOPOS_RV_CIPA reg
    .endm
    .macro HOPOS_RV_CPA reg
    .endm
    .macro HOPOS_RV_SYNC
    .endm
    .macro HOPOS_RV_CIALL
    .endm
"#
    };
}

// De switcher. Invariant in rotate en daarna: sp = het sched-blok van dit
// hart, mtvec = mentry, alle GPR's vrij. In de ingang: t0..t2 staan in de
// scratch van het sched-blok tot ze in het ctx-blok belanden.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(
    cache_macros!(),
    r#"
    .section .text.hopos_mmode, "ax"

    // De kill-tick: de comparator van DIT hart op nu + SCHED_TICK_TICKS en
    // alleen MTIE aan, vlak vóór elke sprong naar een bewoner (koude boot,
    // hervatting, de terugkeer van een tick). Een machine-timer-interrupt
    // wordt in S-mode altijd genomen, ongeacht mstatus.MIE; in machine mode
    // (MIE = 0) nooit. Zonder comparator of zonder periode: mie = 0, het
    // gedrag van vóór de tick (Go, `ARMTICK` in cpu/mmode/switch.s).
    // Clobbert t0..t2; sp is het sched-blok. De volgorde van mtimecmp is die
    // van de spec (lo = alles-één, hi, lo): de c900-CLINT weigert 64-bit
    // MMIO, en een halve waarde in het verleden zou meteen vuren.
    .macro HOPOS_RV_ARMTICK
    csrw mie, zero
    ld t0, {tick}(sp)
    ld t1, {clint}(sp)
    beqz t0, 1f
    beqz t1, 1f
    rdtime t2
    add t2, t2, t0
    li t0, -1
    sw t0, 0(t1)
    srli t0, t2, 32
    sw t0, 4(t1)
    sw t2, 0(t1)
    li t0, 128
    csrw mie, t0
1:
    .endm

    // De GPR's van de bewoner naar zijn ctx-blok (\base): xN op
    // CTX_GPRS + 8*(N-1); x2 uit mscratch, x5..x7 uit de scratch van het
    // sched-blok (sp). Clobbert t0.
    .macro HOPOS_RV_SAVE_GPRS base
    sd x1, {gprs}+0(\base)
    csrr t0, mscratch
    sd t0, {gprs}+8(\base)
    sd x3, {gprs}+16(\base)
    sd x4, {gprs}+24(\base)
    ld t0, {scratch}+0(sp)
    sd t0, {gprs}+32(\base)
    ld t0, {scratch}+8(sp)
    sd t0, {gprs}+40(\base)
    ld t0, {scratch}+16(sp)
    sd t0, {gprs}+48(\base)
    sd x8, {gprs}+56(\base)
    sd x9, {gprs}+64(\base)
    sd x10, {gprs}+72(\base)
    sd x11, {gprs}+80(\base)
    sd x12, {gprs}+88(\base)
    sd x13, {gprs}+96(\base)
    sd x14, {gprs}+104(\base)
    sd x15, {gprs}+112(\base)
    sd x16, {gprs}+120(\base)
    sd x17, {gprs}+128(\base)
    sd x18, {gprs}+136(\base)
    sd x19, {gprs}+144(\base)
    sd x20, {gprs}+152(\base)
    sd x21, {gprs}+160(\base)
    sd x22, {gprs}+168(\base)
    sd x23, {gprs}+176(\base)
    sd x24, {gprs}+184(\base)
    sd x25, {gprs}+192(\base)
    sd x26, {gprs}+200(\base)
    sd x27, {gprs}+208(\base)
    sd x28, {gprs}+216(\base)
    sd x29, {gprs}+224(\base)
    sd x30, {gprs}+232(\base)
    sd x31, {gprs}+240(\base)
    .endm
    // De FP-registers en fcsr naar / uit CTX_FPRS van \base. Clobbert t0.
    .macro HOPOS_RV_FSAVE base
    .option push
    .option arch, +f, +d
    fsd f0, {fprs}+0(\base)
    fsd f1, {fprs}+8(\base)
    fsd f2, {fprs}+16(\base)
    fsd f3, {fprs}+24(\base)
    fsd f4, {fprs}+32(\base)
    fsd f5, {fprs}+40(\base)
    fsd f6, {fprs}+48(\base)
    fsd f7, {fprs}+56(\base)
    fsd f8, {fprs}+64(\base)
    fsd f9, {fprs}+72(\base)
    fsd f10, {fprs}+80(\base)
    fsd f11, {fprs}+88(\base)
    fsd f12, {fprs}+96(\base)
    fsd f13, {fprs}+104(\base)
    fsd f14, {fprs}+112(\base)
    fsd f15, {fprs}+120(\base)
    fsd f16, {fprs}+128(\base)
    fsd f17, {fprs}+136(\base)
    fsd f18, {fprs}+144(\base)
    fsd f19, {fprs}+152(\base)
    fsd f20, {fprs}+160(\base)
    fsd f21, {fprs}+168(\base)
    fsd f22, {fprs}+176(\base)
    fsd f23, {fprs}+184(\base)
    fsd f24, {fprs}+192(\base)
    fsd f25, {fprs}+200(\base)
    fsd f26, {fprs}+208(\base)
    fsd f27, {fprs}+216(\base)
    fsd f28, {fprs}+224(\base)
    fsd f29, {fprs}+232(\base)
    fsd f30, {fprs}+240(\base)
    fsd f31, {fprs}+248(\base)
    frcsr t0
    sd t0, {fprs}+256(\base)
    .option pop
    .endm
    .macro HOPOS_RV_FLOAD base
    .option push
    .option arch, +f, +d
    fld f0, {fprs}+0(\base)
    fld f1, {fprs}+8(\base)
    fld f2, {fprs}+16(\base)
    fld f3, {fprs}+24(\base)
    fld f4, {fprs}+32(\base)
    fld f5, {fprs}+40(\base)
    fld f6, {fprs}+48(\base)
    fld f7, {fprs}+56(\base)
    fld f8, {fprs}+64(\base)
    fld f9, {fprs}+72(\base)
    fld f10, {fprs}+80(\base)
    fld f11, {fprs}+88(\base)
    fld f12, {fprs}+96(\base)
    fld f13, {fprs}+104(\base)
    fld f14, {fprs}+112(\base)
    fld f15, {fprs}+120(\base)
    fld f16, {fprs}+128(\base)
    fld f17, {fprs}+136(\base)
    fld f18, {fprs}+144(\base)
    fld f19, {fprs}+152(\base)
    fld f20, {fprs}+160(\base)
    fld f21, {fprs}+168(\base)
    fld f22, {fprs}+176(\base)
    fld f23, {fprs}+184(\base)
    fld f24, {fprs}+192(\base)
    fld f25, {fprs}+200(\base)
    fld f26, {fprs}+208(\base)
    fld f27, {fprs}+216(\base)
    fld f28, {fprs}+224(\base)
    fld f29, {fprs}+232(\base)
    fld f30, {fprs}+240(\base)
    fld f31, {fprs}+248(\base)
    ld t0, {fprs}+256(\base)
    fscsr t0
    .option pop
    .endm
    // Een koude boot begint met lege f-registers: niets van de voorganger.
    .macro HOPOS_RV_FZERO
    .option push
    .option arch, +f, +d
    fmv.d.x f0, zero
    fmv.d.x f1, zero
    fmv.d.x f2, zero
    fmv.d.x f3, zero
    fmv.d.x f4, zero
    fmv.d.x f5, zero
    fmv.d.x f6, zero
    fmv.d.x f7, zero
    fmv.d.x f8, zero
    fmv.d.x f9, zero
    fmv.d.x f10, zero
    fmv.d.x f11, zero
    fmv.d.x f12, zero
    fmv.d.x f13, zero
    fmv.d.x f14, zero
    fmv.d.x f15, zero
    fmv.d.x f16, zero
    fmv.d.x f17, zero
    fmv.d.x f18, zero
    fmv.d.x f19, zero
    fmv.d.x f20, zero
    fmv.d.x f21, zero
    fmv.d.x f22, zero
    fmv.d.x f23, zero
    fmv.d.x f24, zero
    fmv.d.x f25, zero
    fmv.d.x f26, zero
    fmv.d.x f27, zero
    fmv.d.x f28, zero
    fmv.d.x f29, zero
    fmv.d.x f30, zero
    fmv.d.x f31, zero
    fscsr zero
    .option pop
    .endm
    .balign 4
    .global __hopos_parkenter
__hopos_parkenter:
    csrw mie, zero
    // FP aan in machine mode (mstatus.FS = Initial): de switcher bewaart
    // de f-registers van zijn bewoners zelf.
    li t0, 1 << 13
    csrs mstatus, t0
    // Geen erfenis: MIE uit (park rekent erop dat een wek nooit als trap
    // genomen wordt) en mscratch meteen op het sched-blok, zodat een trap
    // vóór de eerste bewoner niet op een willekeurige sp spilt (Go,
    // `parkenter`).
    csrci mstatus, 8
    // De TIME-CSR voor de bewoners (mcounteren.TM): de klok van een app is
    // `rdtime`, en zonder dit bit is dat in S-mode een illegal instruction.
    li t0, 2
    csrw mcounteren, t0
    mv sp, a1
    csrw mscratch, sp
    la t0, __hopos_mentry
    csrw mtvec, t0
    j 50f

    .balign 4
    .global __hopos_mentry
__hopos_mentry:
    csrrw sp, mscratch, sp
    sd t0, {scratch}+0(sp)
    sd t1, {scratch}+8(sp)
    sd t2, {scratch}+16(sp)
    // EERST de interruptbit: sinds de kill-tick staat MTIE aan terwijl een
    // bewoner draait, en een tick die in de fault-tak viel, meldde een
    // gezonde bewoner dood (Go, de kop van cpu/mmode/switch.s).
    csrr t0, mcause
    bltz t0, 30f
    li t1, 9
    bne t0, t1, 40f

    // --- yield of exit (ecall uit S-mode) -----------------------------
    ld t0, {current}(sp)
    ld t1, {s2}(sp)
    slli t0, t0, {shift}
    add t1, t1, t0
    li t0, {ctxoff}
    add t1, t1, t0
    li t0, 2
    beq a7, t0, 46f
    bnez a7, 45f

    // De GPR's: xN op CTX_GPRS + 8*(N-1); x2 uit mscratch, x5..x7 uit de
    // scratch.
    HOPOS_RV_SAVE_GPRS t1
    csrr t0, mepc
    addi t0, t0, 4
    sd t0, {resume}+0(t1)
    csrr t0, sstatus
    sd t0, {resume}+8(t1)
    csrr t0, satp
    sd t0, {regime}+{rsatp}(t1)
    csrr t0, stvec
    sd t0, {regime}+{rstvec}(t1)
    csrr t0, sscratch
    sd t0, {regime}+{rsscratch}(t1)
    HOPOS_RV_FSAVE t1
    // Het regime van de kooi zelf verandert niet (de bewoner kan PMP niet
    // aanraken); de kern schreef het, en het staat al in het ctx-blok.
    sd a0, {wake}(t1)
    // De intrekking: vers lezen, de kern schrijft hem vanaf zijn eigen hart.
    li t0, {revoke}
    add t0, t0, t1
    HOPOS_RV_CIPA t0
    HOPOS_RV_SYNC
    ld t0, 0(t0)
    bnez t0, 45f
    li t0, {saved}
    sd t0, {state}(t1)
    fence
    HOPOS_RV_CIPA t1
    HOPOS_RV_SYNC
    j 50f

    // --- een interrupt terwijl een bewoner draait: de kill-tick -----------
    // Eén vraag: wil de kern deze bewoner dood (CTX_REVOKE, vers gelezen)?
    // Zo ja, de teardown; zo nee, opnieuw wapenen en terug de bewoner in,
    // zonder rotatie (dit is geen preemptie) en zonder +4 op mepc (bij een
    // interrupt wijst mepc naar de instructie die nog moet komen). Een
    // andere bron dan de timer hoort hier niet: bron dicht en terug.
30:
    slli t0, t0, 1
    srli t0, t0, 1
    li t1, 7
    bne t0, t1, 39f
    ld t0, {current}(sp)
    beqz t0, 39f
    ld t1, {s2}(sp)
    slli t0, t0, {shift}
    add t1, t1, t0
    li t0, {ctxoff}
    add t1, t1, t0
    addi t0, t1, {revoke}
    HOPOS_RV_CIPA t0
    HOPOS_RV_SYNC
    ld t0, 0(t0)
    bnez t0, 45f
    // De tijdschijf: met meer dan één bewoner in de lijst is de tick ook het
    // einde van de beurt. De volle context weg zoals bij een yield (mepc
    // zoals hij is: een interrupt wijst naar de instructie die nog komt),
    // wektijd 0 (meteen weer aan de beurt), Saved, en de rotatie beslist wie
    // nu mag; de rotor begint ná deze bewoner, dus de buren gaan voor.
    ld t0, {count}(sp)
    li t2, 2
    bltu t0, t2, 37f
    HOPOS_RV_SAVE_GPRS t1
    csrr t0, mepc
    sd t0, {resume}+0(t1)
    csrr t0, sstatus
    sd t0, {resume}+8(t1)
    csrr t0, satp
    sd t0, {regime}+{rsatp}(t1)
    csrr t0, stvec
    sd t0, {regime}+{rstvec}(t1)
    csrr t0, sscratch
    sd t0, {regime}+{rsscratch}(t1)
    HOPOS_RV_FSAVE t1
    sd zero, {wake}(t1)
    li t0, {saved}
    sd t0, {state}(t1)
    fence
    HOPOS_RV_CIPA t1
    HOPOS_RV_SYNC
    j 50f
37:
    HOPOS_RV_ARMTICK
    j 38f
39:
    csrw mie, zero
38:
    ld t0, {scratch}+0(sp)
    ld t1, {scratch}+8(sp)
    ld t2, {scratch}+16(sp)
    csrrw sp, mscratch, sp
    mret

    // --- de kick (ecall, a7 = 2): de bel van de OS-core, en terug ---------
    // Geen beurtwissel en geen ctx: alleen t0..t2 waren klad (de scratch).
    // De publicatie op de TX-ring staat al vóór de ecall (de staart is
    // device-gemapt); de fence zet de bel erachter.
46:
    ld t0, {osbell}(sp)
    beqz t0, 47f
    fence iorw, iorw
    li t1, 1
    sw t1, 0(t0)
47:
    csrr t0, mepc
    addi t0, t0, 4
    csrw mepc, t0
    j 38b

    // --- fault --------------------------------------------------------
    // mcause, mtval en vec 1 naar de control-page van de bewoner, mepc,
    // ra, sp en satp voor het post-mortem in het ctx-blok.
40:
    ld t0, {current}(sp)
    ld t1, {s2}(sp)
    slli t0, t0, {shift}
    add t1, t1, t0
    li t0, {ctxoff}
    add t1, t1, t0
    ld t2, {ctrl}(t1)
    beqz t2, 41f
    csrr t0, mcause
    sd t0, {fesr}(t2)
    csrr t0, mtval
    sd t0, {ffar}(t2)
    li t0, 1
    sd t0, {fvec}(t2)
    fence
    addi t0, t2, 0x40
    HOPOS_RV_CIPA t0
41:
    csrr t0, mepc
    sd t0, {resume}+0(t1)
    sd x1, {gprs}+0(t1)
    csrr t0, mscratch
    sd t0, {gprs}+8(t1)
    csrr t0, satp
    sd t0, {regime}+{rsatp}(t1)

    // --- teardown: exit, fault, intrekking -----------------------------
    // De cache van de dode bewoner naar DRAM vóór de dood gemeld wordt: de
    // kern hergebruikt de partitie zodra hij Dead ziet.
45:
    HOPOS_RV_CIALL
    li t0, {dead}
    sd t0, {state}(t1)
    fence
    HOPOS_RV_CIPA t1
    HOPOS_RV_SYNC
    sd zero, {current}(sp)

    // --- rotate ---------------------------------------------------------
    // s1 = lijstlengte, s2 = index, s3 = pogingen, s4 = vroegste wektijd,
    // s7 = gewekt door de kick (msip): dan is elke geyielde bewoner één keer
    // aan de beurt, want de reden om te wachten kan net vervallen zijn (een
    // intrekking, RX). Te vroeg hervatten is altijd veilig: de bewoner kijkt,
    // vindt niets en yieldt met een verse wektijd. Zo hoeft de kern een
    // wektijd nooit te overschrijven (dat woord heeft één schrijver, Go).
50:
    li s7, 0
53:
    csrw mie, zero
    csrw satp, zero
    sfence.vma
    addi t0, sp, 64
    HOPOS_RV_CIPA t0
    addi t0, sp, 128
    HOPOS_RV_CIPA t0
    addi t0, sp, 192
    HOPOS_RV_CIPA t0
    HOPOS_RV_SYNC
    ld s1, {count}(sp)
    ld s2, {rotor}(sp)
    li s3, 0
    li s4, -1
    beqz s1, 60f
51:
    addi s2, s2, 1
    bltu s2, s1, 52f
    li s2, 0
52:
    addi t0, sp, {list}
    add t0, t0, s2
    lbu s5, 0(t0)
    beqz s5, 55f
    ld s6, {s2}(sp)
    slli t0, s5, {shift}
    add s6, s6, t0
    li t0, {ctxoff}
    add s6, s6, t0
    HOPOS_RV_CIPA s6
    HOPOS_RV_SYNC
    ld t0, {state}(s6)
    li t1, {bootpending}
    beq t0, t1, 56f
    li t1, {saved}
    bne t0, t1, 55f
    // Ingetrokken terwijl hij sliep of nog niet draaide: nooit meer
    // binnenlaten, meteen dood. Een bewoner met een verre wektijd voelde de
    // intrekking anders pas op die wektijd (de les van `el2::evict`).
56:
    addi t2, s6, {revoke}
    HOPOS_RV_CIPA t2
    HOPOS_RV_SYNC
    ld t2, 0(t2)
    beqz t2, 57f
    mv t1, s6
    j 45b
57:
    li t1, {bootpending}
    beq t0, t1, 70f
    bnez s7, 80f
    ld t0, {wake}(s6)
    li t1, {nopeek}
    and t3, t0, t1
    not t1, t1
    and t0, t0, t1
    rdtime t1
    bgeu t1, t0, 80f
    bnez t3, 54f
    // Niet aan de beurt, maar de deurbel dan? Dezelfde vraag als
    // `el2::rx_due` op ARM: is de drempel gewapend (bit 63 van CTRL_RX_DOOR)
    // en groeide de kop van de RX-ring erVOORBIJ? Voorbij, niet ongelijk: de
    // kop mag achterlopen op wat de bewoner zag (04-09). Beide woorden vers:
    // de drempel schrijft de bewoner, de kop de switch van de kern.
    ld t2, {ctrl}(s6)
    beqz t2, 54f
    addi t2, t2, {rxdoor}
    HOPOS_RV_CIPA t2
    HOPOS_RV_SYNC
    ld t2, 0(t2)
    bgez t2, 54f
    ld t1, {ringhead}(s6)
    beqz t1, 54f
    HOPOS_RV_CIPA t1
    HOPOS_RV_SYNC
    ld t1, 0(t1)
    slli t2, t2, 1
    srli t2, t2, 1
    bltu t2, t1, 80f
54:
    bgeu t0, s4, 55f
    mv s4, t0
55:
    addi s3, s3, 1
    bltu s3, s1, 51b

    // --- park -----------------------------------------------------------
    // Niemand aan de beurt: slapen tot de vroegste wektijd, geklemd op de
    // slaapgrens, of tot de kick. Zonder wekker (CLINT_PA 0) of zonder
    // grens (SLEEP_CAP 0): spinnen (dat kan niet hangen, Go 30-07), met de
    // pauze van de Go-switcher tussen twee rondes (62). SCHED_CURRENT = 0 en
    // naar DRAM: "dit hart draait niemand" is wat de kern hier leest.
60:
    sd zero, {current}(sp)
    fence
    HOPOS_RV_CPA sp
    HOPOS_RV_SYNC
    ld a3, {clint}(sp)
    ld a4, {cap}(sp)
    beqz a3, 62f
    beqz a4, 62f
    rdtime t0
    add t0, t0, a4
    bltu t0, s4, 61f
    mv t0, s4
61:
    li t1, -1
    sw t1, 0(a3)
    srli t2, t0, 32
    sw t2, 4(a3)
    sw t0, 0(a3)
    // De wekkers aan, alleen hier en met MIE uit: `wfi` kijkt naar
    // mip & mie, niet naar mstatus.MIE, dus hij wekt zonder dat er een trap
    // genomen wordt, en een kick vlak vóór de `wfi` staat al pending.
    li t1, 8 | 128
    csrw mie, t1
    wfi
    csrr t2, mip
    andi s7, t2, 8
    csrw mie, zero
    li t1, -1
    sw t1, 0(a3)
    sw t1, 4(a3)
    ld a4, {msip}(sp)
    beqz a4, 53b
    sw zero, 0(a4)
    j 53b

    // --- spinnen (de C906L) ----------------------------------------------
    // Een ronde zonder bewoner is op de C906 geen lege lus: elke ronde
    // veegt de regels van het sched-blok en het ctx-blok van elke bewoner
    // (`th.dcache.cipa` plus `th.sync.is`) en leest ze opnieuw uit DRAM,
    // naast de kern en de DMA van de NIC. Daarom eerst 0x4000 rondjes niets,
    // zoals de Go-switcher (`spin`/`pause` in cpu/mmode/switch.s, de
    // v2-getallen van dit board): enkele tientallen µs op de 700 MHz van de C906L, onder elke wektijd
    // van een app en onder de 300 µs van de NIC-poll van de kern.
62:
    li t0, 0x4000
63:
    addi t0, t0, -1
    bnez t0, 63b
    j 50b

    // --- koude boot van de bewoner in s6 (slot s5, index s2) ------------
    // Eerst de lijst nog eens, NÁ de staat. De kern haalt een slot uit de
    // lijst van zijn vorige hart vóór hij het op een ander hart BootPending
    // zet (`el2::roster::forget`); een rotatie die de lijst nog oud las
    // en de staat al nieuw, zou het slot anders op twee harts starten. Wie de
    // nieuwe staat ziet, ziet ook de lijst van daarvóór.
70:
    addi t0, sp, {list}
    add t0, t0, s2
    HOPOS_RV_CIPA t0
    HOPOS_RV_SYNC
    lbu t0, 0(t0)
    bne t0, s5, 55b
    // Het regime vers uit DRAM: de kern schreef het vanaf zijn hart.
    addi t0, s6, 256
    HOPOS_RV_CIPA t0
    addi t0, s6, 320
    HOPOS_RV_CIPA t0
    addi t0, s6, 384
    HOPOS_RV_CIPA t0
    addi t0, s6, 512
    HOPOS_RV_CIPA t0
    HOPOS_RV_SYNC
    // De kooi uit het ctx-blok: pmpaddr0..7, dan pmpcfg0, en teruglezen.
    ld t0, {regime}+{pa0}+0(s6)
    csrw pmpaddr0, t0
    ld t0, {regime}+{pa0}+8(s6)
    csrw pmpaddr1, t0
    ld t0, {regime}+{pa0}+16(s6)
    csrw pmpaddr2, t0
    ld t0, {regime}+{pa0}+24(s6)
    csrw pmpaddr3, t0
    ld t0, {regime}+{pa0}+32(s6)
    csrw pmpaddr4, t0
    ld t0, {regime}+{pa0}+40(s6)
    csrw pmpaddr5, t0
    ld t0, {regime}+{pa0}+48(s6)
    csrw pmpaddr6, t0
    ld t0, {regime}+{pa0}+56(s6)
    csrw pmpaddr7, t0
    ld t0, {regime}+{cfg}(s6)
    csrw pmpcfg0, t0
    csrr t1, pmpcfg0
    beq t0, t1, 71f
    // De kooi staat niet zoals de kern hem schreef: nooit binnenlaten.
    ld t2, {ctrl}(s6)
    beqz t2, 72f
    li t0, {verify}
    sd t0, {fvec}(t2)
    sd t1, {fesr}(t2)
    fence
    addi t0, t2, 0x40
    HOPOS_RV_CIPA t0
72:
    mv t1, s6
    j 45b
71:
    ld t0, {regime}+{rsatp}(s6)
    csrw satp, t0
    sfence.vma
    ld t0, {regime}+{rstvec}(s6)
    csrw stvec, t0
    ld t0, {regime}+{rsscratch}(s6)
    csrw sscratch, t0
    ld t0, {bootpc}(s6)
    csrw mepc, t0
    // mstatus: MPP = S (01), MPIE = 0, SUM en MXR uit.
    li t0, 3 << 11
    csrc mstatus, t0
    li t0, 1 << 11
    csrs mstatus, t0
    li t0, 1 << 7
    csrc mstatus, t0
    csrw medeleg, zero
    csrw mideleg, zero
    sd s2, {rotor}(sp)
    sd s5, {current}(sp)
    fence
    HOPOS_RV_CPA sp
    li t0, {running}
    sd t0, {state}(s6)
    fence
    HOPOS_RV_CIPA s6
    HOPOS_RV_SYNC
    HOPOS_RV_ARMTICK
    csrw mscratch, sp
    ld a0, {bootarg}(s6)
    li a1, 0
    li ra, 0
    li sp, 0
    li gp, 0
    li tp, 0
    HOPOS_RV_FZERO
    fence.i
    mret

    // --- hervatten van de bewoner in s6 ---------------------------------
80:
    ld t0, {regime}+{pa0}+0(s6)
    csrw pmpaddr0, t0
    ld t0, {regime}+{pa0}+8(s6)
    csrw pmpaddr1, t0
    ld t0, {regime}+{pa0}+16(s6)
    csrw pmpaddr2, t0
    ld t0, {regime}+{pa0}+24(s6)
    csrw pmpaddr3, t0
    ld t0, {regime}+{pa0}+32(s6)
    csrw pmpaddr4, t0
    ld t0, {regime}+{pa0}+40(s6)
    csrw pmpaddr5, t0
    ld t0, {regime}+{pa0}+48(s6)
    csrw pmpaddr6, t0
    ld t0, {regime}+{pa0}+56(s6)
    csrw pmpaddr7, t0
    ld t0, {regime}+{cfg}(s6)
    csrw pmpcfg0, t0
    ld t0, {regime}+{rsatp}(s6)
    csrw satp, t0
    sfence.vma
    ld t0, {regime}+{rstvec}(s6)
    csrw stvec, t0
    ld t0, {regime}+{rsscratch}(s6)
    csrw sscratch, t0
    ld t0, {resume}+0(s6)
    csrw mepc, t0
    ld t0, {resume}+8(s6)
    csrw sstatus, t0
    li t0, 3 << 11
    csrc mstatus, t0
    li t0, 1 << 11
    csrs mstatus, t0
    sd s2, {rotor}(sp)
    sd s5, {current}(sp)
    fence
    HOPOS_RV_CPA sp
    li t0, {running}
    sd t0, {state}(s6)
    fence
    HOPOS_RV_CIPA s6
    HOPOS_RV_SYNC
    HOPOS_RV_ARMTICK
    csrw mscratch, sp
    HOPOS_RV_FLOAD s6
    mv x31, s6
    ld x1, {gprs}+0(x31)
    ld x2, {gprs}+8(x31)
    ld x3, {gprs}+16(x31)
    ld x4, {gprs}+24(x31)
    ld x5, {gprs}+32(x31)
    ld x6, {gprs}+40(x31)
    ld x7, {gprs}+48(x31)
    ld x8, {gprs}+56(x31)
    ld x9, {gprs}+64(x31)
    ld x10, {gprs}+72(x31)
    ld x11, {gprs}+80(x31)
    ld x12, {gprs}+88(x31)
    ld x13, {gprs}+96(x31)
    ld x14, {gprs}+104(x31)
    ld x15, {gprs}+112(x31)
    ld x16, {gprs}+120(x31)
    ld x17, {gprs}+128(x31)
    ld x18, {gprs}+136(x31)
    ld x19, {gprs}+144(x31)
    ld x20, {gprs}+152(x31)
    ld x21, {gprs}+160(x31)
    ld x22, {gprs}+168(x31)
    ld x23, {gprs}+176(x31)
    ld x24, {gprs}+184(x31)
    ld x25, {gprs}+192(x31)
    ld x26, {gprs}+200(x31)
    ld x27, {gprs}+208(x31)
    ld x28, {gprs}+216(x31)
    ld x29, {gprs}+224(x31)
    ld x30, {gprs}+232(x31)
    ld x31, {gprs}+240(x31)
    mret
    .global __hopos_mmode_end
__hopos_mmode_end:
"#,
    scratch = const SCHED_SCRATCH,
    current = const SCHED_CURRENT,
    rotor = const SCHED_ROTOR,
    count = const SCHED_COUNT,
    list = const SCHED_LIST,
    s2 = const SCHED_S2_PA,
    clint = const SCHED_CLINT_PA,
    cap = const SCHED_SLEEP_CAP,
    msip = const SCHED_MSIP_PA,
    osbell = const SCHED_OS_BELL,
    tick = const SCHED_TICK_TICKS,
    shift = const CAGE_SHIFT,
    ctxoff = const CTX_OFF,
    state = const CTX_STATE,
    ctrl = const CTX_CTRL_PA,
    bootpc = const CTX_BOOT_PC,
    bootarg = const CTX_BOOT_ARG,
    gprs = const CTX_GPRS,
    fprs = const CTX_FPRS,
    resume = const CTX_RESUME,
    regime = const CTX_REGIME,
    rsatp = const REGIME_SATP,
    rstvec = const REGIME_STVEC,
    rsscratch = const REGIME_SSCRATCH,
    pa0 = const REGIME_PMPADDR0,
    cfg = const REGIME_PMPCFG0,
    wake = const CTX_WAKE,
    nopeek = const CTX_WAKE_NO_PEEK,
    revoke = const CTX_REVOKE,
    ringhead = const CTX_RING_HEAD_PA,
    rxdoor = const abi::hopabi::CTRL_RX_DOOR,
    bootpending = const CtxState::BootPending as u64,
    saved = const CtxState::Saved as u64,
    running = const CtxState::Running as u64,
    dead = const CtxState::Dead as u64,
    fesr = const abi::hopabi::CTRL_FAULT_ESR,
    ffar = const abi::hopabi::CTRL_FAULT_FAR,
    fvec = const abi::hopabi::CTRL_FAULT_VEC,
    verify = const FAULT_CAGE_VERIFY,
);

#[cfg(test)]
mod tests {
    use super::*;
    use abi::layout::{
        CTX_BOOT_ARG, CTX_FPRS, CTX_GPRS, CTX_REGIME, CTX_RESUME, CTX_WAKE, SCHED_MSIP_PA,
    };

    #[test]
    fn the_offsets_fit_the_immediates() {
        // Een `ld`/`sd`-offset is 12 bits met teken: alles wat de switcher
        // als immediate gebruikt, moet onder 2048 blijven.
        for off in [
            CTX_GPRS + 240,
            CTX_RESUME + 8,
            CTX_REGIME + REGIME_PMPADDR0 + 56,
            CTX_WAKE,
            CTX_BOOT_ARG,
            CTX_FPRS + 256,
            SCHED_MSIP_PA,
        ] {
            assert!(off < 2048, "{off}");
        }
        // De regime-woorden eindigen vóór de wektijd.
        const { assert!(CTX_REGIME + REGIME_PMPADDR0 + 64 <= CTX_WAKE) };
        // x31 ligt op CTX_GPRS + 8*30: de laatste van de 31 woorden.
        assert_eq!(CTX_GPRS + 240 + 8, abi::layout::CTX_SP);
        assert_eq!(entry_pc(), 0);
    }
}
