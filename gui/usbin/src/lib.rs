//! De USB-invoer naar het glas: de toetsen en de muis die de bouwsteen
//! `usb` (met `hid`) van de bus leest, als KVM-JSON naar de app die de
//! framebuffer-grant houdt. De bus zelf, de scan en de opslag zijn niet van
//! hier: die zijn van `usb`, en media gebruikt ze zonder gui
//! (docs/sequence-hopos.md §14.3).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod deliver;

#[cfg(test)]
mod tests;
