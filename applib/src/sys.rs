//! De system-API-client: calls van de app naar de kern over één blijvende
//! verbinding naar 10.100.0.1:10100 op het slot-LAN.
//!
//! De client praat over een [`Conn`]-trait; wie een verbinding levert,
//! levert hem via [`Dial`]. De echte levert [`crate::appnet`]: een
//! `TcpStream` over de eigen netstack ([`crate::appnet::Net::system_client`]).
//! De bytes zijn die van `systemapi` en `hopabi` in de Go-boom: een framekop
//! van 12 bytes (`"HOPS"`, versie, soort, lengte) met daarin een request of
//! response met een kop van 24 bytes.
//!
//! De regels van de Go-client gaan mee:
//!
//! - **Eén keer opnieuw als het transport wegviel** (reset, EOF, een write
//!   die niet wegkon). Dat gebeurt bij een kern-flip: de TCP-stack onder de
//!   app wordt vervangen en de eerste call erna krijgt een RST (gemeten
//!   06-09 op QEMU: "connection reset by peer", en de app viel om terwijl de
//!   kern onder hem netjes geland was). Herhalen is veilig: elke op is
//!   idempotent, en voor `remove` betekent "bestaat niet" bij de tweede
//!   poging precies wat de aanroeper wilde.
//! - **Een timeout wordt nooit herhaald**: die call kan aan de overkant nog
//!   lopen.
//! - **Geen allocatie**: een read landt rechtstreeks in de buffer van de
//!   aanroeper (kop eerst, dan de data). In Go hield 2 MiB afval per call de
//!   GC aan het werk, en op één core kreeg de RX-pomp dan zijn beurt niet
//!   (gemeten 04-09).
//!
//! Dit module bezit de verbinding van de client; hij gaat met de client mee.

use crate::contract::{
    HOPABI_HDR_LEN, MAX_IO_CHUNK, MAX_PAYLOAD, OP_LIST, OP_READ, OP_READ_MANY, OP_REMOVE, OP_STAT,
    OP_SYNC, OP_TRUNCATE, OP_WRITE, STATUS_NOENT, STATUS_OK, SYS_HEADER_LEN, SYS_PORT,
};
use abi::hopabi::many;
use abi::systemapi::{Kind, decode_header, encode_header};
use core::fmt;
use core::future::Future;
use core::time::Duration;
use sync::{Either, select};

/// Het adres van de kern op het slot-LAN.
pub const ADDRESS: ([u8; 4], u16) = (abi::layout::HOST_IP4.to_be_bytes(), SYS_PORT);

/// De bulkgrens per call.
pub const MAX_CHUNK: usize = MAX_IO_CHUNK;

/// De timeout van een gewone call.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Zoveel lezingen hoogstens in één bundel ([`Client::read_many`]); de
/// NVMe-kern neemt er twee bundels van tegelijk.
pub const MAX_READS: usize = many::MAX_OPS;

/// Zoveel bytes hoogstens samen in één bundel; een grote lees is één
/// [`Client::read_into`].
pub const MAX_READ_BYTES: usize = many::MAX_BYTES;

/// De timeout van de store-ops: een pull of push duurt zo lang als het
/// object groot is (de kern streamt hem van of naar de bucket).
pub const STORE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Een transportfout.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConnError {
    /// De verbinding is gereset.
    Reset,
    /// De overkant sloot (EOF), of een write nam niets aan.
    Closed,
    /// Verbinden lukte niet.
    Refused,
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Reset => "connection reset",
            Self::Closed => "connection closed",
            Self::Refused => "connection refused",
        })
    }
}

/// Een bytestroom naar de kern: de verbinding zoals de netstack hem levert.
pub trait Conn {
    /// Leest hooguit `buf.len()` bytes; 0 is EOF.
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, ConnError>>;
    /// Schrijft hooguit `buf.len()` bytes.
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = Result<usize, ConnError>>;
}

/// Wie een nieuwe verbinding naar [`ADDRESS`] opent.
pub trait Dial {
    /// Het soort verbinding.
    type Conn: Conn;
    /// Opent een verbinding.
    fn dial(&mut self) -> impl Future<Output = Result<Self::Conn, ConnError>>;
}

/// De timer van de client (de executor van de app, of een nep in tests).
///
/// Alleen de slaap, en daarom niet [`sync::Timer`] van de kern en de
/// drivers: de client heeft geen klok nodig, en Hop implementeert deze
/// trait zelf (`hopos-runner::fake::NeverTimer`), dus een `now` erbij
/// breekt de bouw van Hop tegen deze boom.
pub trait Timer {
    /// Slaapt `d`.
    fn sleep(&self, d: Duration) -> impl Future<Output = ()>;
}

/// Elke [`sync::Timer`] (de executor van de app, [`crate::appnet::ExecTimer`])
/// is een timer van de client.
impl<T: sync::Timer> Timer for T {
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        sync::Timer::sleep(self, d)
    }
}

/// Een foutmelding van de kern, afgekapt op een vaste maat.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Msg {
    buf: [u8; 96],
    len: usize,
}

impl Msg {
    const fn empty() -> Self {
        Self {
            buf: [0; 96],
            len: 0,
        }
    }

    /// De tekst (of wat er als UTF-8 van te maken was).
    #[must_use]
    pub fn as_str(&self) -> &str {
        let b = self.buf.get(..self.len).unwrap_or_default();
        match core::str::from_utf8(b) {
            Ok(s) => s,
            Err(e) => core::str::from_utf8(b.get(..e.valid_up_to()).unwrap_or_default())
                .unwrap_or_default(),
        }
    }
}

impl fmt::Debug for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Waarom een call niet lukte.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Het transport viel weg, ook na één herhaling.
    Transport(ConnError),
    /// De deadline verstreek; de call kan aan de overkant nog lopen.
    Timeout,
    /// De kern antwoordde iets dat niet klopt (magie, versie, soort,
    /// volgnummer, lengte). De verbinding is dicht.
    Protocol(&'static str),
    /// Het pad bestaat niet.
    NotFound {
        /// De op.
        op: u8,
    },
    /// De kern deed de call en gaf een fout terug.
    Call {
        /// De op.
        op: u8,
        /// De status.
        status: u16,
        /// De tekst van de kern.
        msg: Msg,
    },
    /// De call is groter dan het contract toelaat; niets verstuurd.
    TooLarge {
        /// De lengte.
        len: usize,
        /// Het maximum.
        max: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "system call transport: {e}"),
            Self::Timeout => f.write_str("system call timed out"),
            Self::Protocol(why) => write!(f, "system call protocol: {why}"),
            Self::NotFound { op } => write!(f, "system call op {op}: not found"),
            Self::Call { op, status, msg } => {
                write!(f, "system call op {op}: status {status}: {}", msg.as_str())
            }
            Self::TooLarge { len, max } => write!(f, "system call payload {len} exceeds {max}"),
        }
    }
}

/// Het resultaat van een call.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// De kop van een response.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Resp {
    /// De op.
    pub op: u8,
    /// De status.
    pub status: u16,
    /// Het volgnummer.
    pub seq: u32,
    /// De grootte (stat, pull, push).
    pub size: u64,
}

/// Een request, zonder kopie: pad en data blijven van de aanroeper.
#[derive(Copy, Clone, Debug)]
pub struct Req<'a> {
    /// De op.
    pub op: u8,
    /// Het volgnummer (de client zet het).
    pub seq: u32,
    /// De offset.
    pub off: u64,
    /// De lengte of maat.
    pub n: u64,
    /// Het pad.
    pub path: &'a str,
    /// De data.
    pub data: &'a [u8],
}

impl<'a> Req<'a> {
    /// Een request zonder offset, lengte en data.
    #[must_use]
    pub const fn path(op: u8, path: &'a str) -> Self {
        Self {
            op,
            seq: 0,
            off: 0,
            n: 0,
            path,
            data: &[],
        }
    }

    /// Dezelfde request in de vorm van het contract, voor de encoder van
    /// `abi` (de kop van 24 bytes).
    fn wire(&self) -> abi::hopabi::Req<'a> {
        abi::hopabi::Req {
            op: self.op,
            seq: self.seq,
            off: self.off,
            n: self.n,
            path: self.path.as_bytes(),
            data: self.data,
        }
    }

    /// De payloadlengte, of `TooLarge` als hij niet op de draad mag.
    pub fn payload_len(&self) -> Result<usize> {
        let len = HOPABI_HDR_LEN + self.path.len() + self.data.len();
        if self.path.len() > usize::from(u16::MAX) || len > MAX_PAYLOAD {
            return Err(Error::TooLarge {
                len,
                max: MAX_PAYLOAD,
            });
        }
        Ok(len)
    }
}

/// Eén lees van een bundel ([`Client::read_many`]): vanaf `off` hooguit
/// `dst.len()` bytes rechtstreeks in `dst`.
#[derive(Debug)]
pub struct ReadOp<'a> {
    /// De offset in het bestand.
    pub off: u64,
    /// De bestemming.
    pub dst: &'a mut [u8],
    /// De uitkomst van deze ene lees: zoveel bytes vooraan in `dst` (0 is
    /// het einde van het bestand), of de status van de kern (een blokfout).
    pub got: core::result::Result<usize, u16>,
}

impl<'a> ReadOp<'a> {
    /// Een lees van hooguit `dst.len()` bytes vanaf `off`.
    pub fn new(off: u64, dst: &'a mut [u8]) -> Self {
        Self {
            off,
            dst,
            got: Ok(0),
        }
    }
}

/// Waar de data van een antwoord landt.
enum Sink<'d, 'o> {
    /// Aaneen in één buffer.
    Buf(&'d mut [u8]),
    /// De tabel van een bundel, dan elke lees in haar eigen buffer.
    Many(&'d mut [ReadOp<'o>]),
}

/// Leest het antwoord van een bundel (`data` bytes): de tabel met de
/// uitkomsten, dan de bytes van elke lees rechtstreeks in haar `dst`.
async fn land_many<C: Conn>(
    c: &mut C,
    ops: &mut [ReadOp<'_>],
    data: usize,
) -> core::result::Result<(), Attempt> {
    let fatal = |why| Attempt::fatal(Error::Protocol(why));
    let mut table = [0u8; many::MAX_OPS * many::RESULT_LEN];
    let t = table
        .get_mut(..ops.len() * many::RESULT_LEN)
        .ok_or_else(|| fatal("bundle table"))?;
    if t.len() > data {
        return Err(fatal("bundle shorter than its table"));
    }
    read_exact(c, t)
        .await
        .map_err(|e| Attempt::fatal(Error::Transport(e)))?;
    let mut left = data - t.len();
    for (i, op) in ops.iter_mut().enumerate() {
        let (got, status) = many::result(t, i).ok_or_else(|| fatal("bundle table"))?;
        let got = got as usize;
        if status != STATUS_OK {
            op.got = Err(status);
            if got != 0 {
                return Err(fatal("bytes for a failed read"));
            }
            continue;
        }
        if got > left {
            return Err(fatal("bundle shorter than its reads"));
        }
        let dst = op
            .dst
            .get_mut(..got)
            .ok_or_else(|| fatal("read longer than its buffer"))?;
        read_exact(c, dst)
            .await
            .map_err(|e| Attempt::fatal(Error::Transport(e)))?;
        (op.got, left) = (Ok(got), left - got);
    }
    if left != 0 {
        return Err(fatal("bundle longer than its reads"));
    }
    Ok(())
}

/// Een draadfout van de kern (de framekop of de responskop, door `abi`
/// gelezen) als protocolfout van de client.
fn protocol(e: abi::Error) -> Error {
    Error::Protocol(match e {
        abi::Error::BadMagic(_) => "bad magic",
        abi::Error::BadVersion { .. } => "version",
        abi::Error::PayloadTooLarge { .. } => "frame too large",
        abi::Error::BadKind(_) => "unexpected frame",
        _ => "header",
    })
}

async fn write_all<C: Conn>(c: &mut C, mut p: &[u8]) -> core::result::Result<(), ConnError> {
    while !p.is_empty() {
        let n = c.write(p).await?;
        if n == 0 {
            return Err(ConnError::Closed);
        }
        p = p.get(n..).unwrap_or_default();
    }
    Ok(())
}

async fn read_exact<C: Conn>(c: &mut C, mut buf: &mut [u8]) -> core::result::Result<(), ConnError> {
    while !buf.is_empty() {
        let n = c.read(buf).await?;
        if n == 0 {
            return Err(ConnError::Closed);
        }
        buf = buf.get_mut(n..).unwrap_or_default();
    }
    Ok(())
}

/// Leest `n` bytes en gooit ze weg, zodat de verbinding schoon blijft.
async fn drain<C: Conn>(c: &mut C, mut n: usize) -> core::result::Result<(), ConnError> {
    let mut scratch = [0u8; 64];
    while n > 0 {
        let k = n.min(scratch.len());
        read_exact(c, scratch.get_mut(..k).unwrap_or_default()).await?;
        n -= k;
    }
    Ok(())
}

/// De uitkomst van één poging: een fout met de vlag "mag herhaald".
struct Attempt {
    err: Error,
    retry: bool,
}

impl From<ConnError> for Attempt {
    fn from(e: ConnError) -> Self {
        Self {
            err: Error::Transport(e),
            retry: true,
        }
    }
}

impl Attempt {
    const fn fatal(err: Error) -> Self {
        Self { err, retry: false }
    }
}

/// Eén poging op een open verbinding: request schrijven, response lezen
/// met de data in `sink` (die er niet in past: protocolfout).
async fn exchange<C: Conn>(
    c: &mut C,
    req: &Req<'_>,
    sink: &mut Sink<'_, '_>,
) -> core::result::Result<(Resp, usize), Attempt> {
    let len = req.payload_len().map_err(Attempt::fatal)?;
    let fatal = |e| Attempt::fatal(protocol(e));
    let fh = encode_header(Kind::Call, len).map_err(fatal)?;
    let mut rh = [0u8; HOPABI_HDR_LEN];
    abi::hopabi::encode_req_head(&mut rh, &req.wire()).map_err(fatal)?;
    write_all(c, &fh).await?;
    write_all(c, &rh).await?;
    write_all(c, req.path.as_bytes()).await?;
    write_all(c, req.data).await?;

    let mut fh = [0u8; SYS_HEADER_LEN];
    read_exact(c, &mut fh).await?;
    let h = decode_header(&fh).map_err(fatal)?;
    let n = h.len;
    if h.kind != Kind::Result || n < HOPABI_HDR_LEN {
        return Err(Attempt::fatal(Error::Protocol("unexpected frame")));
    }
    read_exact(c, &mut rh)
        .await
        .map_err(|e| Attempt::fatal(Error::Transport(e)))?;
    let r = abi::hopabi::decode_resp(&rh).map_err(fatal)?;
    let resp = Resp {
        op: r.op,
        status: r.status,
        seq: r.seq,
        size: r.size,
    };
    let data = n - HOPABI_HDR_LEN;
    if resp.seq != req.seq {
        return Err(Attempt::fatal(Error::Protocol("response seq")));
    }
    if resp.status != STATUS_OK {
        // De fouttekst is klein: in een vast bufje, de rest weggooien, en
        // de verbinding blijft bruikbaar.
        let mut msg = Msg::empty();
        let keep = data.min(msg.buf.len());
        let slot = msg.buf.get_mut(..keep).unwrap_or_default();
        read_exact(c, slot)
            .await
            .map_err(|e| Attempt::fatal(Error::Transport(e)))?;
        drain(c, data - keep)
            .await
            .map_err(|e| Attempt::fatal(Error::Transport(e)))?;
        msg.len = keep;
        let err = if resp.status == STATUS_NOENT {
            Error::NotFound { op: req.op }
        } else {
            Error::Call {
                op: req.op,
                status: resp.status,
                msg,
            }
        };
        return Err(Attempt::fatal(err));
    }
    match sink {
        Sink::Buf(dst) => {
            let Some(out) = dst.get_mut(..data) else {
                return Err(Attempt::fatal(Error::Protocol(
                    "response longer than the buffer",
                )));
            };
            read_exact(c, out)
                .await
                .map_err(|e| Attempt::fatal(Error::Transport(e)))?;
        }
        Sink::Many(ops) => land_many(c, ops, data).await?,
    }
    Ok((resp, data))
}

/// De client: één verbinding, één call tegelijk (hij is van één taak).
pub struct Client<D: Dial, T: Timer> {
    dial: D,
    timer: T,
    conn: Option<D::Conn>,
    seq: u32,
}

impl<D: Dial, T: Timer> Client<D, T> {
    /// Een client die verbindt met `dial` en zijn deadlines op `timer` zet.
    /// Er wordt pas verbonden bij de eerste call.
    pub fn new(dial: D, timer: T) -> Self {
        Self {
            dial,
            timer,
            conn: None,
            seq: 0,
        }
    }

    /// Is er een open verbinding?
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// De open verbinding, als die er is: voor wie hem wil flushen na een
    /// call zonder antwoord ([`Client::log`]).
    pub fn conn_mut(&mut self) -> Option<&mut D::Conn> {
        self.conn.as_mut()
    }

    /// Eén poging, met verbinden en deadline. Een fout sluit de verbinding,
    /// behalve een nette foutstatus van de kern.
    async fn once(
        &mut self,
        req: &Req<'_>,
        sink: &mut Sink<'_, '_>,
        timeout: Duration,
    ) -> core::result::Result<(Resp, usize), Attempt> {
        let Self {
            dial, timer, conn, ..
        } = self;
        if conn.is_none() {
            *conn = Some(dial.dial().await?);
        }
        let Some(c) = conn.as_mut() else {
            return Err(Attempt::from(ConnError::Refused));
        };
        let r = match select(timer.sleep(timeout), exchange(c, req, sink)).await {
            // Een stil dode peer (een kern-flip onder een lange wacht) ziet
            // TCP alleen zo: één nieuwe poging op een verse verbinding.
            Either::Left(()) => Err(Attempt {
                err: Error::Timeout,
                retry: true,
            }),
            Either::Right(r) => r,
        };
        if let Err(a) = &r {
            let clean = matches!(
                a.err,
                Error::NotFound { .. } | Error::Call { .. } | Error::TooLarge { .. }
            );
            if !clean {
                *conn = None;
            }
        }
        r
    }

    /// Doet één call en herhaalt hem precies één keer als het transport
    /// wegviel of zweeg tot de termijn. `dst` krijgt de data van de response.
    pub async fn call(
        &mut self,
        req: Req<'_>,
        dst: &mut [u8],
        timeout: Duration,
    ) -> Result<(Resp, usize)> {
        self.call_into(req, &mut Sink::Buf(dst), timeout).await
    }

    async fn call_into(
        &mut self,
        mut req: Req<'_>,
        sink: &mut Sink<'_, '_>,
        timeout: Duration,
    ) -> Result<(Resp, usize)> {
        self.seq = self.seq.wrapping_add(1);
        req.seq = self.seq;
        match self.once(&req, sink, timeout).await {
            Ok(r) => Ok(r),
            Err(a) if !a.retry => Err(a.err),
            Err(_) => {
                self.seq = self.seq.wrapping_add(1);
                req.seq = self.seq;
                match self.once(&req, sink, timeout).await {
                    Ok(r) => Ok(r),
                    Err(a) => Err(a.err),
                }
            }
        }
    }

    /// Doet één call ZONDER herhaling: voor ops die niet idempotent zijn.
    /// Twee keer dezelfde codec-feed is twee happen bitstream; valt het
    /// transport weg, dan beslist de aanroeper (na een kern-flip bestaat de
    /// sessie toch niet meer).
    pub async fn call_once(
        &mut self,
        mut req: Req<'_>,
        dst: &mut [u8],
        timeout: Duration,
    ) -> Result<(Resp, usize)> {
        self.seq = self.seq.wrapping_add(1);
        req.seq = self.seq;
        self.once(&req, &mut Sink::Buf(dst), timeout)
            .await
            .map_err(|a| a.err)
    }

    /// Bevestigt data, namen en groottes op duurzame opslag; retourneert de
    /// HopFS-generatie. Ook een directory mag (na journalverwijdering).
    /// Geen herhaling na een verloren bevestiging: de aanroeper moet een
    /// onzekere commit afhandelen. Een oude kern of vluchtige FS weigert.
    pub async fn sync(&mut self, path: &str) -> Result<u64> {
        let (r, n) = self
            .call_once(Req::path(OP_SYNC, path), &mut [], RPC_TIMEOUT)
            .await?;
        if n != 0 {
            return Err(Error::Protocol("sync response data"));
        }
        Ok(r.size)
    }

    /// De grootte van een bestand (0 voor een map).
    pub async fn stat(&mut self, path: &str) -> Result<u64> {
        let (r, _) = self
            .call(Req::path(OP_STAT, path), &mut [], RPC_TIMEOUT)
            .await?;
        Ok(r.size)
    }

    /// Leest hooguit `dst.len()` bytes (≤ [`MAX_CHUNK`]) vanaf `off`
    /// rechtstreeks in `dst`; 0 is het einde van het bestand.
    pub async fn read_into(&mut self, path: &str, off: u64, dst: &mut [u8]) -> Result<usize> {
        if dst.len() > MAX_CHUNK {
            return Err(Error::TooLarge {
                len: dst.len(),
                max: MAX_CHUNK,
            });
        }
        let req = Req {
            off,
            n: dst.len() as u64,
            ..Req::path(OP_READ, path)
        };
        let (_, n) = self.call(req, dst, RPC_TIMEOUT).await?;
        Ok(n)
    }

    /// Leest een bundel: tot [`MAX_READS`] lezingen uit één bestand in één
    /// call (`OP_READ_MANY`), samen hooguit [`MAX_READ_BYTES`]. De kern zet
    /// ze in één keer op het device en antwoordt één keer; elke lees landt
    /// rechtstreeks in haar eigen `dst` en krijgt haar eigen uitkomst in
    /// `got` (een blokfout in de ene laat de andere staan). Geeft de som van
    /// de gelezen bytes. Een lege lijst is nul zonder call.
    ///
    /// Meer bundels tegelijk in de lucht: een tweede [`Client`] (een eigen
    /// verbinding) en beide futures samen afwachten (`sync::join`); de kern
    /// laat lezingen van één app naast elkaar lopen. Een app heeft er twee
    /// (`MAX_SYSTEM_CONNS` in de kern), en de node dertien voor iedereen.
    /// Een kern zonder de op antwoordt met een nette fout.
    pub async fn read_many(&mut self, path: &str, ops: &mut [ReadOp<'_>]) -> Result<usize> {
        if ops.is_empty() {
            return Ok(0);
        }
        if ops.len() > MAX_READS {
            return Err(Error::TooLarge {
                len: ops.len(),
                max: MAX_READS,
            });
        }
        let mut list = [0u8; MAX_READS * many::OP_LEN];
        let mut sum = 0usize;
        for (i, op) in ops.iter().enumerate() {
            sum = sum.saturating_add(op.dst.len());
            let len = u32::try_from(op.dst.len()).unwrap_or(u32::MAX);
            many::put_op(&mut list, i, op.off, len);
        }
        if sum > MAX_READ_BYTES {
            return Err(Error::TooLarge {
                len: sum,
                max: MAX_READ_BYTES,
            });
        }
        let req = Req {
            n: ops.len() as u64,
            data: list.get(..ops.len() * many::OP_LEN).unwrap_or_default(),
            ..Req::path(OP_READ_MANY, path)
        };
        let (r, _) = self
            .call_into(req, &mut Sink::Many(ops), RPC_TIMEOUT)
            .await?;
        Ok(usize::try_from(r.size).unwrap_or(usize::MAX))
    }

    /// Schrijft één chunk (≤ [`MAX_CHUNK`]) op `off`, zonder te truncaten.
    pub async fn write_at(&mut self, path: &str, off: u64, data: &[u8]) -> Result<usize> {
        if data.len() > MAX_CHUNK {
            return Err(Error::TooLarge {
                len: data.len(),
                max: MAX_CHUNK,
            });
        }
        let req = Req {
            off,
            data,
            ..Req::path(OP_WRITE, path)
        };
        self.call(req, &mut [], RPC_TIMEOUT).await?;
        Ok(data.len())
    }

    /// Zet de logische grootte van een bestand.
    pub async fn truncate(&mut self, path: &str, size: u64) -> Result {
        let req = Req {
            n: size,
            ..Req::path(OP_TRUNCATE, path)
        };
        self.call(req, &mut [], RPC_TIMEOUT).await.map(|_| ())
    }

    /// Vervangt de inhoud van `path` door `data`, in chunks. Eerst op nul:
    /// de schrijf-op alleen kan een bestand niet korter maken, en zonder de
    /// truncate bleef de oude staart staan. Een halve schrijf is zo een kort
    /// bestand, geen gemengd bestand, en dat is de fout die je ziet.
    pub async fn write_file(&mut self, path: &str, data: &[u8]) -> Result {
        self.truncate(path, 0).await?;
        let mut off = 0u64;
        for chunk in data.chunks(MAX_CHUNK) {
            self.write_at(path, off, chunk).await?;
            off += chunk.len() as u64;
        }
        Ok(())
    }

    /// De namen in een map, `\n`-gescheiden in `dst` ("naam/" is een map).
    /// Geeft het aantal bytes; [`names`] splitst ze.
    pub async fn list(&mut self, path: &str, dst: &mut [u8]) -> Result<usize> {
        let (_, n) = self
            .call(Req::path(OP_LIST, path), dst, RPC_TIMEOUT)
            .await?;
        Ok(n)
    }

    /// Verwijdert een bestand of lege map. "Bestaat niet" na een herhaling
    /// is geen fout: dan was de eerste poging wel aangekomen.
    pub async fn remove(&mut self, path: &str) -> Result {
        self.seq = self.seq.wrapping_add(1);
        let mut req = Req::path(OP_REMOVE, path);
        req.seq = self.seq;
        match self.once(&req, &mut Sink::Buf(&mut []), RPC_TIMEOUT).await {
            Ok(_) => Ok(()),
            Err(a) if !a.retry => Err(a.err),
            Err(_) => {
                self.seq = self.seq.wrapping_add(1);
                req.seq = self.seq;
                match self.once(&req, &mut Sink::Buf(&mut []), RPC_TIMEOUT).await {
                    Ok(_)
                    | Err(Attempt {
                        err: Error::NotFound { .. },
                        ..
                    }) => Ok(()),
                    Err(a) => Err(a.err),
                }
            }
        }
    }

    /// Eén logregel over de verbinding (`KindLog`), zonder antwoord. Een
    /// fout sluit de verbinding. `log!` gaat via de outbox; dit is de weg
    /// van de Go-apps (`Logf` in de Go-SDK), en appspike toetst er de kern
    /// mee.
    pub async fn log(&mut self, line: &[u8]) -> Result {
        let fh = encode_header(Kind::Log, line.len()).map_err(|_| Error::TooLarge {
            len: line.len(),
            max: MAX_PAYLOAD,
        })?;
        let Self {
            dial, timer, conn, ..
        } = self;
        if conn.is_none() {
            *conn = Some(dial.dial().await.map_err(Error::Transport)?);
        }
        let Some(c) = conn.as_mut() else {
            return Err(Error::Transport(ConnError::Refused));
        };
        let send = async {
            write_all(c, &fh).await?;
            write_all(c, line).await
        };
        let r = match select(timer.sleep(Duration::from_millis(100)), send).await {
            Either::Left(()) => Err(Error::Timeout),
            Either::Right(r) => r.map_err(Error::Transport),
        };
        if r.is_err() {
            *conn = None;
        }
        r
    }
}

/// Splitst de `\n`-gescheiden namen van [`Client::list`]; de kern zet geen
/// regeleinde achteraan, en een lege lijst is geen naam.
pub fn names(b: &[u8]) -> impl Iterator<Item = &str> {
    b.split(|&c| c == b'\n')
        .filter(|n| !n.is_empty())
        .filter_map(|n| core::str::from_utf8(n).ok())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// Pollt tot klaar; de nep-verbindingen zijn altijd meteen klaar.
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..1000 {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
        panic!("future bleef hangen");
    }

    /// Een script: per verbinding de bytes die de kern terugstuurt, of een
    /// reset bij de eerste write.
    #[derive(Default)]
    struct Script {
        conns: VecDeque<Option<Vec<u8>>>,
        sent: Vec<Vec<u8>>,
        dials: u32,
    }

    struct Mock {
        rx: VecDeque<u8>,
        reset: bool,
        script: Rc<RefCell<Script>>,
        idx: usize,
    }

    impl Conn for Mock {
        async fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, ConnError> {
            let n = buf.len().min(self.rx.len());
            for b in buf.iter_mut().take(n) {
                *b = self.rx.pop_front().unwrap();
            }
            Ok(n)
        }
        async fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, ConnError> {
            if self.reset {
                return Err(ConnError::Reset);
            }
            self.script.borrow_mut().sent[self.idx].extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    struct MockDial(Rc<RefCell<Script>>);

    impl Dial for MockDial {
        type Conn = Mock;
        async fn dial(&mut self) -> core::result::Result<Mock, ConnError> {
            let mut s = self.0.borrow_mut();
            s.dials += 1;
            let next = s.conns.pop_front().ok_or(ConnError::Refused)?;
            s.sent.push(Vec::new());
            let idx = s.sent.len() - 1;
            Ok(Mock {
                reset: next.is_none(),
                rx: next.unwrap_or_default().into(),
                script: self.0.clone(),
                idx,
            })
        }
    }

    struct Never;
    impl Timer for Never {
        fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
            core::future::pending()
        }
    }

    struct Now;
    impl Timer for Now {
        async fn sleep(&self, _: Duration) {}
    }

    /// Een antwoord-frame zoals de kern het schrijft: de encoders van `abi`.
    pub(crate) fn resp(seq: u32, status: u16, size: u64, data: &[u8]) -> Vec<u8> {
        let r = abi::hopabi::Resp {
            op: 0,
            status,
            seq,
            size,
            data,
        };
        let mut p = vec![0u8; HOPABI_HDR_LEN + data.len()];
        let n = abi::hopabi::encode_resp(&mut p, &r).unwrap();
        let mut f = encode_header(Kind::Result, n).unwrap().to_vec();
        f.extend_from_slice(&p);
        f
    }

    fn client(conns: Vec<Option<Vec<u8>>>) -> (Client<MockDial, Never>, Rc<RefCell<Script>>) {
        let s = Rc::new(RefCell::new(Script {
            conns: conns.into(),
            ..Script::default()
        }));
        (Client::new(MockDial(s.clone()), Never), s)
    }

    #[test]
    fn sync_uses_the_new_opcode_and_never_repeats_an_uncertain_barrier() {
        let (mut c, script) = client(vec![Some(resp(1, STATUS_OK, 7, b""))]);
        assert_eq!(block_on(c.sync("/db")), Ok(7));
        let sent = &script.borrow().sent[0];
        assert_eq!(sent[13], OP_SYNC);
        assert_eq!(&sent[20..36], &[0; 16]);
        assert_eq!(&sent[36..], b"/db");
        let (mut c, script) = client(vec![None, Some(resp(2, STATUS_OK, 8, b""))]);
        assert_eq!(
            block_on(c.sync("/db")),
            Err(Error::Transport(ConnError::Reset))
        );
        assert_eq!(script.borrow().dials, 1);
        let (mut c, _) = client(vec![Some(resp(1, 1, 0, b"flush failed"))]);
        assert!(block_on(c.sync("/db")).is_err());
    }

    #[test]
    fn stat_speaks_the_go_wire_format() {
        let (mut c, s) = client(vec![Some(resp(1, STATUS_OK, 4096, b""))]);
        assert_eq!(block_on(c.stat("/data/db.bin")), Ok(4096));
        let sent = &s.borrow().sent[0];
        // Framekop: "HOPS", versie 1, soort call, lengte 24 + pad.
        assert_eq!(&sent[0..4], b"HOPS");
        assert_eq!(sent[4], 1);
        assert_eq!(sent[5], Kind::Call as u8);
        assert_eq!(u32::from_le_bytes(sent[8..12].try_into().unwrap()), 24 + 12);
        // Requestkop: versie, op, padlengte, seq.
        assert_eq!(sent[12], abi::hopabi::VERSION);
        assert_eq!(sent[13], OP_STAT);
        assert_eq!(u16::from_le_bytes([sent[14], sent[15]]), 12);
        assert_eq!(u32::from_le_bytes(sent[16..20].try_into().unwrap()), 1);
        assert_eq!(&sent[36..], b"/data/db.bin");
    }

    #[test]
    fn read_lands_in_the_callers_buffer() {
        let (mut c, _) = client(vec![Some(resp(1, STATUS_OK, 0, b"hello"))]);
        let mut buf = [0u8; 16];
        assert_eq!(block_on(c.read_into("/f", 0, &mut buf)), Ok(5));
        assert_eq!(&buf[..5], b"hello");
        assert!(c.is_connected());
    }

    #[test]
    fn a_reset_is_retried_once_on_a_fresh_connection() {
        let (mut c, s) = client(vec![None, Some(resp(2, STATUS_OK, 7, b""))]);
        assert_eq!(block_on(c.stat("/x")), Ok(7));
        assert_eq!(s.borrow().dials, 2);
    }

    #[test]
    fn a_second_reset_is_the_error() {
        let (mut c, _) = client(vec![None, None]);
        assert_eq!(
            block_on(c.stat("/x")),
            Err(Error::Transport(ConnError::Reset))
        );
        assert!(!c.is_connected());
    }

    #[test]
    fn remove_not_found_after_a_retry_is_success() {
        let (mut c, _) = client(vec![None, Some(resp(2, STATUS_NOENT, 0, b"gone"))]);
        assert_eq!(block_on(c.remove("/x")), Ok(()));
        // Zonder herhaling is "bestaat niet" gewoon de fout.
        let (mut c, _) = client(vec![Some(resp(1, STATUS_NOENT, 0, b"gone"))]);
        assert_eq!(
            block_on(c.remove("/x")),
            Err(Error::NotFound { op: OP_REMOVE })
        );
    }

    #[test]
    fn wrong_seq_is_a_protocol_error_and_not_retried() {
        let (mut c, s) = client(vec![
            Some(resp(9, STATUS_OK, 0, b"")),
            Some(resp(2, STATUS_OK, 0, b"")),
        ]);
        assert_eq!(block_on(c.stat("/x")), Err(Error::Protocol("response seq")));
        assert_eq!(s.borrow().dials, 1);
        assert!(!c.is_connected());
    }

    #[test]
    fn error_status_carries_the_message_and_keeps_the_connection() {
        let long = [b'x'; 200];
        let mut script = resp(1, 1, 0, &long);
        script.extend(resp(2, STATUS_OK, 3, b""));
        let (mut c, _) = client(vec![Some(script)]);
        match block_on(c.stat("/x")) {
            Err(Error::Call { op, status, msg }) => {
                assert_eq!((op, status), (OP_STAT, 1));
                assert_eq!(msg.as_str().len(), 96);
            }
            other => panic!("verwacht een call-fout, kreeg {other:?}"),
        }
        assert!(c.is_connected());
        assert_eq!(block_on(c.stat("/y")), Ok(3)); // de rest is weggelezen
    }

    #[test]
    fn a_response_longer_than_the_buffer_is_refused() {
        let (mut c, _) = client(vec![Some(resp(1, STATUS_OK, 0, &[1; 32]))]);
        let mut buf = [0u8; 8];
        assert!(matches!(
            block_on(c.read_into("/f", 0, &mut buf)),
            Err(Error::Protocol(_))
        ));
        assert!(!c.is_connected());
    }

    #[test]
    fn oversize_calls_never_reach_the_wire() {
        let (mut c, s) = client(vec![]);
        let big = vec![0u8; MAX_CHUNK + 1];
        assert!(matches!(
            block_on(c.write_at("/f", 0, &big)),
            Err(Error::TooLarge { .. })
        ));
        let mut dst = vec![0u8; MAX_CHUNK + 1];
        assert!(matches!(
            block_on(c.read_into("/f", 0, &mut dst)),
            Err(Error::TooLarge { .. })
        ));
        assert_eq!(s.borrow().dials, 0);
    }

    #[test]
    fn a_timeout_is_retried_once_on_a_fresh_connection() {
        let s = Rc::new(RefCell::new(Script {
            conns: vec![Some(Vec::new()), Some(Vec::new()), Some(Vec::new())].into(),
            ..Script::default()
        }));
        let mut c = Client::new(MockDial(s.clone()), Now);
        assert_eq!(block_on(c.stat("/x")), Err(Error::Timeout));
        assert_eq!(s.borrow().dials, 2);
        assert!(!c.is_connected());
    }

    #[test]
    fn write_file_truncates_then_writes() {
        let mut script = resp(1, STATUS_OK, 0, b"");
        script.extend(resp(2, STATUS_OK, 0, b""));
        let (mut c, s) = client(vec![Some(script)]);
        assert_eq!(block_on(c.write_file("/p", b"abc")), Ok(()));
        let sent = &s.borrow().sent[0];
        assert_eq!(sent[13], OP_TRUNCATE);
        let second = 12 + 24 + 2;
        assert_eq!(sent[second + 13], OP_WRITE);
        assert!(sent.ends_with(b"/pabc"));
    }

    #[test]
    fn names_split_like_the_kernel_joins() {
        let got: Vec<&str> = names(b"a.txt\nsub/\nz").collect();
        assert_eq!(got, ["a.txt", "sub/", "z"]);
        assert_eq!(names(b"").count(), 0);
    }

    #[test]
    fn log_goes_out_as_a_log_frame() {
        let (mut c, s) = client(vec![Some(Vec::new())]);
        assert_eq!(block_on(c.log(b"hello")), Ok(()));
        let sent = &s.borrow().sent[0];
        assert_eq!(sent[5], Kind::Log as u8);
        assert_eq!(&sent[12..], b"hello");
    }

    /// Het antwoord op een bundel: per lees (bytes, status), dan de bytes.
    fn bundle(seq: u32, reads: &[(&[u8], u16)]) -> Vec<u8> {
        let mut d = vec![0u8; reads.len() * many::RESULT_LEN];
        let mut size = 0u64;
        for (i, (b, st)) in reads.iter().enumerate() {
            many::put_result(&mut d, i, b.len() as u32, *st).unwrap();
            size += b.len() as u64;
        }
        for (b, _) in reads {
            d.extend_from_slice(b);
        }
        resp(seq, STATUS_OK, size, &d)
    }

    #[test]
    fn read_many_sends_one_list_and_lands_each_read_in_its_own_buffer() {
        let (mut c, s) = client(vec![Some(bundle(
            1,
            &[(b"abcd", STATUS_OK), (b"", 1), (b"xy", STATUS_OK)],
        ))]);
        let (mut a, mut b, mut z) = ([0u8; 4], [7u8; 4], [0u8; 4]);
        let mut ops = [
            ReadOp::new(4096, &mut a),
            ReadOp::new(1 << 33, &mut b),
            ReadOp::new(10, &mut z),
        ];
        assert_eq!(block_on(c.read_many("/db", &mut ops)), Ok(6));
        assert_eq!(ops[0].got, Ok(4));
        assert_eq!(ops[1].got, Err(1), "een fout in de ene");
        assert_eq!(ops[2].got, Ok(2), "het einde van het bestand");
        assert_eq!((a, b, &z[..2]), (*b"abcd", [7; 4], &b"xy"[..]));
        assert!(c.is_connected());
        let sent = &s.borrow().sent[0];
        assert_eq!(sent[13], OP_READ_MANY);
        assert_eq!(u64::from_le_bytes(sent[28..36].try_into().unwrap()), 3, "n");
        let list = &sent[36 + 3..];
        assert_eq!(list.len(), 3 * many::OP_LEN);
        assert_eq!(many::op(list, 1), Some((1 << 33, 4)));
        assert_eq!(many::op(list, 2), Some((10, 4)));
    }

    #[test]
    fn read_many_refuses_an_empty_long_or_large_list_without_a_call() {
        let (mut c, s) = client(vec![]);
        assert_eq!(block_on(c.read_many("/f", &mut [])), Ok(0));
        let mut bufs = [[0u8; 1]; MAX_READS + 1];
        let mut ops: Vec<ReadOp<'_>> = bufs.iter_mut().map(|b| ReadOp::new(0, b)).collect();
        assert_eq!(
            block_on(c.read_many("/f", &mut ops)),
            Err(Error::TooLarge {
                len: MAX_READS + 1,
                max: MAX_READS
            })
        );
        let mut big = vec![0u8; MAX_READ_BYTES + 1];
        let mut ops = [ReadOp::new(0, &mut big)];
        assert!(matches!(
            block_on(c.read_many("/f", &mut ops)),
            Err(Error::TooLarge { .. })
        ));
        assert_eq!(s.borrow().dials, 0);
    }

    #[test]
    fn a_bundle_answer_that_does_not_fit_is_a_protocol_error() {
        // Meer bytes dan de buffer van de lees.
        let (mut c, _) = client(vec![Some(bundle(1, &[(b"abcdef", STATUS_OK)]))]);
        let mut a = [0u8; 4];
        let mut ops = [ReadOp::new(0, &mut a)];
        assert!(matches!(
            block_on(c.read_many("/f", &mut ops)),
            Err(Error::Protocol(_))
        ));
        assert!(!c.is_connected());
        // Een reset: één keer opnieuw, op een verse verbinding.
        let (mut c, s) = client(vec![None, Some(bundle(2, &[(b"ok", STATUS_OK)]))]);
        let mut ops = [ReadOp::new(0, &mut a)];
        assert_eq!(block_on(c.read_many("/f", &mut ops)), Ok(2));
        assert_eq!(s.borrow().dials, 2);
    }
}
