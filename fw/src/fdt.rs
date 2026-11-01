//! Een minimale, allocatievrije lezer van het Flattened Device Tree-formaat
//! (DTB) dat elke arm64-firmware bij boot meegeeft.
//!
//! De draagbare bron voor "hoeveel RAM heeft dit board", "waar staan de
//! virtio-transports en de GIC", de boot-parameterregel en de
//! firmware-framebuffer. HopOS carveert zijn slots niet op
//! compile-time-constanten maar op wat hier gevonden wordt.
//!
//! Bewust géén volledige parser: alleen wat de kern vraagt. Alles is
//! big-endian (Devicetree v0.4). De blob is onvertrouwde firmware-input:
//! de lezer werkt op een `&[u8]` en indexeert alleen met `get`, dus een
//! kromme DTB levert `None` of een [`Error`], nooit een panic. Het board
//! maakt de slice van het adres (het is de enige met `unsafe`); de lezer
//! kent geen adressen.
//!
//! De Go-voorganger (`metal/fw/fdt`) woog de header op vier plekken los;
//! hier is dat één keer [`Fdt::new`], en elke lezer loopt daarna alleen
//! binnen gedeclareerde blokken.

use crate::bytes::{be32, be64};
use bounded::BoundedVec;
use core::fmt;
use core::ops::ControlFlow;

const MAGIC: u32 = 0xd00d_feed;
const TOK_BEGIN: u32 = 1;
const TOK_END: u32 = 2;
const TOK_PROP: u32 = 3;
const TOK_NOP: u32 = 4;
const TOK_END_TREE: u32 = 9;

/// Een DTB groter dan 2 MB is onzin: dat begrenst al het rekenwerk.
pub const MAX_BLOB: usize = 2 << 20;
/// Een initrd groter dan 64 MB is niet van ons. Los van [`MAX_BLOB`]: de
/// initrd is al lang niet alleen de config meer maar ook het image van Hop
/// (de Pi's laden het als `initramfs`, de Radxa draagt het samen met
/// `hopos.cfg`), en dat is 1,5 MB gestript en groeiend (30-09). Onder de
/// oude grens van 2 MB viel een groter image stil weg ("no initramfs").
/// Elke aanroeper begrenst daarna nog op zijn eigen laadvenster.
pub const MAX_INITRD: u64 = 64 << 20;
/// De vaste headergrootte (tot en met `size_dt_struct`).
pub const HEADER_LEN: usize = 40;
/// Zoveel geheugenbanken houden we bij.
pub const MAX_MEM_REGIONS: usize = 16;
/// Zoveel /memreserve/-regio's; een gezonde DTB heeft er een handvol.
pub const MAX_RESERVE: usize = 64;
/// Zoveel virtio-mmio-transports (QEMU virt heeft er 32).
pub const MAX_VIRTIO: usize = 32;

/// Waarom een blob niet te lezen is.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Geen FDT-magic op offset 0 (of minder dan acht bytes).
    NoMagic,
    /// De gedeclareerde grootte past niet: kleiner dan de header, groter
    /// dan [`MAX_BLOB`], of groter dan wat de aanroeper gaf.
    BadSize(usize),
    /// Een blok-offset wijst buiten de blob.
    BadOffset(&'static str),
    /// Een property loopt voorbij het einde van het structure-blok.
    Truncated,
    /// Een onbekend token.
    BadToken(u32),
    /// `#address-cells` of `#size-cells` buiten 1 of 2.
    BadCells,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMagic => f.write_str("fdt: no magic"),
            Self::BadSize(n) => write!(f, "fdt: declared size {n} does not fit"),
            Self::BadOffset(which) => write!(f, "fdt: {which} outside the blob"),
            Self::Truncated => f.write_str("fdt: truncated property"),
            Self::BadToken(t) => write!(f, "fdt: unknown token {t:#x}"),
            Self::BadCells => f.write_str("fdt: unsupported cell count"),
        }
    }
}

/// De `Result` van deze module.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Een fysiek geheugenbereik in bytes (een `reg`-paar): dezelfde
/// [`abi::Region`] waar het PA-plan en de pool van de kern mee rekenen.
pub use abi::Region;

/// De firmware-simple-framebuffer uit /chosen (de simple-framebuffer-
/// binding): het scherm dat de bootloader al aanzette.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Fb {
    /// Het fysieke adres van de pixels.
    pub base: u64,
    /// De breedte in pixels.
    pub width: u32,
    /// De hoogte in pixels.
    pub height: u32,
    /// Bytes per regel.
    pub stride: u32,
    /// 32 (a8r8g8b8/x8r8g8b8) of 16 (r5g6b5).
    pub bpp: u32,
}

/// Eén virtio-mmio-transport.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct VirtioMmio {
    /// Het registerblok.
    pub reg: Region,
    /// De INTID van zijn interrupt (SPI n = 32 + n), 0 = geen.
    pub intid: u32,
}

/// De adressen van een GICv3.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct GicV3 {
    /// De distributor.
    pub dist: Region,
    /// De eerste redistributor-reeks.
    pub redist: Region,
}

/// Leest de gedeclareerde blobgrootte uit de eerste acht bytes: zo weet
/// een board hoeveel het moet aanbieden voordat het [`Fdt::new`] roept.
#[must_use]
pub fn total_size(head: &[u8]) -> Option<usize> {
    if be32(head, 0)? != MAGIC {
        return None;
    }
    let n = be32(head, 4)? as usize;
    (HEADER_LEN..=MAX_BLOB).contains(&n).then_some(n)
}

/// Een gevalideerde DTB: de enige plek waar de header gewogen wordt.
#[derive(Clone, Copy, Debug)]
pub struct Fdt<'a> {
    blob: &'a [u8],
    structs: (usize, usize),
    strings: (usize, usize),
    rsv: (usize, usize),
}

/// Eén token uit het structure-blok.
#[derive(Clone, Copy, Debug)]
enum Token<'a> {
    /// Een node begint; zijn naam (zonder NUL).
    Begin(&'a [u8]),
    /// Een node sluit.
    End,
    /// Een property met een geldige naam.
    Prop(&'a [u8], &'a [u8]),
}

impl<'a> Fdt<'a> {
    /// Weegt de header: het magic, een grootte tussen de header en
    /// [`MAX_BLOB`] die in `blob` past, en drie blok-offsets binnen die
    /// grootte. De blokgroottes (`size_dt_struct`, `size_dt_strings`)
    /// gelden waar de firmware ze plausibel vult; anders is de grens het
    /// einde van de blob (nooit slechter dan de Go-lezer vóór zijn helper).
    pub fn new(blob: &'a [u8]) -> Result<Self> {
        if be32(blob, 0) != Some(MAGIC) {
            return Err(Error::NoMagic);
        }
        let total = be32(blob, 4).ok_or(Error::NoMagic)? as usize;
        if !(HEADER_LEN..=MAX_BLOB).contains(&total) || total > blob.len() {
            return Err(Error::BadSize(total));
        }
        let blob = blob.get(..total).ok_or(Error::BadSize(total))?;
        let field = |off: usize| be32(blob, off).map_or(0, |v| v as usize);
        let (struct_off, strings_off, rsv_off) = (field(8), field(12), field(16));
        if struct_off >= total {
            return Err(Error::BadOffset("off_dt_struct"));
        }
        if strings_off >= total {
            return Err(Error::BadOffset("off_dt_strings"));
        }
        if rsv_off >= total {
            return Err(Error::BadOffset("off_mem_rsvmap"));
        }
        let block_end = |off: usize, size: usize| {
            if size > 0 && off + size <= total {
                off + size
            } else {
                total
            }
        };
        Ok(Self {
            blob,
            structs: (struct_off, block_end(struct_off, field(36))),
            strings: (strings_off, block_end(strings_off, field(32))),
            rsv: (rsv_off, total),
        })
    }

    /// De gedeclareerde blobgrootte.
    #[must_use]
    pub fn size(&self) -> usize {
        self.blob.len()
    }

    /// De naam van een property uit het strings-blok, of `None` als de
    /// nameoff buiten dat blok wijst of de naam er niet in eindigt.
    fn prop_name(&self, off: usize) -> Option<&'a [u8]> {
        let s = self.blob.get(self.strings.0..self.strings.1)?;
        let rest = s.get(off..)?;
        let nul = rest.iter().position(|&b| b == 0)?;
        rest.get(..nul)
    }

    /// Loopt het structure-blok af en ontleedt elk token één keer. `f`
    /// krijgt de diepte (root = 1; voor een property de diepte van zijn
    /// node, voor `End` de diepte van de node die sluit) en mag stoppen.
    /// Een property met een kromme nameoff wordt overgeslagen.
    fn walk(&self, mut f: impl FnMut(usize, Token<'a>) -> ControlFlow<()>) -> Result {
        let (mut p, end) = self.structs;
        let mut depth = 0usize;
        while p + 4 <= end {
            let tok = be32(self.blob, p).ok_or(Error::Truncated)?;
            p += 4;
            let flow = match tok {
                TOK_BEGIN => {
                    depth += 1;
                    let rest = self.blob.get(p..end).unwrap_or_default();
                    let n = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                    let name = rest.get(..n).unwrap_or_default();
                    p = (p + n + 1).next_multiple_of(4);
                    f(depth, Token::Begin(name))
                }
                TOK_END => {
                    let flow = f(depth, Token::End);
                    depth = depth.saturating_sub(1);
                    flow
                }
                TOK_PROP => {
                    let len = be32(self.blob, p).ok_or(Error::Truncated)? as usize;
                    let name_off = be32(self.blob, p + 4).ok_or(Error::Truncated)? as usize;
                    p += 8;
                    let data = self.blob.get(p..p + len).ok_or(Error::Truncated)?;
                    p = (p + len).next_multiple_of(4);
                    if p > end {
                        return Err(Error::Truncated);
                    }
                    match self.prop_name(name_off) {
                        Some(name) => f(depth, Token::Prop(name, data)),
                        None => ControlFlow::Continue(()),
                    }
                }
                TOK_NOP => ControlFlow::Continue(()),
                TOK_END_TREE => return Ok(()),
                other => return Err(Error::BadToken(other)),
            };
            if flow.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// De cellen van de root: (`#address-cells`, `#size-cells`), default
    /// 2 en 1 per spec.
    fn root_cells(&self) -> Result<(usize, usize)> {
        let (mut a, mut s) = (2, 1);
        self.walk(|depth, tok| match tok {
            Token::Prop(name, data) if depth == 1 && data.len() == 4 => {
                if name == b"#address-cells" {
                    a = be32(data, 0).map_or(0, |v| v as usize);
                } else if name == b"#size-cells" {
                    s = be32(data, 0).map_or(0, |v| v as usize);
                }
                ControlFlow::Continue(())
            }
            Token::Begin(_) if depth == 2 => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        })?;
        Ok((a, s))
    }

    /// Alle /memory-banken, met de cellen van de root (alleen 1 of 2).
    /// Leeg is een fout van de aanroeper om te beslissen; zie
    /// [`mem_total`](Self::mem_total).
    pub fn mem_regions(&self) -> Result<BoundedVec<Region, MAX_MEM_REGIONS>> {
        let (ac, sc) = self.root_cells()?;
        if !(1..=2).contains(&ac) || !(1..=2).contains(&sc) {
            return Err(Error::BadCells);
        }
        let mut regs = BoundedVec::new();
        let mut in_mem = false;
        self.walk(|depth, tok| {
            match tok {
                Token::Begin(name) => in_mem = depth == 2 && name_is(name, b"memory", true),
                Token::End => in_mem = false,
                Token::Prop(name, data) if in_mem && name == b"reg" => {
                    for r in reg_pairs(data, ac, sc) {
                        let _ = regs.push(r);
                    }
                }
                Token::Prop(..) => {}
            }
            ControlFlow::Continue(())
        })?;
        Ok(regs)
    }

    /// De som van alle /memory-banken, of `None` bij een ongeldige blob of
    /// zonder banken: de aanroeper valt dan terug op een veilige default.
    #[must_use]
    pub fn mem_total(&self) -> Option<u64> {
        let regs = self.mem_regions().ok()?;
        if regs.is_empty() {
            return None;
        }
        Some(regs.iter().map(|r| r.size).sum())
    }

    /// De ruwe bytes van property `name` van de root (`node` = `None`) of
    /// van het directe kind `node` (bijvoorbeeld "chosen").
    fn node_bytes(&self, node: Option<&[u8]>, name: &[u8]) -> Option<&'a [u8]> {
        let want_depth = if node.is_some() { 2 } else { 1 };
        let mut inside = node.is_none();
        let mut found = None;
        self.walk(|depth, tok| {
            match tok {
                Token::Begin(n) if node.is_some() && depth == 2 => {
                    inside = node.is_some_and(|want| name_is(n, want, false));
                }
                Token::End if node.is_some() && depth == 2 => inside = false,
                Token::Prop(n, data) if inside && depth == want_depth && n == name => {
                    found = Some(data);
                    return ControlFlow::Break(());
                }
                _ => {}
            }
            ControlFlow::Continue(())
        })
        .ok()?;
        found
    }

    /// Een string-property zonder de NUL.
    fn node_str(&self, node: Option<&[u8]>, name: &[u8]) -> Option<&'a str> {
        let b = self.node_bytes(node, name)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        core::str::from_utf8(b.get(..end)?).ok()
    }

    /// /chosen/bootargs: de boot-parameterregel (op de Pi cmdline.txt, op
    /// QEMU `-append`). Het kanaal voor node-configuratie zonder rebuild
    /// (Derek, 2026-07-11).
    #[must_use]
    pub fn bootargs(&self) -> Option<&'a str> {
        self.node_str(Some(b"chosen"), b"bootargs")
    }

    /// Een string-property van de root (bijvoorbeeld "serial-number", door
    /// de Pi-firmware gezet): de stabiele bron voor een board-identiteit.
    #[must_use]
    pub fn root_string(&self, name: &str) -> Option<&'a str> {
        self.node_str(None, name.as_bytes())
    }

    /// `[start, end)` van het door de firmware geladen initramfs
    /// (/chosen/linux,initrd-start en -end; 4- of 8-byte cellen). HopOS
    /// gebruikt het als config-kanaal: `initramfs hopos.cfg <addr>` laadt
    /// een bestand van élke maat, zonder het 1024-byte bootargs-plafond
    /// (gemeten 19-07: elke cmdline verloor zijn staart).
    #[must_use]
    pub fn initrd(&self) -> Option<(u64, u64)> {
        let cell = |name: &[u8]| {
            let b = self.node_bytes(Some(b"chosen"), name)?;
            match b.len() {
                4 => be32(b, 0).map(u64::from),
                8 => be64(b, 0),
                _ => None,
            }
        };
        let s = cell(b"linux,initrd-start")?;
        let e = cell(b"linux,initrd-end")?;
        (e > s && e - s <= MAX_INITRD).then_some((s, e))
    }

    /// Het /memreserve/-blok: regio's die de firmware voor zichzelf houdt,
    /// nooit uit te delen als pool. Leeg is geldig.
    #[must_use]
    pub fn mem_reserve(&self) -> BoundedVec<Region, MAX_RESERVE> {
        let mut regs = BoundedVec::new();
        let (mut p, end) = self.rsv;
        while p + 16 <= end {
            let (Some(addr), Some(size)) = (be64(self.blob, p), be64(self.blob, p + 8)) else {
                break;
            };
            p += 16;
            if addr == 0 && size == 0 {
                break;
            }
            if regs.push(Region::new(addr, size)).is_err() {
                break;
            }
        }
        regs
    }

    /// /chosen/framebuffer@...: de geometrie, of `None` als de node er
    /// niet (compleet) is.
    ///
    /// De cellen van het DICHTSTBIJZIJNDE niveau winnen: de Pi-firmware
    /// schrijft /chosen mét een eigen `#address-cells = 1` en de reg als
    /// `<u32 base><u32 size>`. Wie alleen de root-cellen (2) leest, plakt
    /// base en size aaneen tot een adres in de stratosfeer, en de eerste
    /// pixel-veeg is dan een bus-fault-reset. GEMETEN 04-08 (FBDBG, Pi 5):
    /// base=0x3f800000003f4800; dat was de "32-bpp-freeze" van 19-07, drie
    /// weken vermomd als silicium.
    #[must_use]
    pub fn framebuffer(&self) -> Option<Fb> {
        let mut in_chosen = false;
        let mut in_fb = false;
        let mut ac = 2usize;
        let mut fb = Fb {
            bpp: 32,
            ..Fb::default()
        };
        let mut found = false;
        self.walk(|depth, tok| {
            match tok {
                Token::Begin(name) => match depth {
                    2 => in_chosen = name_is(name, b"chosen", false),
                    3 => in_fb = in_chosen && name_is(name, b"framebuffer", true),
                    _ => {}
                },
                Token::End => {
                    if in_fb && depth == 3 {
                        if fb.base != 0 && fb.width != 0 && fb.height != 0 && fb.stride != 0 {
                            found = true;
                            return ControlFlow::Break(());
                        }
                        in_fb = false;
                    }
                    if depth == 2 {
                        in_chosen = false;
                    }
                }
                Token::Prop(name, data) => {
                    if data.len() == 4
                        && (depth == 1 || (depth == 2 && in_chosen))
                        && name == b"#address-cells"
                    {
                        ac = be32(data, 0).map_or(0, |v| v as usize);
                    }
                    if in_fb {
                        fb_prop(&mut fb, name, data, ac);
                    }
                }
            }
            ControlFlow::Continue(())
        })
        .ok()?;
        found.then_some(fb)
    }

    /// Alle virtio-mmio-transports (`compatible = "virtio,mmio"`) onder de
    /// root, met hun INTID. De volgorde is die van de blob; welk slot een
    /// `-device` krijgt is een QEMU-detail, dus de aanroeper scant op
    /// DeviceID.
    pub fn virtio_mmio(&self) -> Result<BoundedVec<VirtioMmio, MAX_VIRTIO>> {
        let (ac, sc) = self.root_cells()?;
        let mut out = BoundedVec::new();
        let mut cur: Option<(bool, VirtioMmio)> = None;
        self.walk(|depth, tok| {
            match tok {
                Token::Begin(_) if depth == 2 => cur = Some((false, VirtioMmio::default())),
                Token::End if depth == 2 => {
                    if let Some((true, v)) = cur.take()
                        && v.reg.size != 0
                    {
                        let _ = out.push(v);
                    }
                }
                Token::Prop(name, data) if depth == 2 => {
                    if let Some((compat, v)) = cur.as_mut() {
                        match name {
                            b"compatible" => *compat |= has_compatible(data, b"virtio,mmio"),
                            b"reg" => v.reg = reg_pairs(data, ac, sc).next().unwrap_or_default(),
                            b"interrupts" => v.intid = gic_intid(data).unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// Het aantal cores: de `cpu@`-nodes onder /cpus.
    pub fn cpu_count(&self) -> Result<usize> {
        let mut in_cpus = false;
        let mut n = 0;
        self.walk(|depth, tok| {
            match tok {
                Token::Begin(name) if depth == 2 => in_cpus = name_is(name, b"cpus", false),
                Token::Begin(name) if depth == 3 && in_cpus && name_is(name, b"cpu", true) => {
                    n += 1;
                }
                Token::End if depth == 2 => in_cpus = false,
                _ => {}
            }
            ControlFlow::Continue(())
        })?;
        Ok(n)
    }

    /// De eerste GICv3 (`compatible = "arm,gic-v3"`) onder de root: de
    /// distributor en de eerste redistributor-reeks.
    #[must_use]
    pub fn gic_v3(&self) -> Option<GicV3> {
        let (ac, sc) = self.root_cells().ok()?;
        let mut compat = false;
        let mut regs: Option<GicV3> = None;
        let mut found = None;
        self.walk(|depth, tok| {
            match tok {
                Token::Begin(_) if depth == 2 => {
                    compat = false;
                    regs = None;
                }
                Token::End if depth == 2 => {
                    if compat && regs.is_some() {
                        found = regs;
                        return ControlFlow::Break(());
                    }
                }
                Token::Prop(name, data) if depth == 2 => match name {
                    b"compatible" => compat |= has_compatible(data, b"arm,gic-v3"),
                    b"reg" => {
                        let mut it = reg_pairs(data, ac, sc);
                        if let (Some(dist), Some(redist)) = (it.next(), it.next()) {
                            regs = Some(GicV3 { dist, redist });
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            ControlFlow::Continue(())
        })
        .ok()?;
        found
    }

    /// Staat de eerste node met `compatible` aan? `None` = geen zo'n node
    /// (of een kromme blob); `Some(true)` = geen `status`, of "okay"/"ok".
    ///
    /// Waarom een board dat vraagt: een driver die een blok aanraakt dat er
    /// niet is, krijgt een synchrone external abort. Gemeten 29-09 op QEMU
    /// `raspi4b`: GENET staat er op "disabled" en de eerste lees op
    /// 0xFD58_0000 gaf ESR 0x96000010.
    ///
    /// De properties van een node staan vóór zijn kinderen, dus het oordeel
    /// valt bij het eerste kind of het einde van de node, en de status van
    /// een kind lekt nooit naar zijn ouder.
    #[must_use]
    pub fn enabled(&self, compatible: &str) -> Option<bool> {
        // (past, staat aan) van de node waarvan nu de properties lopen;
        // `None` na een kind.
        let mut cur: Option<(bool, bool)> = None;
        let mut found = None;
        self.walk(|_, tok| {
            match tok {
                Token::Prop(name, data) => {
                    if let Some((hit, ok)) = cur.as_mut() {
                        match name {
                            b"compatible" => *hit |= has_compatible(data, compatible.as_bytes()),
                            b"status" => {
                                let v = data.split(|&c| c == 0).next().unwrap_or_default();
                                *ok = v == b"okay" || v == b"ok";
                            }
                            _ => {}
                        }
                    }
                }
                Token::Begin(_) | Token::End => {
                    if let Some((true, ok)) = cur {
                        found = Some(ok);
                        return ControlFlow::Break(());
                    }
                    cur = matches!(tok, Token::Begin(_)).then_some((false, true));
                }
            }
            ControlFlow::Continue(())
        })
        .ok()?;
        found
    }
}

/// Eén property van de framebuffer-node.
fn fb_prop(fb: &mut Fb, name: &[u8], data: &[u8], ac: usize) {
    let u32_of = |d: &[u8]| (d.len() == 4).then(|| be32(d, 0)).flatten();
    match name {
        b"reg" => {
            fb.base = match ac {
                1 => be32(data, 0).map_or(0, u64::from),
                2 => be64(data, 0).unwrap_or(0),
                _ => fb.base,
            };
        }
        b"width" => fb.width = u32_of(data).unwrap_or(fb.width),
        b"height" => fb.height = u32_of(data).unwrap_or(fb.height),
        b"stride" => fb.stride = u32_of(data).unwrap_or(fb.stride),
        b"format" => {
            if data.len() >= 6 && data.starts_with(b"r5") {
                fb.bpp = 16;
            }
        }
        _ => {}
    }
}

/// De (adres, grootte)-paren van een `reg` met `ac`/`sc` cellen van 1 of 2.
fn reg_pairs(data: &[u8], ac: usize, sc: usize) -> impl Iterator<Item = Region> + '_ {
    let stride = (ac + sc) * 4;
    let ok = (1..=2).contains(&ac) && (1..=2).contains(&sc);
    let n = if ok { data.len() / stride } else { 0 };
    (0..n).filter_map(move |i| {
        let off = i * stride;
        Some(Region {
            base: cells(data, off, ac)?,
            size: cells(data, off + ac * 4, sc)?,
        })
    })
}

/// Een waarde van één of twee cellen.
fn cells(data: &[u8], off: usize, n: usize) -> Option<u64> {
    match n {
        1 => be32(data, off).map(u64::from),
        2 => be64(data, off),
        _ => None,
    }
}

/// De INTID uit een GIC-`interrupts`-triple: type 0 = SPI (32 + n), 1 = PPI
/// (16 + n).
fn gic_intid(data: &[u8]) -> Option<u32> {
    let kind = be32(data, 0)?;
    let n = be32(data, 4)?;
    match kind {
        0 => n.checked_add(32),
        1 => n.checked_add(16),
        _ => None,
    }
}

/// Staat `want` in een `compatible`-stringlijst (NUL-gescheiden)?
fn has_compatible(data: &[u8], want: &[u8]) -> bool {
    data.split(|&b| b == 0).any(|s| s == want)
}

/// Is de node-naam exact `s`, of (met `unit`) `s` gevolgd door `@`?
fn name_is(name: &[u8], s: &[u8], unit: bool) -> bool {
    match name.strip_prefix(s) {
        Some([]) => true,
        Some([b'@', ..]) => unit,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    //! De tests van `fdt_test.go`, geport, plus de echte DTB van QEMU virt
    //! (`-machine dumpdtb`, gepakt met `dtc`).
    use super::*;

    /// Bouwt een DTB met een geldige v17-header, inclusief de blokgroottes
    /// die echte firmware ook vult.
    #[derive(Default)]
    struct Builder {
        structs: Vec<u8>,
        strs: Vec<u8>,
        rsv: Vec<u8>,
    }

    fn app32(d: &mut Vec<u8>, v: u32) {
        d.extend_from_slice(&v.to_be_bytes());
    }
    fn u32s(vs: &[u32]) -> Vec<u8> {
        let mut d = Vec::new();
        for &v in vs {
            app32(&mut d, v);
        }
        d
    }
    fn u64s(vs: &[u64]) -> Vec<u8> {
        vs.iter().flat_map(|v| v.to_be_bytes()).collect()
    }
    fn pad4(d: &mut Vec<u8>) {
        while !d.len().is_multiple_of(4) {
            d.push(0);
        }
    }

    impl Builder {
        fn str_off(&mut self, name: &str) -> u32 {
            let n = name.as_bytes();
            for i in 0..self.strs.len() {
                if self.strs[i..].starts_with(n) && self.strs.get(i + n.len()) == Some(&0) {
                    return i as u32;
                }
            }
            let off = self.strs.len() as u32;
            self.strs.extend_from_slice(n);
            self.strs.push(0);
            off
        }
        fn begin(&mut self, name: &str) -> &mut Self {
            app32(&mut self.structs, TOK_BEGIN);
            self.structs.extend_from_slice(name.as_bytes());
            self.structs.push(0);
            pad4(&mut self.structs);
            self
        }
        fn end(&mut self) -> &mut Self {
            app32(&mut self.structs, TOK_END);
            self
        }
        fn prop(&mut self, name: &str, data: &[u8]) -> &mut Self {
            let off = self.str_off(name);
            app32(&mut self.structs, TOK_PROP);
            app32(&mut self.structs, data.len() as u32);
            app32(&mut self.structs, off);
            self.structs.extend_from_slice(data);
            pad4(&mut self.structs);
            self
        }
        fn prop32(&mut self, name: &str, v: u32) -> &mut Self {
            self.prop(name, &v.to_be_bytes())
        }
        fn reserve(&mut self, addr: u64, size: u64) -> &mut Self {
            self.rsv.extend(u64s(&[addr, size]));
            self
        }
        fn blob(&self) -> Vec<u8> {
            let mut structs = self.structs.clone();
            app32(&mut structs, TOK_END_TREE);
            let mut rsv = self.rsv.clone();
            rsv.extend([0u8; 16]);
            let rsv_off = HEADER_LEN;
            let struct_off = rsv_off + rsv.len();
            let strings_off = struct_off + structs.len();
            let total = strings_off + self.strs.len();
            let mut out = u32s(&[
                MAGIC,
                total as u32,
                struct_off as u32,
                strings_off as u32,
                rsv_off as u32,
                17,
                16,
                0,
                self.strs.len() as u32,
                structs.len() as u32,
            ]);
            out.extend(rsv);
            out.extend(structs);
            out.extend(&self.strs);
            out
        }
    }

    /// Een DTB in de vorm die de Pi-firmware afgeeft: twee geheugenbanken,
    /// /chosen met bootargs en initrd, een framebuffer en een memreserve.
    fn pi() -> Builder {
        let mut b = Builder::default();
        b.begin("")
            .prop32("#address-cells", 2)
            .prop32("#size-cells", 2)
            .prop("serial-number", b"100000001a2b3c4d\0");
        b.begin("memory@0")
            .prop("reg", &u64s(&[0x4000_0000, 0x4000_0000]))
            .end();
        b.begin("memory@80000000")
            .prop("reg", &u64s(&[0x8000_0000, 0x1_0000_0000]))
            .end();
        b.begin("chosen")
            .prop(
                "bootargs",
                b"console=serial0,115200 hopos.node=hop-1 hopos.tick=1\0",
            )
            .prop("linux,initrd-start", &u32s(&[0x200_0000]))
            .prop("linux,initrd-end", &u32s(&[0x200_0100]));
        b.begin("framebuffer@3e000000")
            .prop("reg", &u64s(&[0x3e00_0000, 0x80_0000]))
            .prop32("width", 1920)
            .prop32("height", 1080)
            .prop32("stride", 7680)
            .prop("format", b"a8r8g8b8\0")
            .end();
        b.end(); // chosen
        b.end(); // root
        b.reserve(0x3f00_0000, 0x10_0000);
        b
    }

    #[test]
    fn mem_regions() {
        let blob = pi().blob();
        let f = Fdt::new(&blob).unwrap();
        let regs = f.mem_regions().unwrap();
        assert_eq!(
            regs.as_slice(),
            &[
                Region {
                    base: 0x4000_0000,
                    size: 0x4000_0000
                },
                Region {
                    base: 0x8000_0000,
                    size: 0x1_0000_0000
                }
            ]
        );
        assert_eq!(f.mem_total(), Some(0x1_4000_0000));
    }

    #[test]
    fn chosen_and_root() {
        let blob = pi().blob();
        let f = Fdt::new(&blob).unwrap();
        assert_eq!(
            f.bootargs(),
            Some("console=serial0,115200 hopos.node=hop-1 hopos.tick=1")
        );
        assert_eq!(f.root_string("serial-number"), Some("100000001a2b3c4d"));
        // bootargs is een /chosen-property, niet van de root.
        assert_eq!(f.root_string("bootargs"), None);
        assert_eq!(f.initrd(), Some((0x200_0000, 0x200_0100)));
    }

    /// Een initrd met het image van Hop erin is groter dan een DTB mag zijn;
    /// alleen boven [`MAX_INITRD`] (of leeg, of achterstevoren) is hij weg.
    #[test]
    fn initrd_larger_than_a_dtb() {
        let chosen = |s: u64, e: u64| {
            let mut b = Builder::default();
            b.begin("").begin("chosen");
            b.prop("linux,initrd-start", &u64s(&[s]))
                .prop("linux,initrd-end", &u64s(&[e]));
            b.end().end();
            b.blob()
        };
        let s = 0x0a20_0000;
        let big = chosen(s, s + (3 << 20));
        assert_eq!(Fdt::new(&big).unwrap().initrd(), Some((s, s + (3 << 20))));
        let edge = chosen(s, s + MAX_INITRD);
        assert!(Fdt::new(&edge).unwrap().initrd().is_some());
        for (a, b) in [(s, s + MAX_INITRD + 1), (s, s), (s + 1, s)] {
            assert_eq!(Fdt::new(&chosen(a, b)).unwrap().initrd(), None);
        }
    }

    #[test]
    fn framebuffer_and_mem_reserve() {
        let blob = pi().blob();
        let f = Fdt::new(&blob).unwrap();
        assert_eq!(
            f.framebuffer(),
            Some(Fb {
                base: 0x3e00_0000,
                width: 1920,
                height: 1080,
                stride: 7680,
                bpp: 32
            })
        );
        assert_eq!(
            f.mem_reserve().as_slice(),
            &[Region {
                base: 0x3f00_0000,
                size: 0x10_0000
            }]
        );
    }

    /// DE REGRESSIE (04-08): /chosen met eigen cellen van 1, de reg als
    /// `<u32 base><u32 size>`. De pi()-blob bouwde de node met root-cellen en
    /// bevestigde dus de verkeerde aanname; deze bouwt hem zoals het ijzer.
    #[test]
    fn framebuffer_with_chosen_cells() {
        let mut b = Builder::default();
        b.begin("")
            .prop32("#address-cells", 2)
            .prop32("#size-cells", 2);
        b.begin("chosen")
            .prop32("#address-cells", 1)
            .prop32("#size-cells", 1);
        b.begin("framebuffer@3f800000")
            .prop("reg", &u32s(&[0x3f80_0000, 0x3f_4800]))
            .prop32("width", 1920)
            .prop32("height", 1080)
            .prop32("stride", 3840)
            .prop("format", b"r5g6b5\0")
            .end();
        b.end();
        b.end();
        let blob = b.blob();
        let fb = Fdt::new(&blob).unwrap().framebuffer().unwrap();
        assert_eq!((fb.base, fb.stride, fb.bpp), (0x3f80_0000, 3840, 16));
    }

    /// DE REGRESSIE: een header die zichzelf niet kan dragen (8 gedeclareerde
    /// bytes) mag niet door, want elke lezer leest daarna +8/+12/+16.
    #[test]
    fn refuses_a_header_too_small() {
        let mut short = u32s(&[MAGIC, 8]);
        short.extend([0u8; 4]);
        assert_eq!(Fdt::new(&short).err(), Some(Error::BadSize(8)));
        assert_eq!(total_size(&short), None);
        assert_eq!(Fdt::new(&[]).err(), Some(Error::NoMagic));
        let mut garbage = u32s(&[0xdead_beef, 1024]);
        garbage.extend([0u8; 64]);
        assert_eq!(Fdt::new(&garbage).err(), Some(Error::NoMagic));
        let blob = pi().blob();
        assert_eq!(Fdt::new(&blob).unwrap().size(), blob.len());
        assert_eq!(total_size(&blob), Some(blob.len()));
        // Een blob die langer declareert dan de aanroeper gaf.
        assert!(matches!(
            Fdt::new(&blob[..blob.len() - 1]),
            Err(Error::BadSize(_))
        ));
    }

    /// Offsets buiten de gedeclareerde blob zetten geen enkele lezer aan het
    /// lezen, ook niet met een plausibele totalsize.
    #[test]
    fn refuses_offsets_outside_the_blob() {
        let blob = pi().blob();
        for (off, name) in [
            (8, "off_dt_struct"),
            (12, "off_dt_strings"),
            (16, "off_mem_rsvmap"),
        ] {
            let mut broken = blob.clone();
            broken[off..off + 4].copy_from_slice(&((blob.len() + 0x1000) as u32).to_be_bytes());
            assert_eq!(Fdt::new(&broken).err(), Some(Error::BadOffset(name)));
        }
    }

    /// Een nameoff buiten het strings-blok: de property wordt overgeslagen en
    /// er wordt niet buiten het blok gelezen.
    #[test]
    fn nameoff_outside_the_strings_block() {
        let mut b = Builder::default();
        b.begin("")
            .prop32("#address-cells", 2)
            .prop32("#size-cells", 2);
        b.begin("memory@0")
            .prop("reg", &u64s(&[0x4000_0000, 0x100_0000]))
            .end();
        b.end();
        let mut blob = b.blob();
        let s_off = be32(&blob, 8).unwrap() as usize;
        let s_size = be32(&blob, 36).unwrap() as usize;
        let last = (s_off..s_off + s_size)
            .step_by(4)
            .rfind(|&p| be32(&blob, p) == Some(TOK_PROP))
            .unwrap();
        blob[last + 8..last + 12].copy_from_slice(&0xffffu32.to_be_bytes());
        let f = Fdt::new(&blob).unwrap();
        assert!(f.mem_regions().unwrap().is_empty());
        assert_eq!(f.mem_total(), None);
    }

    #[test]
    fn a_truncated_property_is_an_error_not_a_panic() {
        let mut b = Builder::default();
        b.begin("").prop("x", &[1, 2, 3, 4, 5, 6, 7, 8]).end();
        let mut blob = b.blob();
        // De lengte van de property opblazen tot voorbij het blok.
        let s_off = be32(&blob, 8).unwrap() as usize;
        let p = s_off + 12; // BEGIN + naam "" (4) + PROP-token
        blob[p..p + 4].copy_from_slice(&0x7fffu32.to_be_bytes());
        let f = Fdt::new(&blob).unwrap();
        assert_eq!(f.mem_regions().err(), Some(Error::Truncated));
        assert_eq!(f.bootargs(), None);
    }

    #[test]
    fn virtio_and_gic_from_a_built_blob() {
        let mut b = Builder::default();
        b.begin("")
            .prop32("#address-cells", 2)
            .prop32("#size-cells", 2);
        b.begin("virtio_mmio@a000200")
            .prop("interrupts", &u32s(&[0, 0x11, 1]))
            .prop("reg", &u64s(&[0xa00_0200, 0x200]))
            .prop("compatible", b"virtio,mmio\0")
            .end();
        b.begin("pl011@9000000")
            .prop("reg", &u64s(&[0x900_0000, 0x1000]))
            .prop("compatible", b"arm,pl011\0arm,primecell\0")
            .end();
        b.begin("intc@8000000")
            .prop("reg", &u64s(&[0x800_0000, 0x1_0000, 0x80a_0000, 0xf6_0000]))
            .prop("compatible", b"arm,gic-v3\0")
            .end();
        b.end();
        let blob = b.blob();
        let f = Fdt::new(&blob).unwrap();
        let v = f.virtio_mmio().unwrap();
        assert_eq!(
            v.as_slice(),
            &[VirtioMmio {
                reg: Region {
                    base: 0xa00_0200,
                    size: 0x200
                },
                intid: 49
            }]
        );
        assert_eq!(
            f.gic_v3(),
            Some(GicV3 {
                dist: Region {
                    base: 0x800_0000,
                    size: 0x1_0000
                },
                redist: Region {
                    base: 0x80a_0000,
                    size: 0xf6_0000
                }
            })
        );
    }

    /// De echte DTB van QEMU virt met `-m 3G -smp 4` en
    /// `-append "hopos.node=virt-1 hopos.init[]=a"`.
    #[test]
    fn qemu_virt_dtb() {
        let blob = include_bytes!("../testdata/qemu-virt.dtb");
        assert_eq!(total_size(blob), Some(blob.len()));
        let f = Fdt::new(blob).unwrap();
        assert_eq!(f.mem_total(), Some(3 << 30));
        assert_eq!(
            f.mem_regions().unwrap().as_slice(),
            &[Region {
                base: 0x4000_0000,
                size: 0xc000_0000
            }]
        );
        assert_eq!(f.bootargs(), Some("hopos.node=virt-1 hopos.init[]=a"));
        let v = f.virtio_mmio().unwrap();
        assert_eq!(v.len(), 32);
        let first = v.iter().find(|t| t.reg.base == 0xa00_0000).unwrap();
        assert_eq!(first.intid, 32 + 16);
        let last = v.iter().find(|t| t.reg.base == 0xa00_3e00).unwrap();
        assert_eq!(last.intid, 32 + 0x2f);
        let gic = f.gic_v3().unwrap();
        assert_eq!(gic.dist.base, 0x800_0000);
        assert_eq!(gic.redist.base, 0x80a_0000);
        assert_eq!(gic.redist.size, 0xf6_0000);
        assert_eq!(f.framebuffer(), None);
        assert_eq!(f.cpu_count(), Ok(4));
    }

    /// `nodes.dts`, met `dtc` gebouwd: GENET uit met een kind dat aan staat.
    const NODES: &[u8] = include_bytes!("../testdata/nodes.dtb");

    fn enabled(blob: &[u8], compatible: &str) -> Option<bool> {
        Fdt::new(blob).ok()?.enabled(compatible)
    }

    #[test]
    fn status_decides_and_children_do_not_leak() {
        assert_eq!(enabled(NODES, "brcm,bcm2711-genet-v5"), Some(false));
        assert_eq!(enabled(NODES, "brcm,genet-mdio-v5"), Some(true));
        assert_eq!(enabled(NODES, "brcm,bcm2711-pcie"), Some(true));
        assert_eq!(enabled(NODES, "brcm,bcm2835-mbox"), Some(true));
        assert_eq!(enabled(NODES, "brcm,bcm2712-pcie"), None);
        // De root-compatible is een lijst: het tweede woord telt ook.
        assert_eq!(enabled(NODES, "brcm,bcm2711"), Some(true));
    }

    #[test]
    fn a_broken_blob_is_not_enabled() {
        assert_eq!(enabled(&NODES[..40], "brcm,bcm2711-pcie"), None);
        assert_eq!(enabled(&[0; 64], "x"), None);
        let mut b = NODES.to_vec();
        b[4..8].copy_from_slice(&0x40u32.to_be_bytes());
        assert_eq!(enabled(&b, "brcm,bcm2711-pcie"), None);
    }
}
