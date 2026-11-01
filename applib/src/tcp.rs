//! Een `appnet::TcpStream` als verbinding voor leanhttp (feature `http`).
//!
//! Bezit de stroom en zijn twee termijnen. De stroom heeft alleen async
//! methodes (`read`, `write`); leanhttp vraagt poll-methodes. De brug maakt
//! per poll de future van één `read` of `write` en pollt hem één keer: bij
//! `WouldBlock` zet die de waker van deze taak op het handvat in de stack en
//! geeft `Pending`, en wegvallen kost niets, want hij hield niets vast
//! buiten die registratie.
//!
//! De termijnen van de server (KAM: verzoekkop, body, schrijven) lopen op
//! het timerwiel van de executor van de app-core: een verbinding die zwijgt,
//! wordt na haar termijn gewekt en gesloten, en houdt geen taak uit een
//! vaste pool vast.
//!
//! Waarom hier: tot alpha.9 stond deze brug twee keer, in `apps/welcome` en
//! in Hop's `hop-http`, en de twee liepen al uit elkaar (de kap als
//! constructor-argument tegen een bouwer, een net andere foutvertaling).
//! Eén brug, één plek voor een fix. Achter een feature, want niet elke app
//! praat HTTP en leanhttp hoort dan niet in zijn image.
//!
//! Daarnaast de [`Dialer`] voor leanhttp als client (decode, vitals).
//!
//! Wat hier niet staat: de server zelf (leanhttp), de stack (`appnet`).

use crate::appnet::{NetError, StackError, TcpStream};
use crate::rt::Exec;
use alloc::boxed::Box;
use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Poll};
use core::time::Duration;
use leanhttp::{AsyncRead, AsyncWrite, Close, Dial, IoError, Target};

/// Wat de brug van een stroom vraagt: lezen, schrijven, sluiten.
///
/// [`TcpStream`] is de enige echte; de trait bestaat zodat de brug op de
/// host te toetsen is tegen een nep-stroom, zonder stack en zonder ringen.
pub trait Stream {
    /// Leest hooguit `buf.len()` bytes; 0 is EOF.
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, NetError>>;

    /// Schrijft een deel van `data`; het aantal bytes.
    fn write(&mut self, data: &[u8]) -> impl Future<Output = Result<usize, NetError>>;

    /// Sluit de stroom: FIN na de gebufferde data.
    fn close(self) -> Result<(), NetError>;
}

impl Stream for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, NetError>> {
        TcpStream::read(self, buf)
    }

    fn write(&mut self, data: &[u8]) -> impl Future<Output = Result<usize, NetError>> {
        TcpStream::write(self, data)
    }

    fn close(self) -> Result<(), NetError> {
        TcpStream::close(self)
    }
}

/// Een wekker op het timerwiel: de future van `Exec::until`.
type Alarm = Pin<Box<dyn Future<Output = ()>>>;

/// Eén richting: de deadline en de wekker die erbij hoort.
#[derive(Default)]
struct Deadline {
    at: Option<u64>,
    alarm: Option<Alarm>,
}

impl Deadline {
    /// Zet de termijn op `d` vanaf nu; `None` wist hem.
    fn set(&mut self, exec: &'static Exec, d: Option<Duration>) {
        self.at = d.map(|d| {
            let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
            exec.now().saturating_add(ns)
        });
        self.alarm = self.at.map(|at| -> Alarm { Box::pin(exec.until(at)) });
    }

    /// Is de termijn verstreken? Zo niet, dan staat de wekker op de waker
    /// van `cx`.
    fn is_expired(&mut self, exec: &'static Exec, cx: &mut Context<'_>) -> bool {
        let Some(at) = self.at else {
            return false;
        };
        if exec.now() >= at {
            return true;
        }
        match self.alarm.as_mut() {
            // De wekker registreert de waker van deze taak in het wiel; is
            // hij net afgelopen, dan is de termijn ook verstreken.
            Some(a) => a.as_mut().poll(cx).is_ready(),
            None => false,
        }
    }
}

/// Een TCP-verbinding van de app als leanhttp-verbinding.
pub struct TcpConn<S: Stream = TcpStream> {
    stream: Option<S>,
    exec: &'static Exec,
    read: Deadline,
    write: Deadline,
    cap: Option<Duration>,
}

impl<S: Stream> TcpConn<S> {
    /// Neemt `stream` over; de termijnen lopen op `exec`.
    pub fn new(stream: S, exec: &'static Exec) -> Self {
        Self {
            stream: Some(stream),
            exec,
            read: Deadline::default(),
            write: Deadline::default(),
            cap: None,
        }
    }

    /// Kapt elke leestermijn die de server zet af op `cap`.
    ///
    /// Waarom: de keep-alive-stilte van leanhttp is 60 s, en een browser
    /// houdt zijn verbinding zolang open. Met een vaste pool van werkers
    /// (of een poort die één verbinding tegelijk bedient) houdt zo'n stille
    /// verbinding een werker een minuut vast; na `cap` gaat hij dicht en
    /// opent de browser gewoon een nieuwe. Een termijn `None` (de server
    /// leest dan niet) blijft `None`.
    #[must_use]
    pub fn with_read_cap(mut self, cap: Duration) -> Self {
        self.cap = Some(cap);
        self
    }
}

/// Verbindingen voor leanhttp over de stack van de app: de naam via de
/// DNS-server uit de env (een adres meteen), dan TCP met termijn `connect`.
/// Wat er misging, staat in [`Dialer::last_error`].
pub struct Dialer {
    exec: &'static Exec,
    connect: Duration,
    last_error: Option<DialError>,
}

/// Waarom een [`Dialer`] geen verbinding kreeg, voor de logregel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialError {
    /// De naam kon niet worden opgezocht.
    Resolve(NetError),
    /// Het adres nam de verbinding niet aan.
    Connect {
        /// Het opgezochte adres.
        ip: [u8; 4],
        /// De poort.
        port: u16,
        /// Waarom niet.
        error: NetError,
    },
}

impl core::fmt::Display for DialError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Resolve(e) => write!(f, "resolve: {e}"),
            Self::Connect { ip, port, error } => {
                let [a, b, c, d] = ip;
                write!(f, "connect {a}.{b}.{c}.{d}:{port}: {error}")
            }
        }
    }
}

impl Dialer {
    /// Een dialer op de executor `exec`, met `connect` om te verbinden.
    pub fn new(exec: &'static Exec, connect: Duration) -> Self {
        Self {
            exec,
            connect,
            last_error: None,
        }
    }

    /// Waarom de laatste dial mislukte, als dat zo was.
    pub fn last_error(&self) -> Option<DialError> {
        self.last_error
    }
}

impl Dial for Dialer {
    type Conn = TcpConn;

    async fn dial(&mut self, t: Target<'_>) -> leanhttp::Result<TcpConn> {
        let ip = match crate::appnet::resolve(t.host).await {
            Ok(ip) => ip,
            Err(e) => {
                self.last_error = Some(DialError::Resolve(e));
                return Err(leanhttp::Error::Dial);
            }
        };
        let s = match TcpStream::connect_timeout(ip, t.port, self.connect).await {
            Ok(s) => s,
            Err(error) => {
                self.last_error = Some(DialError::Connect {
                    ip,
                    port: t.port,
                    error,
                });
                return Err(leanhttp::Error::Dial);
            }
        };
        self.last_error = None;
        Ok(TcpConn::new(s, self.exec))
    }
}

/// Een netfout als fout van de verbinding.
#[must_use]
pub fn io_error(e: NetError) -> IoError {
    match e {
        NetError::Timeout | NetError::Stack(StackError::DeadlineExceeded) => IoError::TimedOut,
        NetError::Stack(StackError::Reset) => IoError::Reset,
        NetError::Stack(StackError::Closed | StackError::TcpClosed | StackError::StackClosed) => {
            IoError::Closed
        }
        _ => IoError::Other,
    }
}

impl<S: Stream> AsyncRead for TcpConn<S> {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, IoError>> {
        let Some(s) = self.stream.as_mut() else {
            return Poll::Ready(Err(IoError::Closed));
        };
        if let Poll::Ready(r) = pin!(s.read(buf)).poll(cx) {
            return Poll::Ready(r.map_err(io_error));
        }
        if self.read.is_expired(self.exec, cx) {
            return Poll::Ready(Err(IoError::TimedOut));
        }
        Poll::Pending
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        let timeout = match (timeout, self.cap) {
            (Some(t), Some(c)) => Some(t.min(c)),
            (t, _) => t,
        };
        self.read.set(self.exec, timeout);
        Ok(())
    }
}

impl<S: Stream> AsyncWrite for TcpConn<S> {
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, IoError>> {
        let Some(s) = self.stream.as_mut() else {
            return Poll::Ready(Err(IoError::Closed));
        };
        if let Poll::Ready(r) = pin!(s.write(buf)).poll(cx) {
            return Poll::Ready(r.map_err(io_error));
        }
        if self.write.is_expired(self.exec, cx) {
            return Poll::Ready(Err(IoError::TimedOut));
        }
        Poll::Pending
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.write.set(self.exec, timeout);
        Ok(())
    }
}

impl<S: Stream> Close for TcpConn<S> {
    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        // Synchroon: FIN na de gebufferde data, de pomp stuurt hem. Twee keer
        // sluiten is één keer sluiten.
        match self.stream.take() {
            Some(s) => Poll::Ready(s.close().map_err(io_error)),
            None => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests;
