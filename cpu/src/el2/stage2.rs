//! De stage-2-vertaaltabellen (ARMv8, VMSAv8-64) waarmee HOP een app-core
//! hardwarematig insluit: de kooi.
//!
//! De app op EL1 kan mappen wat hij wil, maar de IPA-naar-PA-vertaling die
//! hier vastligt laat alleen zijn eigen partitie door. Dít is de
//! isolatiebelofte: geen conventie maar een MMU-grens die de app niet kan
//! aanraken (de tabellen zelf staan in geen enkele map).
//!
//! De partitie-map is tevens de relocatie: een image is canoniek gelinkt en
//! de stage-2 vertaalt dat IPA-bereik naar de fysieke partitie van dít slot.
//! Zelfde artifact op elk slot, nul relocatiewerk, nul overhead.
//!
//! 4 KB-granule, 39-bit IPA (VTCR.T0SZ=25, startlevel 1): elke L1-entry
//! wijst één L2-tabel van 2 MB-blokken aan. GB0 blijft vrij voor een
//! optionele framebuffer-grant ([`grant_window`], en [`has_grant_window`]
//! voor de adoptie na een flip).
//!
//! Alles hier is geheugenrekenkunde over `dev`: op de host test het over een
//! buffer, en een walker leest de tabellen terug zoals de MMU dat zou doen.
//! Wélk blok bij welk slot hoort (het PA-plan) is van de kern; deze module
//! krijgt het blok als [`Pa`].

use super::Error;
use super::layout::{CAGE_STRIDE, CTX_LEN, CTX_OFF, FB_IPA, SMP_CTX_OFF};
use dev::Pa;

/// L1/L2-entry naar de volgende tabel.
pub(crate) const DESC_TABLE: u64 = 0x3;
/// L2-entry: een 2 MB-blok.
pub(crate) const DESC_BLOCK: u64 = 0x1;
/// L3-entry: een 4 KB-pagina.
pub(crate) const DESC_PAGE: u64 = 0x3;
/// Access flag.
pub(crate) const ATTR_AF: u64 = 1 << 10;
/// Inner shareable.
pub(crate) const ATTR_SH_INNER: u64 = 0x3 << 8;
/// S2AP: lezen en schrijven.
pub(crate) const ATTR_RW: u64 = 0x3 << 6;
/// MemAttr: Normal, write-back cacheable (stage-1 wint bij device).
pub(crate) const ATTR_NORMAL: u64 = 0xF << 2;
/// MemAttr: Normal non-cacheable (framebuffer-grant: de scanout leest mee,
/// dus geen cache-contract met de app).
pub(crate) const ATTR_NORM_NC: u64 = 0x5 << 2;

const BLOCK_RW: u64 = DESC_BLOCK | ATTR_AF | ATTR_SH_INNER | ATTR_RW | ATTR_NORMAL;
const BLOCK_RW_NC: u64 = DESC_BLOCK | ATTR_AF | ATTR_SH_INNER | ATTR_RW | ATTR_NORM_NC;
const PAGE_RW_NC: u64 = DESC_PAGE | ATTR_AF | ATTR_SH_INNER | ATTR_RW | ATTR_NORM_NC;

/// De exclusieve bovengrens van het 39-bit stage-2-regime.
pub const IPA_LIMIT: u64 = 1 << 39;
/// De bovengrens van de fysieke adresruimte die de trampolines toelaten:
/// beide klemmen VTCR.PS op 44 bits (16 TB, ruim boven elk DRAM-plan).
pub const PA_LIMIT: u64 = 1 << 44;

const BLOCK: u64 = 2 << 20;
const GB: u64 = 1 << 30;
const PAGE: u64 = 4096;

/// De L1 staat vooraan het kooiblok.
const L1_OFF: u64 = 0x0000;
/// Elf bestaande tabelpagina's in het kooiblok dekken kleine apps zonder
/// extra reservering: +1000..5000 en +a000..f000. De contexten op
/// +6000/+6800 en de framebuffer-pagina's op +7000..9000 houden hun plek.
const INLINE_L2_PAGES: u64 = 11;
/// FB-grant-L2: het venster op de firmware-framebuffer.
const L2_FB_OFF: u64 = 0x7000;
/// De twee rand-L3's van dat venster (kop en staart pagina-precies).
const L3_FB_HEAD_OFF: u64 = 0x8000;
const L3_FB_TAIL_OFF: u64 = 0x9000;

/// De node-only bytes die ná de zichtbare partitie van een kooi horen: 0
/// als de tabelpagina's van het kooiblok volstaan, anders één 2 MB-korrel
/// die elke L2 van het 39-bit-regime draagt. Onzin-bereiken geven 0;
/// [`build`] weigert ze.
#[must_use]
pub const fn table_reserve(ipa_base: u64, size: u64) -> u64 {
    if size == 0 || ipa_base >= IPA_LIMIT || size > IPA_LIMIT - ipa_base {
        return 0;
    }
    let pages = ((ipa_base & (GB - 1)) + size + GB - 1) >> 30;
    if pages > INLINE_L2_PAGES { BLOCK } else { 0 }
}

/// Toetst een partitie zonder iets aan te raken: alle weigeringen van
/// [`build`] staan hier, vóór de eerste schrijf.
fn check(ipa_base: u64, pa_base: u64, size: u64) -> Result<u64, Error> {
    if size == 0 || (ipa_base | pa_base | size) & (BLOCK - 1) != 0 {
        return Err(Error::Misaligned {
            ipa: ipa_base,
            pa: pa_base,
            size,
        });
    }
    // Eerst de grenzen, dan pas optellen: ongeldige invoer mag de
    // adresruimte niet laten omlopen en geen bestaande tabel beschadigen.
    if !(GB..IPA_LIMIT).contains(&ipa_base) || size > IPA_LIMIT - ipa_base {
        return Err(Error::IpaWindow {
            ipa: ipa_base,
            size,
        });
    }
    let extra = table_reserve(ipa_base, size);
    if pa_base >= PA_LIMIT || size > PA_LIMIT - pa_base || extra > PA_LIMIT - pa_base - size {
        return Err(Error::PaSpace { pa: pa_base, size });
    }
    Ok(extra)
}

/// Schrijft de stage-2-tabellen van één kooi in het kooiblok `block` en
/// geeft het fysieke adres van de L1 (voor VTTBR_EL2).
///
/// Het IPA-bereik `[ipa_base, ipa_base+size)` gaat op `[pa_base,
/// pa_base+size)`; het mag GB-grenzen kruisen binnen het 39-bit-venster. De
/// ABI-staart van de partitie valt in dezelfde map. Bij een grote partitie
/// ([`table_reserve`] niet nul) bezit de aanroeper óók die bytes op
/// `pa_base+size`: daar komen de L2's, buiten de zichtbare map.
///
/// Het secundaire ctx-blok in het kooiblok kan van een ándere kooi zijn en
/// blijft staan; al het andere in het blok begint schoon, ook een oude
/// framebuffer-grant en het primaire ctx-blok.
pub fn build(block: Pa, ipa_base: u64, pa_base: u64, size: u64) -> Result<Pa, Error> {
    let extra = check(ipa_base, pa_base, size)?;
    let smp_end = SMP_CTX_OFF + CTX_LEN;
    dev::clear(block, SMP_CTX_OFF as usize);
    dev::clear(block.add(smp_end), (CAGE_STRIDE - smp_end) as usize);
    let tables = Pa(pa_base + size);
    if extra != 0 {
        dev::clear(tables, extra as usize);
    }

    let first_gb = ipa_base >> 30;
    let mut off = 0;
    while off < size {
        let ipa = ipa_base + off;
        let gb = ipa >> 30;
        let table = gb - first_gb;
        let l2 = if extra == 0 {
            // Pagina 1..5, dan (contexten en framebuffer overslaand) 10..15.
            let page = if table >= 5 { table + 5 } else { table + 1 };
            block.add(page * PAGE)
        } else {
            tables.add(table * PAGE)
        };
        dev::write64(block.add(L1_OFF + gb * 8), l2.0 | DESC_TABLE);
        dev::write64(l2.add(((ipa >> 21) & 511) * 8), (pa_base + off) | BLOCK_RW);
        off += BLOCK;
    }

    // Coherentie ná de tabel-writes: de walker van de app-core leest deze
    // tabellen cacheable (VTCR IRGN/ORGN=WB), HOP schreef ze ongecached. Een
    // stale (clean) regel van een eerdere huurder van dit blok zou de walker
    // een oude tabel laten walken. Vegen vóór de dispatch; er draait nu geen
    // walker op dit blok, dus niets kan tussen de veeg en de start hercachen.
    dev::pull(block, CAGE_STRIDE as usize);
    if extra != 0 {
        dev::pull(tables, extra as usize);
    }
    dev::mb();
    Ok(block.add(L1_OFF))
}

/// Trekt de kooi in blok `block` in: de hard-kill.
///
/// Alleen de TABELLEN (tot het ctx-blok): een bewoner van een gedeelde core
/// kan op dit moment precies zijn staat aan het saven zijn, en een clear
/// daaroverheen liet een halve context achter die de rotatie als "saved"
/// hervat. De intrekking bereikt hem toch: zijn eerstvolgende hervatting
/// faultt op de genulde tabel en de switcher zet hem op dead.
///
/// De volgorde is heilig, want de walker van de app drááit nog en leest
/// cacheable: (1) de nullen schrijven, (2) clean+invalidate zodat een
/// her-walk nullen uit DRAM leest (andersom kon de walker tussen veeg en
/// nullen de oude tabel opnieuw cachen: de app overleefde de kill op echt
/// silicium, QEMU verhult dat), (3) pas dan de TLBI (`tlbi alle1is`, ter
/// plekke op EL2: in Go was dat een HVC naar de revoke-vector, vandaar de
/// naam `hvc_revoke` in `super::arch`).
///
/// EERLIJKE GRENS: een WFE-slaper doet geen toegangen en overleefde de
/// intrekking (verbrande core, 19-07); daarom eindigt de intrekking met
/// een SEV. Resteert de self-branch-lus uit een loop-buffer: per silicium
/// te meten.
pub fn revoke(block: Pa) {
    dev::clear(block, CTX_OFF as usize);
    dev::pull(block, CTX_OFF as usize);
    dev::mb();
    super::arch::hvc_revoke();
}

/// Mapt een fysiek venster Normal-NC op het vaste IPA-venster `FB_IPA` in
/// de bestaande kooi in blok `block`: de framebuffer-grant. Aanroepen ná
/// [`build`] en vóór de dispatch.
///
/// Een vast laag IPA houdt de framebuffer los van app-RAM, waar hij fysiek
/// ook ligt (QEMU-ramfb: 0x1bc7a0000, de vondst van 19-07).
///
/// PAGINA-PRECIES aan de randen. Een firmware-framebuffer is zelden
/// 2 MB-aligned, en met alleen 2 MB-blokken kreeg de grant-houder tot ~4 MB
/// firmware-geheugen rond de buffer erbij, RW. Daarom het volledig gedekte
/// middenstuk als blokken en de twee randen via één L3 elk: de overmapping
/// is dan hooguit de 4 KB-afronding aan elke kant. Het venster moet binnen
/// het FB-GB passen; een gevulde L1-entry daar is een plan-fout.
pub fn grant_window(block: Pa, pa: u64, size: u64) -> Result<(), Error> {
    if pa == 0 || size == 0 || pa.checked_add(size).is_none() {
        return Err(Error::Grant { pa, size });
    }
    // `lo` is het IPA-anker en blijft 2 MB-aligned: de app ziet de buffer op
    // FB_IPA + (pa-lo), hetzelfde contract als het FB_BASE dat de kern
    // afgeeft; hier niet aan rekenen zonder die kant mee te nemen.
    let lo = pa & !(BLOCK - 1);
    let pg_lo = pa & !(PAGE - 1);
    let pg_hi = (pa + size + PAGE - 1) & !(PAGE - 1);
    if pg_hi - lo > GB - (FB_IPA & (GB - 1)) {
        return Err(Error::Grant { pa, size });
    }
    let l2fb = block.add(L2_FB_OFF);
    let gb = FB_IPA >> 30;
    let l1e = block.add(L1_OFF + gb * 8);
    match dev::read64(l1e) {
        0 => dev::write64(l1e, l2fb.0 | DESC_TABLE),
        cur if cur == l2fb.0 | DESC_TABLE => {} // her-grant op hetzelfde slot
        cur => return Err(Error::GrantCollides { gb, entry: cur }),
    }
    // Verse tabellen: een her-grant mag geen pagina's van een vorig venster
    // laten staan.
    dev::clear(l2fb, PAGE as usize);
    dev::clear(block.add(L3_FB_HEAD_OFF), PAGE as usize);
    dev::clear(block.add(L3_FB_TAIL_OFF), PAGE as usize);

    let gb_base = gb << 30;
    let ipa_of = |p: u64| FB_IPA + (p - lo);
    let b_start = (pg_lo + BLOCK - 1) & !(BLOCK - 1);
    let b_end = pg_hi & !(BLOCK - 1);
    let mut off = b_start;
    while off < b_end {
        dev::write64(
            l2fb.add(((ipa_of(off) - gb_base) >> 21) * 8),
            off | BLOCK_RW_NC,
        );
        off += BLOCK;
    }
    // Eén L3 in het 2 MB-blok van [from, to), met uitsluitend die pagina's.
    let map_edge = |l3: Pa, from: u64, to: u64| {
        if from >= to {
            return;
        }
        let blk = ipa_of(from) & !(BLOCK - 1);
        dev::write64(l2fb.add(((blk - gb_base) >> 21) * 8), l3.0 | DESC_TABLE);
        let mut p = from;
        while p < to {
            dev::write64(l3.add(((ipa_of(p) - blk) >> 12) * 8), p | PAGE_RW_NC);
            p += PAGE;
        }
    };
    if b_start > pg_lo {
        map_edge(block.add(L3_FB_HEAD_OFF), pg_lo, b_start.min(pg_hi));
    }
    if b_end >= b_start && b_end < pg_hi {
        map_edge(block.add(L3_FB_TAIL_OFF), b_end.max(pg_lo), pg_hi);
    }
    // Zelfde coherentie-contract als `build`: de walker leest cacheable.
    for off in [L1_OFF, L2_FB_OFF, L3_FB_HEAD_OFF, L3_FB_TAIL_OFF] {
        dev::pull(block.add(off), PAGE as usize);
    }
    dev::mb();
    Ok(())
}

/// Toetst zonder te wijzigen of de kooi in blok `block` precies het venster
/// `[pa, pa+size)` mapt zoals [`grant_window`] het legt: de adoptie na een
/// kern-flip. Alleen de eigen vaste tabellen van het kooiblok worden
/// gevolgd; app-geheugen is nooit een tabelpointer. Een lege FB-ingang is
/// `false`; een afwijkende map is een fout, want een bredere oude grant is
/// geen bewijs van eigendom van déze framebuffer.
pub fn has_grant_window(block: Pa, pa: u64, size: u64) -> Result<bool, Error> {
    let bad = Error::Grant { pa, size };
    if pa == 0 || size == 0 || pa >= 1 << 48 || size > (1 << 48) - pa {
        return Err(bad);
    }
    let lo = pa & !(BLOCK - 1);
    let pg_lo = pa & !(PAGE - 1);
    let pg_hi = (pa + size + PAGE - 1) & !(PAGE - 1);
    if pg_hi - lo > GB - (FB_IPA & (GB - 1)) {
        return Err(bad);
    }
    let l2fb = block.add(L2_FB_OFF);
    match dev::read64(block.add(L1_OFF + (FB_IPA >> 30) * 8)) {
        0 => return Ok(false),
        root if root == l2fb.0 | DESC_TABLE => {}
        _ => return Err(bad),
    }
    let first = ((FB_IPA + pg_lo - lo) >> 21) & 511;
    let last = ((FB_IPA + pg_hi - lo - 1) >> 21) & 511;
    for idx in 0..512u64 {
        let e = dev::read64(l2fb.add(idx * 8));
        if idx < first || idx > last {
            if e != 0 {
                return Err(bad);
            }
            continue;
        }
        let p = (lo + (idx << 21)).wrapping_sub(FB_IPA & (GB - 1));
        if p >= pg_lo && p + BLOCK <= pg_hi && e == p | BLOCK_RW_NC {
            continue;
        }
        let table = [L3_FB_HEAD_OFF, L3_FB_TAIL_OFF]
            .map(|off| block.add(off))
            .into_iter()
            .find(|t| e == t.0 | DESC_TABLE)
            .ok_or(bad)?;
        for j in 0..512u64 {
            let page = p + j * PAGE;
            let want = if (pg_lo..pg_hi).contains(&page) {
                page | PAGE_RW_NC
            } else {
                0
            };
            if dev::read64(table.add(j * 8)) != want {
                return Err(bad);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Een kooiblok in host-geheugen, 4 KB-aligned (tabel-eis).
    struct Cage {
        _buf: Vec<u8>,
        block: Pa,
    }

    fn cage() -> Cage {
        let mut buf = vec![0xA5u8; (CAGE_STRIDE + PAGE) as usize];
        let p = buf.as_mut_ptr() as usize as u64;
        let block = Pa((p + PAGE - 1) & !(PAGE - 1));
        Cage { _buf: buf, block }
    }

    /// Echte host-pagina's voor de extra tabellen van een grote partitie;
    /// de payload zelf is een ondoorzichtig PA-bereik dat eindigt waar de
    /// tabellen beginnen (Build schrijft het alleen als descriptor-waarde).
    struct Extra {
        _buf: Vec<u8>,
        tables: u64,
    }

    fn extra() -> Extra {
        let mut buf = vec![0xA5u8; (2 * BLOCK) as usize];
        let p = buf.as_mut_ptr() as usize as u64;
        Extra {
            _buf: buf,
            tables: (p + BLOCK - 1) & !(BLOCK - 1),
        }
    }

    const POOL: u64 = 0x2_0000_0000;
    const SLOT1: u64 = 0x5000_0000;

    fn rd(pa: u64) -> u64 {
        dev::read64(Pa(pa))
    }

    fn pa_of(d: u64) -> u64 {
        d & 0x0000_FFFF_FFFF_F000
    }

    #[derive(Debug, PartialEq)]
    struct Leaf {
        ipa: u64,
        pa: u64,
        size: u64,
        rw: bool,
    }

    /// Loopt de tabellen af zoals de MMU. Een tabel mag alleen in het eigen
    /// kooiblok of in de eigen reservering wonen.
    fn walk(block: Pa, extra: Option<u64>) -> Vec<Leaf> {
        fn go(block: u64, extra: Option<u64>, tbl: u64, ipa: u64, level: u32, out: &mut Vec<Leaf>) {
            let in_cage = tbl >= block && tbl + PAGE <= block + CAGE_STRIDE;
            let in_extra = extra.is_some_and(|e| tbl >= e && tbl + PAGE <= e + BLOCK);
            assert!(in_cage || in_extra, "table {tbl:#x} outside cage metadata");
            assert_eq!(tbl & (PAGE - 1), 0);
            let shift = 39 - level * 9;
            for idx in 0..512u64 {
                let d = rd(tbl + idx * 8);
                let addr = ipa + (idx << shift);
                match (level, d & 3) {
                    _ if d == 0 => {}
                    (1 | 2, 3) => go(block, extra, pa_of(d), addr, level + 1, out),
                    (2, 1) => out.push(Leaf {
                        ipa: addr,
                        pa: pa_of(d),
                        size: BLOCK,
                        rw: (d >> 6) & 3 == 3,
                    }),
                    (3, 3) => out.push(Leaf {
                        ipa: addr,
                        pa: pa_of(d),
                        size: PAGE,
                        rw: (d >> 6) & 3 == 3,
                    }),
                    _ => panic!("unexpected descriptor {d:#x} at level {level} index {idx}"),
                }
            }
        }
        let mut out = Vec::new();
        go(block.0, extra, block.0 + L1_OFF, 0, 1, &mut out);
        out
    }

    fn assert_partition(block: Pa, extra: Option<u64>, ipa: u64, pa: u64, size: u64) {
        let leaves = walk(block, extra);
        assert_eq!(leaves.len() as u64, size >> 21);
        for (n, l) in leaves.iter().enumerate() {
            let off = (n as u64) << 21;
            assert_eq!(
                *l,
                Leaf {
                    ipa: ipa + off,
                    pa: pa + off,
                    size: BLOCK,
                    rw: true
                }
            );
        }
    }

    #[test]
    fn private_partition_in_every_shape() {
        let inline_max = 11 * GB - (SLOT1 & (GB - 1));
        for (ipa, size) in [
            (SLOT1, 64 << 20),
            (SLOT1, (768 << 20) + BLOCK),
            (SLOT1, 2 * GB),
            (SLOT1, 5 * GB),
            (SLOT1, inline_max),
            (SLOT1, 20 * GB),
            (SLOT1, IPA_LIMIT - SLOT1),
            (GB, BLOCK),
            (IPA_LIMIT - BLOCK, BLOCK),
        ] {
            let c = cage();
            let (pa, ex) = if table_reserve(ipa, size) != 0 {
                let e = extra();
                // De tabellen liggen fysiek direct achter de partitie, dus
                // de nep-PA van de partitie is "buffer min maat". Op de host
                // is het bufferadres een heap-adres van een paar GB; een
                // partitie die groter is dan dat adres is hier niet uit te
                // beelden en wordt overgeslagen (de walker-tests dekken de
                // tabelvorm van grote partities apart).
                if e.tables < size || e.tables + BLOCK > PA_LIMIT {
                    continue;
                }
                (e.tables - size, Some(e))
            } else {
                (POOL, None)
            };
            let l1 = build(c.block, ipa, pa, size).unwrap();
            assert_eq!(l1, c.block);
            assert_partition(c.block, ex.as_ref().map(|e| e.tables), ipa, pa, size);
        }
    }

    #[test]
    fn invalid_input_is_rejected_without_writes() {
        let c = cage();
        build(c.block, SLOT1, POOL, 2 * GB).unwrap();
        let before: Vec<u64> = (0..CAGE_STRIDE / 8)
            .map(|i| rd(c.block.0 + i * 8))
            .collect();
        for (ipa, pa, size) in [
            (SLOT1, POOL, 0),
            (SLOT1 + 4096, POOL, BLOCK),
            (SLOT1, POOL + 4096, BLOCK),
            (SLOT1, POOL, BLOCK + 4096),
            (FB_IPA, POOL, BLOCK),
            (SLOT1, POOL, IPA_LIMIT - SLOT1 + BLOCK),
            (IPA_LIMIT, POOL, BLOCK),
            (!(BLOCK - 1), POOL, BLOCK),
            (SLOT1, POOL, !(BLOCK - 1)),
            (SLOT1, !(BLOCK - 1), BLOCK),
            (SLOT1, PA_LIMIT - BLOCK, 2 * BLOCK),
            (SLOT1, PA_LIMIT - 20 * GB, 20 * GB),
        ] {
            assert!(
                build(c.block, ipa, pa, size).is_err(),
                "{ipa:#x} {pa:#x} {size:#x}"
            );
            let now: Vec<u64> = (0..CAGE_STRIDE / 8)
                .map(|i| rd(c.block.0 + i * 8))
                .collect();
            assert_eq!(before, now, "rejected input changed the cage");
        }
    }

    #[test]
    fn neighbors_stay_isolated() {
        let (a, b) = (cage(), cage());
        build(a.block, SLOT1, POOL, 5 * GB).unwrap();
        build(b.block, SLOT1, POOL + 5 * GB, 5 * GB).unwrap();
        assert_partition(a.block, None, SLOT1, POOL, 5 * GB);
        assert_partition(b.block, None, SLOT1, POOL + 5 * GB, 5 * GB);
    }

    #[test]
    fn rebuild_keeps_the_secondary_context_and_clears_the_rest() {
        let c = cage();
        let secondary = c.block.add(SMP_CTX_OFF);
        for off in (0..CTX_LEN).step_by(8) {
            dev::write64(secondary.add(off), 0x5350_4d00_0000_0000 | off);
        }
        build(c.block, SLOT1, POOL, 5 * GB).unwrap();
        dev::write64(c.block.add(CTX_OFF), 4);
        grant_window(c.block, 0x3e10_8000, 8 << 20).unwrap();
        build(c.block, SLOT1, POOL, 8 << 20).unwrap();
        for off in (0..CTX_LEN).step_by(8) {
            assert_eq!(dev::read64(secondary.add(off)), 0x5350_4d00_0000_0000 | off);
        }
        assert_eq!(dev::read64(c.block.add(CTX_OFF)), 0);
        assert_partition(c.block, None, SLOT1, POOL, 8 << 20);
    }

    #[test]
    fn revoke_zeroes_tables_but_not_contexts() {
        let c = cage();
        build(c.block, SLOT1, POOL, 64 << 20).unwrap();
        dev::write64(c.block.add(CTX_OFF), 2);
        revoke(c.block);
        assert!(walk(c.block, None).is_empty());
        assert_eq!(dev::read64(c.block.add(CTX_OFF)), 2);
    }

    #[test]
    fn reservation_boundary() {
        let inline_max = 11 * GB - (SLOT1 & (GB - 1));
        for (size, want) in [
            (0, 0),
            (5 * GB, 0),
            (inline_max, 0),
            (inline_max + BLOCK, BLOCK),
            (20 * GB, BLOCK),
            (IPA_LIMIT - SLOT1, BLOCK),
        ] {
            assert_eq!(table_reserve(SLOT1, size), want, "{size:#x}");
        }
    }

    /// De fysieke pagina waarop de kooi pagina `p` van de grant afbeeldt (0
    /// = niet gemapt), met de Normal-NC-toets onderweg.
    fn grant_mapped(block: Pa, lo: u64, p: u64) -> u64 {
        let gb_base = (FB_IPA >> 30) << 30;
        let ipa = FB_IPA + (p - lo);
        let e = rd(block.0 + L2_FB_OFF + ((ipa - gb_base) >> 21) * 8);
        match e & 3 {
            0 => 0,
            1 => {
                assert_eq!((e >> 2) & 0xF, 0x5);
                pa_of(e) + (ipa & (BLOCK - 1))
            }
            _ => {
                let pe = rd(pa_of(e) + ((ipa & (BLOCK - 1)) >> 12) * 8);
                if pe == 0 {
                    return 0;
                }
                assert_eq!(pe & 3, DESC_PAGE);
                assert_eq!((pe >> 2) & 0xF, 0x5);
                pa_of(pe) + (ipa & (PAGE - 1))
            }
        }
    }

    #[test]
    fn grant_window_is_page_exact() {
        let c = cage();
        build(c.block, SLOT1, POOL, 4 << 20).unwrap();
        let fb = 0x3E10_8000u64;
        let size = 1920 * 4 * 1080 - 3;
        grant_window(c.block, fb, size).unwrap();
        let l1e = rd(c.block.0 + (FB_IPA >> 30) * 8);
        assert_eq!(l1e, (c.block.0 + L2_FB_OFF) | DESC_TABLE);
        let lo = fb & !(BLOCK - 1);
        let (pg_lo, pg_hi) = (fb & !(PAGE - 1), (fb + size + PAGE - 1) & !(PAGE - 1));
        let mut p = pg_lo;
        while p < pg_hi {
            assert_eq!(grant_mapped(c.block, lo, p), p);
            p += PAGE;
        }
        assert_eq!(grant_mapped(c.block, lo, pg_lo - PAGE), 0);
        assert_eq!(grant_mapped(c.block, lo, pg_hi), 0);
        // Idempotent, en een framebuffer boven 4 GB werkt gewoon.
        grant_window(c.block, fb, size).unwrap();
        let high = 0x1_BC7A_0000u64;
        grant_window(c.block, high, size).unwrap();
        let hlo = high & !(BLOCK - 1);
        assert_eq!(grant_mapped(c.block, hlo, high), high);
        assert_eq!(grant_mapped(c.block, hlo, high - PAGE), 0);
        // Een botsing in het FB-GB wordt geweigerd.
        dev::write64(c.block.add((FB_IPA >> 30) * 8), 0x1234_5003);
        assert!(grant_window(c.block, fb, size).is_err());
    }

    /// De toets van de adoptie leest precies terug wat `grant_window` legde,
    /// en niets anders: een kortere of langere buffer, een vreemde
    /// tabelpointer, een lege of te zwakke rand-pagina of een extra blok
    /// is een fout.
    #[test]
    fn has_grant_window_reads_back_exactly_the_grant() {
        let size = (8u64 << 20) - 3;
        let fb = 0x3E10_8000u64;
        let c = cage();
        build(c.block, SLOT1, POOL, 4 << 20).unwrap();
        assert_eq!(has_grant_window(c.block, fb, size), Ok(false));
        for pa in [fb, 0x1_bc7a_0000, 0x4000_0000] {
            let c = cage();
            build(c.block, SLOT1, POOL, 4 << 20).unwrap();
            grant_window(c.block, pa, size).unwrap();
            assert_eq!(has_grant_window(c.block, pa, size), Ok(true));
            assert!(has_grant_window(c.block, pa, size - PAGE).is_err());
            assert!(has_grant_window(c.block, pa, size + PAGE).is_err());
        }
        let head = L3_FB_HEAD_OFF + ((fb & (BLOCK - 1)) >> 12) * 8;
        for (off, value) in [
            (L1_OFF + (FB_IPA >> 30) * 8, 0x1003),
            (L2_FB_OFF + (FB_IPA >> 21) * 8, 0x1003),
            (head, 0),
            (head, fb | (PAGE_RW_NC & !ATTR_RW)),
            (L2_FB_OFF, 0x4000_0000 | BLOCK_RW_NC),
        ] {
            let c = cage();
            build(c.block, SLOT1, POOL, 4 << 20).unwrap();
            grant_window(c.block, fb, size).unwrap();
            dev::write64(c.block.add(off), value);
            assert!(!matches!(has_grant_window(c.block, fb, size), Ok(true)));
        }
        for pa in [0, u64::MAX - 1, 1 << 48] {
            assert!(has_grant_window(c.block, pa, 4096).is_err());
        }
    }
}
