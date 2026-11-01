//! De OS-core op riscv64 (PORT.md beslissing 2): het hart van de kern als
//! deelbare core. De kern is er de eerste bewoner van, en zijn idle is een
//! beurt voor de andere bewoners (Hop, en de groepen die de kern het
//! toestaat). De riscv-helft van `cpu::el2::OsCore`, en bedoeld om ernaast
//! gelezen te worden.
//!
//! Dit bezit: de overgang van de kern (machine mode) naar een bewoner
//! (supervisor mode achter PMP plus Sv39) en terug, de rotatie over de
//! bewonerslijst van logische core 0 (sched-blok 0 van het plan), de eigen
//! `mtimecmp` die de deadline van de executor bewaakt terwijl een bewoner
//! draait, en de meetlat (`cpu::el2::OS_STATS`, dezelfde getallen als op
//! arm64). Niet van hier: wie er bewoner wordt (de kooi,
//! `hopos/src/kooi.rs`) en wat de kern doet als hij terug is (de
//! executor).
//!
//! Waarom niet de M-mode-switcher van de app-harts ([`super::switch`]): die
//! slaapt zelf op de CLINT en kent alleen bewoners; de kern draait op dit
//! hart met zijn eigen trap-ingang, stack en interrupts. Hier is het
//! omgekeerd en klein: de kern roept [`OsCore::run`] als zijn executor niets
//! te doen heeft, de bewoner draait tot hij yieldt, exit doet of faultt, of
//! tot een interrupt (de PLIC, de kick, de wekker op de deadline) naar
//! machine mode trapt. Een machine-interrupt wordt in S-mode altijd genomen,
//! ongeacht `mstatus.MIE`, dus de bewoner kan de kern nooit buitensluiten.
//! De trap landt op [`os_trap`](self) (niet op de trap van de kern), die de
//! bewoner in zijn ctx-blok bewaart en de kern hervat, alsof `enter` gewoon
//! terugkeerde. De interrupt blijft pending en wordt genomen zodra de kern
//! zijn masker opent, precies zoals uit een `wfi`.
//!
//! De ctx-blokken zijn die van de switcher, byte voor byte: alles wat de kern
//! van een bewoner leest (`ctx_state`, `rx_due`, het fault-rapport) klopt
//! hier ook.
//!
//! FP: een beurt kan asynchroon eindigen (PLIC, kick, wekker), en daarna
//! kan een ándere bewoner aan de beurt komen. Daarom bewaart de trap f0..f31
//! en `fcsr` in `CTX_FPRS` (dezelfde plek en volgorde als de switcher) en zet
//! de overgang ze terug bij het hervatten; een koude start begint met
//! nullen. Bij elke trap, ook een `ecall`: applib laat de f-registers bij
//! een yield niet zelf bewaren (`clobber_abi("C")` houdt fs0..fs11 voor
//! levend), net als op de app-harts. `mstatus.FS` gaat er eerst aan: een
//! bewoner die zijn FPU uitzette, mag de kern geen illegal instruction in
//! machine mode bezorgen. De kern zelf gebruikt geen f-registers (gemeten
//! 29-09; `tools/qemu-riscv-test.sh` telt ze buiten de switcher en deze
//! overgang), dus de zijne hoeven niet mee.

use super::clint::{Clint, NEVER};
use super::csr;
use super::pmp;
use crate::el2::oscore::{begin, end, home, round};
use crate::el2::{
    self, Back, OS_STATS as STATS, Turn, ctx_read, ctx_write, os_ctx_read, os_ctx_write,
};
use abi::hopabi::{CTRL_FAULT_ESR, CTRL_FAULT_FAR, CTRL_FAULT_VEC};
use abi::layout::{
    CTX_ASID, CTX_BOOT_PC, CTX_CTRL_PA, CTX_FPRS, CTX_GPRS, CTX_LEN, CTX_REGIME, CTX_RESUME,
    CTX_STATE, CTX_WAKE, Core, CtxState, Plan,
};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use dev::Pa;

/// `mcause` van een `ecall` uit S-mode.
const CAUSE_ECALL_S: u64 = 9;
/// De interruptbit van `mcause`.
const CAUSE_INTERRUPT: u64 = 1 << 63;
/// Interrupt-codes: de kick (`msip`), de wekker, de PLIC.
const IRQ_MSI: u64 = 3;
const IRQ_MTI: u64 = 7;
/// Wat `enter` teruggeeft als `pmpcfg0` anders terugleest dan de kern hem
/// schreef: de bewoner is nooit binnengelaten. Geen geldige `mcause`
/// (bit 62 bestaat daar niet).
const CAUSE_CAGE_VERIFY: u64 = 1 << 62;

/// Het woord van `a7` (x17) en `a0` (x10) in de GPR's van het ctx-blok:
/// xN staat op `CTX_GPRS + 8 * (N - 1)`.
const CTX_A7: u64 = CTX_GPRS + 8 * 16;
/// `a7` van de kick (`ecall`, de tegenhanger van HVC #6): op de OS-core een
/// yield naar nu, de kern draait zijn ronde (en leest de TX-ring) en geeft
/// het hart terug, zoals op arm64.
const A7_KICK: u64 = 2;
const CTX_A0: u64 = CTX_GPRS + 8 * 9;

/// De bewaarplaats van de kern tijdens een beurt (`mscratch` wijst erheen):
/// ra, sp, gp, tp, s0..s11, het ctx-blok, `mtvec`, `mscratch`, `mtval` van
/// de trap, en drie werkwoorden. Eén OS-core per node, dus één static; de
/// kern is de enige die hem aanraakt, en alleen binnen `enter`.
static SAVE: [AtomicU64; SAVE_WORDS] = [const { AtomicU64::new(0) }; SAVE_WORDS];
const SAVE_WORDS: usize = 24;
/// De offset van `mtval` in [`SAVE`] (bytes); de rest van de indeling
/// staat bij de assembly (`arch`).
const SAVE_MTVAL: u64 = 152;

/// De rotatie van de OS-core. Eén, eigendom van de slaap van de executor op
/// dat hart (`RvSleeper::host`).
pub struct OsCore {
    sched: Pa,
    clint: Clint,
    hart: usize,
    pmp: pmp::Profile,
    /// De ASID-breedte van dit hart ([`asid_bits`]).
    asid_bits: u32,
    /// Elke bewoner zijn eigen ASID (de kooi deelt hem uit en zet hem in
    /// `CTX_ASID`), dus geen TLB-flush per wissel: het hart heeft ze en de
    /// kern wil ze. Anders een volle `sfence.vma` bij elke beurt, zoals tot
    /// 03-10.
    asids: bool,
}

/// Het ASID-veld van `satp` (Sv39: bits 59..44).
const SATP_ASID_SHIFT: u64 = 44;
const SATP_ASID: u64 = 0xffff << SATP_ASID_SHIFT;

/// Hoeveel ASID-bits dit hart heeft: alle enen in het veld schrijven en
/// teruglezen (de spec, en Linux' `asids_init`, een WARL-proef). `satp`
/// raakt de kern in machine mode niet (geen MPRV), dus de proef kan hier.
/// De kooi deelt er de ASID's van de bewoners van dit hart mee uit
/// (`hopos/src/cage_riscv.rs`); ASID 0 blijft van de zelftest (Bare).
#[must_use]
pub fn asid_bits() -> u32 {
    let field =
        (arch::satp_probe(SATP_ASID | super::sv39::SATP_SV39) & SATP_ASID) >> SATP_ASID_SHIFT;
    field.trailing_ones()
}

impl OsCore {
    /// De rotatie over het plan `plan`, op hart `hart` met zijn wekker op
    /// `clint`. `pmp` is de PMP van de CPU, voor de zelftest. `asids`: een
    /// ASID per bewoner als het hart ze heeft (`false`: een flush per
    /// wissel, de vergelijking op ijzer).
    pub fn new(
        plan: &Plan,
        clint: Clint,
        hart: usize,
        pmp: pmp::Profile,
        asids: bool,
    ) -> Result<OsCore, el2::Error> {
        let core0 = Core::new(0).ok_or(el2::Error::BadContext { pa: 0 })?;
        let sched = plan.park_mbox_pa(core0).map_err(el2::Error::Plan)?;
        // ASID's in plaats van een flush per wissel, de VMID van arm64. Op
        // riscv schrijft een bewoner in S-mode `satp` zelf, ASID inbegrepen;
        // dan kon hij de vertalingen van een ander (met diens PMP) lenen.
        // Daarom TVM: op de OS-core is `satp` (en `sfence.vma`) van de kern,
        // een bewoner die eraan komt is een illegal instruction en dood.
        // Geen app doet het (applib, Hop); een kooi op een app-hart mag het
        // nog steeds.
        let asid_bits = asid_bits();
        let asids = asids && asid_bits > 0;
        if asids {
            arch::trap_vm();
        }
        Ok(OsCore {
            sched,
            clint,
            hart,
            pmp,
            asid_bits,
            asids,
        })
    }

    /// De ASID-breedte van dit hart, en of de rotatie ze gebruikt (en dus
    /// niet flusht per wissel).
    #[must_use]
    pub fn asid(&self) -> (u32, bool) {
        (self.asid_bits, self.asids)
    }

    /// Eén beurt: de bewoner die [`el2::next`] aanwijst (dezelfde regel als
    /// op arm64 en in de switcher) draait tot hij het hart teruggeeft of tot
    /// `deadline` (TIME-tikken). Aanroepen met `mstatus.MIE` uit, ná de
    /// laatste `ready()`-toets van de executor.
    pub fn run(&mut self, deadline: u64) -> Turn {
        let sched = self.sched;
        round(sched, csr::rdtime(), |i, ctx, fresh| {
            self.turn(i, ctx, deadline, fresh)
        })
    }

    /// De beurt van de bewoner met ctx-blok `ctx` op lijstplek `i`.
    fn turn(&mut self, i: usize, ctx: Pa, deadline: u64, fresh: bool) -> Back {
        begin(self.sched, i, ctx);
        // De ASID van de bewoner (`CTX_ASID`, de kooi deelde hem uit): de
        // kern zet hem, elke beurt (TVM houdt de bewoner er vanaf). Een flush
        // alleen bij een koude start: de vertalingen van een vorige huurder
        // met deze ASID weg, zoals de TLBI bij een verse bewoner op arm64.
        // Een bewoner zonder ASID (0) flusht elke beurt.
        let asid = if self.asids {
            os_ctx_read(ctx, CTX_ASID)
        } else {
            0
        };
        let flush = fresh || asid == 0;
        if asid != 0 {
            let at = ctx.add(CTX_REGIME + super::switch::REGIME_SATP);
            let satp = dev::read64(at);
            let want = (satp & !SATP_ASID) | (asid << SATP_ASID_SHIFT & SATP_ASID);
            if satp >> 60 != 0 && satp != want {
                dev::write64(at, want);
            }
        }
        let t0 = csr::rdtime();
        let (cause, mtval) = self.enter(ctx, fresh, flush, deadline);
        home(self.sched, csr::rdtime().wrapping_sub(t0));
        end(ctx, settle(ctx, cause, mtval, true))
    }

    /// De overgang zelf: de wekker op `deadline`, de drie bronnen aan die de
    /// kern terughalen (PLIC, kick, wekker), de sprong, en alles terug zoals
    /// het stond. Geeft `mcause` en `mtval` van de trap.
    fn enter(&self, ctx: Pa, fresh: bool, flush: bool, deadline: u64) -> (u64, u64) {
        const BACK: u64 = csr::MIP_MEIP | csr::MIP_MSIP | csr::MIP_MTIP;
        let mie = csr::mie();
        self.clint.set_timecmp(self.hart, deadline);
        csr::mie_set(BACK);
        let cause = arch::enter(
            ctx.0,
            u64::from(fresh),
            SAVE.as_ptr() as u64,
            u64::from(flush),
        );
        csr::mie_clear(BACK);
        csr::mie_set(mie & BACK);
        self.clint.set_timecmp(self.hart, NEVER);
        (cause, SAVE[(SAVE_MTVAL / 8) as usize].load(Relaxed))
    }

    /// De zelftest bij boot: een bewoner zonder vertaling, alleen de
    /// whitelist over een pagina met een stub in het kern-image, die spint,
    /// yieldt of exit doet, met `kick` vlak vóór de overgang en de wekker op
    /// `ticks` na nu. Geeft wat de proef zag ([`Seen`]); `None` als de kooi
    /// van de stub niet te coderen was.
    pub fn selftest(&mut self, probe: Probe, ticks: u64, kick: &dyn Fn()) -> Option<Seen> {
        let code: &[u32] = match probe {
            Probe::Spin => &[0x0000_006f],
            Probe::Yield => &[0x0000_0513, 0x0000_0893, 0x0000_0073, 0x0000_006f],
            Probe::Exit => &[0x0010_0893, 0x0000_0073, 0x0000_006f],
        };
        for (w, c) in STUB.0.iter().zip(code) {
            w.store(*c, Relaxed);
        }
        let base = STUB.0.as_ptr() as u64;
        let win = pmp::Window {
            base,
            size: 4096,
            r: true,
            w: false,
            x: true,
        };
        let enc = pmp::encode(&[win], self.pmp).ok()?;
        let ctx = Pa(SCRATCH_CTX.0.as_ptr() as u64);
        dev::clear(ctx, CTX_LEN as usize);
        let regime = ctx.add(CTX_REGIME);
        dev::write64(regime.add(super::switch::REGIME_PMPCFG0), enc.cfg);
        for (k, a) in enc.addr.iter().enumerate() {
            dev::write64(
                regime.add(super::switch::REGIME_PMPADDR0 + 8 * k as u64),
                *a,
            );
        }
        dev::write64(ctx.add(CTX_BOOT_PC), base);
        ctx_write(ctx, CTX_STATE, CtxState::Running.raw());
        let prev = csr::mask();
        kick();
        let t0 = csr::rdtime();
        let (cause, mtval) = self.enter(ctx, true, true, t0.wrapping_add(ticks));
        let dt = csr::rdtime().wrapping_sub(t0);
        // De kick staat nog: de dispatch van de kern wist hem pas later.
        self.clint.set_msip(self.hart, false);
        csr::restore(prev);
        Some(Seen {
            back: settle(ctx, cause, mtval, false),
            ticks: dt,
            cause,
        })
    }
}

/// Wat één proef van [`OsCore::selftest`] zag: waardoor de kern terugkwam,
/// na hoeveel tikken, en de `mcause` van de trap. Bij een [`Back::Irq`]
/// zegt de interruptcode wélke bron (11 de PLIC, 1, 5 of 9 een S-mode-bron
/// zonder delegatie); de riscv-tegenhanger van `el2::Probe`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Seen {
    /// Waardoor de kern terugkwam.
    pub back: Back,
    /// Na hoeveel TIME-tikken.
    pub ticks: u64,
    /// De `mcause` van de trap, met de interruptbit (bit 63).
    pub cause: u64,
}

/// Wat de zelftest van [`OsCore::selftest`] doet.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Probe {
    /// `j .`: alleen de wekker of de kick haalt de kern terug.
    Spin,
    /// `li a0,0; li a7,0; ecall`: de yield.
    Yield,
    /// `li a7,1; ecall`: de exit.
    Exit,
}

/// De stub van de zelftest: één pagina, zodat de whitelist hem precies
/// beschrijft. Atomics en geen `[u32]`: de kern schrijft er code in.
#[repr(C, align(4096))]
struct Page([AtomicU32; 1024]);
static STUB: Page = Page([const { AtomicU32::new(0) }; 1024]);

/// Het ctx-blok van de zelftest, met de FP-kier die de trap beschrijft.
#[repr(C, align(64))]
struct CtxScratch([AtomicU64; SCRATCH_WORDS]);
const SCRATCH_WORDS: usize = ((CTX_FPRS + 33 * 8) / 8) as usize;
static SCRATCH_CTX: CtxScratch = CtxScratch([const { AtomicU64::new(0) }; SCRATCH_WORDS]);

/// Zet de staat van een bewoner na zijn beurt, en zegt waardoor de kern
/// terug is. `count` = meetellen in de meetlat (niet voor de zelftest).
fn settle(ctx: Pa, cause: u64, mtval: u64, count: bool) -> Back {
    let tally = |c: &AtomicU64| {
        if count {
            c.fetch_add(1, Relaxed);
        }
    };
    if cause == CAUSE_CAGE_VERIFY {
        report(ctx, super::switch::FAULT_CAGE_VERIFY, mtval, 0);
        tally(&STATS.faults);
        return Back::Fault;
    }
    if cause & CAUSE_INTERRUPT != 0 {
        // Onderbroken midden in zijn werk: meteen weer aan de beurt, op
        // mepc zelf (een interrupt wijst naar de instructie die nog moet).
        os_ctx_write(ctx, CTX_WAKE, 0);
        os_ctx_write(ctx, CTX_STATE, CtxState::Saved.raw());
        return match cause & !CAUSE_INTERRUPT {
            IRQ_MTI => {
                tally(&STATS.timer);
                Back::Timer
            }
            IRQ_MSI => {
                tally(&STATS.ipi);
                Back::Ipi
            }
            _ => {
                tally(&STATS.irq);
                Back::Irq
            }
        };
    }
    if cause != CAUSE_ECALL_S {
        report(ctx, 1, cause, mtval);
        tally(&STATS.faults);
        return Back::Fault;
    }
    // Een ecall wijst met mepc naar zichzelf: hervatten op + 4. De woorden
    // van de register-staat (mepc, a7, a0) schreef de trap net op dit hart,
    // en alleen de overgang op dit hart leest ze terug: gewone toegang, geen
    // `th.dcache.cipa` die de regel eerst naar DRAM schrijft en dan uit de
    // cache gooit (03-10, de hop op de C906).
    let resume = ctx.add(CTX_RESUME);
    dev::write64(resume, dev::read64(resume).wrapping_add(4));
    let a7 = dev::read64(ctx.add(CTX_A7));
    if a7 != 0 && a7 != A7_KICK {
        os_ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
        tally(&STATS.exits);
        return Back::Exit;
    }
    // Een intrekking leest de rotatie, niet de terugweg: `el2::next` doodt
    // hem vóór zijn volgende beurt, op beide architecturen.
    let wake = if a7 == A7_KICK {
        0
    } else {
        dev::read64(ctx.add(CTX_A0))
    };
    os_ctx_write(ctx, CTX_WAKE, wake);
    os_ctx_write(ctx, CTX_STATE, CtxState::Saved.raw());
    tally(&STATS.yields);
    Back::Yield
}

/// Het fault-rapport op de control-page (vec, mcause, mtval, zoals de
/// switcher), en de bewoner dood.
fn report(ctx: Pa, vec: u64, esr: u64, far: u64) {
    let cp = ctx_read(ctx, CTX_CTRL_PA);
    if cp != 0 {
        for (off, v) in [
            (CTRL_FAULT_VEC, vec),
            (CTRL_FAULT_ESR, esr),
            (CTRL_FAULT_FAR, far),
        ] {
            dev::write64(Pa(cp).add(off), v);
            dev::push(Pa(cp).add(off), 8);
        }
    }
    ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
mod arch {
    use super::{
        CAUSE_CAGE_VERIFY, CTX_BOOT_PC, CTX_FPRS, CTX_GPRS, CTX_REGIME, CTX_RESUME, SAVE_MTVAL,
        SAVE_WORDS,
    };
    use crate::riscv::csr::MSTATUS_FS_INITIAL;
    use crate::riscv::switch::{
        REGIME_PMPADDR0, REGIME_PMPCFG0, REGIME_SATP, REGIME_SSCRATCH, REGIME_STVEC,
    };
    use abi::layout::CTX_BOOT_ARG;

    /// Offsets in `SAVE` (bytes): ra, sp, gp, tp en s0..s11 op 0..128, dan
    /// het ctx-blok, `mtvec`, `mscratch`, `mtval` en drie werkwoorden.
    const SAVE_CTX: u64 = 128;
    const SAVE_MTVEC: u64 = 136;
    const SAVE_MSCRATCH: u64 = 144;
    const SAVE_SCRATCH: u64 = 160;
    const _: () = assert!(SAVE_MTVAL == 152 && SAVE_SCRATCH + 24 <= 8 * SAVE_WORDS as u64);

    unsafe extern "C" {
        /// De overgang (hieronder): a0 = ctx-blok, a1 = koud (1) of
        /// hervatten (0), a2 = de bewaarplaats, a3 = de TLB flushen (1) of
        /// niet (0, de ASID van de bewoner is genoeg). Geeft `mcause`.
        fn __hopos_os_enter(ctx: u64, fresh: u64, save: u64, flush: u64) -> u64;
    }

    /// Schrijft `v` in `satp`, leest hem terug en zet `satp` weer op 0 met
    /// een flush: de proef op de ASID-breedte.
    pub(super) fn satp_probe(v: u64) -> u64 {
        let r: u64;
        // SAFETY: machine mode vertaalt niet (geen MPRV), dus `satp` raakt
        // hier niets dan de TLB, en die is na de flush leeg.
        unsafe {
            core::arch::asm!(
                "csrw satp, {v}",
                "csrr {r}, satp",
                "csrw satp, zero",
                "sfence.vma",
                v = in(reg) v,
                r = out(reg) r,
                options(nostack),
            );
        }
        r
    }

    /// `mstatus.TVM`: `satp` en `sfence.vma` zijn in S-mode een illegal
    /// instruction (zie [`super::OsCore::new`]).
    pub(super) fn trap_vm() {
        // SAFETY: raakt alleen S-mode van dit hart; de kern zelf draait in
        // machine mode.
        unsafe { core::arch::asm!("csrs mstatus, {}", in(reg) 1u64 << 20, options(nostack)) };
    }

    /// De overgang naar de bewoner van `ctx` en terug.
    pub(super) fn enter(ctx: u64, fresh: u64, save: u64, flush: u64) -> u64 {
        // SAFETY: `ctx` is een ctx-blok van het plan met een kooi die de
        // kern bouwde (of die van de zelftest); `save` is de static `SAVE`.
        // De assembly bewaart ra, sp, gp, tp en s0..s11 van de kern en zet
        // ze terug, zet `mtvec` en `mscratch` terug, en keert als een gewone
        // C-functie terug; de caller-saved registers zijn volgens de ABI
        // verloren. De f-registers van de kern hoeven niet mee: hij gebruikt
        // ze niet (de kop van dit bestand).
        unsafe { __hopos_os_enter(ctx, fresh, save, flush) }
    }

    core::arch::global_asm!(
        concat!(
            r#"
    .section .text.hopos_os, "ax"
    .balign 4
    .global __hopos_os_enter
__hopos_os_enter:
    sd ra, 0(a2)
    sd sp, 8(a2)
    sd gp, 16(a2)
    sd tp, 24(a2)
    sd s0, 32(a2)
    sd s1, 40(a2)
    sd s2, 48(a2)
    sd s3, 56(a2)
    sd s4, 64(a2)
    sd s5, 72(a2)
    sd s6, 80(a2)
    sd s7, 88(a2)
    sd s8, 96(a2)
    sd s9, 104(a2)
    sd s10, 112(a2)
    sd s11, 120(a2)
    sd a0, {sctx}(a2)
    csrr t0, mtvec
    sd t0, {smtvec}(a2)
    csrr t0, mscratch
    sd t0, {smscratch}(a2)
    // De kooi: pmpaddr0..7, dan pmpcfg0, en teruglezen. Een kooi die niet
    // aantoonbaar staat, wordt niet betreden.
    ld t0, {regime}+{pa0}+0(a0)
    csrw pmpaddr0, t0
    ld t0, {regime}+{pa0}+8(a0)
    csrw pmpaddr1, t0
    ld t0, {regime}+{pa0}+16(a0)
    csrw pmpaddr2, t0
    ld t0, {regime}+{pa0}+24(a0)
    csrw pmpaddr3, t0
    ld t0, {regime}+{pa0}+32(a0)
    csrw pmpaddr4, t0
    ld t0, {regime}+{pa0}+40(a0)
    csrw pmpaddr5, t0
    ld t0, {regime}+{pa0}+48(a0)
    csrw pmpaddr6, t0
    ld t0, {regime}+{pa0}+56(a0)
    csrw pmpaddr7, t0
    ld t0, {regime}+{cfg}(a0)
    csrw pmpcfg0, t0
    csrr t1, pmpcfg0
    beq t0, t1, 1f
    sd t1, {smtval}(a2)
    li a0, {verify}
    j 9f
1:
    // Geen delegatie (elke trap van de bewoner komt hier, niet in S-mode),
    // en de TIME-CSR voor zijn klok (mcounteren.TM), zoals `parkenter` op de
    // app-harts. De firmware kan beide anders achterlaten op dit hart.
    csrw medeleg, zero
    csrw mideleg, zero
    li t0, 2
    csrw mcounteren, t0
    csrw mscratch, a2
    la t0, __hopos_os_trap
    csrw mtvec, t0
    ld t0, {regime}+{rsatp}(a0)
    csrw satp, t0
    beqz a3, 3f
    sfence.vma
3:
    ld t0, {regime}+{rstvec}(a0)
    csrw stvec, t0
    ld t0, {regime}+{rsscratch}(a0)
    csrw sscratch, t0
    // FP aan voor beide wegen hieronder (de FS van een vorige bewoner kan
    // uit staan); de `.option pop` staat na het laden bij het hervatten.
    li t0, {fs}
    csrs mstatus, t0
    .option push
    .option arch, +f, +d
    beqz a1, 2f
    // Koud: de ingang, a0 = het argument, alle andere registers nul (geen
    // kern-adres en niets van een voorganger lekt de kooi in).
    .irp n, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31
    fmv.d.x f\n, zero
    .endr
    fscsr zero
    ld t0, {bootpc}(a0)
    csrw mepc, t0
    li t0, 3 << 11
    csrc mstatus, t0
    li t0, 1 << 11
    csrs mstatus, t0
    li t0, 1 << 7
    csrc mstatus, t0
    ld a0, {bootarg}(a0)
    li ra, 0
    li sp, 0
    li gp, 0
    li tp, 0
    li t0, 0
    li t1, 0
    li t2, 0
    li s0, 0
    li s1, 0
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    li a5, 0
    li a6, 0
    li a7, 0
    li s2, 0
    li s3, 0
    li s4, 0
    li s5, 0
    li s6, 0
    li s7, 0
    li s8, 0
    li s9, 0
    li s10, 0
    li s11, 0
    li t3, 0
    li t4, 0
    li t5, 0
    li t6, 0
    fence.i
    mret

    // Hervatten: de f-registers, dan sstatus (met de FS van de bewoner)
    // vóór de MPP-bits (sstatus is een venster op mstatus), dan de GPR's
    // met x31 als basis, x31 als laatste.
2:
    .irp n, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31
    fld f\n, {fprs}+8*\n(a0)
    .endr
    ld t0, {fprs}+256(a0)
    fscsr t0
    .option pop
    ld t0, {resume}+0(a0)
    csrw mepc, t0
    ld t0, {resume}+8(a0)
    csrw sstatus, t0
    li t0, 3 << 11
    csrc mstatus, t0
    li t0, 1 << 11
    csrs mstatus, t0
    mv x31, a0
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

    // Elke trap tijdens een beurt: de bewoner bewaren in zijn ctx-blok (de
    // volgorde van de switcher), mepc rauw (de Rust-kant telt 4 bij een
    // ecall), en de kern terug alsof `enter` terugkeerde, met mcause.
    .balign 4
    .global __hopos_os_trap
__hopos_os_trap:
    csrrw sp, mscratch, sp
    sd t0, {sscratch}+0(sp)
    sd t1, {sscratch}+8(sp)
    sd t2, {sscratch}+16(sp)
    ld t0, {sctx}(sp)
    sd x1, {gprs}+0(t0)
    csrr t1, mscratch
    sd t1, {gprs}+8(t0)
    sd x3, {gprs}+16(t0)
    sd x4, {gprs}+24(t0)
    ld t1, {sscratch}+0(sp)
    sd t1, {gprs}+32(t0)
    ld t1, {sscratch}+8(sp)
    sd t1, {gprs}+40(t0)
    ld t1, {sscratch}+16(sp)
    sd t1, {gprs}+48(t0)
    sd x8, {gprs}+56(t0)
    sd x9, {gprs}+64(t0)
    sd x10, {gprs}+72(t0)
    sd x11, {gprs}+80(t0)
    sd x12, {gprs}+88(t0)
    sd x13, {gprs}+96(t0)
    sd x14, {gprs}+104(t0)
    sd x15, {gprs}+112(t0)
    sd x16, {gprs}+120(t0)
    sd x17, {gprs}+128(t0)
    sd x18, {gprs}+136(t0)
    sd x19, {gprs}+144(t0)
    sd x20, {gprs}+152(t0)
    sd x21, {gprs}+160(t0)
    sd x22, {gprs}+168(t0)
    sd x23, {gprs}+176(t0)
    sd x24, {gprs}+184(t0)
    sd x25, {gprs}+192(t0)
    sd x26, {gprs}+200(t0)
    sd x27, {gprs}+208(t0)
    sd x28, {gprs}+216(t0)
    sd x29, {gprs}+224(t0)
    sd x30, {gprs}+232(t0)
    sd x31, {gprs}+240(t0)
    csrr t1, mepc
    sd t1, {resume}+0(t0)
    csrr t1, sstatus
    sd t1, {resume}+8(t0)
    csrr t1, satp
    sd t1, {regime}+{rsatp}(t0)
    csrr t1, stvec
    sd t1, {regime}+{rstvec}(t0)
    csrr t1, sscratch
    sd t1, {regime}+{rsscratch}(t0)
    // De FP-staat, na sstatus (die draagt de FS van de bewoner zelf), en
    // alleen als hij niet Clean is: Clean betekent dat de f-registers sinds
    // de vorige bewaring niet veranderd zijn, en die staat al in zijn
    // ctx-blok (lazy FP zoals Linux' `__fstate_save` bij SR_FS_DIRTY). Na
    // het bewaren hervat hij Clean, en pas een FP-schrijf maakt hem weer
    // Dirty. Initial (een koude start die nog niets bewaarde) en Off bewaren
    // wel, zoals tot 03-10.
    ld t1, {resume}+8(t0)
    li t2, {fs_mask}
    and t1, t1, t2
    li t2, {fs_clean}
    beq t1, t2, 7f
    li t1, {fs}
    csrs mstatus, t1
    .option push
    .option arch, +f, +d
    .irp n, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31
    fsd f\n, {fprs}+8*\n(t0)
    .endr
    frcsr t1
    sd t1, {fprs}+256(t0)
    .option pop
    // Hervatten als Clean; een bewoner die FP uit had (Off), houdt het uit.
    ld t1, {resume}+8(t0)
    li t2, {fs_mask}
    and t2, t1, t2
    beqz t2, 7f
    li t2, {fs_mask}
    or t1, t1, t2
    li t2, {fs}
    xor t1, t1, t2
    sd t1, {resume}+8(t0)
7:
    csrr t1, mtval
    sd t1, {smtval}(sp)
    csrr a0, mcause
    mv a2, sp

    // De kern terug (a2 = de bewaarplaats, a0 = wat enter geeft). `satp`
    // blijft staan: machine mode vertaalt niet, en de volgende beurt zet de
    // zijne (met zijn ASID, of met een flush).
9:
    ld t0, {smtvec}(a2)
    csrw mtvec, t0
    ld t0, {smscratch}(a2)
    csrw mscratch, t0
    ld ra, 0(a2)
    ld gp, 16(a2)
    ld tp, 24(a2)
    ld s0, 32(a2)
    ld s1, 40(a2)
    ld s2, 48(a2)
    ld s3, 56(a2)
    ld s4, 64(a2)
    ld s5, 72(a2)
    ld s6, 80(a2)
    ld s7, 88(a2)
    ld s8, 96(a2)
    ld s9, 104(a2)
    ld s10, 112(a2)
    ld s11, 120(a2)
    ld sp, 8(a2)
    ret
    // Het einde van de overgang: tot hier telt de FP-toets niet mee.
    .global __hopos_os_end
__hopos_os_end:
"#
        ),
        sctx = const SAVE_CTX,
        smtvec = const SAVE_MTVEC,
        smscratch = const SAVE_MSCRATCH,
        smtval = const SAVE_MTVAL,
        sscratch = const SAVE_SCRATCH,
        regime = const CTX_REGIME,
        rsatp = const REGIME_SATP,
        rstvec = const REGIME_STVEC,
        rsscratch = const REGIME_SSCRATCH,
        pa0 = const REGIME_PMPADDR0,
        cfg = const REGIME_PMPCFG0,
        bootpc = const CTX_BOOT_PC,
        bootarg = const CTX_BOOT_ARG,
        gprs = const CTX_GPRS,
        resume = const CTX_RESUME,
        verify = const CAUSE_CAGE_VERIFY,
        fprs = const CTX_FPRS,
        fs = const MSTATUS_FS_INITIAL,
        fs_mask = const 3 * MSTATUS_FS_INITIAL,
        fs_clean = const 2 * MSTATUS_FS_INITIAL,
    );
}

#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
mod arch {
    //! Host-stub: er is geen bewoner; een beurt is meteen een yield.
    pub(super) fn enter(_ctx: u64, _fresh: u64, _save: u64, _flush: u64) -> u64 {
        super::CAUSE_ECALL_S
    }
    /// Host: een hart zonder ASID's.
    pub(super) fn satp_probe(_v: u64) -> u64 {
        0
    }
    pub(super) fn trap_vm() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::el2::ctx_state;

    fn block() -> (Vec<u64>, Pa) {
        let mut v = vec![0u64; (CTX_LEN / 8) as usize];
        let pa = Pa(v.as_mut_ptr() as u64);
        (v, pa)
    }

    #[test]
    fn settle_reads_the_trap_like_the_switcher() {
        let (_v, ctx) = block();
        // Een yield met wektijd 500: Saved, hervat op + 4.
        ctx_write(ctx, CTX_A0, 500);
        ctx_write(ctx, CTX_RESUME, 0x5000_0010);
        assert_eq!(settle(ctx, CAUSE_ECALL_S, 0, false), Back::Yield);
        assert_eq!(ctx_read(ctx, CTX_WAKE), 500);
        assert_eq!(ctx_read(ctx, CTX_RESUME), 0x5000_0014);
        assert_eq!(ctx_state(ctx), Some(CtxState::Saved));
        // De wekker: Saved, meteen weer aan de beurt, mepc blijft.
        assert_eq!(
            settle(ctx, CAUSE_INTERRUPT | IRQ_MTI, 0, false),
            Back::Timer
        );
        assert_eq!(ctx_read(ctx, CTX_WAKE), 0);
        assert_eq!(ctx_read(ctx, CTX_RESUME), 0x5000_0014);
        assert_eq!(settle(ctx, CAUSE_INTERRUPT | IRQ_MSI, 0, false), Back::Ipi);
        assert_eq!(settle(ctx, CAUSE_INTERRUPT | 11, 0, false), Back::Irq);
        // De kick (a7 = 2): een yield naar nu, wat a0 ook zegt.
        ctx_write(ctx, CTX_A7, A7_KICK);
        assert_eq!(settle(ctx, CAUSE_ECALL_S, 0, false), Back::Yield);
        assert_eq!(ctx_read(ctx, CTX_WAKE), 0);
        assert_eq!(ctx_read(ctx, CTX_RESUME), 0x5000_0018);
        assert_eq!(ctx_state(ctx), Some(CtxState::Saved));
        // Een intrekking leest de terugweg niet: dat doet `el2::next`, vóór
        // de volgende beurt, op beide architecturen.
        ctx_write(ctx, abi::layout::CTX_REVOKE, 1);
        assert_eq!(settle(ctx, CAUSE_ECALL_S, 0, false), Back::Yield);
        assert_eq!(ctx_state(ctx), Some(CtxState::Saved));
        // Exit (a7 = 1): dood.
        ctx_write(ctx, CTX_A7, 1);
        assert_eq!(settle(ctx, CAUSE_ECALL_S, 0, false), Back::Exit);
        assert_eq!(ctx_state(ctx), Some(CtxState::Dead));
    }

    #[test]
    fn a_fault_is_reported_on_the_control_page() {
        let (_v, ctx) = block();
        let mut page = vec![0u64; 512];
        ctx_write(ctx, CTX_CTRL_PA, page.as_mut_ptr() as u64);
        assert_eq!(settle(ctx, 7, 0x8000_0000, false), Back::Fault);
        assert_eq!(page[(CTRL_FAULT_VEC / 8) as usize], 1);
        assert_eq!(page[(CTRL_FAULT_ESR / 8) as usize], 7);
        assert_eq!(page[(CTRL_FAULT_FAR / 8) as usize], 0x8000_0000);
        assert_eq!(ctx_state(ctx), Some(CtxState::Dead));
        assert_eq!(settle(ctx, CAUSE_CAGE_VERIFY, 0x42, false), Back::Fault);
        assert_eq!(
            page[(CTRL_FAULT_VEC / 8) as usize],
            super::super::switch::FAULT_CAGE_VERIFY
        );
    }
}
