//! De framebuffer van de firmware: het Graphics Output Protocol (GOP), vóór
//! `ExitBootServices` uit de EFI-tabellen gelezen en daarna alleen nog als
//! getallen bewaard.
//!
//! Dit bezit de vier getallen van het beeld (basis, maat, pixelformaat,
//! scanlijn) en de ene tabelregel die het in de identity map zet. Wat erin
//! getekend wordt, is van `driver-fb`; wie het krijgt, van `gui-fbgrant`.
//!
//! Waar het vandaan komt: op QEMU onder EDK2 geeft `-device ramfb` een GOP
//! (QemuRamfbDxe), en zo werd de fb-grant op 19-07 bewezen: de klok van de
//! display-app tikte op het glas. De ramfb-vondst van die dag staat in
//! `kern::grants`: de buffer lag op 0x1_bc7a_0000, boven 4 GB, en daarom
//! ziet de app hem op een vast IPA en niet op zijn eigen adres. Op de O6N
//! en de Altra is het de GOP van hun eigen firmware; op ijzer nooit
//! gemeten (docs/gui.md).
//!
//! Na de exit zijn de boot services weg, de framebuffer niet: het is
//! geheugen dat de firmware voor het beeld reserveerde (EfiReservedMemory
//! of MMIO), en de scanout loopt door.

use crate::efi::Efi;
use crate::facts::GOP;
use crate::mmu::{self, Mmu};
use core::sync::atomic::Ordering::Relaxed;
use dev::Pa;
use driver_fb::Desc;

/// EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID (9042a9de-23dc-4a38-96fb-7aded080516a),
/// in geheugenvolgorde.
const GOP_GUID: [u8; 16] = [
    0xde, 0xa9, 0x42, 0x90, 0xdc, 0x23, 0x38, 0x4a, 0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a,
];

/// De offsets in de GOP-structuren (UEFI 2.10, §12.9).
mod off {
    /// `EFI_GRAPHICS_OUTPUT_PROTOCOL.Mode`.
    pub(super) const GOP_MODE: u64 = 0x18;
    /// `EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE.Info`.
    pub(super) const MODE_INFO: u64 = 0x08;
    /// `EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE.FrameBufferBase`.
    pub(super) const MODE_FB_BASE: u64 = 0x18;
    /// `EFI_GRAPHICS_OUTPUT_MODE_INFORMATION.HorizontalResolution`; de
    /// verticale volgt op +4, het pixelformaat op +8.
    pub(super) const INFO_H_RES: u64 = 0x04;
    /// `EFI_GRAPHICS_OUTPUT_MODE_INFORMATION.PixelsPerScanLine`.
    pub(super) const INFO_SCAN: u64 = 0x20;
}

/// PixelRedGreenBlueReserved8BitPerColor: RGB, dus rood en blauw ruilen.
const FORMAT_RGB: u32 = 0;
/// PixelBlueGreenRedReserved8BitPerColor: de gangbare BGR.
const FORMAT_BGR: u32 = 1;

/// Leest de GOP vóór de exit. Geen GOP, of een formaat zonder lineaire
/// buffer (BitMask, BltOnly): geen beeld, en één regel die dat zegt.
pub(crate) fn discover(efi: &Efi) {
    let Some(gop) = efi.locate_protocol(&GOP_GUID) else {
        cpu::println!("uefi: no GOP, headless");
        return;
    };
    let mode = dev::read64(Pa(gop + off::GOP_MODE));
    let info = dev::read64(Pa(mode + off::MODE_INFO));
    let base = dev::read64(Pa(mode + off::MODE_FB_BASE));
    let w = dev::read32(Pa(info + off::INFO_H_RES));
    let h = dev::read32(Pa(info + off::INFO_H_RES + 4));
    let format = dev::read32(Pa(info + off::INFO_H_RES + 8));
    let scan = dev::read32(Pa(info + off::INFO_SCAN));
    match decode(base, w, h, format, scan) {
        Some(d) => {
            GOP[0].store(base, Relaxed);
            GOP[1].store(u64::from(h) << 32 | u64::from(w), Relaxed);
            GOP[2].store(u64::from(format) << 32 | u64::from(scan), Relaxed);
            cpu::println!(
                "uefi: GOP {}x{} stride {} format {format} @ {base:#x} HOPOS_UEFI_GOP",
                d.width,
                d.height,
                d.stride
            );
        }
        None => cpu::println!(
            "uefi: GOP {w}x{h} format {format} scan {scan} @ {base:#x} is not a linear 32 bpp buffer, headless"
        ),
    }
}

/// Maakt van de GOP-getallen een descriptor, of weigert: een breedte die
/// niet in de scanlijn past zou de console per regel voorbij de buffer
/// laten schrijven (Go, `gopDesc`: firmware-invoer kruislings toetsen).
pub(crate) fn decode(base: u64, w: u32, h: u32, format: u32, scan: u32) -> Option<Desc> {
    if base == 0 || w == 0 || h == 0 || scan < w {
        return None;
    }
    let swap_rb = match format {
        FORMAT_RGB => true,
        FORMAT_BGR => false,
        _ => return None,
    };
    let d = Desc {
        base: Pa(base),
        width: w,
        height: h,
        stride: scan.checked_mul(4)?,
        bpp: 32,
        swap_rb,
    };
    d.check().ok()?;
    Some(d)
}

/// De framebuffer uit de stub, of `None`.
pub(crate) fn framebuffer() -> Option<Desc> {
    let base = GOP[0].load(Relaxed);
    let (hw, fs) = (GOP[1].load(Relaxed), GOP[2].load(Relaxed));
    decode(
        base,
        hw as u32,
        (hw >> 32) as u32,
        (fs >> 32) as u32,
        fs as u32,
    )
}

/// Zet de framebuffer Normal-NC in de identity map: de scanout leest DRAM
/// en het fabric mag onze stores gatheren (een 1080p-frame op Device is
/// een miljoen losse transacties, `cpu::memattr`). Aanroepen na de
/// RAM-descriptors, want de laatste mapping wint; op 4 KB precies, zodat
/// geen firmware-buur een ander attribuut krijgt.
pub(crate) fn map(m: &mut Mmu) -> Result<(), mmu::Error> {
    let Some(d) = framebuffer() else {
        return Ok(());
    };
    let Some(size) = d.size() else {
        return Ok(());
    };
    let lo = d.base.0 & !0xFFF;
    let hi = (d.base.0 + size).next_multiple_of(0x1000);
    m.map(lo, hi - lo, mmu::attrs(cpu::boot::ATTR_NORMAL_NC))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_takes_linear_32_bpp_only() {
        // De ramfb van QEMU onder EDK2 (19-07): BGR, boven 4 GB.
        let d = decode(0x1_bc7a_0000, 1024, 768, FORMAT_BGR, 1024).unwrap();
        assert_eq!((d.width, d.height, d.stride, d.bpp), (1024, 768, 4096, 32));
        assert!(!d.swap_rb);
        assert!(
            decode(0x8000_0000, 1920, 1080, FORMAT_RGB, 1920)
                .unwrap()
                .swap_rb
        );
        // Een scanlijn met opvulling is een grotere stride.
        assert_eq!(
            decode(0x8000_0000, 1000, 8, FORMAT_BGR, 1024)
                .unwrap()
                .stride,
            4096
        );
        for bad in [
            (0, 1024, 768, FORMAT_BGR, 1024),
            (0x8000_0000, 1024, 768, FORMAT_BGR, 1000), // breder dan de scanlijn
            (0x8000_0000, 1024, 768, 2, 1024),          // BitMask
            (0x8000_0000, 1024, 768, 3, 1024),          // BltOnly
            (0x8000_0000, 0, 768, FORMAT_BGR, 1024),
        ] {
            assert!(
                decode(bad.0, bad.1, bad.2, bad.3, bad.4).is_none(),
                "{bad:?}"
            );
        }
    }
}
