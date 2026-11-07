//! Het PA-plan: waar op dít board de kern zijn eigen structuren fysiek legt.
//!
//! Het board vult een [`PlanSpec`] en krijgt er na validatie een [`Plan`]
//! voor terug; de kern leest adressen alleen via het plan. Er is geen
//! globale `UsePlan` meer zoals in Go: het plan is een waarde met één
//! eigenaar (de boot-code, daarna de lifecycle-actor), en een plan dat niet
//! valideert bestaat niet. Apps zien hier niets van; hun IPA-beeld is op
//! elk board gelijk en de stage-2 vertaalt.
//!
//! Wat hier NIET staat: wat per slot is. De staart van een slot ligt in zijn
//! partitie (de kern rekent hem uit met [`super::Tail`]), en zijn kooi-blok
//! (stage-2-tabellen en ctx-blok) komt bij de plaatsing uit de pool. Het plan
//! is per core, en cores zijn vast: er is geen maximum aantal slots.

use super::{
    BOOT_SCRATCH_LEN, CORE_CTX_OFF, CORES_MAX, CTRL_STRIDE, CTX_LEN, Core, HOP_RAM_START,
    PARK_CODE_OFF, PARK_MBOX_LEN, PARK_MBOX_OFF, SWITCH_CODE_OFF, USB_DMA_SIZE, cage_len,
    node_ctrl_len,
};
use crate::{Error, Region, Result};
use bounded::BoundedVec;
use dev::Pa;

/// Hoeveel regio's een pool maximaal draagt. Firmware beschrijft
/// aaneengesloten RAM als duizenden descriptors, maar na [`coalesce`] zijn
/// het er een handvol per bank; 64 laat ruimte voor elk gat dat een board
/// uitknipt.
pub const POOL_MAX: usize = 64;

/// De korrel van de pool: stage-2-blokken zijn 2 MB.
const GRAIN: u64 = 2 << 20;

/// Een lijst vrije DRAM-regio's voor app-partities.
pub type Pool = BoundedVec<Region, POOL_MAX>;

/// Toetst dat een regio niet voorbij het einde van de adresruimte loopt.
/// Onbruikbare firmwaregeometrie is een bootfout, geen lege pool die een
/// terugval toestaat.
fn check_region(r: Region) -> Result {
    match r.end() {
        Some(_) => Ok(()),
        None => Err(Error::RegionOverflow {
            base: r.base,
            size: r.size,
        }),
    }
}

/// Voegt `r` achteraan toe, of meldt dat de pool vol is.
fn push(out: &mut Pool, r: Region) -> Result {
    out.push(r).map_err(|_| Error::TooMany {
        what: "pool regions",
        cap: POOL_MAX,
    })
}

/// Een pool uit gegeven regio's (een board met een vaste indeling, of een
/// kaart die al gesorteerd en gesmolten is), of [`Error::TooMany`] boven
/// [`POOL_MAX`].
pub fn pool_of(regions: impl IntoIterator<Item = Region>) -> Result<Pool> {
    let mut out = Pool::new();
    for r in regions {
        push(&mut out, r)?;
    }
    Ok(out)
}

/// Sorteert regio's op basis en smelt overlappende en aangrenzende samen;
/// geeft het aantal regio's dat overblijft (vooraan in `regs`).
///
/// Firmware-memory-maps (UEFI) beschrijven aaneengesloten RAM als duizenden
/// losse descriptors; die grenzen zijn administratie, geen RAM-grenzen.
/// Zonder samensmelten raakt een pool "vol of gefragmenteerd" terwijl er
/// honderden GB vrij is (Altra, gemeten 14-07: 300 GB pool, geen gat van
/// 96 MB meer na 12 taken). Aanroepen VÓÓR uitlijnen: elke kunstmatige grens
/// kost anders tot 4 MB.
pub fn coalesce(regs: &mut [Region]) -> Result<usize> {
    for &r in regs.iter() {
        check_region(r)?;
    }
    regs.sort_unstable_by_key(|r| r.base);
    let mut n = 0usize;
    for i in 0..regs.len() {
        let r = regs[i];
        if r.size == 0 {
            continue;
        }
        if n > 0 {
            let last = &mut regs[n - 1];
            // Beide zijn door check_region gekomen, dus geen omloop.
            let last_end = last.base + last.size;
            if r.base <= last_end {
                let end = r.base + r.size;
                if end > last_end {
                    last.size = end - last.base;
                }
                continue;
            }
        }
        regs[n] = r;
        n += 1;
    }
    Ok(n)
}

/// Bouwt een partitie-pool uit de fysieke geheugenbanken minus alle gaten
/// (de kern, control-regio's, DTB, `/memreserve/`).
///
/// Pure intervalrekenkunde, board-neutraal. Elk resultaat wordt naar binnen
/// op 2 MB uitgelijnd en stukken kleiner dan `min` vallen weg. Een lege pool
/// is een geldig antwoord; een omlopende regio is een fout.
pub fn carve_pool(banks: &[Region], holes: &[Region], min: u64) -> Result<Pool> {
    let mut regs = Pool::new();
    for &b in banks {
        push(&mut regs, b)?;
    }
    let n = coalesce(&mut regs)?;
    regs.truncate(n);
    for &h in holes {
        check_region(h)?;
        if h.size == 0 {
            continue;
        }
        let h_end = h.base + h.size;
        let mut next = Pool::new();
        for &r in regs.iter() {
            let r_end = r.base + r.size;
            if h_end <= r.base || h.base >= r_end {
                push(&mut next, r)?;
                continue;
            }
            if h.base > r.base {
                push(&mut next, Region::new(r.base, h.base - r.base))?;
            }
            if h_end < r_end {
                push(&mut next, Region::new(h_end, r_end - h_end))?;
            }
        }
        regs = next;
    }
    let mut out = Pool::new();
    for &r in regs.iter() {
        let pad = r.base.wrapping_neg() & (GRAIN - 1);
        if pad >= r.size {
            continue;
        }
        let base = r.base + pad;
        let end = (r.base + r.size) & !(GRAIN - 1);
        if end > base && end - base >= min {
            push(&mut out, Region::new(base, end - base))?;
        }
    }
    Ok(out)
}

/// Wat een board over zijn fysieke plan zegt, vóór validatie.
///
/// Een veld op 0 betekent "dit board heeft er geen" waar dat mag; de
/// verplichte velden staan bij [`Plan::new`].
#[derive(Clone, Debug, Default)]
pub struct PlanSpec {
    /// De control-pages van de éígen cores van de kern (node-SMP): die
    /// hebben geen partitie en dus geen staart. Eén pagina per core
    /// ([`node_ctrl_len`]), 4 KB-gealigneerd. Verplicht.
    pub node_ctrl_pa: u64,
    /// De kooi-regio ([`cage_len`]): blok 0 (de EL2-vectoren, de parkeerlus,
    /// de sched-blokken, de switch-code) en de ctx-blokken van de cores.
    /// 2 KB-gealigneerd (de eis van VBAR_EL2). Verplicht: zonder las de
    /// Go-kern vanaf adres nul (gemeten 30-07).
    pub cage_pa: u64,
    /// Het venster dat de identity map van de kern als Device mapt (maat 0 =
    /// geen eis). De hele kooi-regio moet erin vallen: de app-cores lezen
    /// hem op EL2 met de MMU uit, en een gecachte park-mailbox is op ijzer
    /// een verloren startschot.
    pub device_window: Region,
    /// De console-zwarte-doos, buiten alles wat firmware bij een verse boot
    /// beschrijft (maat 0 = geen). Een plan-veld en geen
    /// boot-scratch-offset: op de M4 legt iBoot het bootobject terug over
    /// de scratch (gemeten 01-09).
    pub black_box: Region,
    /// De vectortabel van de kern-core zelf (ARM, 2 KB-gealigneerd; 0 op
    /// RISC-V).
    pub trap_vec_pa: u64,
    /// De fysieke boot-scratch (vast in `cpuinit`). Verplicht.
    pub boot_scratch_pa: u64,
    /// De NIC-DMA-regio, buiten elke RAM-declaratie (0 = geen plan).
    pub net_dma_pa: u64,
    /// De xHCI-DMA-regio van [`USB_DMA_SIZE`] (0 = geen USB). Een board zet
    /// hem in elke smaak, ook headless: één plan per board, niet een plan
    /// dat van vorm verandert met een feature.
    pub usb_dma_pa: u64,
    /// Vrij DRAM voor app-partities, op 2 MB-korrel. Verplicht niet-leeg.
    pub pool: Pool,
    /// De herbruikbare reservering van de koude kern, flip-staart
    /// inbegrepen (maat 0 = buiten de allocator). Bevat geen firmware-,
    /// DMA- of control-structuren.
    pub kernel: Region,
    /// Waar het DRAM van dit board begint, het meetpunt van
    /// [`Plan::required_ram`] (0 = [`HOP_RAM_START`]). Zonder rekende de
    /// LicheeRV "layout requires 1216 MB" op een 256 MB-board (30-07).
    pub ram_base: u64,
    /// Het aantal fysieke app-cores, geklemd op `1..CORES_MAX` (0 = 3, de
    /// Pi- en QEMU-standaard van Go).
    pub app_cores: usize,
    /// De fysieke index van de OS-core: de core waar de kern woont en die
    /// hij met Hop deelt (PORT.md beslissing 2, 30-09). In het plan is dat
    /// altijd logische core 0 (zijn sched-blok is blok 0); de app-cores
    /// `1..=app_cores` zijn de andere fysieke cores op volgorde
    /// ([`Plan::phys_core`]). 0 = de boot-core, de default op elk board.
    pub os_core: usize,
}

/// Een gevalideerd PA-plan.
///
/// # Invariants
///
/// Alle verplichte velden zijn gezet en gealigneerd; geen gereserveerde
/// regio loopt om; elke pool- en kernregio is niet-leeg, 2 MB-gealigneerd
/// en overlapt geen andere pool-, kern- of gereserveerde regio;
/// `1 <= app_cores < CORES_MAX`.
#[derive(Clone, Debug)]
pub struct Plan {
    spec: PlanSpec,
}

/// Toetst een verplicht, gealigneerd adres.
fn required(what: &'static str, v: u64, align: u64) -> Result {
    if v == 0 {
        return Err(Error::Missing(what));
    }
    aligned(what, v, align)
}

/// Toetst een uitlijning.
fn aligned(what: &'static str, v: u64, align: u64) -> Result {
    if !v.is_multiple_of(align) {
        return Err(Error::Misaligned {
            what,
            value: v,
            align,
        });
    }
    Ok(())
}

/// Klemt het aantal app-cores op `1..CORES_MAX`, met 3 voor 0.
fn clamp_cores(n: usize) -> usize {
    if n == 0 { 3 } else { n.min(CORES_MAX - 1) }
}

impl Plan {
    /// Valideert het plan van een board.
    ///
    /// Liever hier hard falen dan een scheve map op een core. Alleen pool
    /// tegen gereserveerd wordt op overlap getoetst: trap-vectoren en
    /// boot-scratch mogen bewust in een grotere admin-regio liggen.
    pub fn new(mut spec: PlanSpec) -> Result<Plan> {
        required("node_ctrl_pa", spec.node_ctrl_pa, 0x1000)?;
        required("boot_scratch_pa", spec.boot_scratch_pa, 8)?;
        if spec.pool.is_empty() {
            return Err(Error::EmptyPool);
        }
        required("cage_pa", spec.cage_pa, 0x800)?;
        aligned("trap_vec_pa", spec.trap_vec_pa, 0x800)?;
        spec.app_cores = clamp_cores(spec.app_cores);
        // De OS-core is een van de `app_cores + 1` fysieke cores.
        if spec.os_core > spec.app_cores {
            return Err(Error::OutOfPlan {
                index: spec.os_core,
                max: spec.app_cores,
            });
        }

        let cage = Region::new(spec.cage_pa, cage_len(spec.app_cores));
        let w = spec.device_window;
        if w.size != 0
            && (cage.base < w.base
                || cage.base.saturating_add(cage.size) > w.base.saturating_add(w.size))
        {
            return Err(Error::Overlap { a: cage, b: w });
        }
        let mut reserved = BoundedVec::<Region, 8>::new();
        let fixed = [
            Region::new(spec.node_ctrl_pa, node_ctrl_len(spec.app_cores)),
            cage,
            Region::new(spec.boot_scratch_pa, BOOT_SCRATCH_LEN),
        ];
        let optional = [
            Region::new(spec.trap_vec_pa, 0x800),
            spec.black_box,
            Region::new(spec.usb_dma_pa, USB_DMA_SIZE),
            // De NIC-maat is board-specifiek (LicheeRV 448 KB, ARM 8 MB);
            // hier alleen het basisadres, het board bewaakt de carve.
            Region::new(spec.net_dma_pa, 1),
        ];
        for r in fixed
            .into_iter()
            .chain(optional.into_iter().filter(|r| r.base != 0))
        {
            check_region(r)?;
            reserved.push(r).map_err(|_| Error::TooMany {
                what: "reserved regions",
                cap: 8,
            })?;
        }

        let kernel = (spec.kernel.size != 0).then_some(spec.kernel);
        let mut seen = BoundedVec::<Region, { POOL_MAX + 1 }>::new();
        for r in spec.pool.iter().copied().chain(kernel) {
            check_region(r)?;
            if r.size == 0 {
                return Err(Error::Misaligned {
                    what: "empty pool region",
                    value: r.base,
                    align: GRAIN,
                });
            }
            aligned("pool region base", r.base, GRAIN)?;
            aligned("pool region size", r.size, GRAIN)?;
            for &o in seen.iter().chain(reserved.iter()) {
                if r.overlaps(o) {
                    return Err(Error::Overlap { a: r, b: o });
                }
            }
            seen.push(r).map_err(|_| Error::TooMany {
                what: "pool regions",
                cap: POOL_MAX,
            })?;
        }
        // INVARIANT: alle eisen hierboven zijn getoetst.
        Ok(Plan { spec })
    }

    /// Het aantal fysieke app-cores.
    #[must_use]
    pub fn app_cores(&self) -> usize {
        self.spec.app_cores
    }

    /// De fysieke index van de OS-core (logische core 0).
    #[must_use]
    pub fn os_core(&self) -> usize {
        self.spec.os_core
    }

    /// De fysieke index van logische core `core`: 0 is de OS-core, app-core
    /// `i` is de `i`-de fysieke core die niet de OS-core is. Met de OS-core
    /// op 0 is dat de identiteit (het plan van vóór 30-09).
    #[must_use]
    pub fn phys_core(&self, core: Core) -> usize {
        match core.get() {
            0 => self.spec.os_core,
            i if i <= self.spec.os_core => i - 1,
            i => i,
        }
    }

    /// De logische core van fysieke core `phys`, de inverse van
    /// [`phys_core`](Self::phys_core); `None` buiten de `app_cores + 1`
    /// cores van dit plan.
    #[must_use]
    pub fn logical_core(&self, phys: usize) -> Option<Core> {
        let os = self.spec.os_core;
        let i = match phys {
            p if p == os => 0,
            p if p < os => p + 1,
            p => p,
        };
        if i > self.spec.app_cores {
            return None;
        }
        Core::new(i)
    }

    /// Toetst een core tegen de cores van dit plan.
    fn within(&self, i: usize) -> Result {
        if i > self.spec.app_cores {
            return Err(Error::OutOfPlan {
                index: i,
                max: self.spec.app_cores,
            });
        }
        Ok(())
    }

    /// De control-page van een eigen core van de kern (node-SMP). Alleen
    /// voor node-cores: een app-slot vindt zijn page in zijn staart.
    pub fn node_ctrl_pa(&self, core: Core) -> Result<Pa> {
        self.within(core.get())?;
        Ok(Pa(self.spec.node_ctrl_pa + core.get() as u64 * CTRL_STRIDE))
    }

    /// De zwarte doos, als het board er een heeft.
    #[must_use]
    pub fn black_box(&self) -> Option<Region> {
        let b = self.spec.black_box;
        (b.base != 0 && b.size != 0).then_some(b)
    }

    /// De vectortabel van de kern-core, als het board er een heeft (ARM).
    #[must_use]
    pub fn trap_vec_pa(&self) -> Option<Pa> {
        (self.spec.trap_vec_pa != 0).then_some(Pa(self.spec.trap_vec_pa))
    }

    /// De NIC-DMA-regio, als het board er een plande.
    #[must_use]
    pub fn net_dma_pa(&self) -> Option<Pa> {
        (self.spec.net_dma_pa != 0).then_some(Pa(self.spec.net_dma_pa))
    }

    /// De xHCI-DMA-regio ([`USB_DMA_SIZE`]), als het board er een plande.
    #[must_use]
    pub fn usb_dma_pa(&self) -> Option<Pa> {
        (self.spec.usb_dma_pa != 0).then_some(Pa(self.spec.usb_dma_pa))
    }

    /// De basis van de kooi-regio: tevens de gedeelde EL2-vectoren van de
    /// app-cores.
    #[must_use]
    pub fn vec_base_pa(&self) -> Pa {
        Pa(self.spec.cage_pa)
    }

    /// Het ctx-blok van een secundaire core van een SMP-app (core 2 tot en
    /// met `app_cores`): de ctx-blokken van de cores achter blok 0.
    pub fn smp_ctx_pa(&self, core: Core) -> Result<Pa> {
        self.within(core.get())?;
        if core.get() < 2 {
            return Err(Error::OutOfPlan {
                index: core.get(),
                max: self.spec.app_cores,
            });
        }
        Ok(Pa(self.spec.cage_pa
            + CORE_CTX_OFF
            + core.get() as u64 * CTX_LEN))
    }

    /// De EL2-parkeerlus.
    #[must_use]
    pub fn park_code_pa(&self) -> Pa {
        Pa(self.spec.cage_pa + PARK_CODE_OFF)
    }

    /// Het sched-blok (met de park-mailbox) van een core.
    pub fn park_mbox_pa(&self, core: Core) -> Result<Pa> {
        self.within(core.get())?;
        Ok(Pa(self.spec.cage_pa
            + PARK_MBOX_OFF
            + core.get() as u64 * PARK_MBOX_LEN))
    }

    /// De switch-code-kopie.
    #[must_use]
    pub fn switch_code_pa(&self) -> Pa {
        Pa(self.spec.cage_pa + SWITCH_CODE_OFF)
    }

    /// De partitie-pool.
    #[must_use]
    pub fn pool(&self) -> &[Region] {
        &self.spec.pool
    }

    /// De herbruikbare kernreservering, als het board er een gaf.
    #[must_use]
    pub fn kernel(&self) -> Option<Region> {
        (self.spec.kernel.size != 0).then_some(self.spec.kernel)
    }

    /// Waar het DRAM van dit board begint.
    #[must_use]
    pub fn ram_base(&self) -> u64 {
        if self.spec.ram_base != 0 {
            self.spec.ram_base
        } else {
            HOP_RAM_START
        }
    }

    /// Het hoogste fysieke adres dat het plan aanraakt: control-pages,
    /// kooi-regio en pool.
    #[must_use]
    pub fn top_addr(&self) -> u64 {
        // Geen omloop: Plan::new toetste deze regio's.
        let mut top = self.spec.node_ctrl_pa + node_ctrl_len(self.spec.app_cores);
        top = top.max(self.spec.cage_pa + cage_len(self.spec.app_cores));
        for r in self.spec.pool.iter() {
            top = top.max(r.base + r.size);
        }
        top
    }

    /// Hoeveel aaneengesloten DRAM vanaf [`Plan::ram_base`] het plan eist.
    /// Minder dan dit, en slots vallen buiten het fysieke RAM: dan moet de
    /// kern weigeren in plaats van fantoomgeheugen uit te delen. 0 als het
    /// plan onder `ram_base` ligt.
    #[must_use]
    pub fn required_ram(&self) -> u64 {
        self.top_addr().saturating_sub(self.ram_base())
    }

    /// Haalt `[base, base + size)` uit de pool, vóór de lifecycle hem
    /// inleest. Een geadopteerde kern-flip gebruikt dit voor de carve van de
    /// vorige kern: daar staan de park-mailboxen van de app-cores nog
    /// (QEMU 17-09: "stage2: empty flip has an unparked core 1").
    pub fn exclude_from_pool(&mut self, base: u64, size: u64) -> Result {
        if size == 0 {
            return Ok(());
        }
        let hole = Region::new(base, size);
        check_region(hole)?;
        let end = base + size;
        let mut out = Pool::new();
        for &r in self.spec.pool.iter() {
            let r_end = r.base + r.size;
            if end <= r.base || base >= r_end {
                push(&mut out, r)?;
                continue;
            }
            if base > r.base {
                push(&mut out, Region::new(r.base, base - r.base))?;
            }
            if end < r_end {
                push(&mut out, Region::new(end, r_end - end))?;
            }
        }
        self.spec.pool = out;
        Ok(())
    }
}
