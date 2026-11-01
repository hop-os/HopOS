//! De architectuurlaag van HopOS v3 (arm64 eerst, riscv64 daarna).
//!
//! Twee sporen delen deze crate en elk heeft zijn eigen bestanden:
//!
//! - het boot-spoor: [`boot`] (de `_start`-stub, stack, BSS, de sprong naar
//!   Rust), [`vectors`] (de vectortabel: IRQ wekt de dispatcher, een
//!   synchrone exception meldt zich en parkeert), [`console`] (de vroege
//!   UART-haak);
//! - het kooi-spoor: [`el2`] (stage-2, de switcher, de HVC-handler),
//!   [`psci`], [`smp`], [`irq`] (het contract: een lijn, een controller, één
//!   werkwoord), [`gicv3`] (de CPU-interface als systeemregisters), [`idle`] (de `Sleeper` van de executor: WFE met event-stream,
//!   WFI op de timer, de rotatie van de OS-core), [`trng`], [`drbg`],
//!   [`memattr`].
//!
//! De specificatie is `OLD/metal/cpu` en de assembly ernaast; elk bestand
//! draagt de gedateerde metingen van zijn Go-voorganger mee.
//!
//! De cfg-splitsing. [`riscv`] heeft zijn instructies achter
//! `all(target_arch = "riscv64", target_os = "none")` en daarbuiten stubs,
//! net als de arm64-modules: zo testen zijn rekenkunde en de riscv-boards op
//! de host, en bouwt de werkruimte als geheel voor elk doel. De arm64-modules bouwen op elk doel:
//! hun instructies staan achter `all(target_arch = "aarch64", target_os =
//! "none")` en daarbuiten is elk een stub met dezelfde signatuur (handboek
//! §7). Dat is bewust zo gelaten: de lijm van `hopos` (slots, cage, flip,
//! main) noemt `cpu::el2`, `cpu::smp` en `cpu::psci` bij naam, en ze achter
//! `target_arch = "aarch64"` zetten breekt de riscv64-build van `hopos` tot
//! die lijm per architectuur gesplitst is (`hopos/src/cage_riscv.rs` is de
//! riscv-helft daarvan). Tot dan draait er op riscv64 geen arm64-instructie:
//! de stubs zijn leeg.

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

pub mod boot;
pub mod console;
pub mod drbg;
pub mod el2;
pub mod gicv3;
pub mod idle;
pub mod irq;
pub mod memattr;
pub mod psci;
pub mod smp;
pub mod trng;
pub mod vectors;

pub use smp::mpidr;

pub mod riscv;
