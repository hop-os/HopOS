//! De handvatten van de kern op `dev` en de console die elke laag erboven
//! krijgt, en de outbox van een slot voor zijn servicer. De kooi zelf (en
//! zijn bevestigingen van de switch) staat in `kooi.rs`.
//!
//! Dit bezit één staat: de deur van de outboxen ([`ring_outboxes`]), de
//! blik van de deur van de OS-core op elke open outbox. De staat van een
//! kooi is van de kooi, de boekhouding van de lifecycle-actor.

use crate::kooi::tail_of;
use abi::layout::{CtxState, RING_DATA_CAP};
use abi::ring;
use cpu::el2;
use cpu::println;
use dev::Pa;
use kern::cage::{Console, PhysMem};
use kern::slots::Outbox;
use kern::{Region, Slot, SlotVec};
use net::ring::{AbiTx, Reader};
use sync::LocalCell;

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

    /// Een regel van een app, als `slot N: ...`.
    fn app_line(&self, slot: Slot, line: &[u8]) {
        let line = line.trim_ascii_end();
        match core::str::from_utf8(line) {
            Ok(s) => println!("slot {slot}: {s}"),
            Err(_) => println!("slot {slot}: {}", line.escape_ascii()),
        }
    }

    fn fatal(&self, args: core::fmt::Arguments<'_>) -> ! {
        cpu::boot::fatal(args)
    }
}

/// De outbox van één levensduur: de lezer op de ring in de staart, en het
/// ctx-blok en de control-page voor de vragen van de servicer.
pub(crate) struct SlotOutbox {
    reader: Option<ring::Reader>,
    slot: Slot,
    ctx: Pa,
    ctrl: Option<Pa>,
}

impl SlotOutbox {
    /// Opent de outbox van slot `slot` in de partitie `part`; `ctx` is het
    /// ctx-blok van het slot. Een partitie zonder geldige staart geeft een
    /// outbox die meteen corrupt meldt, zodat de servicer luid stopt. Een
    /// geldige krijgt zijn deur ([`ring_outboxes`]) tot hij dicht gaat.
    pub(crate) fn open(slot: Slot, part: Region, ctx: Pa) -> SlotOutbox {
        let tail = tail_of(part);
        let reader = tail.and_then(|t| ring::Reader::open(t.outbox(), RING_DATA_CAP).ok());
        let ctrl = tail.map(|t| t.ctrl_page());
        // Een deur die de tabel niet kan dragen, is een outbox die meteen
        // corrupt meldt: de servicer stopt luid.
        let mut reader = reader;
        if let (Some(t), Some(c)) = (tail.filter(|_| reader.is_some()), ctrl) {
            let door = Door {
                outbox: t.outbox(),
                ctx,
                ctrl: c,
            };
            if DOORS.borrow_mut().insert(slot, door).is_err() {
                reader = None;
            }
        }
        SlotOutbox {
            reader,
            slot,
            ctx,
            ctrl,
        }
    }
}

impl Drop for SlotOutbox {
    fn drop(&mut self) {
        DOORS.borrow_mut().remove(self.slot);
    }
}

/// Wat de deur van één open outbox nakijkt: de kop van de ring, het
/// ctx-blok en de control-page (het SMP-verzoek).
#[derive(Copy, Clone)]
struct Door {
    outbox: Pa,
    ctx: Pa,
    ctrl: Pa,
}

/// De deuren van de open outboxen, op slotnummer. Alleen de kern-core
/// raakt ze (de servicers, de deur van de executor).
static DOORS: LocalCell<SlotVec<Door>> = LocalCell::cell(SlotVec::new());

/// De deur van de outboxen (O10, docs/sequence-hopos.md §13.3): luidt de
/// servicer van elk slot met werk in zijn outbox, een SMP-verzoek of een
/// gevallen context. Alleen kale loads plus de `pull` van de kop, zoals de
/// blik in de TX-ringen (`net::switch::Published::ring`); de deur van de
/// executor roept hem vóór en na elke slaap en elke beurt, en een app-core
/// kickt bij leeg-naar-niet-leeg, bij zijn exit en bij een fault. `true` =
/// er werd gebeld.
pub(crate) fn ring_outboxes() -> bool {
    let mut rang = false;
    for (slot, d) in DOORS.borrow().iter() {
        // Het handvat zonder bit 0: de blik doet zijn eigen onderhoud
        // (`net::ring::AbiTx::probe`, dezelfde ABI-ring als een TX-ring).
        let work = <AbiTx as Reader>::probe(d.outbox.0)
            || smp_req(d.ctrl)
            || !is_live(el2::ctx_state(d.ctx));
        if work {
            crate::SERVICERS.ring(slot);
            rang = true;
        }
    }
    rang
}

/// Leeft een context met deze staat (geboekt, draaiend of geyield)?
fn is_live(st: Option<CtxState>) -> bool {
    matches!(
        st,
        Some(CtxState::Running | CtxState::Saved | CtxState::BootPending)
    )
}

/// Staat er een SMP-verzoek op de control-page `ctrl`?
fn smp_req(ctrl: Pa) -> bool {
    dev::pull(ctrl.add(abi::hopabi::CTRL_SMP_REQ), 8);
    dev::read64(ctrl.add(abi::hopabi::CTRL_SMP_REQ)) != 0
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
        is_live(el2::ctx_state(self.ctx))
    }

    fn smp_pending(&self) -> bool {
        self.ctrl.is_some_and(smp_req)
    }
}
