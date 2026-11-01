//! De staging: het image van Hop dat een lader vóór de boot neerlegde
//! (QEMU's `-device loader`, de UEFI-stub, m1n1). Waar het ligt is per
//! board; de vorm is hier één keer. De kern plaatst alleen Hop (O7), dus
//! een gestaged image is Hop: er is geen rol meer.

/// Het image op `at` met zijn maat in het woord op `hdr`, of `None` als die
/// maat nul is of boven `max` ligt. Alleen de maat wordt hier getoetst; de
/// inhoud is onvertrouwd en gaat door de ELF-lezer en `abi::place`.
///
/// De aanroeper geeft een staging die de identity map als Normal-RAM mapt,
/// buiten de kern-RAM en de pool, waar na de lader niemand meer schrijft.
/// Op de host altijd `None`.
#[must_use]
pub fn staged_at(hdr: u64, at: u64, max: u64) -> Option<&'static [u8]> {
    let size = imp::word(hdr);
    if size == 0 || size > max {
        return None;
    }
    imp::slice(at, usize::try_from(size).ok()?)
}

#[cfg(target_os = "none")]
mod imp {
    pub(super) fn word(pa: u64) -> u64 {
        dev::read64(dev::Pa(pa))
    }

    pub(super) fn slice(at: u64, len: usize) -> Option<&'static [u8]> {
        // SAFETY: `[at, at + len)` ligt binnen de staging van het board
        // (`len <= max`, net getoetst in `staged_at`): RAM dat de identity
        // map als Normal mapt, buiten de pool en de kern-RAM, dus niemand
        // schrijft erin nadat de lader het vulde. Alleen lezen.
        Some(unsafe { core::slice::from_raw_parts(at as usize as *const u8, len) })
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    //! Host-stub: er is geen lader die iets neerlegde.
    pub(super) fn word(_pa: u64) -> u64 {
        0
    }

    pub(super) fn slice(_at: u64, _len: usize) -> Option<&'static [u8]> {
        None
    }
}
