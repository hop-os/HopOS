//! Het contract tussen een blok-driver en hopfs.
//!
//! Precies de grens die hopfs nodig heeft: `buf.len()` bytes lezen of
//! schrijven vanaf een LBA van 512 bytes, en alles wat geschreven is
//! duurzaam maken. De virtio-blk-driver op QEMU en de NVMe-driver op ijzer
//! leveren hem; de tests een schijf in RAM. Het trait woont hier en niet in
//! `kern`, omdat een driver de kern niet kent (handboek §7, dezelfde reden
//! als `netdev`).
//!
//! Eén vorm: [`AsyncBlockDevice`] met tickets. Een opdracht krijgt bij
//! [`start_tag`](AsyncBlockDevice::start_tag) een ticket, [`reap`](
//! AsyncBlockDevice::reap) haalt alle completions op en
//! [`poll_tag`](AsyncBlockDevice::poll_tag) rondt er één af. Een driver met
//! tags (de NVMe: [`depth`](AsyncBlockDevice::depth) tickets) zegt hoeveel
//! er tegelijk mogen; een driver zonder tags (virtio-blk) heeft er één,
//! ticket 0, en kijkt in `poll_tag` zelf naar het device.
//!
//! De [`Queue`] is de enige voorkant (Linux blk-mq in het klein) en maakt
//! er de [`BlockIo`] van die hopfs gebruikt: wie I/O wil, krijgt een ticket
//! en wacht op zijn eigen completion; precies één wachter tegelijk pollt het
//! device (de "pacer") op de bel van de IRQ-lijn met een vangrail van
//! [`IRQ_GUARD`], of zonder lijn eerst per ronde en daarna op
//! [`POLL_PERIOD`], en wekt de anderen waarvan de completion binnenkwam.
//! Tijdens het wachten draait de executor door. Zo kost een node met
//! zestien opdrachten in de lucht één poll per ronde, niet zestien.
//! GEMETEN 01-10 op de M4 (de schijf-bench van toen in de kern): willekeurig 4 KiB lezen
//! haalt 11.888 per seconde met één opdracht tegelijk (~84 us per lees) en
//! 175.055 met zestien tegelijk door deze wachtrij. De wachtrij is er voor
//! de node, niet voor de app.
//!
//! Wie vóór de executor iets met de schijf doet (de mount bij de boot, de
//! meetbank), draait precies die futures af met [`block_on`], dat ze elke
//! ronde opnieuw pollt: dezelfde driver, dezelfde wachtrij, geen tweede
//! pad. De mount gebruikt daarbij de wachtrij van de actor zelf, met de
//! [`Pace`] van de executor (`ExecPace` in `hopos/src/storage.rs`); de
//! meetbank en de tests een [`Pace`] die pollt ([`Spin`]). Tot 04-10
//! stond ernaast nog een tweede voorkant voor één opdracht tegelijk (de
//! oude `Paced`, met `start` en `poll_done` in het contract), alleen voor
//! de meetbank en de tests: weg, de meetbank meet nu de wachtrij die hopfs
//! ook gebruikt.
//!
//! # Waarom er één pad is
//!
//! Les van 30-09 (de soak, ruim 1100 runs): tot alpha.14 was de grens
//! synchroon (`read`, `write`, `flush` die terugkeerden als het klaar was).
//! De periodieke hopfs-commit (FLUSH, de boom, FLUSH) wachtte op de executor
//! van de OS-core tot 5 s per verzoek op het device; op macOS is een FLUSH
//! van QEMU een F_FULLFSYNC van 3 tot 786 ms, en zolang stonden de kern, Hop
//! en de switch stil: geen tik, geen verkeer. Met 2,5 s per verzoek
//! nagebootst was dat een stilte van 7 s (`late_ms=6611` op de tik).
//!
//! In de Go-kern was synchroon wachten gratis: elke wachter was een
//! goroutine, en een goroutine is een eigen stack. Wie op de NVMe wachtte,
//! parkeerde zijn stack, en TamaGo's scheduler gaf de core aan de volgende.
//! Rust heeft hier geen goroutines en geen threadscheduler: één executor per
//! core, en een functie die wacht zonder terug te keren, houdt die core vast
//! met alles erop. Wachten is dus `.await`, en de enige manier om zonder
//! scheduler te wachten zonder de core vast te houden is submit plus await
//! (PORT.md §3: "de NVMe-actor: zijn lus ís één tegelijk, `submit(buf) ->
//! InFlight`"; hier een ticket en de wachter van de [`Queue`]).
//!
//! Een synchrone vorm ernaast was daarom geen gemak maar een tweede
//! driverpad (post plus spin) dat de stilte terugbrengt zodra iemand hem
//! vanaf de executor aanroept, en dat apart getest moet worden. Hij is weg:
//! wat vóór de executor moet wachten, wacht met [`block_on`]
//! over dezelfde async code, en daar houdt het de core terecht vast, want er
//! draait nog niets anders.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![forbid(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use sync::Signal;

/// De maat van een LBA van het contract: altijd 512 bytes, ook als het
/// device 4096-byte-blokken heeft (dan rekent de driver om).
pub const LBA_SIZE: u64 = 512;

/// De vangrail op de IRQ-lijn: een verloren flank mag nooit een hang
/// worden, dus wie op de bel wacht, kijkt na deze tijd toch (dezelfde
/// 10 ms als de RX-pomp).
pub const IRQ_GUARD: Duration = Duration::from_millis(10);

/// Zonder lijn: zo lang na een submit of completion pollt de wachter van de
/// [`Queue`] per ronde van de executor (een yield, geen timer). Een 4 KB-lees op NVMe is binnen
/// een paar tientallen microseconden klaar; die hoort geen timer te kosten.
pub const POLL_SPIN_NS: u64 = 100_000;

/// Zonder lijn, na [`POLL_SPIN_NS`]: de pollperiode. Een timer, geen yield:
/// een executor die per ronde yieldt slaapt nooit, en op de OS-core krijgt
/// een bewoner (Hop) alleen tijd als de executor slaapt. Een FLUSH van
/// honderden milliseconden mag Hop niet uithongeren.
pub const POLL_PERIOD: Duration = Duration::from_micros(200);

/// Waarom een blok-verzoek niet lukte. Elke variant draagt de LBA, want
/// "I/O failed" zonder plek is niets waard op een headless node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Het device meldde een fout, of antwoordde niet binnen zijn grens.
    Io {
        /// De LBA waar het verzoek begon.
        lba: u64,
    },
    /// Het verzoek valt buiten de schijf.
    OutOfRange {
        /// De LBA waar het verzoek begon.
        lba: u64,
        /// De lengte in bytes.
        len: usize,
    },
    /// Het device is blijvend dood (een stil device kan nog in zijn
    /// DMA-buffer schrijven, dus na één stilte is er geen weg terug).
    Dead,
    /// Nu geen plaats op het device: alle tickets bezet, of de DMA-ruimte
    /// nog van een opdracht waarvan de wachter wegging (die buffer is van de
    /// controller tot de completion, handboek §1.2).
    Busy,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { lba } => write!(f, "block I/O failed at LBA {lba}"),
            Self::OutOfRange { lba, len } => {
                write!(
                    f,
                    "block request {len} bytes at LBA {lba} falls outside the disk"
                )
            }
            Self::Dead => f.write_str("block device is dead"),
            Self::Busy => f.write_str("block device still busy with an abandoned request"),
        }
    }
}

/// Het resultaat van een blok-verzoek.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Eén opdracht aan het device, hoogstens
/// [`max_transfer`](AsyncBlockDevice::max_transfer) bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op<'a> {
    /// `len` bytes lezen vanaf `lba`; de bytes komen bij de completion.
    Read {
        /// De eerste LBA.
        lba: u64,
        /// Het aantal bytes.
        len: usize,
    },
    /// `data` schrijven vanaf `lba`. De driver neemt de bytes bij de
    /// submit over (naar zijn DMA-buffer): daarna is `data` weer vrij.
    Write {
        /// De eerste LBA.
        lba: u64,
        /// De bytes.
        data: &'a [u8],
    },
    /// Alles wat geschreven is duurzaam maken.
    Flush,
}

/// Een blokapparaat in de vorm van de driver: opdrachten op tickets, met
/// een submit en een completion.
///
/// De driver bezit zijn DMA-buffer; een opdracht die loopt, is van de
/// controller tot de completion hem teruggeeft (handboek §1.2): een ticket
/// komt pas vrij bij een `Ready` van [`poll_tag`](Self::poll_tag), en tot
/// dan raakt niemand zijn bytes.
pub trait AsyncBlockDevice {
    /// De grootste transfer van één opdracht in bytes (een veelvoud van
    /// [`LBA_SIZE`]).
    fn max_transfer(&self) -> usize;

    /// Zet `op` op het device onder een eigen ticket, luidt de doorbell en
    /// keert meteen terug. [`Error::Busy`] = nu geen plaats (alle tags of de
    /// DMA-ruimte bezet): probeer het na een completion opnieuw. Een andere
    /// fout (buiten de schijf, dood) betekent dat er niets naar het device
    /// ging.
    fn start_tag(&mut self, op: Op<'_>) -> Result<usize>;

    /// Is ticket `t` klaar? Een driver met tags kijkt hier alleen naar wat
    /// [`reap`](Self::reap) al ophaalde; een driver zonder kijkt zelf. Bij
    /// `Ready` is het ticket weer vrij en staan de bytes van een lees vooraan
    /// in `into`. De time-out van het device is van de driver: daarna is het
    /// `Ready(Err(Dead))`, nooit eeuwig `Pending`.
    fn poll_tag(&mut self, t: usize, into: &mut [u8]) -> Poll<Result>;

    /// Haalt alle completions op die er zijn en toetst de time-outs; geeft
    /// de tickets die daarbij klaar kwamen (bit `t`). Een fout is een dood
    /// device: daarna geeft elke [`poll_tag`](Self::poll_tag) de fout. Een
    /// driver zonder tags haalt niets op: zijn `poll_tag` kijkt.
    fn reap(&mut self) -> Result<u64> {
        Ok(0)
    }

    /// Hoeveel opdrachten het device tegelijk aanneemt: tickets
    /// `0..depth`. Eén voor een driver zonder tags.
    fn depth(&self) -> usize {
        1
    }

    /// De bel van de IRQ-lijn van het device, als het board er een
    /// bedraadt. `None` = pollen.
    fn irq(&self) -> Option<&'static Signal> {
        None
    }

    /// Zonder lijn: hoe lang de wachter na een beweging op het device per
    /// ronde pollt, en daarna op welke periode. Een driver die zijn
    /// opdrachten kent, kiest zelf (de ANS: 20 us en 20 us); anders
    /// [`POLL_SPIN_NS`] en [`POLL_PERIOD`].
    fn poll_pace(&self) -> (u64, Duration) {
        (POLL_SPIN_NS, POLL_PERIOD)
    }

    /// De meetlat van de driver als tekst voor één consoleregel (opdrachten,
    /// read-ahead, de traagste), en een teller die verandert zodra er iets
    /// gebeurde. Leeg en nul voor een driver zonder meetlat.
    fn stats(&self, out: &mut dyn fmt::Write) -> Result<u64, fmt::Error> {
        let _ = out;
        Ok(0)
    }
}

/// De hele schijf van een board (`board::Board::probe_disk`): het
/// blokcontract plus zijn maat en zijn naam, voor de bootregel van de
/// opslag en de meetbank.
pub trait Disk: AsyncBlockDevice {
    /// De capaciteit in sectoren van [`LBA_SIZE`] bytes.
    fn sectors(&self) -> u64;
    /// De modelnaam, voor de bootregel.
    fn model(&self) -> &str;
}

/// Een geleende driver is ook een driver: zo leent de meetbank de schijf
/// (`Queue::new(&mut disk, Spin)`) en geeft hij hem daarna terug aan de
/// opslag.
impl<D: AsyncBlockDevice + ?Sized> AsyncBlockDevice for &mut D {
    fn max_transfer(&self) -> usize {
        (**self).max_transfer()
    }
    fn irq(&self) -> Option<&'static Signal> {
        (**self).irq()
    }
    fn poll_pace(&self) -> (u64, Duration) {
        (**self).poll_pace()
    }
    fn depth(&self) -> usize {
        (**self).depth()
    }
    fn start_tag(&mut self, op: Op<'_>) -> Result<usize> {
        (**self).start_tag(op)
    }
    fn poll_tag(&mut self, t: usize, into: &mut [u8]) -> Poll<Result> {
        (**self).poll_tag(t, into)
    }
    fn reap(&mut self) -> Result<u64> {
        (**self).reap()
    }
    fn stats(&self, out: &mut dyn fmt::Write) -> Result<u64, fmt::Error> {
        (**self).stats(out)
    }
}

/// De klok en de wekker van wie op een device wacht: de executor levert
/// hem (in de binary `exec.after`).
pub trait Pace {
    /// De future van [`sleep`](Pace::sleep).
    type Sleep: Future<Output = ()> + Unpin;
    /// Monotone nanoseconden.
    fn now(&self) -> u64;
    /// Een future die na `d` klaar is.
    fn sleep(&self, d: Duration) -> Self::Sleep;
}

/// Een [`Pace`] die pollt: nooit slapen, altijd opnieuw kijken. Voor
/// [`block_on`] vóór de executor (de meetbank) en de tests; de mount bij de
/// boot draait met [`block_on`] over de wachtrij van de actor en diens
/// `Pace`. De
/// klok staat stil, dus de wachter van de [`Queue`] blijft in zijn
/// yield-venster en toetst het device bij elke ronde van `block_on`; de
/// time-out van een verzoek is van de driver (zijn eigen klok), dus ook zo
/// wordt een stil device nooit een eeuwige lus.
#[derive(Debug, Clone, Copy, Default)]
pub struct Spin;

impl Pace for Spin {
    type Sleep = core::future::Ready<()>;
    fn now(&self) -> u64 {
        0
    }
    fn sleep(&self, _d: Duration) -> Self::Sleep {
        core::future::ready(())
    }
}

/// Eén lees van een batch ([`BlockIo::read_batch`]): `into.len()` bytes
/// vanaf `lba`, hoogstens één opdracht van het device groot
/// ([`max_transfer`](AsyncBlockDevice::max_transfer)).
#[derive(Debug)]
pub struct BatchRead<'b> {
    /// De eerste LBA.
    pub lba: u64,
    /// De bestemming.
    pub into: &'b mut [u8],
    /// De uitkomst, gezet door [`BlockIo::read_batch`].
    pub result: Result,
    /// Het ticket op het device, zolang de lees daar staat.
    ticket: Option<usize>,
    /// De uitkomst staat er.
    done: bool,
}

impl<'b> BatchRead<'b> {
    /// Een lees van `into.len()` bytes vanaf `lba`, nog niet gedaan.
    pub fn new(lba: u64, into: &'b mut [u8]) -> Self {
        Self {
            lba,
            into,
            result: Err(Error::Busy),
            ticket: None,
            done: false,
        }
    }
}

/// Het blokapparaat zoals hopfs het ziet: lezen, schrijven en flushen als
/// futures, van elke lengte (de brokken zijn van de implementatie).
pub trait BlockIo {
    /// Leest `buf.len()` bytes vanaf `lba`.
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> impl Future<Output = Result>;
    /// Schrijft `buf` vanaf `lba`.
    fn write(&mut self, lba: u64, buf: &[u8]) -> impl Future<Output = Result>;
    /// Maakt alles wat geschreven is duurzaam.
    fn flush(&mut self) -> impl Future<Output = Result>;
    /// Leest alle `ops` en zet bij elk de uitkomst. Een device met tickets
    /// ([`Queue`]) zet ze allemaal tegelijk op het device en wacht er één
    /// keer op (Linux: de plug van blk-mq, io_uring in het klein); zonder
    /// tickets gaan ze na elkaar. Een fout in de ene laat de andere staan.
    fn read_batch(&mut self, ops: &mut [BatchRead<'_>]) -> impl Future<Output = ()> {
        async move {
            for op in ops.iter_mut() {
                op.result = self.read(op.lba, op.into).await;
                op.done = true;
            }
        }
    }
}

impl<D: BlockIo + ?Sized> BlockIo for &mut D {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> impl Future<Output = Result> {
        (**self).read(lba, buf)
    }
    fn read_batch(&mut self, ops: &mut [BatchRead<'_>]) -> impl Future<Output = ()> {
        (**self).read_batch(ops)
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> impl Future<Output = Result> {
        (**self).write(lba, buf)
    }
    fn flush(&mut self) -> impl Future<Output = Result> {
        (**self).flush()
    }
}

/// Draait een future af zonder executor: pollen tot hij klaar is. Voor wat
/// vóór `exec.run` op de schijf wacht (de mount bij de boot, de meetbank) en
/// voor de tests; op de executor hoort hij nooit, want hij houdt de core
/// vast. Het is geen tweede driverpad: dezelfde [`Queue`]-futures, dezelfde
/// tickets, alleen zonder iemand die intussen iets anders doet.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = core::pin::pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        core::hint::spin_loop();
    }
}

mod queue;
pub use queue::{MAX_DEPTH, Queue};

#[cfg(test)]
mod tests;
