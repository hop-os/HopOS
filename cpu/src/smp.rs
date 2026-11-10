//! De opgang van een core op het EL2-regime van de kern: de verhuizing
//! van de kern naar de OS-core bij boot ([`start_one`]), en de CPU_ON van
//! het board ([`cpu_on`]).
//!
//! De opgang van één core:
//!
//! 1. Core 0 leest zijn eigen EL2-regime van de levende registers (MAIR,
//!    TCR, TTBR0, HCR, VBAR, SCTLR) en legt het met de stack-top en de
//!    Rust-main in een [`Handoff`]. Van de bron gelezen, niet afgeleid: een
//!    core met een andere view van de map kan de hoge periferie niet
//!    vertalen (Altra, 17-07: UART en SBSA-watchdog op 16 TB, fault,
//!    watchdog-reset).
//! 2. De handoff gaat naar DRAM (`dev::push`), niet alleen naar onze cache:
//!    de nieuwe core leest hem met de MMU UIT, dus langs elke cache heen. Op
//!    QEMU (geen cachemodel) viel dat nooit op; op de M4 las de tweede core
//!    de vorige inhoud: rommel als sp en ttbr0 (02-09).
//! 3. CPU_ON naar `hopos_smp_entry` met de handoff in x0. De entry zet
//!    het regime, de MMU aan en de stack, en springt naar de main van het
//!    board.
//!
//! CPU_ON is PSCI, behalve op een board dat een eigen haak zet
//! ([`set_cpu_on`]): Apple silicium heeft geen PSCI (een SMC zonder EL3) en
//! start een core via m1n1's spin-table of PMGR plus een brievenbus
//! (`board_apple::cores`). Alles in de kern dat een core koud start, gaat
//! door [`cpu_on`].

extern crate alloc;

use crate::psci;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{
    AtomicPtr, AtomicU64,
    Ordering::{Acquire, Release},
};
use dev::Pa;

/// De stack van de kern na zijn verhuizing ([`start_one`]): 256 KiB, gelijk
/// aan de boot-stack (`STACK_SIZE` in elk linkscript), want hij draagt
/// dezelfde boot. Tot 04-10 was hij 64 KiB: op QEMU virt (`OSCORE=1`) haalde
/// de verhuisde kern 122 KB, op de O6N haalt de boot 156 KB, en de rest liep
/// zonder wachtpagina onder de stack door de heap in. Een wachtpagina heeft
/// hij nog steeds niet (de heap is in blokken gemapt); de hartslag meet hem
/// ([`moved_stack`]).
pub const NODE_STACK: usize = 256 << 10;

/// Onderkant en top van de stack van [`start_one`], 0 = de kern verhuisde
/// niet.
static MOVED_STACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Onderkant en top van de stack waarop de verhuisde kern draait, of `None`
/// als hij op de boot-stack bleef.
#[must_use]
pub fn moved_stack() -> Option<(u64, u64)> {
    let (bottom, top) = (MOVED_STACK[0].load(Acquire), MOVED_STACK[1].load(Acquire));
    (top != 0).then_some((bottom, top))
}

/// De Rust-main van een node-core: draait de executor van die core en keert
/// nooit terug. Het board levert hem; hij krijgt de core-index (1 tot
/// `cores - 1`).
pub type CoreMain = fn(core: usize) -> !;

/// De CPU_ON van een board zonder PSCI: start de core met MPIDR `target`
/// op `entry` (fysiek, EL2, MMU uit) met `ctx` in x0. De fout in
/// PSCI-vorm, zodat de kern één soort weigering telt.
pub type CpuOn = fn(target: u64, entry: u64, ctx: u64) -> core::result::Result<(), psci::Error>;

/// De haak van het board als rauwe pointer; null = PSCI.
static CPU_ON_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Wat een node-core bij zijn entry leest, met de MMU uit.
///
/// De offsets staan als constanten in de entry-assembly; de asserties
/// eronder houden ze gelijk. Een cacheline groot en gealigneerd, zodat
/// één `dev::push` hem helemaal naar DRAM brengt.
#[repr(C, align(64))]
pub struct Handoff {
    /// De stack-top, 16-gealigneerd.
    pub sp: u64,
    /// De main van deze core.
    pub main: CoreMain,
    /// De core-index.
    pub core: u64,
    /// MAIR_EL2 van core 0.
    pub mair: u64,
    /// TCR_EL2 van core 0.
    pub tcr: u64,
    /// TTBR0_EL2 van core 0: dezelfde map, dus dezelfde heap.
    pub ttbr0: u64,
    /// HCR_EL2 van core 0.
    pub hcr: u64,
    /// VBAR_EL2 van core 0: dezelfde vectoren.
    pub vbar: u64,
    /// SCTLR_EL2 van core 0 (MMU en caches aan).
    pub sctlr: u64,
}

const OFF_SP: usize = 0;
const OFF_MAIR: usize = 24;
const OFF_TCR: usize = 32;
const OFF_TTBR0: usize = 40;
const OFF_HCR: usize = 48;
const OFF_VBAR: usize = 56;
const OFF_SCTLR: usize = 64;
const _: () = assert!(core::mem::offset_of!(Handoff, sp) == OFF_SP);
const _: () = assert!(core::mem::offset_of!(Handoff, main) == 8);
const _: () = assert!(core::mem::offset_of!(Handoff, core) == 16);
const _: () = assert!(core::mem::offset_of!(Handoff, mair) == OFF_MAIR);
const _: () = assert!(core::mem::offset_of!(Handoff, tcr) == OFF_TCR);
const _: () = assert!(core::mem::offset_of!(Handoff, ttbr0) == OFF_TTBR0);
const _: () = assert!(core::mem::offset_of!(Handoff, hcr) == OFF_HCR);
const _: () = assert!(core::mem::offset_of!(Handoff, vbar) == OFF_VBAR);
const _: () = assert!(core::mem::offset_of!(Handoff, sctlr) == OFF_SCTLR);

/// Het EL2-regime van de dispatchende core, in de volgorde van [`Handoff`].
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Regime {
    /// MAIR_EL2.
    pub mair: u64,
    /// TCR_EL2.
    pub tcr: u64,
    /// TTBR0_EL2.
    pub ttbr0: u64,
    /// HCR_EL2.
    pub hcr: u64,
    /// VBAR_EL2.
    pub vbar: u64,
    /// SCTLR_EL2.
    pub sctlr: u64,
}

impl Handoff {
    /// De handoff voor `core` met stack-top `sp` en het regime van core 0.
    #[must_use]
    pub const fn new(core: usize, sp: u64, main: CoreMain, r: Regime) -> Self {
        Self {
            sp,
            main,
            core: core as u64,
            mair: r.mair,
            tcr: r.tcr,
            ttbr0: r.ttbr0,
            hcr: r.hcr,
            vbar: r.vbar,
            sctlr: r.sctlr,
        }
    }
}

/// Waarom een core niet opkwam.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// Geen heap voor de stack of de handoff.
    OutOfMemory {
        /// De core-index.
        core: usize,
    },
    /// De firmware weigerde CPU_ON.
    Psci {
        /// Het MPIDR-target.
        target: u64,
        /// De reden.
        err: psci::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::OutOfMemory { core } => {
                write!(f, "smp: core {core}: no heap for its stack or handoff")
            }
            Self::Psci { target, err } => write!(f, "smp: CPU_ON {target:#x}: {err}"),
        }
    }
}

/// Het resultaat van deze module.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Zet de CPU_ON van het board, in plaats van PSCI. Eén keer bij boot, in
/// `discover`, vóór de eerste core koud start (de verhuizing naar de
/// OS-core, een kooi).
pub fn set_cpu_on(f: CpuOn) {
    CPU_ON_HOOK.store(f as *mut (), Release);
}

fn cpu_on_hook() -> Option<CpuOn> {
    let p = CPU_ON_HOOK.load(Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: alleen `set_cpu_on` schrijft dit woord, met een geldige
    // `CpuOn`; null is hierboven uitgesloten.
    Some(unsafe { core::mem::transmute::<*mut (), CpuOn>(p) })
}

/// Start de core met MPIDR `target` op `entry` met `ctx` in x0: de haak van
/// het board als die er is, anders PSCI CPU_ON.
pub fn cpu_on(target: u64, entry: u64, ctx: u64) -> core::result::Result<(), psci::Error> {
    match cpu_on_hook() {
        Some(f) => f(target, entry, ctx),
        None => psci::cpu_on(target, entry, ctx),
    }
}

/// MPIDR_EL1 van deze core: de affiniteit voor de GIC-route, de kick en
/// `core_of` van het board. Op de host 0.
#[must_use]
pub fn mpidr() -> u64 {
    arch::mpidr()
}

/// Start precies één core op het EL2-regime van deze core, met een verse
/// stack en `main`: de verhuizing van de kern naar de OS-core bij boot
/// (PORT.md beslissing 2, `hopos.system.core`). De aanroeper geeft daarna zijn
/// eigen core op (`cpu::el2::hold`).
pub fn start_one(core: usize, target: u64, main: CoreMain) -> Result {
    let regime = arch::regime();
    let entry = arch::entry_pa();
    let (bottom, sp) = new_stack().ok_or(Error::OutOfMemory { core })?;
    let h = new_handoff(Handoff::new(core, sp, main, regime)).ok_or(Error::OutOfMemory { core })?;
    let pa = Pa(core::ptr::from_ref(h) as usize as u64);
    dev::push(pa, core::mem::size_of::<Handoff>());
    MOVED_STACK[0].store(bottom, Release);
    MOVED_STACK[1].store(sp, Release);
    cpu_on(target, entry, pa.0).map_err(|err| {
        MOVED_STACK[1].store(0, Release);
        Error::Psci { target, err }
    })
}

/// Een stack van [`NODE_STACK`] bytes van de heap, voor altijd: de
/// onderkant en de top, 16-gealigneerd.
///
/// `try_reserve_exact` eerst: dan alloceert `resize` niet meer, en een
/// volle heap is `None` in plaats van een abort (handboek §6).
fn new_stack() -> Option<(u64, u64)> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(NODE_STACK).ok()?;
    v.resize(NODE_STACK, 0);
    let s: &'static mut [u8] = v.leak();
    let bottom = s.as_ptr() as usize as u64;
    Some((bottom, (bottom + s.len() as u64) & !15))
}

/// De handoff op de heap, voor altijd: de core leest hem na zijn entry
/// niet meer, maar niemand anders mag de plek krijgen terwijl hij nog
/// onderweg is.
fn new_handoff(h: Handoff) -> Option<&'static Handoff> {
    let mut v: Vec<Handoff> = Vec::new();
    v.try_reserve_exact(1).ok()?;
    v.push(h);
    let s: &'static [Handoff] = v.leak();
    s.first()
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    //! De registers van het regime en de entry van een node-core.
    use super::{
        Handoff, OFF_HCR, OFF_MAIR, OFF_SCTLR, OFF_SP, OFF_TCR, OFF_TTBR0, OFF_VBAR, Regime,
    };
    use core::arch::asm;

    macro_rules! mrs {
        ($reg:literal) => {{
            let v: u64;
            // SAFETY: een systeemregister van het eigen EL2-regime (of
            // MPIDR_EL1) lezen heeft geen neveneffect.
            unsafe { asm!(concat!("mrs {}, ", $reg), out(reg) v, options(nomem, nostack)) };
            v
        }};
    }

    pub(super) fn mpidr() -> u64 {
        mrs!("mpidr_el1")
    }

    pub(super) fn regime() -> Regime {
        Regime {
            mair: mrs!("mair_el2"),
            tcr: mrs!("tcr_el2"),
            ttbr0: mrs!("ttbr0_el2"),
            hcr: mrs!("hcr_el2"),
            vbar: mrs!("vbar_el2"),
            sctlr: mrs!("sctlr_el2"),
        }
    }

    unsafe extern "C" {
        /// De entry hieronder; alleen zijn adres wordt gebruikt.
        fn hopos_smp_entry();
    }

    /// Het fysieke adres van de entry (identity-map: gelijk aan het
    /// virtuele).
    pub(super) fn entry_pa() -> u64 {
        hopos_smp_entry as *const () as usize as u64
    }

    /// De Rust-kant van de entry: de main van het board.
    extern "C" fn node_entry(h: &'static Handoff) -> ! {
        (h.main)(h.core as usize)
    }

    // De entry van een node-core, waar CPU_ON hem neerzet: EL2, x0 = de
    // handoff. Alles wat Rust nog niet kan en niets meer:
    //
    // - Niet op EL2 (een firmware die ons ergens anders aflevert): parkeren.
    //   Stil, want er is nog geen stack om te melden.
    // - De maskers dicht en SCTLR_EL2.M/C/I uit. Na PSCI staat dat al zo, en
    //   is dit niets. Maar m1n1 ROEPT een core uit zijn spin-table AAN als
    //   functie, met zijn eigen MMU en caches nog aan (Apple, 29-08): de
    //   loads van de handoff hieronder zouden dan door m1n1's map gaan en TCR
    //   en TTBR0 wisselen onder een draaiende MMU. Geen cache-onderhoud op
    //   set/way: de machine reset erop terwijl een andere core loopt (29-08).
    // - Het regime van core 0 zetten. MMU uit betekent dat de loads
    //   Device-toegang zijn: gealigneerd, en `dev::push` bracht ze naar DRAM.
    //   HCR_EL2 eerst, met een ISB: een kern onder E2H = 1 (VHE, de O6N)
    //   geeft TCR_EL2 en SCTLR_EL2 in de vorm van TCR_EL1 en SCTLR_EL1, en
    //   E2H bepaalt hoe de core die velden leest (Linux' `init_el2` zet
    //   HCR_EL2 ook als eerste). Onder nVHE maakt de volgorde niets uit:
    //   MMU uit en DAIF dicht (29-09).
    // - Eerst de TLB en de I-cache van deze core schoon, dan SCTLR (MMU en
    //   caches aan), ISB.
    // - De stack, en `bl` naar Rust met x0 ongewijzigd: de handoff is het
    //   eerste argument van `node_entry`.
    //
    // Veiligheid (als gewone regel: clippy weigert een SAFETY-tag op
    // `global_asm!`, er is geen `unsafe`-blok om te dragen): de entry raakt alleen de registers van zijn eigen core en leest
    // de handoff die core 0 voor hem klaarzette en naar DRAM veegde; de
    // stack is een verse heap-allocatie van precies deze core. `node_entry`
    // keert nooit terug; de parkeerlus erachter is de vangrail.
    core::arch::global_asm!(
        r#"
    .section .text.hopos_smp, "ax"
    .global hopos_smp_entry
    .balign 8
hopos_smp_entry:
    mrs x1, CurrentEL
    lsr x1, x1, #2
    and x1, x1, #3
    cmp x1, #2
    b.ne 9f

    msr daifset, #0xf
    mrs x1, sctlr_el2
    bic x1, x1, #(1 << 0)
    bic x1, x1, #(1 << 2)
    bic x1, x1, #(1 << 12)
    dsb sy
    msr sctlr_el2, x1
    isb

    ldr x1, [x0, #{hcr}]
    msr hcr_el2, x1
    isb
    ldr x1, [x0, #{mair}]
    msr mair_el2, x1
    ldr x1, [x0, #{tcr}]
    msr tcr_el2, x1
    ldr x1, [x0, #{ttbr0}]
    msr ttbr0_el2, x1
    ldr x1, [x0, #{vbar}]
    msr vbar_el2, x1
    dsb ish
    isb
    tlbi alle2
    ic iallu
    dsb ish
    isb
    ldr x1, [x0, #{sctlr}]
    msr sctlr_el2, x1
    isb

    ldr x1, [x0, #{sp}]
    mov sp, x1
    bl {rust}
9:
    wfe
    b 9b
"#,
        mair = const OFF_MAIR,
        tcr = const OFF_TCR,
        ttbr0 = const OFF_TTBR0,
        hcr = const OFF_HCR,
        vbar = const OFF_VBAR,
        sctlr = const OFF_SCTLR,
        sp = const OFF_SP,
        rust = sym node_entry,
    );
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod arch {
    //! Host-stub: geen registers, geen entry. De opgang loopt tot CPU_ON,
    //! en PSCI zegt op de host NOT_SUPPORTED.
    use super::Regime;

    pub(super) fn mpidr() -> u64 {
        0
    }
    pub(super) fn regime() -> Regime {
        Regime::default()
    }
    pub(super) fn entry_pa() -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    fn main_stub(_core: usize) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }

    #[test]
    fn start_one_on_the_host() {
        // De host heeft geen firmware: CPU_ON zegt NOT_SUPPORTED.
        assert_eq!(
            start_one(1, 1, main_stub),
            Err(Error::Psci {
                target: 1,
                err: psci::Error::NotSupported
            })
        );
    }

    #[test]
    fn handoff_carries_the_regime() {
        let r = Regime {
            mair: 1,
            tcr: 2,
            ttbr0: 3,
            hcr: 4,
            vbar: 5,
            sctlr: 6,
        };
        let h = Handoff::new(3, 0x1000, main_stub, r);
        assert_eq!(
            [h.sp, h.core, h.mair, h.tcr, h.ttbr0, h.hcr, h.vbar, h.sctlr],
            [0x1000, 3, 1, 2, 3, 4, 5, 6]
        );
        assert_eq!(core::mem::size_of::<Handoff>(), 128);
        let (bottom, top) = new_stack().unwrap();
        assert_eq!(top % 16, 0);
        assert!(top - bottom <= NODE_STACK as u64 && top - bottom > NODE_STACK as u64 - 16);
    }

    /// Het doel dat de test-haak aanneemt; elk ander zegt NOT_SUPPORTED,
    /// zoals PSCI op de host (de andere tests draaien parallel en zien de
    /// haak ook).
    const HOOKED: u64 = 0xA11E;
    static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn board_cpu_on(target: u64, entry: u64, ctx: u64) -> core::result::Result<(), psci::Error> {
        if target != HOOKED {
            return Err(psci::Error::NotSupported);
        }
        assert_eq!((entry, ctx), (0x8000, 0x4000_1000));
        HOOK_CALLS.fetch_add(1, Release);
        Ok(())
    }

    #[test]
    fn a_board_cpu_on_replaces_psci() {
        set_cpu_on(board_cpu_on);
        assert_eq!(cpu_on(HOOKED, 0x8000, 0x4000_1000), Ok(()));
        assert_eq!(HOOK_CALLS.load(Acquire), 1);
        assert_eq!(cpu_on(7, 0x8000, 0), Err(psci::Error::NotSupported));
    }
}
