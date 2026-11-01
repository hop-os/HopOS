//! De payloads van de codec-ops (Go: `OLD/metal/abi/hopabi/codec.go`), alleen
//! met de feature `media`.
//!
//! Ze doen iets anders dan de rest van de ABI: de opslag-ops dragen bytes,
//! deze dragen AANWIJZINGEN naar bytes die al op hun plek liggen. Een beeld
//! van 4K in P010 is 24 MB, bij 24 fps 597 MB/s, en het slot-LAN piekt op
//! 550 MB/s: die beelden kunnen niet over de system-calls, en ze horen er
//! ook niet (handboek §6: een beeld is een grant). De buffer zelf staat in
//! `Req::off`/`Req::n`: een afstand vanaf het begin van de eigen partitie.
//!
//! Elk record heeft een vaste lengte, little-endian, zonder varint of
//! optionele velden: de kern leest ze op de rand van zijn vertrouwensgrens.

use crate::{Error, Result};

/// De lengte van [`OpenArgs`] op de draad.
pub const OPEN_ARGS_LEN: usize = 8;
/// De lengte van [`FeedArgs`].
pub const FEED_ARGS_LEN: usize = 24;
/// De lengte van [`BufArgs`].
pub const BUF_ARGS_LEN: usize = 4;
/// De lengte van één [`Event`].
pub const EVENT_LEN: usize = 64;

/// Het soort van een [`Event`]. Bewust NIET de nummering van
/// `driver-codec`: dit is een draadformaat, en dat verschuift niet als
/// er intern een soort bijkomt. De kern vertaalt expliciet.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Kind {
    /// Niets (of een soort die deze kant nog niet kent).
    #[default]
    None = 0,
    /// De stream is herkend: `width`, `height`, `pixel`, de vlakken, en
    /// in `size` de minimale buffermaat en in `bytes` het aantal buffers.
    Format = 1,
    /// Een invoerbuffer is weer van de app.
    Consumed = 2,
    /// Een resultaat staat in de buffer; `bytes` 0 is "niet om te tonen".
    Produced = 3,
    /// De stream is af.
    Done = 4,
    /// De sessie is stuk: sluiten en opnieuw openen.
    Fault = 5,
}

impl Kind {
    /// Het soort van een rauw getal; onbekend is [`Kind::None`].
    #[must_use]
    pub const fn from_raw(v: u8) -> Kind {
        match v {
            1 => Kind::Format,
            2 => Kind::Consumed,
            3 => Kind::Produced,
            4 => Kind::Done,
            5 => Kind::Fault,
            _ => Kind::None,
        }
    }
}

fn short(b: &[u8], need: usize) -> Result {
    if b.len() < need {
        return Err(Error::Short { len: b.len(), need });
    }
    Ok(())
}

fn le16(b: &[u8], i: usize) -> u16 {
    super::le16(b, i)
}

/// Opent een sessie. Codec, richting en pixel zijn de nummering van
/// `driver-codec`; de app kent die namen via applib.
///
/// ```text
/// 0 codec u8 | dir u8 | pixel u8 | _ u8 | width u16 | height u16
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct OpenArgs {
    /// De codec.
    pub codec: u8,
    /// 0 decode, 1 encode.
    pub dir: u8,
    /// Het pixelformaat.
    pub pixel: u8,
    /// Breedte (hint bij decode).
    pub width: u16,
    /// Hoogte (hint bij decode).
    pub height: u16,
}

impl OpenArgs {
    /// De draadvorm.
    #[must_use]
    pub fn encode(&self) -> [u8; OPEN_ARGS_LEN] {
        let mut b = [0u8; OPEN_ARGS_LEN];
        b[0] = self.codec;
        b[1] = self.dir;
        b[2] = self.pixel;
        b[4..6].copy_from_slice(&self.width.to_le_bytes());
        b[6..8].copy_from_slice(&self.height.to_le_bytes());
        b
    }

    /// Leest de draadvorm.
    pub fn decode(b: &[u8]) -> Result<OpenArgs> {
        short(b, OPEN_ARGS_LEN)?;
        Ok(OpenArgs {
            codec: b[0],
            dir: b[1],
            pixel: b[2],
            width: le16(b, 4),
            height: le16(b, 6),
        })
    }
}

/// Hoort bij `OP_CODEC_FEED`; de buffer staat in `off`/`n` van de call.
///
/// ```text
/// 0 handle u32 | flags u32 | filled u64 | tag u64
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct FeedArgs {
    /// Het sessiehandvat uit open.
    pub handle: u32,
    /// EOS 1, headers 2, keyframe 4.
    pub flags: u32,
    /// Hoeveel bytes er werkelijk in staan.
    pub filled: u64,
    /// Komt ongewijzigd terug op het resultaat.
    pub tag: u64,
}

impl FeedArgs {
    /// De draadvorm.
    #[must_use]
    pub fn encode(&self) -> [u8; FEED_ARGS_LEN] {
        let mut b = [0u8; FEED_ARGS_LEN];
        b[0..4].copy_from_slice(&self.handle.to_le_bytes());
        b[4..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..16].copy_from_slice(&self.filled.to_le_bytes());
        b[16..24].copy_from_slice(&self.tag.to_le_bytes());
        b
    }

    /// Leest de draadvorm.
    pub fn decode(b: &[u8]) -> Result<FeedArgs> {
        short(b, FEED_ARGS_LEN)?;
        Ok(FeedArgs {
            handle: super::le32(b, 0),
            flags: super::le32(b, 4),
            filled: super::le64(b, 8),
            tag: super::le64(b, 16),
        })
    }
}

/// De kale sessieverwijzing van offer, poll en close.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct BufArgs {
    /// Het sessiehandvat.
    pub handle: u32,
}

impl BufArgs {
    /// De draadvorm.
    #[must_use]
    pub fn encode(&self) -> [u8; BUF_ARGS_LEN] {
        self.handle.to_le_bytes()
    }

    /// Leest de draadvorm.
    pub fn decode(b: &[u8]) -> Result<BufArgs> {
        short(b, BUF_ARGS_LEN)?;
        Ok(BufArgs {
            handle: super::le32(b, 0),
        })
    }
}

/// Eén event uit een poll, met de buffer waar het over gaat. `off` is
/// weer de afstand vanaf het begin van de partitie, zodat de app hem
/// herkent zonder iets van fysiek geheugen te weten.
///
/// Een Format-event gaat over geen buffer en hergebruikt twee velden:
/// `size` is de minimale buffermaat, `bytes` het aantal buffers dat het
/// ijzer tegelijk wil vasthouden.
///
/// ```text
/// 0  kind u8 | key u8 | pixel u8 | _ u8
/// 4  width u16 | height u16
/// 8  off u64 | size u64 | bytes u64 | tag u64
/// 40 stride[3] u16 | _ u16
/// 48 plane[3] u32 | _ u32      (= 64)
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Event {
    /// Het soort.
    pub kind: Kind,
    /// Bij een bitstream-resultaat: een keyframe.
    pub key: bool,
    /// Bij Format: het pixelformaat.
    pub pixel: u8,
    /// Zichtbaar beeld.
    pub width: u16,
    /// Zichtbaar beeld.
    pub height: u16,
    /// De buffer, vanaf het begin van de partitie.
    pub off: u64,
    /// De maat van de buffer; bij Format de minimale buffermaat.
    pub size: u64,
    /// Bruikbare bytes (0 = niets); bij Format het aantal buffers.
    pub bytes: u64,
    /// De tag van de invoer.
    pub tag: u64,
    /// Bytes per regel per vlak; 0 = het vlak bestaat niet.
    pub stride: [u16; 3],
    /// Begin van elk vlak, vanaf het begin van de buffer.
    pub plane: [u32; 3],
}

impl Event {
    /// Schrijft het event op `b[..EVENT_LEN]`; te kort schrijft niets.
    pub fn encode(&self, b: &mut [u8]) -> Result {
        let n = b.len();
        let b = b.get_mut(..EVENT_LEN).ok_or(Error::Short {
            len: n,
            need: EVENT_LEN,
        })?;
        b.fill(0);
        b[0] = self.kind as u8;
        b[1] = u8::from(self.key);
        b[2] = self.pixel;
        b[4..6].copy_from_slice(&self.width.to_le_bytes());
        b[6..8].copy_from_slice(&self.height.to_le_bytes());
        b[8..16].copy_from_slice(&self.off.to_le_bytes());
        b[16..24].copy_from_slice(&self.size.to_le_bytes());
        b[24..32].copy_from_slice(&self.bytes.to_le_bytes());
        b[32..40].copy_from_slice(&self.tag.to_le_bytes());
        for (i, s) in self.stride.iter().enumerate() {
            b[40 + 2 * i..42 + 2 * i].copy_from_slice(&s.to_le_bytes());
        }
        for (i, p) in self.plane.iter().enumerate() {
            b[48 + 4 * i..52 + 4 * i].copy_from_slice(&p.to_le_bytes());
        }
        Ok(())
    }

    /// Leest één event.
    pub fn decode(b: &[u8]) -> Result<Event> {
        short(b, EVENT_LEN)?;
        let mut e = Event {
            kind: Kind::from_raw(b[0]),
            key: b[1] != 0,
            pixel: b[2],
            width: le16(b, 4),
            height: le16(b, 6),
            off: super::le64(b, 8),
            size: super::le64(b, 16),
            bytes: super::le64(b, 24),
            tag: super::le64(b, 32),
            ..Event::default()
        };
        for i in 0..3 {
            e.stride[i] = le16(b, 40 + 2 * i);
            e.plane[i] = super::le32(b, 48 + 4 * i);
        }
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// De offsets van Go's `EncodeEvent`, byte voor byte.
    #[test]
    fn event_bytes_are_those_of_go() {
        let e = Event {
            kind: Kind::Produced,
            key: true,
            pixel: 4,
            width: 3840,
            height: 2160,
            off: 0x1000,
            size: 0x2000,
            bytes: 12,
            tag: 0xcafe,
            stride: [7680, 7680, 0],
            plane: [0, 0x7e_9000, 0],
        };
        let mut b = [0xffu8; EVENT_LEN];
        e.encode(&mut b).unwrap();
        assert_eq!(&b[..4], &[3, 1, 4, 0]);
        assert_eq!(&b[4..8], &[0x00, 0x0f, 0x70, 0x08]);
        assert_eq!(&b[32..40], &0xcafeu64.to_le_bytes());
        assert_eq!(&b[40..42], &7680u16.to_le_bytes());
        assert_eq!(&b[52..56], &0x7e_9000u32.to_le_bytes());
        assert_eq!(&b[60..64], &[0; 4]);
        assert_eq!(Event::decode(&b).unwrap(), e);
        assert!(Event::decode(&b[..63]).is_err());
        for k in [
            Kind::Format,
            Kind::Consumed,
            Kind::Produced,
            Kind::Done,
            Kind::Fault,
        ] {
            assert_eq!(Kind::from_raw(k as u8), k);
        }
        assert_eq!(Kind::from_raw(9), Kind::None, "een soort van later");
        assert!(e.encode(&mut [0u8; 10]).is_err());
    }

    #[test]
    fn args_roundtrip_and_refuse_short() {
        let o = OpenArgs {
            codec: 2,
            dir: 0,
            pixel: 4,
            width: 3840,
            height: 2160,
        };
        assert_eq!(OpenArgs::decode(&o.encode()).unwrap(), o);
        assert!(OpenArgs::decode(&[0; 7]).is_err());
        let f = FeedArgs {
            handle: 3,
            flags: 1,
            filled: 1 << 40,
            tag: 9,
        };
        assert_eq!(FeedArgs::decode(&f.encode()).unwrap(), f);
        assert!(FeedArgs::decode(&[0; 23]).is_err());
        let b = BufArgs { handle: 7 };
        assert_eq!(BufArgs::decode(&b.encode()).unwrap(), b);
        assert!(BufArgs::decode(&[0; 3]).is_err());
    }
}
