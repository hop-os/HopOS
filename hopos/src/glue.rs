//! De handvatten van de kern op `dev` en de console die elke laag erboven
//! krijgt, en de outbox van een slot voor zijn servicer. De kooi zelf (en
//! zijn bevestigingen van de switch) staat in `kooi.rs`.
//!
//! Dit bezit geen staat: de staat van een kooi is van de kooi, de
//! boekhouding van de lifecycle-actor.

use crate::kooi::tail_of;
use abi::layout::{CtxState, RING_DATA_CAP};
use abi::ring;
use cpu::el2;
use cpu::println;
use dev::Pa;
use kern::cage::{Console, PhysMem};
use kern::slots::Outbox;
use kern::{Region, Slot};

/// Fysiek geheugen over `dev`, voor de kooi, de image-stream van de
/// system-API en de flip. De adressen komen uit het plan, uit de partitie
/// van een grant (die bewaakt de lifecycle) en uit `layout`, nergens anders
/// vandaan. Wat de kern schrijft, gaat naar DRAM: een app leest zijn
/// partitie met de MMU uit, een nieuwe kern zijn overdracht ook.
pub(crate) struct DevMem;

impl PhysMem for DevMem {
    fn read64(&self, pa: u64) -> u64 {
        dev::read64(Pa(pa))
    }

    fn write64(&mut self, pa: u64, v: u64) {
        dev::write64(Pa(pa), v);
    }

    fn clear(&mut self, pa: u64, len: u64) {
        let Ok(n) = usize::try_from(len) else { return };
        dev::clear(Pa(pa), n);
        dev::push(Pa(pa), n);
    }

    fn clean_inv(&mut self, pa: u64, len: u64) {
        if let Ok(n) = usize::try_from(len) {
            dev::pull(Pa(pa), n);
        }
    }

    fn copy_in(&mut self, pa: u64, src: &[u8]) {
        dev::copy_in(Pa(pa), src);
        dev::push(Pa(pa), src.len());
    }

    fn copy_out(&self, dst: &mut [u8], pa: u64) {
        dev::pull(Pa(pa), dst.len());
        dev::copy_out(dst, Pa(pa));
    }
}

/// De console van de kern: `cpu::println!`, en een app-regel als
/// `slot N: <regel>` zonder zijn regeleinde; een regel die geen UTF-8 is,
/// ge-escaped.
#[derive(Copy, Clone)]
pub(crate) struct KernConsole;

impl Console for KernConsole {
    fn log(&self, args: core::fmt::Arguments<'_>) {
        println!("{args}");
    }

    /// Een regel van een app, als `slot N: ...`; niet in de regelring van
    /// de kern (`conport::KERN_LOG`): Hop haalt hem al per slot.
    fn app_line(&self, slot: Slot, line: &[u8]) {
        let line = line.trim_ascii_end();
        crate::conport::KERN_LOG.app(true);
        match core::str::from_utf8(line) {
            Ok(s) => println!("slot {slot}: {s}"),
            Err(_) => println!("slot {slot}: {}", line.escape_ascii()),
        }
        crate::conport::KERN_LOG.app(false);
    }

    fn fatal(&self, args: core::fmt::Arguments<'_>) -> ! {
        cpu::boot::fatal(args)
    }
}

/// De outbox van één levensduur: de lezer op de ring in de staart, en het
/// ctx-blok en de control-page voor de vragen van de servicer.
pub(crate) struct SlotOutbox {
    reader: Option<ring::Reader>,
    ctx: Pa,
    ctrl: Option<Pa>,
}

impl SlotOutbox {
    /// Opent de outbox van de partitie `part`; `ctx` is het ctx-blok van
    /// het slot. Een partitie zonder geldige staart geeft een outbox die
    /// meteen corrupt meldt, zodat de servicer luid stopt.
    pub(crate) fn open(part: Region, ctx: Pa) -> SlotOutbox {
        let tail = tail_of(part);
        SlotOutbox {
            reader: tail.and_then(|t| ring::Reader::open(t.outbox(), RING_DATA_CAP).ok()),
            ctx,
            ctrl: tail.map(|t| t.ctrl_page()),
        }
    }
}

impl Outbox for SlotOutbox {
    fn read_into(&mut self, buf: &mut [u8]) -> Option<(u8, usize)> {
        let rec = self.reader.as_mut()?.read_into(buf)?;
        Some((
            u8::try_from(rec.kind.raw()).unwrap_or(u8::MAX),
            rec.payload.len(),
        ))
    }

    fn corrupt(&self) -> bool {
        self.reader.as_ref().is_none_or(ring::Reader::is_corrupt)
    }

    fn live(&self) -> bool {
        matches!(
            el2::ctx_state(self.ctx),
            Some(CtxState::Running | CtxState::Saved | CtxState::BootPending)
        )
    }

    fn smp_pending(&self) -> bool {
        self.ctrl.is_some_and(|c| {
            dev::pull(c.add(abi::hopabi::CTRL_SMP_REQ), 8);
            dev::read64(c.add(abi::hopabi::CTRL_SMP_REQ)) != 0
        })
    }
}
