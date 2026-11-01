//! De ANS: dezelfde NVMe, een andere aanlanding (Apple silicon, de interne
//! SSD van de Mac mini M4; Linux `drivers/nvme/host/apple.c`).
//!
//! Op Apple silicon zit de SSD niet als PCIe-device op een bus maar achter
//! een coprocessor (ANS, Apple NVMe Storage) met zijn eigen RTKit-firmware.
//! Het NVMe daarboven is gewoon NVMe en dat doet de core. Vier dingen zijn
//! anders, en die staan in dit bestand:
//!
//! 1. De submission-deurbel is "lineair": je schrijft niet de nieuwe tail
//!    maar het SLOT waar de opdracht staat ([`Transport::LINEAR`]). De
//!    completion-deurbellen zijn de gewone (0x1004 en 0x100c, DSTRD = 0).
//! 2. Elke opdracht heeft naast zijn SQE een TCB van 128 bytes in de
//!    zijtabel van zijn queue: de NVMMU leest daaruit welke kant de DMA op
//!    gaat en welke buffers erbij horen. Na elke completion moet dat slot
//!    ongeldig verklaard worden, anders loopt de tabel vol.
//! 3. De coprocessor ervoor moet gewekt worden, zijn mailbox moet tijdens
//!    elke wachtlus leeggetrokken worden, en bij afsluiten moet hij in
//!    slaap. Dat gesprek (RTKit) woont niet hier maar achter het trait
//!    [`Coprocessor`], dat het board met `driver-rtkit` invult: deze crate
//!    kent geen RTKit.
//! 4. De schijf is niet van ons. macOS staat erop (iBootSystemContainer,
//!    APFS, RecoveryOS), dus lezen mag overal (de GPT staat op blok 1), maar
//!    schrijven alleen binnen een venster dat de aanroeper expliciet zet
//!    ([`Nvme::set_window`], typisch het grootste gat uit `fw::gpt` na de
//!    macOS-partities). Zonder venster weigert elke schrijf, luid.
//!
//! De vier lessen van 29-08 (`OLD/docs/v1/archief/apple-m4.md`, "De
//! leeskant werkt") staan op de plek waar ze gelden: het wekbericht
//! ([`Nvme::start`]), CC bijstellen in plaats van overschrijven
//! ([`Nvme::start`]), de opdracht op het slot dat de deurbel aanwijst
//! ([`Apple::submit`](Transport::submit)), en netjes afsluiten
//! ([`Nvme::shutdown`]).
//!
//! Schrijven is dezelfde weg als lezen met opcode 0x01: m1n1 en Linux zetten
//! de DMA-richting van de TCB op `opcode & 1`. Referentie: m1n1
//! `src/nvme.c`, Linux `drivers/nvme/host/apple.c`.

use super::{
    CC_EN, CC_SHN_MASK, Cmd, Error, IO_READ, Nvme, Q_ENTRIES, Queue, Result, Transport, Window,
    write_lo_hi,
};
use core::fmt;
use core::mem::{offset_of, size_of};
use dev::{Pa, Reg};

/// Wat het ANS-pad van de coprocessor ervoor nodig heeft. Het board vult
/// dit in met `driver-rtkit` (de ASC-basis, de bufferregio en de klok zijn
/// van die kant); deze crate kent geen RTKit.
pub trait Coprocessor {
    /// Wat er mis kan gaan in het gesprek.
    type Error: fmt::Display + fmt::Debug + Copy;

    /// Het opstartgesprek: wekken, HELLO, de endpointkaart, wachten tot hij
    /// "aan" meldt. Ook op een coprocessor die al draait.
    fn boot(&mut self) -> core::result::Result<(), Self::Error>;

    /// De mailbox leegtrekken: syslogregels bevestigen, geheugenverzoeken
    /// beantwoorden. Hoort in elke wachtlus; een fout hier is meestal een
    /// crashmelding.
    fn service(&mut self) -> core::result::Result<(), Self::Error>;

    /// In slaap: AP-kant stil, coprocessor slapen, zijn kern stil.
    fn sleep(&mut self) -> core::result::Result<(), Self::Error>;

    /// Schrijft zijn crashlog naar `out`, als hij er een heeft.
    fn crashlog(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        let _ = out;
        Ok(())
    }
}

/// Apples toevoegingen aan het NVMe-venster uit de ADT (m1n1
/// `src/nvme.c`). De standaardregisters en de deurbellen ervoor zijn van
/// de core; alle 64-bit registers gaan in twee helften.
#[repr(C)]
struct Regs {
    _nvme: [u8; 0x1200],
    /// M4 (`nvme-secure-bar`): de I/O-SQ-basis nog eens, hier.
    ioq_cmds: [Reg<u32>; 2],
    /// M4 (`nvme-secure-bar`): de I/O-CQ-basis nog eens, hier.
    ioq_cqes: [Reg<u32>; 2],
    max_pend: Reg<u32>,
    _r3: [u8; 0x1300 - 0x1214],
    boot_status: Reg<u32>,
    _r4: [u8; 0x24908 - 0x1304],
    linear_sq_ctrl: Reg<u32>,
    db_linear_asq: Reg<u32>,
    db_linear_iosq: Reg<u32>,
}

/// De NVMMU op zijn eigen venster (M4: `reg[3]`; M1-M3: hetzelfde als
/// NVMe), met dezelfde offsets.
#[repr(C)]
struct Nvmmu {
    _r0: [u8; 0x28100],
    num: Reg<u32>,
    _r1: u32,
    asq_base: [Reg<u32>; 2],
    iosq_base: [Reg<u32>; 2],
    tcb_inval: Reg<u32>,
    _r2: [u8; 0x29120 - 0x2811c],
    tcb_stat: Reg<u32>,
}

/// Eén NVMMU-slot (Linux `struct apple_nvmmu_tcb`).
#[repr(C)]
struct Tcb {
    opcode: u8,
    dma_flags: u8,
    command_id: u8,
    _unk0: u8,
    /// NLB, 0-based: CDW12 van de opdracht.
    length: u32,
    _unk1: [u64; 2],
    prp1: u64,
    prp2: u64,
    _unk2: [u64; 2],
    _aes_iv: [u8; 8],
    _aes: [u8; 64],
}

const _: () = {
    assert!(offset_of!(Regs, ioq_cmds) == 0x1200);
    assert!(offset_of!(Regs, ioq_cqes) == 0x1208);
    assert!(offset_of!(Regs, max_pend) == 0x1210);
    assert!(offset_of!(Regs, boot_status) == 0x1300);
    assert!(offset_of!(Regs, linear_sq_ctrl) == 0x24908);
    assert!(offset_of!(Regs, db_linear_asq) == 0x2490c);
    assert!(offset_of!(Regs, db_linear_iosq) == 0x24910);
    assert!(offset_of!(Nvmmu, num) == 0x28100);
    assert!(offset_of!(Nvmmu, asq_base) == 0x28108);
    assert!(offset_of!(Nvmmu, iosq_base) == 0x28110);
    assert!(offset_of!(Nvmmu, tcb_inval) == 0x28118);
    assert!(offset_of!(Nvmmu, tcb_stat) == 0x29120);
    assert!(size_of::<Tcb>() == TCB as usize);
    assert!(offset_of!(Tcb, dma_flags) == 1);
    assert!(offset_of!(Tcb, command_id) == 2);
    assert!(offset_of!(Tcb, length) == 4);
    assert!(offset_of!(Tcb, prp1) == 24);
    assert!(offset_of!(Tcb, prp2) == 32);
    // De richting van de DMA leidt de NVMMU niet zelf af: oneven schrijft,
    // even leest (m1n1 en Linux: `opcode & 1`).
    assert!(super::IO_WRITE & 1 == 1 && IO_READ & 1 == 0 && super::ADM_IDENTIFY & 1 == 0);
};

/// Hoeveel van het NVMe-venster het board moet mappen.
pub const MMIO_LEN: u64 = size_of::<Regs>() as u64;
/// Hoeveel van het NVMMU-venster het board moet mappen.
pub const NVMMU_LEN: u64 = size_of::<Nvmmu>() as u64;

const TCB: u64 = 128;
/// De NVMMU leest in de DMA-richting: van het device naar ons.
const TCB_FROM_DEVICE: u8 = 1 << 0;
/// En van ons naar het device.
const TCB_TO_DEVICE: u8 = 1 << 1;

const CC_SHN_NORMAL: u32 = 1 << 14;
const CSTS_SHST_MASK: u32 = 3 << 2;
const CSTS_SHST_DONE: u32 = 2 << 2;
const LINEAR_SQ_EN: u32 = 1 << 0;
/// `NVME_BOOT_STATUS_OK`: de NVMe-kant van de firmware staat.
pub const BOOT_STATUS_OK: u32 = 0xde71_ce55;

const ADM_DELETE_SQ: u8 = 0x00;
const ADM_DELETE_CQ: u8 = 0x04;

/// De blokmaat van de ANS: m1n1 neemt 4 KB aan, en de namespace bevragen
/// mag niet (29-08: de firmware antwoordt met `NVME_PERM_ERR` en valt om).
pub const BLOCK: u64 = 4096;

/// De uitlijning van de DMA-regio: de paginamaat van dit silicium.
pub const DMA_ALIGN: u64 = 0x4000;
/// Hoe lang de firmware mag doen over BOOT_STATUS na het gesprek.
pub const BOOT_TIMEOUT_NS: u64 = 1_000_000_000;

/// De adressen die het board uit de ADT haalt.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Het NVMe-venster (M4: `reg[9]` van de ans-node).
    pub nvme: Pa,
    /// Het NVMMU-venster (M4: `reg[3]`; eerder hetzelfde als `nvme`).
    pub nvmmu: Pa,
    /// De DMA-regio: queues, TCB's, PRP-lijsten en databuffer. DMA-adres ==
    /// fysiek adres (de SART is een filter, geen vertaling).
    pub dma: Pa,
    /// De maat van de DMA-regio, minstens [`DMA_NEED`](super::DMA_NEED).
    pub dma_size: u64,
    /// De ADT-vlag `nvme-secure-bar` (M4): dan wil de controller de
    /// I/O-queue-bases nog eens in NVME_IOQ_CQES/CMDS, of hij crasht.
    pub secure_bar: bool,
}

/// Het transport van de ANS: de vensters, de coprocessor, en het bruikbare
/// deel van de schijf uit de GPT.
pub struct Apple<C: Coprocessor> {
    cfg: Config,
    cop: C,
    /// Eerste en laatste bruikbare blok uit de GPT-header.
    usable: (u64, u64),
}

/// De NVMe-controller achter de ANS.
pub type Ans<C> = Nvme<Apple<C>>;

impl<C: Coprocessor> Apple<C> {
    fn regs(&self) -> &'static Regs {
        // SAFETY: de voorwaarde van `Nvme::new`: het NVMe-venster is gemapt
        // voor MMIO_LEN bytes.
        unsafe { dev::regs(self.cfg.nvme) }
    }

    fn nvmmu(&self) -> &'static Nvmmu {
        // SAFETY: de voorwaarde van `Nvme::new`: het NVMMU-venster is gemapt
        // voor NVMMU_LEN bytes.
        unsafe { dev::regs(self.cfg.nvmmu) }
    }
}

impl<C: Coprocessor> Transport for Apple<C> {
    type Error = C::Error;
    /// Les 3 (29-08): de opdracht staat op de plek die de deurbel aanwijst;
    /// ergens anders neerzetten laat de firmware een oude opdracht lezen bij
    /// een verse TCB, en dat meldt hij als NVME_PERM_ERR, met een crash
    /// erachteraan die de hele coprocessor tot de volgende power-reset
    /// onbruikbaar maakt (vier boots gekost). De firmware leest per 64 bytes
    /// ONGEACHT CC.IOSQES (wij houden iBoots 7, les 2), zoals Linux: GEMETEN
    /// 01-10 op de M4, op `tag * 128` (M25) faalde elke opdracht op tag 1.
    const LINEAR: bool = true;
    const DMA_ALIGN: u64 = DMA_ALIGN;

    /// De TCB, de mailbox leeg (een wachtende coprocessor doet geen DMA),
    /// dan de lineaire deurbel met het slot.
    fn submit(&mut self, q: &Queue, slot: u16, m: &Cmd) -> Result<(), C::Error> {
        write_tcb(q.side, slot, m);
        self.service()?;
        let r = self.regs();
        let db = if q.id == 0 {
            &r.db_linear_asq
        } else {
            &r.db_linear_iosq
        };
        db.write(u32::from(slot));
        Ok(())
    }

    /// De NVMMU houdt het slot vast tot het ongeldig verklaard is; zonder
    /// dat is de tabel na 64 opdrachten vol en hangt de volgende.
    fn completed(&mut self, cid: u16) -> Result<(), C::Error> {
        let mmu = self.nvmmu();
        mmu.tcb_inval.write(u32::from(cid));
        match mmu.tcb_stat.read() {
            0 => Ok(()),
            stat => Err(Error::Nvmmu { slot: cid, stat }),
        }
    }

    fn service(&mut self) -> Result<(), C::Error> {
        self.cop.service().map_err(Error::Transport)
    }
}

impl<C: Coprocessor> Nvme<Apple<C>> {
    /// De ANS op de adressen van `cfg`, met coprocessor `cop`; nog zonder
    /// één registertoegang. [`start`](Self::start) brengt hem op; die stap
    /// is los, zodat het board bij een weigering de coprocessor nog heeft
    /// (in slaap, power-reset van zijn domein, opnieuw: m1n1's volgorde).
    ///
    /// # Safety
    ///
    /// `cfg.nvme` en `cfg.nvmmu` zijn de vensters uit de ADT, gemapt als
    /// Device voor minstens [`MMIO_LEN`] en [`NVMMU_LEN`] bytes en voor
    /// altijd. `[cfg.dma, cfg.dma + cfg.dma_size)` is gemapt geheugen dat
    /// alleen deze driver en de ANS gebruiken, binnen een SART-venster, op
    /// een adres dat de ANS ziet zoals de CPU. Het datablok (vanaf
    /// [`DATA_OFF`](super::DATA_OFF)) is Normal gemapt (write-back of NC; de
    /// driver kopieert er met `memcpy` in en uit, en doet zelf het
    /// cache-onderhoud); de rest niet gecached. GEMETEN 01-10 op de M4: de
    /// vluchtige 8-byte-lus kostte het leespad een kwart van zijn tijd.
    pub unsafe fn new(cfg: Config, cop: C, now: fn() -> u64) -> Result<Self, C::Error> {
        let x = Apple {
            cfg,
            cop,
            usable: (0, 0),
        };
        Self::at(x, cfg.nvme, cfg.dma, cfg.dma_size, now)
    }

    /// De coprocessor ervoor.
    pub fn coprocessor(&mut self) -> &mut C {
        &mut self.x.cop
    }

    /// Neemt de NVMe-controller van de ANS over.
    ///
    /// Eerst het gesprek met de coprocessor, ook als hij al draait: zonder
    /// wekbericht blijft hij in de slaapstand die iBoot achterliet
    /// (`CC.SHN = 1`), en negeert de controller elke schrijf naar CC
    /// (les 1, 29-08: CSTS.RDY werd nooit 1).
    pub fn start(&mut self) -> Result<(), C::Error> {
        self.started = false;
        self.dead = false;
        self.window = None;
        self.x.cop.boot().map_err(Error::Transport)?;

        // Dan wachten tot de NVMe-kant van de firmware er staat. Dit hoort ná
        // het gesprek: dat zet die kant opnieuw op, dus een OK van ervoor zegt
        // niets (29-08: ervoor kijken gaf een controller die de ene boot wel
        // en de andere niet ready werd).
        let a = self.x.regs();
        if !self.wait(BOOT_TIMEOUT_NS, |_| a.boot_status.read() == BOOT_STATUS_OK)? {
            return Err(Error::BootStatus {
                status: a.boot_status.read(),
            });
        }

        // Oude DMA moet aantoonbaar gestopt zijn voordat queuegeheugen
        // wijzigt.
        let r = self.regs();
        r.cc.update(|c| c & !CC_EN);
        self.wait_ready(false)?;

        // De lineaire modus en de NVMMU (hoeveel slots, waar de twee
        // TCB-tabellen liggen): vóór het inschakelen.
        let q = u32::from(Q_ENTRIES - 1);
        let m = self.x.nvmmu();
        a.linear_sq_ctrl.update(|v| v | LINEAR_SQ_EN);
        a.max_pend.write((q << 16) | q);
        m.num.write(q);
        write_lo_hi(&m.asq_base, self.admin.side.0);
        write_lo_hi(&m.iosq_base, self.io.side.0);
        dev::mb();

        // Vanaf hier gewoon NVMe. CC wordt NIET overschreven maar bijgesteld
        // (les 2, 29-08). iBoot laat er een waarde in achter (0x474000:
        // shutdown-normal, IOSQES 7, IOCQES 4). Een verse CC met onze eigen
        // maten komt niet ready; m1n1 wist daarom alleen SHN en zet EN.
        self.enable((r.cc.read() & !CC_SHN_MASK) | CC_EN)?;
        self.create_io_queues()?;
        if self.x.cfg.secure_bar {
            // M4 (`nvme-secure-bar`): zonder deze twee crasht de coprocessor
            // met een crashlog waarin de I/O-queues op nul staan (m1n1,
            // NVME_T8132).
            write_lo_hi(&a.ioq_cqes, self.io.cq.0);
            write_lo_hi(&a.ioq_cmds, self.io.sq.0);
            dev::mb();
        }
        self.capacity_from_gpt()?;
        self.started = true;
        Ok(())
    }

    /// Hoe groot de schijf is. De ANS meldt dat niet (TNVMCAP is nul, en de
    /// namespace bevragen mag niet); de schijf zegt het zelf: de GPT-header
    /// op blok 1 draagt het adres van zijn reservekopie, en die ligt op het
    /// laatste blok. Zonder GPT weigeren we: een transfer zonder bovengrens
    /// is hoe je andermans data overschrijft.
    fn capacity_from_gpt(&mut self) -> Result<(), C::Error> {
        self.block_size = BLOCK;
        self.blocks = 2; // Net genoeg om blok 1 te mogen lezen.
        let mut b = [0u8; BLOCK as usize];
        let res = self.read_blocks(1, &mut b);
        self.blocks = 0;
        res?;
        let le = |o: usize| {
            b.get(o..o + 8)
                .and_then(|s| s.try_into().ok())
                .map_or(0, u64::from_le_bytes)
        };
        if le(0) != u64::from_le_bytes(*b"EFI PART") {
            return Err(Error::NoGpt);
        }
        let (backup, first, last) = (le(32), le(40), le(48));
        if !(2..=1 << 44).contains(&backup) || first < 2 || first > last || last >= backup {
            return Err(Error::Gpt {
                backup,
                first,
                last,
            });
        }
        self.blocks = backup + 1;
        self.x.usable = (first, last);
        Ok(())
    }

    /// Zet het schrijfvenster: `blocks` blokken van [`BLOCK`] vanaf
    /// `first`. Het moet binnen het bruikbare deel van de GPT vallen (nooit
    /// over de headers of de entry-tabellen); dat het ook buiten de
    /// partities van macOS valt, is de keuze van de aanroeper (het gat uit
    /// `fw::gpt`).
    pub fn set_window(&mut self, first: u64, blocks: u64) -> Result<(), C::Error> {
        self.ready()?;
        let (uf, ul) = self.x.usable;
        let ok = blocks != 0
            && first >= uf
            && first.checked_add(blocks - 1).is_some_and(|last| last <= ul);
        if !ok {
            return Err(Error::BadWindow {
                first,
                blocks,
                usable_first: uf,
                usable_last: ul,
            });
        }
        // INVARIANT: het venster ligt binnen `usable`, en dat binnen de
        // schijf; getoetst hierboven en in `capacity_from_gpt`.
        self.window = Some(Window { first, blocks });
        Ok(())
    }

    /// Geeft de ANS terug zoals we hem aantroffen: I/O-queues weg, de
    /// controller via CC.SHN netjes uit, en de coprocessor in slaap.
    ///
    /// Les 4 (29-08): een omgevallen of achtergelaten coprocessor blijft dat,
    /// ook na een warme herstart; elke volgende boot komt de controller niet
    /// meer ready. Daarom loopt dit ook op een dode driver door (zonder de
    /// queue-opdrachten) en eindigt het altijd met de slaap; de eerste fout
    /// komt terug.
    pub fn shutdown(&mut self) -> Result<(), C::Error> {
        let mut first: Option<Error<C::Error>> = None;
        let mut note = |r: Result<(), C::Error>| {
            if let Err(e) = r {
                first.get_or_insert(e);
            }
        };
        if self.started && !self.dead {
            let id = u32::from(self.io.id);
            for opc in [ADM_DELETE_SQ, ADM_DELETE_CQ] {
                note(self.admin(Cmd {
                    opc,
                    cdw10: id,
                    ..Cmd::default()
                }));
            }
        }
        self.started = false;
        self.window = None;

        // CC.SHN op "normaal" en wachten tot SHST "klaar" meldt; pas dan
        // de enable eraf.
        let r = self.regs();
        r.cc.update(|c| (c & !CC_SHN_MASK) | CC_SHN_NORMAL);
        let done = |n: &Self| n.regs().csts.read() & CSTS_SHST_MASK == CSTS_SHST_DONE;
        note(match self.wait(self.timeout_ns, done) {
            Ok(false) => Err(Error::Shutdown {
                csts: r.csts.read(),
            }),
            r => r.map(drop),
        });
        r.cc.update(|c| c & !CC_EN);
        note(self.wait_ready(false));
        note(self.service());
        note(self.x.cop.sleep().map_err(Error::Transport));
        first.map_or(Ok(()), Err)
    }

    /// Wat de controller, de NVMMU en de coprocessor ervan vinden, als iets
    /// dat je kunt printen: voor een opdracht die niet terugkomt. Leest
    /// alleen registers als [`new`](Self::new) slaagde, en dat is zo als
    /// deze waarde bestaat.
    #[must_use]
    pub fn diag(&self) -> Diag<'_, C> {
        Diag { ans: self }
    }
}

/// Een diagnose van de ANS (zie [`Nvme::diag`]).
pub struct Diag<'a, C: Coprocessor> {
    ans: &'a Ans<C>,
}

impl<C: Coprocessor> fmt::Display for Diag<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = self.ans;
        let r = a.regs();
        let q = a.admin;
        let cqe = q.cq.add(u64::from(q.head) * super::CQE);
        write!(
            f,
            "boot={:#x} csts={:#x} cc={:#x} tcb_stat={:#x} adm_cqe[{}]={:08x}/{:08x}/{:08x}/{:08x} dead={} commands={}",
            a.x.regs().boot_status.read(),
            r.csts.read(),
            r.cc.read(),
            a.x.nvmmu().tcb_stat.read(),
            q.head,
            dev::read32(cqe),
            dev::read32(cqe.add(4)),
            dev::read32(cqe.add(8)),
            dev::read32(cqe.add(12)),
            a.dead,
            a.commands,
        )?;
        f.write_str(" crashlog=")?;
        a.x.cop.crashlog(f)
    }
}

/// Vult de TCB van `slot` in `table`. De richting van de DMA: oneven
/// opcodes schrijven (0x01), even lezen (0x02, identify 0x06); zonder
/// buffer geen richting (de Go-versie, op ijzer bewezen voor lezen). De
/// opcode zelf gaat erin zoals Linux het doet.
fn write_tcb(table: Pa, slot: u16, m: &Cmd) {
    let tcb = table.add(u64::from(slot) * TCB);
    dev::clear(tcb, TCB as usize);
    let at = |off: usize| tcb.add(off as u64);
    dev::write8(at(offset_of!(Tcb, opcode)), m.opc);
    if m.prp1 != 0 {
        let dir = if m.opc & 1 != 0 {
            TCB_TO_DEVICE
        } else {
            TCB_FROM_DEVICE
        };
        dev::write8(at(offset_of!(Tcb, dma_flags)), dir);
    }
    dev::write8(at(offset_of!(Tcb, command_id)), slot as u8);
    dev::write32(at(offset_of!(Tcb, length)), m.cdw12);
    dev::write64(at(offset_of!(Tcb, prp1)), m.prp1);
    dev::write64(at(offset_of!(Tcb, prp2)), m.prp2);
    dev::mb();
}

#[cfg(test)]
mod tests {
    //! Wat alleen de ANS doet, tegen de nep-controller van de core met de
    //! eigenaardigheden van de ANS erbij: hij wordt alleen ready na het
    //! wekbericht en met iBoots IOSQES = 7 in CC (les 1 en 2), voert de
    //! opdracht uit op het slot dat een lineaire deurbel aanwijst (les 3),
    //! toetst dat de CID en de TCB van dat slot bij de opdracht horen, en
    //! telt de invalidaties van de NVMMU. De
    //! schijf is RAM met een GPT-header op blok 1.

    use super::*;
    use crate::tests::{Ctl, Mem, machine, now, with};
    use crate::{ADM_IDENTIFY, DMA_NEED, IO_WRITE, SECTOR};
    use blkdev::{BlockIo, Spin, block_on};
    use std::cell::RefCell;
    use std::string::ToString;
    use std::vec;
    use std::vec::Vec;

    const NBLOCKS: u64 = 64;
    const USABLE: (u64, u64) = (6, NBLOCKS - 6);
    /// Wat iBoot in CC achterlaat (29-08 gemeten): SHN = 01, IOSQES = 7,
    /// IOCQES = 4.
    const IBOOT_CC: u32 = 0x0047_4000;
    const NOTHING: u32 = u32::MAX;

    /// De NVMMU van de test, naast de controller.
    #[derive(Default)]
    struct Mmu {
        base: Pa,
        tcb_mismatch: u32,
        invalidated: Vec<u32>,
        /// De TCB-richting per uitgevoerde opdracht.
        dirs: Vec<u8>,
    }

    thread_local! {
        static MMU: RefCell<Mmu> = RefCell::new(Mmu::default());
    }

    fn lo_hi(p: Pa) -> u64 {
        u64::from(dev::read32(p)) | (u64::from(dev::read32(p.add(4))) << 32)
    }

    /// De ANS-kant van een ronde: invalidaties tellen, en de lineaire
    /// deurbellen naar de controller, met de TCB van het slot getoetst.
    fn ans(c: &mut Ctl) {
        MMU.with(|m| {
            let mut m = m.borrow_mut();
            let inval = m.base.add(0x28118);
            if dev::read32(inval) != NOTHING {
                let v = dev::read32(inval);
                m.invalidated.push(v);
                dev::write32(inval, NOTHING);
            }
            for (qi, db, tcbs) in [(0, 0x2490c, 0x28108), (1, 0x24910, 0x28110)] {
                let slot = dev::read32(c.regs.add(db));
                if slot == NOTHING {
                    continue;
                }
                dev::write32(c.regs.add(db), NOTHING);
                let sq = if qi == 0 {
                    Pa(lo_hi(c.regs.add(0x28)))
                } else {
                    c.q[1].unwrap().sq
                };
                let sqe = sq.add(u64::from(slot) * 64);
                let tcb = Pa(lo_hi(m.base.add(tcbs))).add(u64::from(slot) * TCB);
                m.dirs.push(dev::read8(tcb.add(1)));
                let ok = u32::from(dev::read8(tcb.add(2))) == slot
                    && u32::from(dev::read16(sqe.add(2))) == slot
                    && dev::read8(tcb) == dev::read8(sqe)
                    && dev::read32(tcb.add(4)) == dev::read32(sqe.add(48))
                    && dev::read64(tcb.add(24)) == dev::read64(sqe.add(24));
                m.tcb_mismatch += u32::from(!ok);
                c.rung.push((qi, slot as u16));
            }
        });
    }

    /// De coprocessor van de test: het wekbericht zet BOOT_STATUS en maakt
    /// de controller bereid om ready te worden (les 1).
    struct Cop;

    impl Coprocessor for Cop {
        type Error = u32;
        fn boot(&mut self) -> core::result::Result<(), u32> {
            with(|c| {
                c.booted = true;
                c.events.push("boot");
                dev::write32(c.regs.add(0x1300), BOOT_STATUS_OK);
            });
            Ok(())
        }
        fn service(&mut self) -> core::result::Result<(), u32> {
            with(|c| {
                c.services += 1;
                if c.crashed { Err(7) } else { Ok(()) }
            })
        }
        fn sleep(&mut self) -> core::result::Result<(), u32> {
            with(|c| c.events.push("sleep"));
            Ok(())
        }
        fn crashlog(&self, out: &mut dyn fmt::Write) -> fmt::Result {
            out.write_str("none")
        }
    }

    struct Machine {
        _m: Mem,
        _mmu: Vec<u64>,
        cfg: Config,
    }

    fn machine_ans(secure_bar: bool, gpt: bool) -> Machine {
        let m = machine(MMIO_LEN, DMA_ALIGN);
        let mut mmu = vec![0u64; NVMMU_LEN as usize / 8 + 1];
        let base = Pa(mmu.as_mut_ptr() as usize as u64);
        dev::write32(m.base.add(0x14), IBOOT_CC);
        dev::write32(m.base.add(0x2490c), NOTHING);
        dev::write32(m.base.add(0x24910), NOTHING);
        dev::write32(base.add(0x28118), NOTHING);
        MMU.with(|x| {
            *x.borrow_mut() = Mmu {
                base,
                ..Mmu::default()
            }
        });
        with(|c| {
            c.lbads = 12;
            c.disk = vec![0u8; (NBLOCKS * BLOCK) as usize];
            if gpt {
                let h = &mut c.disk[BLOCK as usize..];
                h[..8].copy_from_slice(b"EFI PART");
                h[32..40].copy_from_slice(&(NBLOCKS - 1).to_le_bytes());
                h[40..48].copy_from_slice(&USABLE.0.to_le_bytes());
                h[48..56].copy_from_slice(&USABLE.1.to_le_bytes());
            }
            c.hook = Some(ans);
            c.ready_if = Some(|c, cc| c.booted && (cc >> 16) & 0xf == 7);
        });
        let cfg = Config {
            nvme: m.base,
            nvmmu: base,
            dma: m.dma,
            dma_size: DMA_NEED,
            secure_bar,
        };
        Machine {
            _m: m,
            _mmu: mmu,
            cfg,
        }
    }

    fn ans_of(m: &Machine) -> Ans<Cop> {
        // SAFETY: vensters en DMA liggen in `m`, dat de toets overleeft.
        unsafe { Ans::new(m.cfg, Cop, now) }.unwrap()
    }

    fn up(m: &Machine) -> Ans<Cop> {
        let mut a = ans_of(m);
        a.start().unwrap();
        a
    }

    /// Schrijft op schijfblok `block` over het I/O-pad en wacht erop.
    fn write_abs(a: &mut Ans<Cop>, block: u64, data: &[u8]) -> Result<(), u32> {
        let i = a.start_write(block, data, 0)?;
        a.wait_ticket(i, &mut [])
    }

    #[test]
    fn start_wakes_first_and_only_clears_shn_and_sets_en() {
        let m = machine_ans(false, true);
        let a = up(&m);
        // Les 2: iBoots entry-maten staan er nog; alleen SHN weg, EN aan.
        assert_eq!(dev::read32(m.cfg.nvme.add(0x14)), 0x0047_0001);
        // Les 1: het wekbericht kwam vóór alles, en de mailbox ging leeg.
        with(|c| {
            assert_eq!(c.events.first(), Some(&"boot"));
            assert!(c.services > 0);
            // De namespace is nooit gevraagd, alleen de controller.
            let ids = c.log.iter().filter(|e| e.q == 0 && e.opc == ADM_IDENTIFY);
            assert!(ids.map(|e| e.at).eq([1]));
        });
        assert_eq!((a.blocks(), a.block_size()), (NBLOCKS, BLOCK));
        assert_eq!(a.max_transfer(), crate::MAX_TRANSFER);
        // De lineaire modus en de NVMMU: de zijtabellen zijn de TCB's.
        assert_eq!(dev::read32(m.cfg.nvme.add(0x24908)) & 1, 1);
        assert_eq!(dev::read32(m.cfg.nvmmu.add(0x28100)), 63);
        assert_eq!(lo_hi(m.cfg.nvmmu.add(0x28108)), m.cfg.dma.0);
        assert_eq!(lo_hi(m.cfg.nvmmu.add(0x28110)), m.cfg.dma.0 + 0xc000);

        // Wie CC overschrijft met eigen maten (IOSQES 6) komt niet ready:
        // de nep-chip kent les 2.
        let m = machine_ans(false, true);
        dev::write32(m.cfg.nvme.add(0x14), 6 << 16);
        let r = ans_of(&m).start();
        assert!(matches!(r, Err(Error::NotReady { want: true, .. })));
        // En de DMA-regio staat op de 16 KiB van dit silicium.
        let mut cfg = m.cfg;
        cfg.dma = cfg.dma.add(0x1000);
        // SAFETY: wordt geweigerd vóór één toegang.
        let r = unsafe { Ans::new(cfg, Cop, now) };
        assert!(matches!(r, Err(Error::Dma { .. })));
    }

    #[test]
    fn secure_bar_writes_the_io_queue_bases_again() {
        let m = machine_ans(true, true);
        let _a = up(&m);
        let r = m.cfg.nvme;
        assert_eq!(lo_hi(r.add(0x1200)), m.cfg.dma.0 + 0x1_0000);
        assert_eq!(lo_hi(r.add(0x1208)), m.cfg.dma.0 + 0x1_4000);
        let m = machine_ans(false, true);
        let _a = up(&m);
        assert_eq!(lo_hi(m.cfg.nvme.add(0x1200)), 0);
    }

    #[test]
    fn each_command_sits_on_its_slot_with_its_tcb_and_is_invalidated() {
        let m = machine_ans(false, true);
        let mut a = up(&m);
        a.set_window(10, 5).unwrap();
        let mut buf = vec![0u8; 3 * BLOCK as usize];
        a.read_at(7, &mut buf).unwrap();
        write_abs(&mut a, 13, &vec![0x5a; 2 * BLOCK as usize]).unwrap();
        a.read_at(13, &mut buf[..BLOCK as usize]).unwrap();
        assert!(buf[..BLOCK as usize].iter().all(|&x| x == 0x5a));
        with(|c| {
            let io: Vec<_> = c
                .log
                .iter()
                .filter(|e| e.q == 1)
                .map(|e| (e.opc, e.at, e.nlb))
                .collect();
            assert_eq!(
                io[io.len() - 3..],
                [(IO_READ, 7, 3), (IO_WRITE, 13, 2), (IO_READ, 13, 1)]
            );
            assert!(c.log.iter().all(|e| e.slot == e.cid));
            assert_eq!(c.log.len() as u64, a.commands);
        });
        MMU.with(|x| {
            let x = x.borrow();
            assert_eq!(x.tcb_mismatch, 0);
            // Na elke completion is het slot ongeldig verklaard.
            assert_eq!(x.invalidated.len() as u64, a.commands);
            // Schrijven naar het device, lezen ervan.
            assert_eq!(x.dirs[x.dirs.len() - 2..], [TCB_TO_DEVICE, TCB_FROM_DEVICE]);
        });
    }

    #[test]
    fn read_anywhere_but_write_only_inside_the_window() {
        let m = machine_ans(false, true);
        let mut a = up(&m);
        // Lezen mag overal: de GPT op blok 1.
        let mut b = vec![0u8; BLOCK as usize];
        a.read_at(1, &mut b).unwrap();
        assert_eq!(&b[..8], b"EFI PART");
        // Zonder venster ziet het contract geen schijf, en elke schrijf
        // wordt luid geweigerd.
        assert_eq!(a.sectors(), 0);
        let q = blkdev::Queue::new(&mut a, Spin);
        let mut io = &q;
        assert!(matches!(
            block_on(io.read(0, &mut b)),
            Err(blkdev::Error::OutOfRange { .. })
        ));
        drop(q);
        let r = write_abs(&mut a, 20, &b);
        assert_eq!(
            r,
            Err(Error::NoWindow {
                block: 20,
                len: 4096
            })
        );
        assert!(r.unwrap_err().to_string().contains("WRITE REFUSED"));
        // Een venster moet binnen het bruikbare deel van de GPT vallen.
        for (first, n) in [(0, 4), (5, 2), (USABLE.1, 2), (10, 0), (u64::MAX, 2)] {
            let r = a.set_window(first, n);
            assert!(matches!(r, Err(Error::BadWindow { .. })), "{first}+{n}");
        }
        a.set_window(10, 5).unwrap();
        for (blk, len) in [(9, 1), (14, 2), (15, 1), (0, 1), (1, 1)] {
            let d = vec![0u8; len * BLOCK as usize];
            let r = write_abs(&mut a, blk, &d);
            assert!(matches!(r, Err(Error::OutsideWindow { .. })), "{blk}+{len}");
        }
        with(|c| assert!(c.disk[2 * BLOCK as usize..].iter().all(|&x| x == 0)));
        // Het contract over het venster: LBA 16 (512 bytes) is blok 2 van
        // het venster, schijfblok 12; voorbij het venster is buiten.
        assert_eq!(a.sectors(), 5 * BLOCK / SECTOR);
        let q = blkdev::Queue::new(&mut a, Spin);
        let mut io = &q;
        block_on(io.write(16, &vec![0x11; 2 * BLOCK as usize])).unwrap();
        let past = block_on(io.write(32, &vec![0; 2 * BLOCK as usize]));
        assert!(matches!(past, Err(blkdev::Error::OutOfRange { .. })));
        block_on(io.flush()).unwrap();
        with(|c| {
            let at = |k: u64| &c.disk[(k * BLOCK) as usize..((k + 1) * BLOCK) as usize];
            assert!(at(12).iter().chain(at(13)).all(|&x| x == 0x11));
            assert!(at(11).iter().chain(at(14)).all(|&x| x == 0));
            assert_eq!(c.events.last(), Some(&"flush"));
        });
    }

    #[test]
    fn no_gpt_keeps_the_disk_closed() {
        let m = machine_ans(false, false);
        let mut a = ans_of(&m);
        assert_eq!(a.start(), Err(Error::NoGpt));
        assert_eq!(a.blocks(), 0);
        let mut b = vec![0u8; BLOCK as usize];
        assert_eq!(a.read_at(1, &mut b), Err(Error::NotStarted));
    }

    #[test]
    fn shutdown_deletes_the_queues_then_shn_then_sleeps_also_when_dead() {
        let m = machine_ans(false, true);
        let mut a = up(&m);
        a.shutdown().unwrap();
        with(|c| {
            let admin: Vec<u8> = c.log.iter().filter(|e| e.q == 0).map(|e| e.opc).collect();
            assert_eq!(admin[admin.len() - 2..], [ADM_DELETE_SQ, ADM_DELETE_CQ]);
            assert_eq!(c.events.last(), Some(&"sleep"));
        });
        let cc = dev::read32(m.cfg.nvme.add(0x14));
        assert_eq!(cc, (IBOOT_CC & !CC_SHN_MASK) | CC_SHN_NORMAL);
        let mut b = vec![0u8; BLOCK as usize];
        assert_eq!(a.read_at(1, &mut b), Err(Error::NotStarted));

        // Een controller die zwijgt: de driver dood, geen queue-opdrachten
        // meer, maar CC.SHN en de slaap wel (les 4).
        let m = machine_ans(false, true);
        let mut a = up(&m);
        with(|c| c.mute = true);
        assert_eq!(a.read_at(1, &mut b), Err(Error::Timeout { opc: IO_READ }));
        assert_eq!(a.read_at(1, &mut b), Err(Error::Dead));
        let before = with(|c| c.log.len());
        a.shutdown().unwrap();
        with(|c| {
            assert_eq!(c.log.len(), before);
            assert_eq!(c.events.last(), Some(&"sleep"));
        });
        assert_eq!(dev::read32(m.cfg.nvme.add(0x14)) & CC_EN, 0);
    }

    #[test]
    fn a_crashed_coprocessor_is_fatal_and_diag_says_so() {
        let m = machine_ans(false, true);
        let mut a = up(&m);
        with(|c| c.crashed = true);
        let mut b = vec![0u8; BLOCK as usize];
        assert_eq!(a.read_at(1, &mut b), Err(Error::Transport(7)));
        assert_eq!(a.read_at(1, &mut b), Err(Error::Dead));
        let d = a.diag().to_string();
        assert!(
            d.contains("boot=0xde71ce55") && d.ends_with("crashlog=none"),
            "{d}"
        );
    }
}
