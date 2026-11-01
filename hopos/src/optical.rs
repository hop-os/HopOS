//! De optical-owner en de berichtenbrug naar de enige USB-eigenaar.
//! Alle buffers hebben één eigenaar; geen RefCell-lening over een await.
use abi::hopabi::{
    HDR_LEN, OP_DEVICE_COMMAND, OP_READ, OP_STAT,
    device::{Command, RESULT_LEN, Reply},
};
use alloc::vec::Vec;
use executor::Executor;
use kern::{Error, Result, deviceabi, rpc::FsCall};
use media_optical::{Data, UsbError, asynchronous::Transport, mmc::Drive};
use sync::{LocalCell, mpsc::Mailbox};
use usb::storage::{BulkError, BulkId, BulkInfo, BulkOp, BulkReq};
pub(crate) static WAKE: sync::Signal = sync::Signal::new();

pub(crate) static INBOX: deviceabi::Inbox<'static> = Mailbox::new();
const CAP: usize = 64 << 10;
// Poortnamen blijven staan nadat een apparaat verdwijnt: disc1 wordt niet
// stilzwijgend disc0 wanneer de gebruiker disc0 loshaalt.
#[derive(Clone, Copy)]
struct Found {
    host: u8,
    port: u8,
    info: Option<BulkInfo>,
}
static FOUND: LocalCell<[Option<Found>; 8]> = LocalCell::cell([None; 8]);
static BULK: Mailbox<Transfer, 2> = Mailbox::new();
static RETURNED: Mailbox<Completed, 2> = Mailbox::new();
struct Transfer {
    req: BulkReq,
    bytes: Vec<u8>,
}
struct Completed {
    bytes: Vec<u8>,
    result: core::result::Result<usize, BulkError>,
}

/// De USB-sink bezit precies één uitstaande optical-transfer.
#[derive(Default)]
pub(crate) struct Bridge {
    pending: Option<Transfer>,
}
impl Bridge {
    pub(crate) fn attached(&mut self, info: &BulkInfo) {
        let mut slots = FOUND.borrow_mut();
        let existing = slots
            .iter()
            .position(|s| s.is_some_and(|s| s.host == info.id.host && s.port == info.port));
        if let Some(i) = existing.or_else(|| slots.iter().position(Option::is_none)) {
            slots[i] = Some(Found {
                host: info.id.host,
                port: info.port,
                info: Some(*info),
            });
            cpu::println!(
                "optical: disc{i}: {} port {} available HOPOS_OPTICAL_ATTACH",
                info.host,
                info.port
            );
        }
    }
    pub(crate) fn gone(&mut self, id: BulkId) {
        for s in FOUND.borrow_mut().iter_mut().flatten() {
            if s.info.is_some_and(|i| i.id == id) {
                s.info = None;
            }
        }
    }
    pub(crate) fn next(&mut self) -> Option<BulkReq> {
        if self.pending.is_some() {
            return None;
        }
        self.pending = BULK.try_recv();
        self.pending.as_ref().map(|t| t.req)
    }
    pub(crate) fn output(&self, req: &BulkReq) -> &[u8] {
        self.pending
            .as_ref()
            .filter(|t| t.req == *req)
            .and_then(|t| t.bytes.get(..req.len))
            .unwrap_or_default()
    }
    pub(crate) fn input(&mut self, req: &BulkReq) -> &mut [u8] {
        self.pending
            .as_mut()
            .filter(|t| t.req == *req)
            .and_then(|t| t.bytes.get_mut(..req.len))
            .unwrap_or(&mut [])
    }
    pub(crate) fn done(&mut self, req: &BulkReq, result: core::result::Result<usize, BulkError>) {
        if !self.pending.as_ref().is_some_and(|t| t.req == *req) {
            return;
        }
        if let Some(t) = self.pending.take() {
            if let Err(e) = result {
                cpu::println!("optical: bulk: {e} HOPOS_OPTICAL_USB_ERROR");
            }
            // Eén owner, één transfer: de vorige reply is opgehaald vóór
            // deze aanvraag ontstaat. Vol is dus een contractfout, luid.
            if RETURNED
                .try_send(Completed {
                    bytes: t.bytes,
                    result,
                })
                .is_err()
            {
                cpu::println!("optical: reply mailbox unexpectedly full HOPOS_OPTICAL_FATAL");
            }
        }
    }
}
struct Link {
    info: BulkInfo,
    scratch: Vec<u8>,
    exec: &'static Executor,
    tag: u32,
    deadline: u64,
}
impl Link {
    fn new(info: BulkInfo, exec: &'static Executor) -> Result<Self> {
        let size = info.max_transfer.min(CAP);
        if size < 2048 {
            return Err(Error::Device {
                reason: "USB transfer limit below one sector",
            });
        }
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(size)
            .map_err(|_| Error::OutOfMemory { bytes: size })?;
        scratch.resize(size, 0);
        Ok(Self {
            info,
            scratch,
            exec,
            tag: 0,
            deadline: 0,
        })
    }
    async fn transfer(&mut self, op: BulkOp, len: usize) -> core::result::Result<usize, UsbError> {
        self.tag = self.tag.wrapping_add(1);
        let deadline = (self.deadline != 0).then_some(self.deadline);
        let req = BulkReq::new(self.info.id, op, len, deadline, self.exec.now(), self.tag)
            .map_err(|_| UsbError(1))?;
        let Some(req) = req else {
            return Ok(0);
        };
        if len > self.scratch.len() {
            return Err(UsbError(2));
        }
        let bytes = core::mem::take(&mut self.scratch);
        if let Err(sync::Full(t)) = BULK.try_send(Transfer { req, bytes }) {
            self.scratch = t.bytes;
            return Err(UsbError(3));
        }
        WAKE.set();
        let done = RETURNED.recv().await;
        self.scratch = done.bytes;
        done.result.map_err(|e| match e {
            BulkError::Gone => UsbError(4),
            _ => UsbError(5),
        })
    }
}
impl Transport for Link {
    async fn out(&mut self, data: &[u8]) -> core::result::Result<(), UsbError> {
        self.scratch
            .get_mut(..data.len())
            .ok_or(UsbError(2))?
            .copy_from_slice(data);
        let n = self.transfer(BulkOp::Out, data.len()).await?;
        if n != data.len() {
            return Err(UsbError(6));
        }
        Ok(())
    }
    async fn input(&mut self, dst: &mut [u8]) -> core::result::Result<usize, UsbError> {
        let n = self.transfer(BulkOp::In, dst.len()).await?;
        if n > dst.len() {
            return Err(UsbError(6));
        }
        dst[..n].copy_from_slice(&self.scratch[..n]);
        Ok(n)
    }
    async fn reset_recovery(&mut self) -> core::result::Result<(), UsbError> {
        self.deadline = self.exec.now().saturating_add(10_000_000_000);
        self.transfer(BulkOp::Reset, 0).await.map(|_| ())
    }
    fn max_transfer(&self) -> usize {
        self.info.max_transfer.min(CAP)
    }
}
fn optical_error(e: media_optical::Error) -> Error {
    cpu::println!("optical: {e} HOPOS_OPTICAL_ERROR");
    let reason = match e {
        media_optical::Error::Check(s) => s.what(),
        media_optical::Error::Invalid => "invalid optical command or medium geometry",
        _ => "USB optical transport failed; reopen and verify medium before retry",
    };
    Error::Device { reason }
}
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor) {
    if exec.spawn(run(exec)).is_err() {
        cpu::println!("optical: owner could not start HOPOS_OPTICAL_FATAL");
    }
}
async fn run(exec: &'static Executor) {
    let mut drives: [Option<Drive<Link>>; 8] = [const { None }; 8];
    loop {
        let mut req = INBOX.recv().await;
        let result = handle(exec, &mut drives, &mut req.call).await;
        req.finish(result);
    }
}
async fn handle(
    exec: &'static Executor,
    drives: &mut [Option<Drive<Link>>; 8],
    c: &mut FsCall,
) -> Result<(u64, usize)> {
    let index = deviceabi::target(
        &crate::SERVICERS,
        c.slot,
        c.generation,
        c.buf.get(c.path.clone()).ok_or(Error::BadPath)?,
    )?
    .ok_or(Error::Denied)?;
    if !matches!(c.op, OP_READ | OP_STAT | OP_DEVICE_COMMAND) {
        return Err(Error::Denied);
    }
    let info = FOUND.borrow()[index]
        .and_then(|s| s.info)
        .ok_or(Error::NoEnt)?;
    if drives[index]
        .as_mut()
        .is_some_and(|d| d.bot().transport().info.id != info.id)
    {
        drives[index] = None;
    }
    if drives[index].is_none() {
        drives[index] = Some(
            Drive::open(Link::new(info, exec)?)
                .await
                .map_err(optical_error)?,
        );
    }
    let drive = drives[index].as_mut().ok_or(Error::NoEnt)?;
    drive.bot().transport().deadline = exec.now().saturating_add(10_000_000_000);
    let out = c
        .out
        .get_mut(HDR_LEN..)
        .ok_or(Error::Corrupt { at: HDR_LEN })?;
    let result = match c.op {
        OP_DEVICE_COMMAND => {
            let cmd = Command::decode(
                c.buf.get(c.data.clone()).ok_or(Error::Corrupt { at: 0 })?,
                CAP,
            )
            .map_err(|_| Error::Corrupt { at: 0 })?;
            drive.bot().transport().deadline = exec
                .now()
                .saturating_add(u64::from(cmd.timeout_ms) * 1_000_000);
            if out.len() < RESULT_LEN + cmd.in_len as usize {
                return Err(Error::TooLarge {
                    len: RESULT_LEN + cmd.in_len as usize,
                    max: out.len(),
                });
            }
            let data = if cmd.in_len != 0 {
                Data::In(&mut out[RESULT_LEN..RESULT_LEN + cmd.in_len as usize])
            } else if !cmd.data_out.is_empty() {
                Data::Out(cmd.data_out)
            } else {
                Data::None
            };
            let r = drive
                .bot()
                .command(cmd.cdb, data)
                .await
                .map_err(optical_error)?;
            let data_len = if cmd.in_len != 0 { r.transferred } else { 0 };
            Reply {
                status: r.status,
                transferred: r.transferred as u32,
                sense: &r.sense[..r.sense_len],
                data: &[],
            }
            .encode(&mut out[..RESULT_LEN])
            .map_err(|_| Error::Corrupt { at: 0 })?;
            Ok((0, RESULT_LEN + data_len))
        }
        OP_STAT => drive.size().await.map(|s| (s, 0)).map_err(optical_error),
        OP_READ => {
            // READ(10) itself checks the medium range. No capacity command
            // per chunk: that extra request would cost throughput and spin.
            let n = (usize::try_from(c.n).unwrap_or(usize::MAX))
                .min(out.len())
                .min(32 << 10);
            drive
                .read_at(c.off, &mut out[..n])
                .await
                .map(|n| (0, n))
                .map_err(optical_error)
        }
        _ => Err(Error::Denied),
    };
    if crate::SERVICERS.current(c.slot) != Some(c.generation) {
        return Err(Error::Denied);
    }
    result
}
