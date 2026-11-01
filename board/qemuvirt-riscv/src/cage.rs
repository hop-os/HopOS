//! De zelftest van de riscv-kooi op QEMU: de M-mode-switcher
//! (`cpu::riscv::switch`) op hart 1, met een bewoner van zes instructies in
//! S-mode achter PMP plus Sv39. Het bewijs dat de onderdelen van de kooi op
//! het doel samen werken, vóór de lijm van `hopos` ze aan de lifecycle hangt
//! (docs/boards-riscv.md):
//!
//! 1. **Parkeren en de kick**: hart 1 komt via de parkeerlus van de boot-stub
//!    (`start_hart`, `msip`) in `parkenter` en slaapt in `wfi` op zijn
//!    `mtimecmp`.
//! 2. **Koude boot met relocatie**: de bewoner staat fysiek in de pool en is
//!    gelinkt op `LINK_BASE`; de Sv39-tabel legt het ene op het andere, het
//!    TOR-venster begrenst. Hij yieldt (`ecall`, a7 = 0) en de switcher
//!    bewaart hem en hervat hem (`mepc + 4`); dan exit (a7 = 1).
//! 3. **De whitelist houdt**: een tweede bewoner zonder vertaling schrijft
//!    naar 0x8000_0000 (de kern); PMP weigert (mcause 7), de switcher meldt
//!    het op zijn control-page en zet hem dood.
//! 4. **De kill-tick**: een derde bewoner die nooit yieldt (`j .`) wordt
//!    ingetrokken (`CTX_REVOKE`) zonder bel; de tick van de switcher ziet het
//!    woord en zet hem dood.
//! 5. **De intrekking van een slaper**: een vierde yieldt met een wektijd in
//!    de verre toekomst en wordt ingetrokken met de bel; de rotatie zet hem
//!    dood zonder hem nog één instructie te geven.
//!
//! Draait bij boot op hart 0, vóór de slots de pool en de kooi-regio
//! krijgen; wat hij beschrijft, wist hij weer.

use abi::hopabi::{CTRL_FAULT_ESR, CTRL_FAULT_FAR, CTRL_FAULT_VEC};
use abi::layout::{
    CTX_BOOT_ARG, CTX_BOOT_PC, CTX_CTRL_PA, CTX_LEN, CTX_REGIME, CTX_RESUME, CTX_REVOKE, CTX_STATE,
    CtxState, LINK_BASE, PARK_MBOX_LEN, SCHED_CLINT_PA, SCHED_MSIP_PA, SCHED_S2_PA,
    SCHED_SLEEP_CAP, SCHED_TICK_TICKS,
};
use cpu::riscv::pmp::{self, Window};
use cpu::riscv::sv39::{self, Attrs, MapWindow, Tables};
use cpu::riscv::switch::{REGIME_PMPADDR0, REGIME_PMPCFG0, REGIME_SATP};
use dev::Pa;

/// De bewoner die yieldt en exit: `li a7,0; li a0,0; ecall; li a7,1;
/// ecall; j .`.
const YIELD_EXIT: [u32; 6] = [
    0x0000_0893,
    0x0000_0513,
    0x0000_0073,
    0x0010_0893,
    0x0000_0073,
    0x0000_006f,
];
/// De bewoner die de kooi test: `li t0,1; slli t0,t0,31; sd zero,0(t0);
/// j .` (een store naar 0x8000_0000, de kern).
const ESCAPE: [u32; 4] = [0x0010_0293, 0x01f2_9293, 0x0002_b023, 0x0000_006f];
/// De bewoner die nooit yieldt: `j .`.
const SPIN: [u32; 1] = [0x0000_006f];
/// De bewoner die slaapt tot nooit: `li a0,-1; li a7,0; ecall; j .` (de
/// wektijd u64::MAX, zonder het no-peek-bit 0x7fff...).
const SLEEP: [u32; 4] = [0xfff0_0513, 0x0000_0893, 0x0000_0073, 0x0000_006f];

/// De kill-tick van de zelftest: 1 ms, zodat de toets snel is.
const TICK_NS: u64 = 1_000_000;

/// Wat de zelftest zag.
#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    /// De bewaarde hervat-PC na de yield (verwacht `LINK_BASE + 12`).
    pub resume: u64,
    /// De eindstaat van de eerste bewoner (verwacht Dead).
    pub first: u64,
    /// De eindstaat van de tweede (verwacht Dead).
    pub second: u64,
    /// mcause, mtval en vec op de control-page van de tweede.
    pub fault: (u64, u64, u64),
    /// De eindstaat van de spinner na de intrekking (verwacht Dead), en na
    /// hoeveel microseconden.
    pub spin: (u64, u64),
    /// De eindstaat van de slaper na de intrekking (verwacht Dead), en zijn
    /// staat ervoor (verwacht Saved).
    pub sleeper: (u64, u64),
}

impl Report {
    /// Slaagde alles?
    #[must_use]
    pub fn ok(&self) -> bool {
        self.resume == LINK_BASE + 12
            && self.first == CtxState::Dead as u64
            && self.second == CtxState::Dead as u64
            && self.fault == (7, 0x8000_0000, 1)
            && self.spin.0 == CtxState::Dead as u64
            && self.sleeper == (CtxState::Dead as u64, CtxState::Saved as u64)
    }
}

fn copy_code(pa: u64, code: &[u32]) {
    for (i, w) in code.iter().enumerate() {
        dev::write32(Pa(pa + 4 * i as u64), *w);
    }
    dev::push(Pa(pa), 4 * code.len());
}

/// Trekt de bewoner van `ctx` in (het woord van de kern, eigen regel).
fn revoke(ctx: Pa) {
    dev::write64(ctx.add(CTX_REVOKE), 1);
    dev::push(ctx.add(CTX_REVOKE), 8);
}

fn wait_state(ctx: Pa, want: u64, ms: u64) -> u64 {
    let mut s = 0;
    let _ = dev::poll_until(cpu::riscv::idle::now, ms * 1_000_000, || {
        dev::pull(ctx, 8);
        s = dev::read64(ctx.add(CTX_STATE));
        s == want
    });
    s
}

/// De zelftest: `sched` en `ctx` zijn het sched-blok van hart `hart` en het
/// ctx-blok van slot 1 in de kooi-regio (`cage_pa`), `part` een stuk van
/// 2 MB uit de pool. Geeft wat hij zag; de aanroeper logt.
pub fn selftest(
    cage_pa: u64,
    sched: Pa,
    ctx: Pa,
    hart: usize,
    part: u64,
    clint: cpu::riscv::clint::Clint,
) -> Result<Report, &'static str> {
    // De bewonerslijst ligt achter de partitie, in de pool (zoals het
    // kooi-geheugen hem uit de pool neemt), net als het ctx-blok.
    let list = Pa(part + 0x21_0000);
    let ctrl = part + 0x10_0000;
    let tables = part + 0x18_0000;
    dev::clear(sched, PARK_MBOX_LEN as usize);
    dev::clear(ctx, CTX_LEN as usize);
    dev::clear(Pa(ctrl), 4096);
    dev::write64(sched.add(SCHED_S2_PA), cage_pa);
    dev::write64(sched.add(SCHED_CLINT_PA), clint.mtimecmp(hart).0);
    dev::write64(sched.add(SCHED_MSIP_PA), clint.msip(hart).0);
    dev::write64(
        sched.add(SCHED_SLEEP_CAP),
        cpu::riscv::idle::ns_to_ticks(cpu::riscv::idle::WFI_CAP_QEMU_NS, cpu::riscv::idle::hz()),
    );
    dev::write64(
        sched.add(SCHED_TICK_TICKS),
        cpu::riscv::idle::ns_to_ticks(TICK_NS, cpu::riscv::idle::hz()),
    );
    cpu::el2::roster::grow(sched, list, 1);
    cpu::el2::roster::reset(sched, Some(ctx)).map_err(|_| "roster")?;
    dev::push(sched, PARK_MBOX_LEN as usize);

    // De eerste bewoner: gelinkt op LINK_BASE, fysiek op `part`.
    copy_code(part, &YIELD_EXIT);
    let window = Window {
        base: part,
        size: 2 << 20,
        r: true,
        w: true,
        x: true,
    };
    let enc = pmp::encode(&[window], pmp::QEMU).map_err(|_| "pmp encode")?;
    let map = [MapWindow {
        link: LINK_BASE,
        phys: part,
        size: 2 << 20,
        r: true,
        w: true,
        x: true,
        device: false,
    }];
    let mut t = Tables::new();
    let satp = sv39::relocate(tables, &map, Attrs::Spec, &mut t).map_err(|_| "sv39")?;
    for (i, page) in t.pages.iter().take(t.used).enumerate() {
        for (j, e) in page.iter().enumerate() {
            dev::write64(Pa(tables + (i * 4096 + j * 8) as u64), *e);
        }
    }
    dev::push(Pa(tables), t.len() as usize);
    arm(ctx, &enc, satp, LINK_BASE, ctrl);
    cpu::riscv::boot::start_hart(
        hart,
        cpu::riscv::switch::park_pc(),
        sched.0,
        clint.msip(hart),
    )
    .map_err(|_| "start_hart")?;
    let first = wait_state(ctx, CtxState::Dead as u64, 200);
    dev::pull(ctx.add(CTX_RESUME), 8);
    let resume = dev::read64(ctx.add(CTX_RESUME));

    // De tweede: zonder vertaling, alleen de whitelist, en hij breekt uit.
    copy_code(part, &ESCAPE);
    arm(ctx, &enc, 0, part, ctrl);
    clint.set_msip(hart, true);
    let second = wait_state(ctx, CtxState::Dead as u64, 200);
    dev::pull(Pa(ctrl + 0x40), 64);
    let fault = (
        dev::read64(Pa(ctrl + CTRL_FAULT_ESR)),
        dev::read64(Pa(ctrl + CTRL_FAULT_FAR)),
        dev::read64(Pa(ctrl + CTRL_FAULT_VEC)),
    );

    // De spinner: draait, en wordt zonder bel ingetrokken. Alleen de
    // kill-tick kan hem zien.
    copy_code(part, &SPIN);
    arm(ctx, &enc, 0, part, ctrl);
    clint.set_msip(hart, true);
    let _ = wait_state(ctx, CtxState::Running as u64, 200);
    let t0 = cpu::riscv::idle::now();
    revoke(ctx);
    let spun = wait_state(ctx, CtxState::Dead as u64, 200);
    let spin = (spun, cpu::riscv::idle::now().saturating_sub(t0) / 1000);

    // De slaper: yieldt tot nooit, en wordt met de bel ingetrokken.
    copy_code(part, &SLEEP);
    arm(ctx, &enc, 0, part, ctrl);
    clint.set_msip(hart, true);
    let before = wait_state(ctx, CtxState::Saved as u64, 200);
    revoke(ctx);
    clint.set_msip(hart, true);
    let after = wait_state(ctx, CtxState::Dead as u64, 200);

    // Opruimen: de lijst weg, de partitie en wat erachter lag terug naar nul.
    dev::clear(sched, PARK_MBOX_LEN as usize);
    dev::push(sched, PARK_MBOX_LEN as usize);
    dev::clear(Pa(part), 2 << 20);
    dev::clear(ctx, CTX_LEN as usize);
    dev::clear(list, abi::layout::CAGE_STRIDE as usize);
    Ok(Report {
        resume,
        first,
        second,
        fault,
        spin,
        sleeper: (after, before),
    })
}

/// Zet een koude boot klaar: het regime, de ingang, de control-page, en als
/// laatste de staat.
fn arm(ctx: Pa, enc: &pmp::Encoded, satp: u64, entry: u64, ctrl: u64) {
    dev::clear(ctx, CTX_LEN as usize);
    let r = ctx.add(CTX_REGIME);
    dev::write64(r.add(REGIME_SATP), satp);
    dev::write64(r.add(REGIME_PMPCFG0), enc.cfg);
    for (k, a) in enc.addr.iter().enumerate() {
        dev::write64(r.add(REGIME_PMPADDR0 + 8 * k as u64), *a);
    }
    dev::write64(ctx.add(CTX_BOOT_PC), entry);
    dev::write64(ctx.add(CTX_BOOT_ARG), 0);
    dev::write64(ctx.add(CTX_CTRL_PA), ctrl);
    dev::write64(ctx.add(CTX_STATE), CtxState::BootPending as u64);
    dev::push(ctx, CTX_LEN as usize);
}
