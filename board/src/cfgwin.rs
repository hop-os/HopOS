//! Het config-venster: `hopos.cfg` IN het kern-image, op elk board.
//!
//! Het formaat van de Go-generatie (`image/hopcfg` en `image/mkcard
//! -cfgwindow` op tag v2.2.8, en hop-imager's `internal/cfgwin`): een
//! magic-kopregel, de config, en `#`-regels als padding tot een vaste maat:
//!
//! ```text
//! #HOPCFG1 window=16384 len=0000000432
//! hopos.node=hop-1
//! ...
//! ################################################################ (padding)
//! ```
//!
//! `len` telt de configbytes na de kopregel, in tien vaste cijfers (de kopregel
//! blijft bij elke herschrijving even lang); `window` is de hele maat. Voor de
//! parser (`fw::bootcfg`) is het hele venster gewoon een configbestand met
//! commentaar. Voor een imager is het een blok bytes dat hij in een image,
//! een flipbundel of op een kaart vindt aan zijn kopregel en ter plekke
//! herschrijft, zonder filesystem: `image/hopcfg.py` in deze repo, `hop
//! image` in de hop-repo.
//!
//! In Go lag het venster op de kaart (`hopos.cfg` als bestand van 1 MiB in
//! de FAT) en alleen op de LicheeRV en de M4 in de kern. Hier ligt het op
//! elk board in de kern zelf, als [`WINDOW`] in `.data.hopcfg`: de kaart, de
//! stick, het bootobject van de M4, de FIP van de LicheeRV en de flipbundel
//! dragen dan dezelfde bytes, en één imager vult ze allemaal. De plek in het
//! image verschilt per board en per build (het eind van `.text` beslist); de
//! kopregel vindt hem, zoals in Go, en hij staat op een 4 KiB-grens.
//!
//! Een gevuld venster IS het configbestand van de node: het vervangt
//! `hopos.cfg` van de ESP, de initrd van de Radxa en het venster van de
//! m1n1-lader, en het wint van de bootargs (zoals op
//! de Radxa). Een leeg venster (`len=0000000000`, zo komt het uit de linker)
//! laat die bronnen staan als terugval.

use core::cell::UnsafeCell;

/// De maat van het venster: 16 KiB, wat de UEFI-stub van `hopos.cfg` op de
/// ESP las. De config van een smaak uit `image/cfg` is hoogstens ~2 KiB, de
/// GUI-config van Go was ~9 KiB.
pub const SIZE: usize = 16 << 10;

/// Het begin van de kopregel.
pub const MAGIC: &[u8] = b"#HOPCFG1 window=";

/// De kopregel van een venster: wat [`parse`] uit de eerste bytes leest.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// De hele maat van het venster.
    pub size: usize,
    /// De lengte van de kopregel, met de newline.
    pub head: usize,
    /// De lengte van de config na de kopregel (0 = leeg venster).
    pub len: usize,
}

/// Leest de kopregel aan het begin van `b`: `None` als het geen venster is.
/// Dezelfde eisen als Go's `parseHeader`: een 512-voud als maat, precies
/// tien cijfers, en de config moet in het venster passen.
#[must_use]
pub fn parse(b: &[u8]) -> Option<Head> {
    let rest = b.strip_prefix(MAGIC)?;
    let sp = rest.iter().take(8).position(|&c| c == b' ')?;
    let size = number(rest.get(..sp)?)?;
    let digits = rest.get(sp..)?.strip_prefix(b" len=")?;
    if digits.get(10) != Some(&b'\n') {
        return None;
    }
    let len = number(digits.get(..10)?)?;
    let head = MAGIC.len() + sp + 5 + 11;
    (sp > 0 && size.is_multiple_of(512) && head + len <= size).then_some(Head { size, head, len })
}

/// Een decimaal getal van alleen cijfers.
fn number(d: &[u8]) -> Option<usize> {
    if d.is_empty() || d.len() > 10 {
        return None;
    }
    d.iter().try_fold(0usize, |n, &c| {
        c.is_ascii_digit().then(|| n * 10 + usize::from(c - b'0'))
    })
}

/// De config in venster `b`, zonder kopregel en padding; "" voor een leeg
/// venster, een krom venster of tekst die geen UTF-8 is (dan geen config,
/// niet een halve).
#[must_use]
pub fn config(b: &[u8]) -> &str {
    let Some(h) = parse(b) else { return "" };
    if h.size > b.len() {
        return "";
    }
    b.get(h.head..h.head + h.len)
        .and_then(|t| core::str::from_utf8(t).ok())
        .unwrap_or("")
}

/// Het lege venster zoals de linker het neerlegt: de kopregel met
/// `len=0000000000` en `#`-regels van 64 tekens plus newline, zoals Go.
const fn empty() -> [u8; SIZE] {
    let mut w = [b'#'; SIZE];
    let mut at = 0;
    while at < MAGIC.len() {
        w[at] = MAGIC[at];
        at += 1;
    }
    // De maat in cijfers, van achter naar voren in een eigen buffer.
    let mut digits = [0u8; 10];
    let (mut n, mut d) = (SIZE, digits.len());
    while n > 0 {
        d -= 1;
        digits[d] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    while d < digits.len() {
        w[at] = digits[d];
        at += 1;
        d += 1;
    }
    let tail = b" len=0000000000\n";
    let mut j = 0;
    while j < tail.len() {
        w[at] = tail[j];
        at += 1;
        j += 1;
    }
    // Padding: elke 65e byte een newline, de laatste byte ook.
    while at < SIZE {
        let end = if at + 65 < SIZE { at + 65 } else { SIZE };
        w[end - 1] = b'\n';
        at = end;
    }
    w
}

/// Het venster, page-aligned zodat een flip het in het nieuwe beeld op
/// 4 KiB-stappen vindt.
#[repr(C, align(4096))]
struct Window(UnsafeCell<[u8; SIZE]>);

// SAFETY: niemand schrijft het venster terwijl de kern draait: de imager
// schreef het bestand vóór de boot, en de flip schrijft het venster van het
// NIEUWE beeld, nooit dit. Gedeeld lezen is dus veilig.
unsafe impl Sync for Window {}

/// Het venster van deze kern. `UnsafeCell` en `#[used]`: de waarde verandert
/// buiten de compiler om (een imager patcht het bestand), dus mag hij de
/// beginwaarde niet invouwen, en de linker mag het niet weggooien (de
/// imager moet in elk image precies één venster vinden).
#[used]
#[cfg_attr(target_os = "none", unsafe(link_section = ".data.hopcfg"))]
static WINDOW: Window = Window(UnsafeCell::new(empty()));

/// De bytes van het venster van deze kern (voor de flip: ons venster mee).
#[must_use]
pub fn bytes() -> &'static [u8] {
    // `black_box`: het adres is voor de optimizer ondoorzichtig, zodat hij
    // de lezingen niet tot de beginwaarde vouwt.
    let p = core::hint::black_box(WINDOW.0.get());
    // SAFETY: `p` wijst naar een static van SIZE bytes die leeft zolang het
    // programma; niemand schrijft erin na de boot (zie `Sync` hierboven).
    unsafe { &*p }
}

/// De config uit het venster van deze kern; "" als het leeg is.
#[must_use]
pub fn text() -> &'static str {
    config(bytes())
}

/// Wat er in het venster van deze kern staat, voor de bootregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Leeg (`len=0000000000`): de bronnen van het board tellen.
    Empty,
    /// Een config van zoveel bytes.
    Config(usize),
    /// Een kromme kopregel of tekst die geen UTF-8 is: genegeerd.
    Bad,
}

/// De toestand van het venster van deze kern.
#[must_use]
pub fn state() -> State {
    state_of(bytes())
}

/// [`state`] van venster `b`.
fn state_of(b: &[u8]) -> State {
    match (parse(b), config(b).len()) {
        (Some(h), 0) if h.len == 0 => State::Empty,
        (Some(_), n) if n > 0 => State::Config(n),
        _ => State::Bad,
    }
}

/// Wat [`carry`] deed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Carry {
    /// Ons venster staat nu in het nieuwe beeld.
    Done,
    /// Het nieuwe beeld heeft een gevuld venster en houdt dat.
    Own,
    /// Wij hebben geen config: niets om mee te geven.
    Nothing,
    /// Het nieuwe beeld heeft geen venster (een kern van vóór dit module).
    NoWindow,
}

/// FLIP: geeft venster `ours` mee aan `image`, het platte beeld van de
/// nieuwe kern vanaf zijn linkadres, als dat een leeg venster heeft. Het
/// venster staat in elk image op een 4 KiB-grens (de uitlijning van
/// [`WINDOW`]) en het linkadres ook, dus de kopregel staat op een
/// 4 KiB-stap vanaf het begin. Een gevuld venster in het beeld (`CFG=` van
/// `image/flip-bundle.sh`, of `hop image`) blijft staan: zo brengt een flip
/// bewust een andere config.
pub fn carry(ours: &[u8], image: &mut [u8]) -> Carry {
    let mut off = 0;
    while let Some(w) = image.get_mut(off..off + SIZE) {
        match parse(w) {
            Some(h) if h.size == SIZE && h.len > 0 => return Carry::Own,
            Some(h) if h.size == SIZE => {
                if config(ours).is_empty() || ours.len() != SIZE {
                    return Carry::Nothing;
                }
                w.copy_from_slice(ours);
                return Carry::Done;
            }
            _ => off += 4096,
        }
    }
    Carry::NoWindow
}

/// De config van de node: het venster als het gevuld is, anders `medium`
/// (`hopos.cfg` van de ESP, de initrd, de lader).
#[must_use]
pub fn or(medium: &'static str) -> &'static str {
    let t = text();
    if t.is_empty() { medium } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Een venster van `size` bytes met `content`, precies zoals Go's
    /// `makeWindow` (en `image/hopcfg.py`, `hop image`) het schrijft.
    fn window(content: &[u8], size: usize) -> Vec<u8> {
        let mut c = content.to_vec();
        if c.last().is_some_and(|&b| b != b'\n') {
            c.push(b'\n');
        }
        let mut w = std::format!("#HOPCFG1 window={size} len={:010}\n", c.len()).into_bytes();
        w.extend_from_slice(&c);
        while w.len() < size {
            let n = (size - w.len()).min(65);
            w.extend(std::iter::repeat_n(b'#', n - 1));
            w.push(b'\n');
        }
        w
    }

    #[test]
    fn the_linker_lays_down_an_empty_go_window() {
        assert_eq!(empty().as_slice(), window(b"", SIZE).as_slice());
        let h = parse(&empty()).unwrap();
        assert_eq!(
            h,
            Head {
                size: SIZE,
                head: 37,
                len: 0
            }
        );
        // De static van deze build: leeg, op een 4 KiB-grens, één venster.
        assert_eq!(bytes(), empty().as_slice());
        assert_eq!(text(), "");
        assert_eq!(bytes().as_ptr() as usize % 4096, 0);
        assert_eq!(or("hopos.node=esp\n"), "hopos.node=esp\n");
        assert_eq!(state(), State::Empty);
    }

    #[test]
    fn a_filled_window_is_the_config_and_the_padding_is_comment() {
        let cfg =
            b"hopos.node=lrv\nhopos.init[]={\"name\":\"a b\"}\n# niet\nhopos.hop.sharegroup=system";
        let w = window(cfg, SIZE);
        assert_eq!(w.len(), SIZE);
        let t = config(&w);
        assert_eq!(t.len(), cfg.len() + 1, "de newline erachter telt mee");
        assert_eq!(fw::bootcfg::get(t, "hopos.node"), "lrv");
        assert_eq!(fw::bootcfg::get(t, "hopos.init[]"), "{\"name\":\"a b\"}");
        // Het hele venster gelezen als bestand (Go las het zo): hetzelfde.
        let whole = core::str::from_utf8(&w).unwrap();
        assert_eq!(fw::bootcfg::get(whole, "hopos.hop.sharegroup"), "system");
        assert_eq!(fw::bootcfg::all(whole, "hopos.node").count(), 1);
        // Een andere maat dan de onze is ook een venster (de lezer volgt de kop).
        let big = window(cfg, 1 << 20);
        assert_eq!(parse(&big).unwrap().size, 1 << 20);
        assert_eq!(config(&big), t);
    }

    #[test]
    fn a_crooked_head_is_no_config() {
        let good = window(b"hopos.node=x\n", 1024);
        assert_eq!(config(&good), "hopos.node=x\n");
        let bad = |from: &str, to: &str| {
            let s = std::string::String::from_utf8(good.clone()).unwrap();
            config(s.replacen(from, to, 1).as_bytes()).is_empty()
        };
        assert!(bad("#HOPCFG1", "#HOPCFG2"), "een andere magic");
        assert!(bad("window=1024", "window=1000"), "geen 512-voud");
        assert!(bad("window=1024", "window=x024"), "geen getal");
        assert!(bad("len=0000000013", "len=000000013"), "negen cijfers");
        assert!(
            bad("len=0000000013", "len=0000002000"),
            "langer dan het venster"
        );
        assert!(bad("len=0000000013\n", "len=0000000013 "), "geen newline");
        // Een venster dat langer zegt te zijn dan de bytes die er zijn.
        assert_eq!(config(&good[..512]), "");
        // Geen UTF-8: geen config, geen paniek.
        let mut w = window(b"hopos.node=xx\n", 1024);
        assert_eq!(state_of(&w), State::Config(14));
        w[40] = 0xff;
        assert_eq!(config(&w), "");
        assert_eq!(state_of(&w), State::Bad);
        assert_eq!(state_of(&good[1..]), State::Bad);
    }

    #[test]
    fn the_window_goes_along_into_a_bundle_with_an_empty_one() {
        let ours = window(b"hopos.node=m4\n", SIZE);
        // Een beeld met rommel en een leeg venster op +0x3000.
        let mut img = std::vec![0x55u8; 0x3000];
        img.extend_from_slice(&empty());
        img.extend_from_slice(&[0xaa; 0x800]);
        assert_eq!(carry(&ours, &mut img), Carry::Done);
        assert_eq!(config(&img[0x3000..]), "hopos.node=m4\n");
        assert_eq!(img[0x2fff], 0x55);
        assert_eq!(img[0x3000 + SIZE], 0xaa);
        // Nu draagt het beeld een config: het houdt de zijne.
        let theirs = window(b"hopos.node=ander\n", SIZE);
        img[0x3000..0x3000 + SIZE].copy_from_slice(&theirs);
        assert_eq!(carry(&ours, &mut img), Carry::Own);
        assert_eq!(config(&img[0x3000..]), "hopos.node=ander\n");
        // Zonder eigen config: niets mee; zonder venster in het beeld ook.
        img[0x3000..0x3000 + SIZE].copy_from_slice(&empty());
        assert_eq!(carry(&empty(), &mut img), Carry::Nothing);
        assert_eq!(config(&img[0x3000..]), "");
        img[0x3000] = 0;
        assert_eq!(carry(&ours, &mut img), Carry::NoWindow);
        // Een venster dat niet op een 4 KiB-stap staat, telt niet.
        let mut odd = std::vec![0u8; 0x10];
        odd.extend_from_slice(&empty());
        assert_eq!(carry(&ours, &mut odd), Carry::NoWindow);
    }
}
