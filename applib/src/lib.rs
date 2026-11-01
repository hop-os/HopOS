//! De app-runtime van HopOS: wat een Rust-app nodig heeft om in een slot
//! te draaien.
//!
//! Een app ziet één regio, zijn eigen partitie. Onderin staat zijn RAM
//! (image, heap, stack), bovenin een staart van 2 MB met de control-page, de
//! outbox-ring en de frame-ringen. Uit twee woorden in zijn image, `RamStart`
//! en `RamSize` (de kern patcht ze bij plaatsing), rekent applib elk adres
//! uit ([`Tail`]); een absoluut adres kent de app niet.
//!
//! - [`App`]: het handvat. Bezit de outbox-producer, de control-page-toegang
//!   en de env; meldt READY en draait de heartbeat.
//! - [`log!`]: logregels naar de outbox, zonder allocatie, droppen bij vol.
//! - [`main!`]: de main-schil. `_start`, de stack, de heap, de executor van
//!   de app-core, en dan de `async fn` van de app.
//! - [`AppSleeper`]: de idle van de app-core (WFE, of de yield naar EL2 op
//!   een gedeelde core), met de deurbel van de RX-ring.
//! - [`net`]: frame-niveau netwerk over de frame-ringen: de [`net::Nic`]
//!   (`netdev::Device`), de RX-pomp en de deurbel.
//! - [`appnet`]: de netstack (`leannet`) over die ringen, met async TCP-
//!   en UDP-handvatten en de system-client over een echte verbinding.
//! - [`sys`]: de system-API-client over een [`sys::Conn`].
//! - [`store`]: pull, push, list en drop tussen de eigen map in de
//!   object-store en het eigen zicht, over die client.
//! - [`mmu`]: de stage-1 van elke app op arm64 (RAM write-back, de
//!   control-page Normal-NC) en de vectortabel die een fault op EL1 meldt.
//! - [`rand`]: de willekeur: een DRBG uit het zaad dat de kern op de
//!   control-page legt (`CTRL_RNG_SEED`) en eigen jitter, met één luide
//!   regel over de bron (`HOPOS_APP_RNG`).
//! - [`smp`]: meer cores voor één app (`cores: N` in de jobspec): één
//!   executor per core en [`smp::spawn_on`].
//! - [`text`]: tekst schrijven zonder te groeien, en JSON-strings.
//! - [`burn`]: de rekenburst van bench en vitals.
//!
//! - `codec` (feature `media`): de codec-client; een stream door de
//!   hardwaredecoder zonder dat er een beeld over de verbinding gaat.
//! - `tcp` (feature `http`): de `TcpConn`-brug tussen een
//!   [`appnet::TcpStream`] en leanhttp, voor elke app die HTTP praat.
//!
//! Wat hier niet staat: de device-ops.

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

extern crate alloc;

mod arch;
mod contract;

pub mod app;
pub mod appnet;
pub mod burn;
pub mod clock;
#[cfg(feature = "media")]
pub mod codec;
pub mod ctrl;
#[cfg(feature = "gui")]
pub mod fb;
pub mod heap;
pub mod log;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod mem;
pub mod mmu;
pub mod net;
pub mod rand;
pub mod ring;
pub mod rt;
pub mod sleep;
pub mod smp;
/// Private stacks voor synchrone C-callbacks die de gewone executor moeten laten lopen.
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    all(target_arch = "x86_64", not(target_os = "windows"))
))]
pub mod stacktask;
pub mod store;
pub mod sys;
pub mod tail;
#[cfg(feature = "http")]
pub mod tcp;
pub mod text;

pub use app::{App, AppError, Beat};
pub use ctrl::{AppStatus, Ctrl, Env};
pub use rt::{EXEC, Exec, app, park};
pub use sleep::AppSleeper;
pub use tail::{Tail, TailError, tail_of};
