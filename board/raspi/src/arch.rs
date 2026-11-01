//! De losse instructies van de Pi's: de TLB na een tabelwijziging, de
//! SError-peek, de slices over firmware-geheugen, en de ingang
//! ([`pi_entry!`](crate::pi_entry)) die vóór `cpu::boot` draait.
//!
//! Op de host staan hier stubs met dezelfde signatuur, zodat de logica
//! erboven test (handboek §7: `cfg` op module-niveau).

/// Maakt nieuwe tabelregels zichtbaar voor de table-walker: de schrijfacties
/// eerst naar het inner-shareable domein, dan de TLB van EL2 leeg.
pub(crate) fn tables_changed() {
    imp::tables_changed();
}

/// ISR_EL1: staat er op deze core een fysieke SError klaar (bit A)? Op
/// EL2 laat ISR_EL1 de fysieke lijn zien (ARM ARM, ISR_EL1.A). De kern
/// draait met PSTATE.A dicht, dus een asynchrone external abort (een
/// PCIe-toegang die fout ging) blijft hier staan tot de eerste ERET naar
/// EL1 hem neemt; deze lezing zegt vóór die tijd welke stap hem gaf. Alleen
/// de VL805-keten vraagt het (usb.rs, de bouwsteen `usb`).
#[cfg(feature = "usb")]
pub(crate) fn serror_pending() -> bool {
    imp::isr() & (1 << 8) != 0
}

/// Het gestagede image als slice; `None` op de host.
pub(crate) fn stage_slice(start: u64, len: u64) -> Option<&'static [u8]> {
    imp::slice(start, len)
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod imp {
    use core::arch::asm;

    #[cfg(feature = "usb")]
    pub(super) fn isr() -> u64 {
        let v: u64;
        // SAFETY: ISR_EL1 lezen heeft geen neveneffect; het neemt geen
        // exception en wist niets.
        unsafe { asm!("mrs {}, isr_el1", out(reg) v, options(nomem, nostack)) };
        v
    }

    pub(super) fn tables_changed() {
        // SAFETY: barrières en een TLB-invalidate van EL2 op deze core; er
        // komen alleen regels bij die eerst leeg waren (geen
        // break-before-make nodig), dus niets wat nu draait verliest zijn
        // mapping.
        unsafe {
            asm!(
                "dsb ishst",
                "tlbi alle2",
                "dsb ish",
                "isb",
                options(nostack)
            )
        };
    }

    pub(super) fn slice(pa: u64, len: u64) -> Option<&'static [u8]> {
        let len = usize::try_from(len).ok()?;
        // SAFETY: de aanroepers geven alleen bereiken in het laadvenster
        // (`map::LOADER`) of de kern-RAM die ze eerst toetsten: RAM dat de
        // vaste tabel als Normal mapt, en waar na de boot niemand meer in
        // schrijft (de DTB en de staging liggen buiten de heap en buiten de
        // pool). Alleen lezen.
        Some(unsafe { core::slice::from_raw_parts(pa as usize as *const u8, len) })
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
mod imp {
    //! Host-stubs.
    #[cfg(feature = "usb")]
    pub(super) fn isr() -> u64 {
        0
    }
    pub(super) fn tables_changed() {}
    pub(super) fn slice(_pa: u64, _len: u64) -> Option<&'static [u8]> {
        None
    }
}

/// De ingang van een Pi-image: de eerste instructie op 0x80000, vóór
/// `cpu::boot::_start`. Het board-crate roept hem één keer aan met zijn
/// UART-basis en de bovengrens van het cache-onderhoud.
///
/// Wat hier gebeurt en niet in `cpu::boot`, en waarom (de lessen van de
/// Go-`cpuinit_body.h`, het duurst gedebugde bestand van de Pi-boot):
///
/// 1. 'P' plus het boot-EL op de UART, met een begrensde poll: het eerste
///    levensteken, vóór welk systeemregister dan ook. Een dode UART houdt
///    de boot niet op.
/// 2. Op EL2 de Linux-`init_el2`-pariteit: SCTLR_EL2 met de MMU uit,
///    VTTBR_EL2 = 0 (óók met stage-2 uit tagt de VMID alle EL1&0-TLB-regels;
///    een garbage-VMID op de A76 is de errata-hoek), MDCR_EL2 (HPMN =
///    PMCR.N, geen debug-traps), MDSCR_EL1 = 0, VPIDR/VMPIDR, HSTR_EL2 = 0,
///    CPTR_EL2 zonder TFP (de TFP-hang van 09-07: 'P2R' en dan niets), en
///    CPACR_EL1 met FP aan. CNTVOFF_EL2 = 0 en CNTHCTL_EL2 met de
///    EL1-teller open én de event-stream van EL2 aan: `cpu::idle` zet
///    alleen CNTKCTL_EL1, en onder E2H=0 bepaalt CNTHCTL_EL2 de stream van
///    EL2. EVNTI 15 op 54 MHz is 1,2 ms. Zonder stream wekt een WFE van de
///    kern alleen op een SEV of interrupt.
/// 3. De D-cache invalideren over `[0x80000, inv_end)`, dan de I-cache: de
///    firmware draaide mét caches, schone regels van vóór de handoff
///    overleven, en `cpu::boot` zet de MMU aan zonder invalidate: de
///    table-walker en de eerste gecachte reads zien dan firmware-spoken in
///    plaats van onze data (de fase-P-les die QEMU verhulde). Onder 0x80000
///    NIET: daar woont TF-A, mét vuile regels.
///
/// 4. x3 gaat ongeschonden door naar `_start`: een geflipte kern draagt
///    daar het merkteken `cpu::boot::FLIP_ENTRY` (de trampoline van
///    `cpu::el2::chain` zet het), en alleen dan mag een andere core dan
///    core 0 door de poort van `_start`. De stappen hierboven gebruiken x1
///    tot en met x3 als kladregister, dus x22 bewaart het (29-09). Verder
///    is deze ingang al core-neutraal: geen MPIDR-toets, geen stack, en
///    wat hij aan EL2-registers zet, is per core. Een tweede keer dezelfde
///    DTB is geen probleem: de firmware legde hem in het laadvenster
///    (`map::DTB_PA`), buiten de kern-RAM en de pool, waar niemand schrijft,
///    en de flip geeft dezelfde x0 door. De DTB ís dus de feitenpagina van
///    de Pi (UEFI heeft er een eigen nodig, `board/uefi/src/flip.rs`, omdat
///    zijn feiten uit ACPI en de boot services komen die na de koude boot
///    weg zijn); `hopos/src/flip.rs` toetst vóór de sprong dat hij er nog
///    staat. De `dc ivac` van stap 3 raakt op een flip alleen schone regels:
///    de trampoline veegde de kern-RAM, en het blob, de staging en de
///    recorder zijn vóór de sprong naar DRAM geduwd.
///
/// CPUECTLR_EL1 (SMPEN) wordt NIET aangeraakt: de EEPROM-bootloader van
/// 2026-05 brengt een BL31 (v2.6-240) mee die EL2 er geen toegang meer toe
/// geeft; de `mrs` trapt naar EL3 en komt nooit terug (gemeten 04-08: P2abc
/// en dan niets). Een register dat de firmware beheert, is niet van ons.
#[macro_export]
macro_rules! pi_entry {
    ($uart:expr, $inv_end:expr) => {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        core::arch::global_asm!(
            r#"
    .section .text.pi_head, "ax"
    .global _pi_start
_pi_start:
    mov x20, x0
    mov x22, x3

    ldr x1, ={uart}
    ldr x3, =100000
1:  subs x3, x3, #1
    b.eq 3f
    ldr w2, [x1, #0x18]
    tbnz w2, #5, 1b
    mov w2, #0x50
    str w2, [x1]
    mrs x2, CurrentEL
    ubfx x2, x2, #2, #2
    add w2, w2, #0x30
    str w2, [x1]
3:
    mrs x2, CurrentEL
    ubfx x2, x2, #2, #2
    cmp x2, #2
    b.ne 5f

    ldr x1, =0x30c50830
    msr sctlr_el2, x1
    isb
    msr vttbr_el2, xzr
    mrs x1, pmcr_el0
    ubfx x1, x1, #11, #5
    msr mdcr_el2, x1
    msr mdscr_el1, xzr
    mrs x1, midr_el1
    msr vpidr_el2, x1
    mrs x1, mpidr_el1
    msr vmpidr_el2, x1
    msr hstr_el2, xzr
    mov x1, #0x33ff
    msr cptr_el2, x1
    mov x1, #(3 << 20)
    msr cpacr_el1, x1
    msr cntvoff_el2, xzr
    mov x1, #0xf7
    msr cnthctl_el2, x1
    isb
5:
    ldr x1, =0x80000
    ldr x2, ={inv_end}
6:  dc ivac, x1
    add x1, x1, #64
    cmp x1, x2
    b.lo 6b
    dsb sy
    ic iallu
    dsb sy
    isb
    mov x0, x20
    mov x3, x22
    b _start
    .ltorg
"#,
            uart = const $uart,
            inv_end = const $inv_end,
        );
    };
}
