//! De payloads tussen de kern en Hop voor de store-ops van een app.
//!
//! Een app vraagt `OP_STORE_PULL/PUSH/LIST/DROP` ([`crate::hopabi`]); de
//! kern zet de call in een rij en Hop, die de sleutels en de TLS heeft,
//! haalt hem op met [`super::PrivOp::NextStore`]. Wat Hop dan krijgt is een
//! [`StoreTask`]: het ticket (de naam van de opdracht in de rij), het slot,
//! de jobnaam (de naamruimte: Hop bouwt er `apps/<cluster>/<job>/` mee), de
//! objectnaam binnen die map en het lokale pad. De kern heeft de naam en het
//! pad al getoetst (geen `..`, geen lege naam); de prefix bouwt Hop, want
//! alleen Hop kent de cluster.
//!
//! Alles little-endian, met vaste koppen: de kern leest dit op de rand van
//! zijn vertrouwensgrens, ook al is de afzender bevoegd.

use crate::{Error, Result};

/// Het langste pad of de langste objectnaam in een opdracht (de grens van
/// de padresolutie in de kern, `kern::rpc::MAX_PATH`).
pub const MAX_STORE_PATH: usize = 1024;
/// De langste jobnaam (de grens van het handoff-blob van de flip,
/// `kern::kernflip::MAX_FLIP_JOB`).
pub const MAX_STORE_JOB: usize = 256;
/// De grootste lijst namen in één antwoord aan de app: de historische
/// grens van Go (`hopabi.MaxChunk`, 8 KiB). Een lijst die niet past, is een
/// fout (een smallere prefix), nooit stil afgekapt.
pub const MAX_STORE_LIST: usize = 8 << 10;
/// De langste wachttijd van [`super::PrivOp::NextStore`] in ms: ruim onder
/// de call-timeout van een client (10 s), zodat een lange wacht nooit een
/// timeout wordt.
pub const MAX_WAIT_MS: u64 = 5_000;
/// De vlag in `off` van [`super::PrivOp::NextStore`]: kom ook terug zodra
/// er nieuwe regels op de console zijn (`NEXT_LOG`), van de kern of van een
/// app.
/// Het antwoord is dan `size` [`LOG_NEWS`] zonder data. Zo wacht Hop met
/// één verbinding, zijn store-taak, op allebei (H8), en pollt hij
/// `NEXT_LOG` niet per tik. Wie zo wacht, logt niet per wek: elke
/// consoleregel wekt hem.
pub const NEXT_STORE_LOGS: u64 = 1;
/// `size` van het antwoord op [`super::PrivOp::NextStore`] met
/// [`NEXT_STORE_LOGS`]: er zijn nieuwe logregels sinds het vorige
/// antwoord; haal ze met `NEXT_LOG` tot leeg. Een store-opdracht gaat voor.
pub const LOG_NEWS: u64 = 2;

/// De lengte van [`TaskHead`] op de draad.
pub const TASK_HEAD_LEN: usize = 24;
/// De lengte van [`DoneHead`] op de draad.
pub const DONE_HEAD_LEN: usize = 8;
/// De lengte van het leesargument van [`super::PrivOp::StoreRead`].
pub const READ_ARG_LEN: usize = 8;

/// De vaste kop van een [`StoreTask`], vooraan in `data` van het antwoord
/// op [`super::PrivOp::NextStore`]. Daarachter `job_len` bytes jobnaam,
/// `key_len` bytes objectnaam en `path_len` bytes lokaal pad.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct TaskHead {
    /// Het ticket: uniek per opdracht sinds de boot van de kern.
    pub ticket: u64,
    /// Het slot van de app.
    pub slot: u32,
    /// De op van de app (`OP_STORE_*`).
    pub op: u8,
    /// Gereserveerd, 0.
    pub reserved: u8,
    /// De lengte van de jobnaam.
    pub job_len: u16,
    /// De lengte van de objectnaam.
    pub key_len: u16,
    /// De lengte van het lokale pad.
    pub path_len: u16,
    /// Gereserveerd, 0.
    pub reserved2: u32,
}

macro_rules! field {
    ($t:ty, $f:ident, $off:expr) => {
        const _: () = assert!(core::mem::offset_of!($t, $f) == $off);
    };
}

const _: () = assert!(core::mem::size_of::<TaskHead>() == TASK_HEAD_LEN);
field!(TaskHead, ticket, 0);
field!(TaskHead, slot, 8);
field!(TaskHead, op, 12);
field!(TaskHead, reserved, 13);
field!(TaskHead, job_len, 14);
field!(TaskHead, key_len, 16);
field!(TaskHead, path_len, 18);
field!(TaskHead, reserved2, 20);

// Elke lengte past in zijn `u16`.
const _: () = assert!(MAX_STORE_PATH <= u16::MAX as usize);
const _: () = assert!(MAX_STORE_JOB <= MAX_STORE_PATH);

/// Eén opdracht zoals Hop hem krijgt, met de namen geleend.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct StoreTask<'a> {
    /// Het ticket.
    pub ticket: u64,
    /// Het slot van de app.
    pub slot: u32,
    /// De op van de app (`OP_STORE_PULL`, `_PUSH`, `_LIST`, `_DROP`).
    pub op: u8,
    /// De jobnaam van het slot: de naamruimte in de bucket.
    pub job: &'a [u8],
    /// De objectnaam binnen de eigen map, genormaliseerd tot `/a/b` (bij
    /// list de prefix; `/` is de hele map).
    pub key: &'a [u8],
    /// Het lokale pad van de app (pull en push), zoals de app het gaf; Hop
    /// geeft het letterlijk terug in STORE_READ en STORE_WRITE.
    pub path: &'a [u8],
}

/// Een little-endian `u16` op `b[i..]`, of 0.
fn le16(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map_or(0, u16::from_le_bytes)
}

/// Een little-endian `u32` op `b[i..]`, of 0.
fn le32(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map_or(0, u32::from_le_bytes)
}

/// Een little-endian `u64` op `b[i..]`, of 0.
fn le64(b: &[u8], i: usize) -> u64 {
    b.get(i..i + 8)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
        .map_or(0, u64::from_le_bytes)
}

impl<'a> StoreTask<'a> {
    /// De lengte op de draad.
    #[must_use]
    pub fn len(&self) -> usize {
        TASK_HEAD_LEN + self.job.len() + self.key.len() + self.path.len()
    }

    /// Altijd `false`: een opdracht heeft minstens zijn kop.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Schrijft de opdracht in `dst`; geeft de lengte. Een te lange naam of
    /// een te kort `dst` is een fout vóór er iets geschreven is.
    pub fn encode(&self, dst: &mut [u8]) -> Result<usize> {
        for (len, max) in [
            (self.job.len(), MAX_STORE_JOB),
            (self.key.len(), MAX_STORE_PATH),
            (self.path.len(), MAX_STORE_PATH),
        ] {
            if len > max {
                return Err(Error::PayloadTooLarge { len, max });
            }
        }
        let need = self.len();
        let len = dst.len();
        let out = dst.get_mut(..need).ok_or(Error::Short { len, need })?;
        let (head, rest) = out.split_at_mut(TASK_HEAD_LEN);
        head.fill(0);
        head[0..8].copy_from_slice(&self.ticket.to_le_bytes());
        head[8..12].copy_from_slice(&self.slot.to_le_bytes());
        head[12] = self.op;
        // Past: hierboven begrensd op MAX_STORE_* (assertie op u16).
        head[14..16].copy_from_slice(&(self.job.len() as u16).to_le_bytes());
        head[16..18].copy_from_slice(&(self.key.len() as u16).to_le_bytes());
        head[18..20].copy_from_slice(&(self.path.len() as u16).to_le_bytes());
        let (job, rest) = rest.split_at_mut(self.job.len());
        let (key, path) = rest.split_at_mut(self.key.len());
        job.copy_from_slice(self.job);
        key.copy_from_slice(self.key);
        path.copy_from_slice(self.path);
        Ok(need)
    }

    /// Leest een opdracht; de namen lenen uit `b`. De lengte moet precies
    /// kloppen.
    pub fn decode(b: &'a [u8]) -> Result<StoreTask<'a>> {
        if b.len() < TASK_HEAD_LEN {
            return Err(Error::Short {
                len: b.len(),
                need: TASK_HEAD_LEN,
            });
        }
        let (job_len, key_len, path_len) = (
            usize::from(le16(b, 14)),
            usize::from(le16(b, 16)),
            usize::from(le16(b, 18)),
        );
        let need = TASK_HEAD_LEN + job_len + key_len + path_len;
        if b.len() != need {
            return Err(Error::Short { len: b.len(), need });
        }
        let rest = &b[TASK_HEAD_LEN..];
        let (job, rest) = rest.split_at(job_len);
        let (key, path) = rest.split_at(key_len);
        Ok(StoreTask {
            ticket: le64(b, 0),
            slot: le32(b, 8),
            op: b[12],
            job,
            key,
            path,
        })
    }
}

/// De kop van `data` in [`super::PrivOp::StoreDone`]: de uitkomst van de
/// opdracht als call-status (`crate::hopabi::STATUS_*`). Daarachter de
/// namen (list, bij `STATUS_OK`) of de fouttekst.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct DoneHead {
    /// De status voor de app.
    pub status: u16,
    /// Gereserveerd, 0.
    pub reserved: [u8; 6],
}

const _: () = assert!(core::mem::size_of::<DoneHead>() == DONE_HEAD_LEN);
field!(DoneHead, status, 0);
field!(DoneHead, reserved, 2);

impl DoneHead {
    /// De bytes op de draad.
    #[must_use]
    pub fn encode(&self) -> [u8; DONE_HEAD_LEN] {
        let mut b = [0u8; DONE_HEAD_LEN];
        b[0..2].copy_from_slice(&self.status.to_le_bytes());
        b
    }

    /// Leest de kop en geeft de rest (namen of tekst) erbij.
    pub fn decode(b: &[u8]) -> Result<(DoneHead, &[u8])> {
        let (head, rest) = b.split_at_checked(DONE_HEAD_LEN).ok_or(Error::Short {
            len: b.len(),
            need: DONE_HEAD_LEN,
        })?;
        Ok((
            DoneHead {
                status: le16(head, 0),
                reserved: [0; 6],
            },
            rest,
        ))
    }
}

/// Het leesargument van [`super::PrivOp::StoreRead`]: de grootste lengte.
#[must_use]
pub fn read_len(max: u64) -> [u8; READ_ARG_LEN] {
    max.to_le_bytes()
}

/// Leest het leesargument terug.
pub fn decode_read_len(b: &[u8]) -> Result<u64> {
    if b.len() != READ_ARG_LEN {
        return Err(Error::Short {
            len: b.len(),
            need: READ_ARG_LEN,
        });
    }
    Ok(le64(b, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_bytes_are_fixed_and_roundtrip() {
        let t = StoreTask {
            ticket: 0x0102_0304_0506_0708,
            slot: 2,
            op: crate::hopabi::OP_STORE_PUSH,
            job: b"spike",
            key: b"/state.json",
            path: b"state.json",
        };
        let mut b = [0xAAu8; 64];
        let n = t.encode(&mut b).unwrap();
        assert_eq!(n, TASK_HEAD_LEN + 5 + 11 + 10);
        assert_eq!(&b[0..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&b[8..14], &[2, 0, 0, 0, 9, 0]);
        assert_eq!(&b[14..20], &[5, 0, 11, 0, 10, 0]);
        assert_eq!(&b[20..24], &[0; 4]);
        assert_eq!(&b[24..29], b"spike");
        assert_eq!(StoreTask::decode(&b[..n]).unwrap(), t);
        // Een byte te veel of te weinig is een fout, geen stille staart.
        assert!(StoreTask::decode(&b[..n - 1]).is_err());
        assert!(StoreTask::decode(&b[..n + 1]).is_err());
        assert!(t.encode(&mut [0u8; 30]).is_err());
        let long = [b'a'; MAX_STORE_JOB + 1];
        assert!(
            StoreTask { job: &long, ..t }
                .encode(&mut [0u8; 512])
                .is_err()
        );
    }

    #[test]
    fn done_head_and_read_len_roundtrip() {
        let h = DoneHead {
            status: crate::hopabi::STATUS_NO_ENT,
            reserved: [0; 6],
        };
        let mut b = std::vec::Vec::from(h.encode());
        b.extend_from_slice(b"no such object");
        let (back, rest) = DoneHead::decode(&b).unwrap();
        assert_eq!(back, h);
        assert_eq!(rest, b"no such object");
        assert!(DoneHead::decode(&b[..7]).is_err());
        assert_eq!(decode_read_len(&read_len(1 << 20)).unwrap(), 1 << 20);
        assert!(decode_read_len(&[0; 7]).is_err());
    }
}
