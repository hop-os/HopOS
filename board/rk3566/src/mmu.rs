//! De identity map van de RK3566, als data in het image.
//!
//! De boot-stub (`cpu::boot`) zet TTBR0_EL2 op `__boot_ttbr0` en doet de
//! MMU aan; welk bereik Device en welk Normal is, weet alleen het board.
//! De tabellen staan vast in `.rodata`.
//!
//! De keuze is die van de Go-kern op dit bord (tamago: Normal alleen binnen
//! de eigen RAM-declaratie, al het overige Device-nGnRnE), met de
//! DMA-regio's als Normal-NC zoals `memattr` ze in Go omzette. GEMETEN
//! 20-09: de NIC-DMA van Device naar Normal-NC bracht de inbound van 15,5
//! naar 56,6 MB/s (Device-geheugen is duur om te LEZEN: elke load een
//! aparte, strikt geordende transactie, en de ontvangstkant kopieert elk
//! frame eruit). Device heeft een tweede voordeel: de CPU speculeert er
//! nooit in, dus een gat in het DRAM (een bord van 1 GB onder een map van 4
//! GB) kan geen SError geven.
//!
//! - 0x0000_0000 tot 0x0020_0000: TF-A (bl31). Device, voor één pagina:
//!   het SCMI-shmem op 0x0010_f000 dat de TF-A aan de normal world geeft
//!   (de klok, `clock`; Linux mapt hetzelfde adres). De rest raken we niet
//!   aan, en Device met XN haalt de CPU nooit speculatief op.
//! - 0x0020_0000 tot 0x0220_0000: de firmware-keten (U-Boot). Device.
//! - 0x0220_0000 tot 0x0620_0000: de kern-RAM, Normal WB (64 MB, zoals Go).
//! - 0x0620_0000 tot 0x0640_0000: de structuren van de kern (control-pages,
//!   kooien, boot-scratch, levenstekens, zwarte doos): Device, coherent
//!   met een core die met de MMU uit binnenkomt.
//! - 0x0640_0000 tot 0x0780_0000: NIC-DMA, USB-DMA en de framebuffer:
//!   Normal-NC, behalve het bufferblok van de NIC (`NET_BUF`, 0x0660_0000
//!   tot 0x0680_0000): Normal-WB en XN, de driver veegt het (een frame uit
//!   NC kopiëren kostte de A55 35 µs, uit WB 1,9: driver/nic/stmmac, dwmac4).
//! - 0x0780_0000 tot 0x0880_0000: het staging-venster van de kern-flip.
//!   Device.
//! - 0x0880_0000 tot 0x1_0000_0000: de pool, de DTB en de initrd van U-Boot
//!   en de MMIO van de SoC (vanaf 0xF000_0000): Device.

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .rodata.pagetables, "a"
    .balign 4096
    .global __boot_ttbr0
__boot_ttbr0:
    .quad __boot_l2_lo + 3
    .quad {dev1}
    .quad {dev2}
    .quad {dev3}
    .fill 508, 8, 0

    .balign 4096
__boot_l2_lo:
    .quad {dev0}
    .set blk, 1
    .rept {fw_blocks}
    .quad {dev0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {kern_blocks}
    .quad {ram0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {struct_blocks}
    .quad {dev0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {nc_lo_blocks}
    .quad {nc0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {buf_blocks}
    .quad {wb_xn0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {nc_hi_blocks}
    .quad {nc0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {pool_blocks}
    .quad {dev0} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
"#,
    dev0 = const cpu::boot::block(0, cpu::boot::ATTR_DEVICE),
    ram0 = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL),
    nc0 = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL_NC),
    wb_xn0 = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL) | cpu::boot::xn(false),
    dev1 = const cpu::boot::block(0x4000_0000, cpu::boot::ATTR_DEVICE),
    dev2 = const cpu::boot::block(0x8000_0000, cpu::boot::ATTR_DEVICE),
    dev3 = const cpu::boot::block(0xC000_0000, cpu::boot::ATTR_DEVICE),
    fw_blocks = const FW_BLOCKS,
    kern_blocks = const KERN_BLOCKS,
    struct_blocks = const STRUCT_BLOCKS,
    nc_lo_blocks = const NC_LO_BLOCKS,
    buf_blocks = const BUF_BLOCKS,
    nc_hi_blocks = const NC_BLOCKS - NC_LO_BLOCKS - BUF_BLOCKS,
    pool_blocks = const POOL_BLOCKS,
);

/// Eén blok van niveau 2.
const MB2: u64 = 0x20_0000;
/// De firmware-blokken na blok 0 (de TF-A, Device).
const FW_BLOCKS: u64 = (crate::KERN_RAM.base.0 - crate::DRAM_BASE) / MB2;
/// De kern-RAM.
const KERN_BLOCKS: u64 = crate::KERN_RAM.size / MB2;
/// Het structurenvenster.
const STRUCT_BLOCKS: u64 = crate::STRUCT_WINDOW.size / MB2;
/// De DMA-regio's (ongecachet, op het bufferblok van de NIC na).
const NC_BLOCKS: u64 = crate::DMA.size / MB2;
/// De NC-blokken vóór het bufferblok van de NIC.
const NC_LO_BLOCKS: u64 = (crate::NET_BUF.base.0 - crate::DMA.base.0) / MB2;
/// Het bufferblok van de NIC, Normal-WB.
const BUF_BLOCKS: u64 = crate::NET_BUF.size / MB2;
/// De rest van de eerste gigabyte.
const POOL_BLOCKS: u64 = 512 - 1 - FW_BLOCKS - KERN_BLOCKS - STRUCT_BLOCKS - NC_BLOCKS;

// De `.rept`-tellingen moeten op het plan passen: aaneengesloten, 2
// MB-gealigneerd, en samen precies de eerste gigabyte.
const _: () = {
    use crate::{DMA, DRAM_BASE, KERN_RAM, NET_BUF, STAGE_WINDOW, STRUCT_WINDOW};
    assert!(DRAM_BASE == MB2);
    assert!(KERN_RAM.base.0.is_multiple_of(MB2) && KERN_RAM.size.is_multiple_of(MB2));
    assert!(KERN_RAM.base.0 + KERN_RAM.size == STRUCT_WINDOW.base.0);
    assert!(STRUCT_WINDOW.base.0 + STRUCT_WINDOW.size == DMA.base.0);
    assert!(DMA.base.0 + DMA.size == STAGE_WINDOW.base.0);
    assert!(1 + FW_BLOCKS + KERN_BLOCKS + STRUCT_BLOCKS + NC_BLOCKS + POOL_BLOCKS == 512);
    assert!(POOL_BLOCKS > 0);
    // Het bufferblok van de NIC: hele blokken, binnen de DMA-regio.
    assert!(NET_BUF.base.0.is_multiple_of(MB2) && NET_BUF.size.is_multiple_of(MB2));
    assert!(NET_BUF.base.0 >= DMA.base.0 && NC_LO_BLOCKS + BUF_BLOCKS <= NC_BLOCKS);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_matches_the_plan() {
        assert_eq!(FW_BLOCKS, 16);
        assert_eq!(KERN_BLOCKS, 32);
        assert_eq!(STRUCT_BLOCKS, 1);
        assert_eq!(NC_BLOCKS, 10);
        // Blok 51 (0x0660_0000) is het bufferblok van de NIC.
        assert_eq!((NC_LO_BLOCKS, BUF_BLOCKS), (1, 1));
        assert_eq!(
            crate::NET_BUF.base.0 / MB2,
            1 + FW_BLOCKS + KERN_BLOCKS + STRUCT_BLOCKS + NC_LO_BLOCKS
        );
        assert_eq!(POOL_BLOCKS, 452);
    }
}
