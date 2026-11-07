//! Het fysieke plan van de Pi's en de geheugenkaart uit de DTB.
//!
//! Beide Pi's booten identiek: de firmware laadt een raw image op 0x80000
//! (de Pi 5-EEPROM negeert `kernel_address`, gemeten 09-07; de Pi 4 laadt
//! daar per default) en levert ons op EL2 af, met TF-A op EL3 als
//! PSCI-leverancier via SMC. Dat is het EEPROM-laadadres, en daar woont de
//! kern dus.
//!
//! De eerste gigabyte, van onder naar boven:
//!
//! - 0x0000_0000 tot 0x0008_0000: TF-A (BL31 of de armstub) en zijn staat.
//!   Wij raken het niet aan, ook geen cache-onderhoud: TF-A draait mét
//!   caches en een invalidate zou zijn vuile regels weggooien.
//! - 0x0008_0000 tot 0x0800_0000: de kern-RAM: image, stack en heap
//!   (Go: `HopKernelStart` + 128 MB).
//! - 0x0800_0000 tot 0x1000_0000: het laadvenster: de DTB (config.txt
//!   `device_tree_address=0x0f000000`), de boot-scratch op 0x0F10_0000, en
//!   het image dat de firmware als `initramfs` neerlegt (0x0F20_0000). Op
//!   QEMU `raspi4b` legt `-initrd` hem op 128 MB en de DTB erachter: ook
//!   hier.
//! - 0x1000_0000 tot 0x1400_0000: de control-pages en de kooi-regio,
//!   Device: de app-cores lezen ze op EL2 met de MMU uit (Go: `NodeCtrlPA`
//!   0x1000_0000, `CagePA` 0x1200_0000).
//! - 0x1400_0000 tot 0x1500_0000: de DMA-regio, Normal non-cacheable: de
//!   NIC-helft onderin (Go: `NetDMAPA`), de mailbox-buffer erboven.
//! - daarboven: DRAM voor de pool, gemapt uit de DTB bij `discover`.
//!
//! Waarom de kaart van de rest uit de DTB komt en niet vast staat: een Pi
//! bestaat met 1 tot 16 GB, en RAM mappen dat er niet is geeft speculatieve
//! loads naar een gat. De vaste tabellen dekken alleen wat op élke Pi
//! bestaat; wat de firmware in `/memory` meldt, komt er bij boot bij, in
//! blokken van 1 GB waar de bank de hele gigabyte dekt en van 2 MB in de
//! gigabytes die een eigen tabel hebben (de eerste, en op de Pi 4 de vierde
//! waar de peripherals in liggen).

use abi::Region;
use bounded::BoundedVec;
use dev::Pa;

/// Een gigabyte: één niveau-1-blok.
pub const GB: u64 = 1 << 30;
/// Twee megabyte: één niveau-2-blok.
pub const MB2: u64 = 1 << 21;

/// Waar de firmware ons laadt en de kern begint.
pub const KERN_BASE: u64 = 0x0008_0000;
/// Het einde van de kern-RAM (`hopos/link-raspi.ld`: `KERN_END`).
pub const KERN_END: u64 = 0x0800_0000;
/// Het laadvenster: DTB, boot-scratch en de staging.
pub const LOADER: Region = Region::new(0x0800_0000, 0x0800_0000);
/// Waar config.txt de DTB legt.
pub const DTB_PA: u64 = 0x0F00_0000;
/// De boot-scratch van het plan (`abi::layout::BOOT_SCRATCH_LEN` bytes).
pub const BOOT_SCRATCH_PA: u64 = 0x0F10_0000;
/// Het maatwoord van een staging door de kern zelf (de flip-bundel,
/// `hopos/src/flip.rs`): het handoff-blob ligt in de 256 KiB eronder, de
/// trampoline op de boot-scratch-pagina's erboven.
pub const STAGE_HDR_PA: u64 = 0x0F1F_F000;
/// Waar config.txt het image van Hop laadt (`initramfs hop.elf 0x0f200000`).
pub const STAGE_PA: u64 = 0x0F20_0000;
/// De control-pages van de eigen cores van de kern.
pub const NODE_CTRL_PA: u64 = 0x1000_0000;
/// De kooi-regio.
pub const CAGE_PA: u64 = 0x1200_0000;
/// Het Device-venster: control-pages en kooi-regio.
pub const DEVICE_WINDOW: Region = Region::new(0x1000_0000, 0x0400_0000);
/// De DMA-regio, Normal non-cacheable.
pub const DMA: Region = Region::new(0x1400_0000, 0x0100_0000);
/// De NIC-helft (`abi::layout::NET_DMA_SIZE`).
pub const NET_DMA: Region = Region::new(0x1400_0000, 0x0080_0000);
/// De property-buffer van de VideoCore-mailbox: in de DMA-regio, dus
/// ongecachet, en onder de 1 GB die de VideoCore ziet.
pub const VCMAIL_BUF: u64 = 0x1480_0000;
/// Alles onder dit adres staat vast in de tabellen en is nooit pool.
pub const FIXED_END: u64 = 0x1500_0000;

const _: () = {
    assert!(KERN_END == LOADER.base);
    assert!(LOADER.base + LOADER.size == DEVICE_WINDOW.base);
    assert!(DEVICE_WINDOW.base + DEVICE_WINDOW.size == DMA.base);
    assert!(DMA.base + DMA.size == FIXED_END);
    assert!(NET_DMA.size == abi::layout::NET_DMA_SIZE);
    assert!(VCMAIL_BUF >= NET_DMA.base + NET_DMA.size);
    assert!(VCMAIL_BUF + driver_vcmail::BUFFER_BYTES as u64 <= DMA.base + DMA.size);
    assert!(BOOT_SCRATCH_PA + 0x3000 + 0x4_0000 <= STAGE_HDR_PA && STAGE_HDR_PA + 8 <= STAGE_PA);
    assert!(DTB_PA >= LOADER.base && STAGE_PA < LOADER.base + LOADER.size);
    assert!(FIXED_END.is_multiple_of(MB2));
};

/// De grootste staging: tot het einde van het laadvenster.
pub const STAGE_MAX: u64 = LOADER.base + LOADER.size - STAGE_PA;

/// Hoeveel stukken gemapt RAM we bijhouden.
pub const MAX_MAPPED: usize = 32;

/// Eén gigabyte met een eigen niveau-2-tabel.
#[derive(Copy, Clone, Debug)]
pub struct L2 {
    /// Het nummer van de gigabyte (het niveau-1-index).
    pub gb: u64,
    /// De tabel.
    pub table: Pa,
    /// Het deel van deze gigabyte dat de vaste tabel al mapt (kern,
    /// vensters, peripherals); daar komt nooit RAM bij.
    pub fixed: Region,
}

/// De tabellen van een board: de niveau-1-tabel en de gigabytes met een
/// eigen niveau-2-tabel.
#[derive(Copy, Clone, Debug)]
pub struct Tables {
    /// De niveau-1-tabel (`__boot_ttbr0`).
    pub l1: Pa,
    /// De niveau-2-tabellen (hoogstens twee: de eerste gigabyte, en op de
    /// Pi 4 die met de peripherals).
    pub l2: [Option<L2>; 2],
}

/// Dekt een van de banken `[base, base+size)` volledig?
fn covered(banks: &[Region], base: u64, size: u64) -> bool {
    banks
        .iter()
        .any(|b| b.base <= base && base + size <= b.base.saturating_add(b.size))
}

/// Voegt `r` toe aan de gemapte stukken, of breidt het laatste uit.
fn note(mapped: &mut BoundedVec<Region, MAX_MAPPED>, r: Region) {
    if let Some(last) = mapped.as_mut_slice().last_mut()
        && last.base + last.size == r.base
    {
        last.size += r.size;
        return;
    }
    // Vol: het stuk wordt niet genoteerd, dus ook nooit pool. Liever RAM
    // laten liggen dan RAM uitdelen dat niemand bijhoudt.
    let _ = mapped.push(r);
}

/// Rekent de geheugenkaart uit de banken van de DTB: geeft elke tabelregel
/// die erbij moet aan `put` (het adres van de regel, de beschrijving) en
/// geeft de stukken RAM die daarna gemapt zijn. Alleen regels die nu leeg
/// zijn: niets wat vast staat wordt overschreven.
///
/// `banks` moeten samengesmolten zijn (`abi::layout::coalesce`).
pub fn plan_ram(
    banks: &[Region],
    t: &Tables,
    normal: fn(u64) -> u64,
    mut put: impl FnMut(Pa, u64),
) -> BoundedVec<Region, MAX_MAPPED> {
    let mut mapped = BoundedVec::new();
    for gb in 0..512u64 {
        let base = gb * GB;
        if let Some(l2) = t.l2.iter().flatten().find(|l| l.gb == gb) {
            for b in 0..512u64 {
                let a = base + b * MB2;
                let blk = Region::new(a, MB2);
                if l2.fixed.overlaps(blk) || !covered(banks, a, MB2) {
                    continue;
                }
                put(l2.table.add(8 * b), normal(a));
                note(&mut mapped, blk);
            }
        } else if covered(banks, base, GB) {
            put(t.l1.add(8 * gb), normal(base));
            note(&mut mapped, Region::new(base, GB));
        }
    }
    mapped
}

/// Hoeveel gaten er kunnen zijn: de drie vaste plus elk
/// `/memreserve/`-blok dat de DTB kan dragen.
pub const MAX_HOLES: usize = 3 + fw::fdt::MAX_RESERVE;

/// De gaten in de pool: alles wat vast staat, plus wat de firmware voor
/// zich houdt, de DTB zelf en de staging.
pub fn holes(reserve: &[Region], dtb: Region, stage: Region) -> BoundedVec<Region, MAX_HOLES> {
    let mut h = BoundedVec::new();
    for r in [Region::new(0, FIXED_END), dtb, stage]
        .into_iter()
        .chain(reserve.iter().copied())
    {
        // Past altijd: `reserve` komt uit een lijst van hoogstens
        // MAX_RESERVE, en de capaciteit telt die mee.
        let _ = h.push(r);
    }
    h
}
