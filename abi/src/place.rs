//! De plaatsingstoets van een app-image: welke segmenten waarheen, welke
//! symbolen gepatcht met welke waarden, en álle invarianten daaromheen.
//!
//! Eén bron van waarheid, want twee uitvoerders delen het plan: de
//! plaatsing door de kern en de zelfplaatsing door de apploader. In Go
//! leefde de validatie eerst dubbel, en een ABI-kritisch pad hoort niet op
//! twee plekken te kunnen divergeren.
//!
//! Het ELF-lezen zelf is van `leanelf` (nog niet gekoppeld): wie een image
//! parset, geeft hier zijn PT_LOAD-segmenten als [`Segment`], de gevonden
//! symbooladressen als [`Symbols`] en de waarde van de ABI-stempel. Alles
//! hier is rekenwerk over die getallen; geen `dev`, dus host-testbaar.
//!
//! Wat hier NIET staat: de streamende plaatsing (`stream.go`), die bytes
//! naar hun doeladres routeert terwijl ze binnenkomen; die leunt op de
//! ELF-lezer en komt met `leanelf`.

use crate::layout::{LINK_BASE, Slot};
use crate::{Error, Result};
use bounded::BoundedVec;

/// Het RAM-begin-symbool: het contract met de runtime van de app (tamago's
/// `goos.RamStart`; een Rust-runtime exporteert dezelfde naam). Het image
/// moet zijn symbooltabel aan boord houden.
pub const SYM_RAM_START: &str = "runtime/goos.RamStart";
/// Het RAM-maat-symbool.
pub const SYM_RAM_SIZE: &str = "runtime/goos.RamSize";
/// Het optionele slot-hint-symbool. De Go-naam van `board/uefi` vervalt;
/// alleen die van `board/hopslot` gaat mee.
pub const SYM_SLOT_HINT: &str = "github.com/xinix00/HopOS/metal/v2/board/hopslot.slotHint";
/// Het symbool met de ABI-versie van het image. De Go-naam blijft, zodat een
/// Go-image een luide "spreekt ABI 10" krijgt in plaats van "geen stempel".
pub const SYM_ABI: &str = "github.com/xinix00/HopOS/metal/v2/app/applib.abiVersion";

/// De stempel van de Go-generatie: slot-ABI 10, de laatste van `OLD/metal`.
/// Die staart is byte voor byte [`crate::ABI_VERSION`] 1, dus een
/// tamago-image met deze stempel mag erop binnen (bewezen 02-10-2026 op
/// QEMU met de Go-welcome; `docs/go-apps.md`). Verschuift er ooit iets in de
/// staart, dan vervalt deze alias met dezelfde versie-ophoging, want dan is
/// een Go-image precies de stille misread waar de stempel tegen is.
pub const GO_ABI_VERSION: u64 = 10;

/// Hoeveel PT_LOAD-segmenten een image mag hebben. Een Go- of Rust-image
/// heeft er drie of vier; zestien is ruim en begrensd.
pub const MAX_SEGMENTS: usize = 16;
/// Hoeveel patches een plan maximaal draagt: RAM-begin, RAM-maat, slot-hint.
pub const MAX_PATCHES: usize = 3;

/// Eén te plaatsen PT_LOAD: `off` in het image naar `paddr` (IPA), `filesz`
/// kopiëren, de rest tot `memsz` nullen (BSS).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Segment {
    /// Het doeladres.
    pub paddr: u64,
    /// De bestandsoffset.
    pub off: u64,
    /// De maat in het bestand.
    pub filesz: u64,
    /// De maat in het geheugen.
    pub memsz: u64,
}

/// De symbooladressen die de ELF-lezer vond, en de waarde van de
/// ABI-stempel. Een symboltabel geeft adressen, geen inhoud; de stempel is
/// inhoud, dus die leest de ELF-lezer uit het segment dat hem draagt.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Symbols {
    /// Het adres van [`SYM_RAM_START`].
    pub ram_start: Option<u64>,
    /// Het adres van [`SYM_RAM_SIZE`].
    pub ram_size: Option<u64>,
    /// Het adres van [`SYM_SLOT_HINT`].
    pub slot_hint: Option<u64>,
    /// De waarde op [`SYM_ABI`].
    pub abi: Option<u64>,
}

/// Een geparst image.
#[derive(Copy, Clone, Debug)]
pub struct Image<'a> {
    /// De maat van het bestand in bytes.
    pub size: u64,
    /// De entry (IPA).
    pub entry: u64,
    /// De PT_LOAD-segmenten; andere soorten laat de lezer weg.
    pub segments: &'a [Segment],
    /// De symbolen.
    pub symbols: Symbols,
}

/// Het venster waarbinnen een image moet vallen.
///
/// Segmenten blijven tussen `lo_off` en `top_off` (offsets vanaf
/// `link_base`): boven wat de architectuur vooraan reserveert (RISC-V zet
/// daar de kooi-stub, op ARM is het nul) en onder de staging of het
/// stub-venster, want de kopieerbron moet de kopie overleven.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Window {
    /// De linkbasis.
    pub link_base: u64,
    /// De app-RAM-maat.
    pub app_ram: u64,
    /// De ondergrens (offset).
    pub lo_off: u64,
    /// De bovengrens (offset).
    pub top_off: u64,
}

impl Window {
    /// Het canonieke venster: elk image is tegen [`LINK_BASE`] gelinkt,
    /// ongeacht het slot waarin het draait.
    #[must_use]
    pub const fn canonical(app_ram: u64, lo_off: u64, top_off: u64) -> Window {
        Window {
            link_base: LINK_BASE,
            app_ram,
            lo_off,
            top_off,
        }
    }

    /// Toetst de geometrie één keer, vóór elke adresberekening.
    pub fn check(&self) -> Result {
        let Window {
            link_base: base,
            app_ram: size,
            lo_off: lo,
            top_off: top,
        } = *self;
        if size < 8 || base.checked_add(size).is_none() || lo >= top || top > size {
            return Err(Error::PlaceWindow {
                base,
                size,
                lo,
                top,
            });
        }
        Ok(())
    }

    /// Het eerste byte voorbij het venster; geldig na [`Window::check`].
    const fn end(&self) -> u64 {
        self.link_base + self.app_ram
    }

    /// Ligt een 64-bit symbool gealigneerd binnen het venster?
    const fn holds_word(&self, addr: u64) -> bool {
        addr.is_multiple_of(8) && addr >= self.link_base && addr <= self.end() - 8
    }
}

/// Eén 64-bit symboolwaarde op zijn (IPA-)adres.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Patch {
    /// Het adres.
    pub addr: u64,
    /// De waarde.
    pub val: u64,
}

/// Het gevalideerde plaatsingsplan van één image. Een plan is compleet
/// geldig of bestaat niet.
#[derive(Clone, Debug)]
pub struct Placement {
    /// De entry (IPA, binnen het venster).
    pub entry: u64,
    /// De segmenten, in de volgorde van het image.
    pub segments: BoundedVec<Segment, MAX_SEGMENTS>,
    /// De patches: RAM-begin, RAM-maat en eventueel de slot-hint.
    pub patches: BoundedVec<Patch, MAX_PATCHES>,
}

/// Toetst één segment tegen venster en image. Headervelden zijn invoer (het
/// image komt van het netwerk): overflow-veilig begrenzen.
fn check_segment(s: &Segment, w: &Window, img_size: u64) -> Result {
    let bad = Error::Segment {
        paddr: s.paddr,
        memsz: s.memsz,
        filesz: s.filesz,
        off: s.off,
    };
    let lo = w.link_base + w.lo_off;
    if s.filesz > s.memsz || s.memsz > w.app_ram || s.paddr < lo || s.paddr > w.end() - s.memsz {
        return Err(bad);
    }
    if s.off > img_size || s.filesz > img_size - s.off {
        return Err(bad);
    }
    // Geen omloop: paddr + memsz <= end is hierboven getoetst.
    if s.paddr + s.memsz > w.link_base + w.top_off {
        return Err(bad);
    }
    Ok(())
}

/// Bouwt en valideert het plan van een image.
///
/// `slot` is de waarde voor de slot-hint. `abi` is de versie die deze kern
/// spreekt ([`crate::ABI_VERSION`]); het image moet dezelfde melden. `None`
/// toetst geen stempel: dat is een KERN-image (de kern-flip), met dezelfde
/// ELF-vorm, grenzen en RAM-symbolen, maar zonder app-runtime erin.
pub fn build(img: &Image<'_>, w: &Window, slot: Slot, abi: Option<u32>) -> Result<Placement> {
    w.check()?;
    if img.size == 0 {
        return Err(Error::ImageSize(img.size));
    }
    if img.entry < w.link_base + w.lo_off || img.entry >= w.end() {
        return Err(Error::Entry {
            entry: img.entry,
            base: w.link_base,
            size: w.app_ram,
        });
    }
    let mut p = Placement {
        entry: img.entry,
        segments: BoundedVec::new(),
        patches: BoundedVec::new(),
    };
    for s in img.segments {
        check_segment(s, w, img.size)?;
        p.segments.push(*s).map_err(|_| Error::TooMany {
            what: "PT_LOAD segments",
            cap: MAX_SEGMENTS,
        })?;
    }
    if p.segments.is_empty() {
        return Err(Error::NoSegments);
    }

    let ram = [
        (SYM_RAM_START, img.symbols.ram_start, w.link_base),
        (SYM_RAM_SIZE, img.symbols.ram_size, w.app_ram),
    ];
    for (name, addr, val) in ram {
        match addr {
            Some(a) if w.holds_word(a) => push_patch(&mut p, a, val)?,
            _ => {
                return Err(Error::Symbol {
                    name,
                    addr: addr.unwrap_or(0),
                });
            }
        }
    }
    // De slot-hint is optioneel en additief: een image zonder merkt niets,
    // en een vreemd adres wordt stil overgeslagen (zelfde semantiek als Go).
    if let Some(a) = img.symbols.slot_hint
        && w.holds_word(a)
    {
        push_patch(&mut p, a, slot.get() as u64)?;
    }

    let Some(want) = abi else {
        return Ok(p);
    };
    match img.symbols.abi {
        None => Err(Error::NoAbiStamp { want }),
        Some(v) if v != u64::from(want) && v != GO_ABI_VERSION => {
            Err(Error::AbiMismatch { image: v, want })
        }
        Some(_) => Ok(p),
    }
}

/// Voegt een patch toe.
fn push_patch(p: &mut Placement, addr: u64, val: u64) -> Result {
    p.patches
        .push(Patch { addr, val })
        .map_err(|_| Error::TooMany {
            what: "patches",
            cap: MAX_PATCHES,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ABI_VERSION;

    const BASE: u64 = 0x8000_0000;
    const RAM: u64 = 96 << 20;

    fn window() -> Window {
        Window {
            link_base: BASE,
            app_ram: RAM,
            lo_off: 0,
            top_off: RAM,
        }
    }

    const SEGS: [Segment; 2] = [
        Segment {
            paddr: BASE + 0x10000,
            off: 0x1000,
            filesz: 0x4000,
            memsz: 0x4000,
        },
        Segment {
            paddr: BASE + 0x20000,
            off: 0x5000,
            filesz: 0x1000,
            memsz: 0x9000,
        },
    ];

    fn image() -> Image<'static> {
        Image {
            size: 0x8000,
            entry: BASE + 0x10000,
            segments: &SEGS,
            symbols: Symbols {
                ram_start: Some(BASE + 0x20000),
                ram_size: Some(BASE + 0x20008),
                slot_hint: Some(BASE + 0x20010),
                abi: Some(u64::from(ABI_VERSION)),
            },
        }
    }

    fn slot() -> Slot {
        Slot::new(3).unwrap()
    }

    #[test]
    fn placement_rejects_invalid_window() {
        for (base, size, lo, top) in [
            (!7u64, 16u64, 0u64, 16u64),
            (0x1000, 7, 0, 7),
            (0x1000, 4096, 4096, 4096),
            (0x1000, 4096, 0, 8192),
        ] {
            let w = Window {
                link_base: base,
                app_ram: size,
                lo_off: lo,
                top_off: top,
            };
            assert!(matches!(
                build(&image(), &w, slot(), None),
                Err(Error::PlaceWindow { .. })
            ));
        }
    }

    #[test]
    fn plan_patcht_de_ram_declaratie() {
        let p = build(&image(), &window(), slot(), Some(ABI_VERSION)).unwrap();
        assert_eq!(p.entry, BASE + 0x10000);
        assert_eq!(p.segments.as_slice(), &SEGS);
        assert_eq!(
            p.patches.as_slice(),
            &[
                Patch {
                    addr: BASE + 0x20000,
                    val: BASE
                },
                Patch {
                    addr: BASE + 0x20008,
                    val: RAM
                },
                Patch {
                    addr: BASE + 0x20010,
                    val: 3
                },
            ]
        );
    }

    #[test]
    fn canoniek_venster_ligt_op_slot_1() {
        let w = Window::canonical(RAM, 0, RAM);
        assert_eq!(w.link_base, LINK_BASE);
    }

    #[test]
    fn segmenten_buiten_de_grenzen_weigeren() {
        let cases = [
            // filesz > memsz
            Segment {
                paddr: BASE,
                off: 0,
                filesz: 16,
                memsz: 8,
            },
            // onder het venster
            Segment {
                paddr: BASE - 8,
                off: 0,
                filesz: 8,
                memsz: 8,
            },
            // over het einde van het venster
            Segment {
                paddr: BASE + RAM - 8,
                off: 0,
                filesz: 8,
                memsz: 16,
            },
            // bestandsoffset buiten het image
            Segment {
                paddr: BASE,
                off: 0x8000,
                filesz: 8,
                memsz: 8,
            },
            // bestandsbereik loopt om
            Segment {
                paddr: BASE,
                off: 8,
                filesz: u64::MAX,
                memsz: u64::MAX,
            },
        ];
        for s in cases {
            let segs = [s];
            let img = Image {
                segments: &segs,
                ..image()
            };
            assert!(
                matches!(
                    build(&img, &window(), slot(), None),
                    Err(Error::Segment { .. })
                ),
                "{s:?}"
            );
        }
        // Tot in de staging: het venster eindigt op RAM, de staging op 1 MB.
        let w = Window {
            top_off: 0x20000,
            ..window()
        };
        assert!(matches!(
            build(&image(), &w, slot(), None),
            Err(Error::Segment { .. })
        ));
    }

    #[test]
    fn entry_en_lege_images_weigeren() {
        let img = Image {
            entry: BASE + RAM,
            ..image()
        };
        assert!(matches!(
            build(&img, &window(), slot(), None),
            Err(Error::Entry { .. })
        ));
        let img = Image {
            segments: &[],
            ..image()
        };
        assert_eq!(
            build(&img, &window(), slot(), None).unwrap_err(),
            Error::NoSegments
        );
        let img = Image { size: 0, ..image() };
        assert_eq!(
            build(&img, &window(), slot(), None).unwrap_err(),
            Error::ImageSize(0)
        );
    }

    #[test]
    fn ram_symbolen_zijn_verplicht_en_begrensd() {
        let mut img = image();
        img.symbols.ram_size = None;
        assert!(matches!(
            build(&img, &window(), slot(), None),
            Err(Error::Symbol {
                name: SYM_RAM_SIZE,
                addr: 0
            })
        ));
        let mut img = image();
        img.symbols.ram_start = Some(BASE + 4); // scheef
        assert!(matches!(
            build(&img, &window(), slot(), None),
            Err(Error::Symbol { .. })
        ));
        let mut img = image();
        img.symbols.ram_start = Some(BASE + RAM - 4); // voorbij het einde
        assert!(matches!(
            build(&img, &window(), slot(), None),
            Err(Error::Symbol { .. })
        ));
    }

    #[test]
    fn vreemde_slot_hint_wordt_overgeslagen() {
        let mut img = image();
        img.symbols.slot_hint = Some(BASE + 3);
        let p = build(&img, &window(), slot(), None).unwrap();
        assert_eq!(p.patches.len(), 2);
    }

    #[test]
    fn abi_stempel_wordt_getoetst() {
        let mut img = image();
        img.symbols.abi = Some(9);
        assert_eq!(
            build(&img, &window(), slot(), Some(ABI_VERSION)).unwrap_err(),
            Error::AbiMismatch {
                image: 9,
                want: ABI_VERSION
            }
        );
        // De Go-stempel is een alias van ABI 1.
        img.symbols.abi = Some(GO_ABI_VERSION);
        assert!(build(&img, &window(), slot(), Some(ABI_VERSION)).is_ok());
        img.symbols.abi = None;
        assert_eq!(
            build(&img, &window(), slot(), Some(ABI_VERSION)).unwrap_err(),
            Error::NoAbiStamp { want: ABI_VERSION }
        );
        // Een kern-image toetst geen stempel.
        assert!(build(&img, &window(), slot(), None).is_ok());
    }
}
