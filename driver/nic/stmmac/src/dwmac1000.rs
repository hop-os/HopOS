//! De 3.x-generatie (DWMAC1000/GMAC): de 100M-poort van de Sophgo SG2002
//! op de Sipeed LicheeRV Nano (RISC-V T-Head C906), via RMII aan de interne
//! ePHY.
//!
//! Geschreven naar de vendor U-Boot-driver (`designware.c`, bindt
//! "cvitek,ethernet") en de Linux stmmac-glue (`dwmac-cvitek.c`,
//! `dwmac1000.h`, `dwmac_dma.h`); Go: `metal/driver/nic/dwmac`. Hier zit de
//! MDIO op 0x10/0x14, het MAC-adres op 0x40, de DMA op 0x1000 zonder
//! kanalen of MTL, en de descriptors zijn het "normal format" van vier
//! woorden met een poll-demand-register in plaats van een tail-pointer.
//!
//! Read-only bevestigd op ijzer (probe 30-07): klokgates open, versie
//! 0x1037 op basis 0x04070000, en de ePHY antwoordt op MDIO-adres 0 met id
//! 0043:5649, precies het id dat de ePHY-init zelf in de PHY schrijft.
//!
//! CACHE-COHERENTIE, het echte verschil met de ARM-boards. Op de C906 in
//! M-mode is er geen MMU (de geheugenattributen komen uit de sysmap van de
//! core), dus de DMA-regio is gewoon cachebaar DRAM. Elke descriptor-
//! toegang loopt daarom door [`dev::push`] (clean, vóór de controller
//! leest) en [`dev::pull`] (clean en invalidate, vóór de CPU leest wat de
//! controller schreef); de buffers krijgen dat onderhoud in de kern, en na
//! de `pull` is een buffer gewoon geheugen: de kopie is `memcpy`. Tot 03-10
//! waren het vluchtige woorden van 8 bytes, 4,9 us per frame van 1514 bytes
//! tegen 1,4 us (gemeten op de C906B, gecachet).
//!
//! En daarom staat elke descriptor en elke buffer op een eigen cacheline
//! van [`LINE`] bytes: descriptors [`DESC_STRIDE`] uit elkaar via de
//! Descriptor Skip Length in DMA_BUS_MODE, buffers een veelvoud van een
//! line groot, en de regio op een line gealigneerd. Aaneengesloten
//! 16B-descriptors zoals de vendor ze gebruikt zetten er vier in één line,
//! en dan overschrijft onze write-back van descriptor i de updates die de
//! DMA net in i+1..i+3 deed; omgekeerd gooit een invalidate van de één een
//! CPU-schrijf naar de ander weg. Gemeten 30-07 (de ring die stilviel):
//! DHCP lukte nog (twee frames, ver uit elkaar), maar ping verloor de helft
//! en een TLS-handshake liep nooit af. U-Boot komt met DSL=0 weg omdat het
//! één frame per keer doet; een netstack niet.

mod regs;
#[cfg(test)]
mod tests;

use crate::{MMC_INT_ALL, Mii, Ops, Result, Rings, lo, set_mac_addr};
use core::fmt;
use dev::{LINE, Pa, Reg};
use netdev::Mac;
use regs::Regs;

pub use regs::CSR_250_300M;

/// Een draaiende DWMAC1000.
pub type Dwmac1000 = crate::Stmmac<Hw>;
/// Het blok vóór de ringen.
pub type Probe = crate::Probe<Hw>;
/// Het interrupt-pad.
pub type IrqAck = crate::IrqAck<Hw>;

// MAC_CONFIG-bits.
/// PS: MII/RMII (10/100) in plaats van GMII.
const CONF_PORT_MII: u32 = 1 << 15;
/// FES: 100 Mbit in plaats van 10.
const CONF_FES100: u32 = 1 << 14;
/// DO: eigen frames niet terugontvangen (half duplex).
const CONF_DIS_RX_OWN: u32 = 1 << 13;
/// DM: full duplex.
const CONF_DUPLEX: u32 = 1 << 11;
/// Transmitter aan.
const CONF_TX_EN: u32 = 1 << 3;
/// Receiver aan.
const CONF_RX_EN: u32 = 1 << 2;

// MAC_FRAME_FILTER-bits.
/// PR: promiscuous.
const FILTER_PR: u32 = 1 << 0;
/// PM: alle multicast doorlaten.
const FILTER_PM: u32 = 1 << 4;

// DMA_BUS_MODE-bits.
/// PBL = 8 beats in [13:8].
const BUS_PBL8: u32 = 8 << 8;
/// Fixed burst.
const BUS_FIXED_BURST: u32 = 1 << 16;
/// Het DSL-veld [6:2].
const BUS_DSL_SHIFT: u32 = 2;
/// DSL is vijf bits breed.
const BUS_DSL_MAX: u64 = 0x1F;

// DMA_OP_MODE-bits.
/// TX pas versturen als het frame compleet in de FIFO staat.
const OP_STORE_FORWARD: u32 = 1 << 21;
/// De TX-FIFO leegmaken.
const OP_FLUSH_TX_FIFO: u32 = 1 << 20;
/// De TX-DMA loopt.
const OP_TX_START: u32 = 1 << 13;
/// De RX-DMA loopt.
const OP_RX_START: u32 = 1 << 1;

// De interrupts (Linux `dwmac_dma.h`). De 3.x-indeling: NIE en NIS op bit
// 16, niet op 15 zoals de 4.10+ van `dwmac4`. Status is W1C.
/// DMA_INTR_ENA: normal interrupt summary enable.
const INTR_NIE: u32 = 1 << 16;
/// DMA_INTR_ENA: receive interrupt enable.
const INTR_RIE: u32 = 1 << 6;
/// DMA_INTR_ENA: transmit interrupt enable.
const INTR_TIE: u32 = 1 << 0;
/// DMA_STATUS: receive interrupt.
const STAT_RI: u32 = 1 << 6;
/// DMA_STATUS: transmit interrupt (een descriptor met IC is klaar).
const STAT_TI: u32 = 1 << 0;
/// DMA_STATUS: normal interrupt summary.
const STAT_NIS: u32 = 1 << 16;

/// GMAC_INT_MASK: RGMII, PCS-link, PCS-AN, PMT, timestamp en LPI dicht. Die
/// lopen buiten DMA_INTR_ENA om naar dezelfde lijn (GLI, GPI in
/// DMA_STATUS), en niemand hier wist ze. Linux (`dwmac1000_core_init`)
/// laat alleen open wat het afhandelt; wij handelen er geen af.
const GMAC_INT_ALL: u32 = 0x60F;

// Het descriptorformaat: "normal format", 16 bytes, géén ALTDESCRIPTOR (bit
// 7 van DMA_BUS_MODE blijft 0, net als bij de vendor).
/// RDES0/TDES0: status en OWN.
const DES_STATUS: u64 = 0;
/// RDES1/TDES1: controle en buffergrootte.
const DES_CNTL: u64 = 4;
/// RDES2/TDES2: het bufferadres.
const DES_BUF: u64 = 8;
/// RDES3/TDES3: buffer 2 of de volgende descriptor; ongebruikt in
/// ring-mode.
const DES_NEXT: u64 = 12;

/// OWN: 1 = van de DMA.
const DESC_OWN: u32 = 1 << 31;

/// De framelengte in RDES0 [29:16], inclusief de FCS.
const RX_LEN_SHIFT: u32 = 16;
/// Het lengteveld is veertien bits.
const RX_LEN_MASK: u32 = 0x3FFF;
/// ES: samengevatte fout.
const RX_STS_ERROR: u32 = 1 << 15;
/// FS: eerste descriptor van het frame.
const RX_STS_FIRST: u32 = 1 << 9;
/// LS: laatste descriptor van het frame.
const RX_STS_LAST: u32 = 1 << 8;

/// TDES1 IC: TI als deze descriptor klaar is (`TDES1_INTERRUPT`).
const TX_CNTL_IC: u32 = 1 << 31;
/// TDES1 LS.
const TX_CNTL_LAST: u32 = 1 << 30;
/// TDES1 FS.
const TX_CNTL_FIRST: u32 = 1 << 29;
/// RER/TER: de laatste descriptor van de ring; de DMA springt terug.
const RING_END: u32 = 1 << 25;
/// RBS1/TBS1: ELF bits, zie [`MAX_FRAME`].
const CNTL_SIZE1_MASK: u32 = 0x7FF;

/// Eén descriptor: vier woorden van 32 bits.
const DESC_SIZE: u64 = 16;

/// De afstand tussen twee descriptors: één cacheline. Geen optimalisatie
/// maar een correctheidseis op dit board, zie de module-doc.
pub const DESC_STRIDE: u64 = LINE;

/// DSL (Descriptor Skip Length): hoeveel 32-bit woorden de DMA tussen twee
/// descriptors overslaat. Afgeleid, niet met de hand: 48 bytes skip, met een
/// 16B-descriptor precies [`DESC_STRIDE`], dus 12.
const BUS_DSL: u32 = (((DESC_STRIDE - DESC_SIZE) / 4) as u32) << BUS_DSL_SHIFT;

/// Het DMA_BUS_MODE-woord na de reset.
const BUS_MODE: u32 = BUS_FIXED_BURST | BUS_PBL8 | BUS_DSL;

/// Ringdiepte, per ring: een tijdsbudget, niet een smaak.
///
/// Bij 100 Mbit duurt één frame van 1500 B 120 µs op de draad, dus
/// `NUM_DESC` frames zijn `NUM_DESC` × 120 µs buffering: hoe lang deze
/// driver niet aan de beurt hoeft te komen zonder dat de MAC frames
/// weggooit.
///
/// 64 gaf ~8 ms, en dat bleek 10-08 op ijzer te krap. GEMETEN met de
/// netmeter-bank: ophalen én verwerken van een frame kost samen 47 µs, dus
/// de lus is twee keer sneller dan de draad. Wat hem wegjoeg was de
/// Go-scheduler met een quantum van ~10 ms: 83 frames, meer dan 64. Gevolg:
/// 3 tot 41 verloren frames per 16 MB-download (missed-ring in [`Diag`]),
/// en de doorvoer zakte van 5,8 naar 4,2 MB/s.
///
/// 128 geeft ~15 ms en dekt een heel quantum; de hele set is dan
/// [`NEED_BYTES`] = 432 KB en past nog in de OS-staart van dit board.
pub const NUM_DESC: u16 = 128;

/// Eén buffer: 26 × 64 B, net boven [`MAX_FRAME`], en een veelvoud van een
/// cacheline zodat twee buffers nooit een line delen.
pub const BUF_SIZE: usize = 1664;

/// Wat we de MAC als buffergrootte MELDEN, en tegelijk het grootste frame
/// dat we versturen. Niet hetzelfde als [`BUF_SIZE`]: het RBS1-veld is 11
/// bits, dus 2048 past er níet in. Dat maskeert naar nul en dan denkt de
/// MAC dat elke buffer nul bytes groot is. Gemeten gevolg (30-07, eerste
/// DMA-boot): de ring gaf 128 descriptors terug zonder één bruikbaar frame.
/// De vendor programmeert hier 1600 (`designware.h`, `MAC_MAX_FRAME_SZ`)
/// terwijl zijn buffers óók 2048 zijn; dat is precies dit onderscheid.
pub const MAX_FRAME: usize = 1600;

/// De descriptors van beide ringen, elk [`DESC_STRIDE`]: de buffers
/// beginnen hier precies achter. Afgeleid, niet met de hand: met een
/// hardgecodeerde 0x1000 lag de TX-ring bovenop de eerste RX-buffers zodra
/// de ring dieper werd dan 32 (30-07).
const DESC_BYTES: u64 = 2 * NUM_DESC as u64 * DESC_STRIDE;

/// De DMA-regio die deze driver nodig heeft: 432 KB. Het board reserveert
/// dit in zijn plan, op een cacheline gealigneerd en onder 4 GB.
pub const NEED_BYTES: u64 = <Hw as Ops>::NEED_BYTES;

const _: () = {
    // Eén descriptor per line, en de DSL past in zijn vijf bits.
    assert!(DESC_STRIDE.is_multiple_of(LINE));
    assert!((DESC_STRIDE - DESC_SIZE) / 4 <= BUS_DSL_MAX);
    assert!(BUS_DSL == 12 << BUS_DSL_SHIFT);
    // Eén buffer per line-reeks, en de buffers beginnen op een line.
    assert!((BUF_SIZE as u64).is_multiple_of(LINE));
    assert!(DESC_BYTES.is_multiple_of(LINE));
    // RBS1 moet de maat dragen, de buffer het frame, en het frame moet
    // boven een volledig ethernetframe (1518) liggen.
    assert!(MAX_FRAME as u32 & !CNTL_SIZE1_MASK == 0);
    assert!(MAX_FRAME <= BUF_SIZE);
    assert!(MAX_FRAME >= 1518);
    assert!(netdev::MAX_FRAME <= MAX_FRAME);
    assert!(NEED_BYTES == 432 * 1024);
};

/// Het RDES1-woord van een RX-descriptor: de buffergrootte die we de MAC
/// melden, plus de ring-end-bit op de laatste. Het woord waar de eerste
/// DMA-boot op stukliep.
const fn rx_cntl(last: bool) -> u32 {
    let c = MAX_FRAME as u32;
    if last { c | RING_END } else { c }
}

/// Het TDES1-woord voor één frame van `len` bytes (`1..=MAX_FRAME`, dus het
/// past in TBS1): eerste én laatste descriptor (wij versturen nooit
/// gefragmenteerd), en de ring-end-bit op de laatste van de ring.
const fn tx_cntl(len: usize, last: bool) -> u32 {
    let c = TX_CNTL_IC | TX_CNTL_FIRST | TX_CNTL_LAST | len as u32;
    if last { c | RING_END } else { c }
}

/// Het MAC_CONFIG-woord voor een link: altijd MII/RMII (PS), DO, TX en RX
/// aan; FES op 100 en DM bij full duplex.
fn mac_conf(mbps: u32, full_duplex: bool) -> u32 {
    let mut conf = CONF_PORT_MII | CONF_DIS_RX_OWN | CONF_TX_EN | CONF_RX_EN;
    if mbps == 100 {
        conf |= CONF_FES100;
    }
    if full_duplex {
        conf |= CONF_DUPLEX;
    }
    conf
}

/// Wat de MAC weggooide zonder het ons aan te bieden, uit één lezing van
/// de Missed Frame and Buffer Overflow Counter: (geen vrije descriptor,
/// volle RX-FIFO, overlopen tellers).
fn missed(v: u32) -> (u64, u64, u64) {
    let ring = u64::from(v & 0xFFFF);
    let fifo = u64::from((v >> 17) & 0x7FF);
    let ovf = u64::from((v >> 16) & 1) + u64::from((v >> 28) & 1);
    (ring, fifo, ovf)
}

/// De ops-tabel van de DWMAC1000.
#[derive(Clone, Copy)]
pub struct Hw;

impl Ops for Hw {
    type Regs = Regs;

    const NUM_RX: u16 = NUM_DESC;
    const NUM_TX: u16 = NUM_DESC;
    const DESC_STRIDE: u64 = DESC_STRIDE;
    const BUF_SIZE: usize = BUF_SIZE;
    const BUF_OFF: u64 = DESC_BYTES;
    const MAX_FRAME: usize = MAX_FRAME;
    const RX_LIMIT: usize = MAX_FRAME;
    // designware.h; Linux stmmac: addr_shift 11, reg_shift 6, clk_csr_shift
    // 2, write = bit 1, lezen zonder opcode.
    const MII: Mii = Mii {
        addr_shift: 11,
        reg_shift: 6,
        csr_shift: 2,
        read: 0,
        write: 1 << 1,
    };
    const INTR: u32 = INTR_NIE | INTR_RIE | INTR_TIE;
    const STAT: u32 = STAT_RI | STAT_TI | STAT_NIS;

    fn version(r: &Regs) -> &Reg<u32> {
        &r.version
    }

    fn bus_mode(r: &Regs) -> &Reg<u32> {
        &r.bus_mode
    }

    fn mii(r: &Regs) -> (&Reg<u32>, &Reg<u32>) {
        (&r.gmii_addr, &r.gmii_data)
    }

    fn irq(r: &Regs) -> (&Reg<u32>, &Reg<u32>) {
        (&r.intr_ena, &r.status)
    }

    /// RMII: 10 of 100.
    fn speed_ok(mbps: u32) -> bool {
        matches!(mbps, 10 | 100)
    }

    /// De volgorde van de Go-driver (en van de vendor).
    fn program(r: &Regs, ring: &Rings<Self>, mac: Mac, mbps: u32, full_duplex: bool) -> Result {
        // Het MAC-adres vóór RX aan, anders draait de MAC even met een
        // 00:00:00:00:00:00-filter.
        set_mac_addr(&r.addr0_hi, &r.addr0_lo, mac);
        // Álle multicast (PM) + promiscuous (PR). PM: mDNS/matter leeft op
        // 224.0.0.251 en 33:33-groepen. PR: de slots zijn met hun éigen
        // MAC's (02:00:00:00:00:XX) L2-burgers op het LAN voor IPv6, en de
        // perfect-filter kent maar één adres. Op een geswitcht LAN bereikt
        // vreemde unicast onze poort niet; de netstack filtert.
        r.filter.write(FILTER_PM | FILTER_PR);

        r.bus_mode.write(BUS_MODE);
        r.rx_list.write(lo(ring.rx_desc));
        r.tx_list.write(lo(ring.tx_desc));
        r.op_mode.write(OP_STORE_FORWARD | OP_FLUSH_TX_FIFO);
        // Alles wat buiten DMA_INTR_ENA om de lijn kan zetten, dicht; de
        // RX-interrupt zelf gaat pas open met `set_irq`.
        r.int_mask.write(GMAC_INT_ALL);
        r.mmc_rx_mask.write(MMC_INT_ALL);
        r.mmc_tx_mask.write(MMC_INT_ALL);
        r.mmc_ipc_mask.write(MMC_INT_ALL);
        r.status.update(|v| v); // sticky bits van vóór de reset wissen (W1C)
        r.conf.write(mac_conf(mbps, full_duplex));
        r.op_mode.update(|v| v | OP_TX_START | OP_RX_START);
        dev::mb();
        Ok(())
    }

    /// De RX-poll-demand: de DMA leest de ring opnieuw.
    fn rx_doorbell(r: &Regs, _tail: Pa) {
        r.rx_poll.write(1);
    }

    fn tx_doorbell(r: &Regs, _tail: Pa) {
        r.tx_poll.write(1);
    }

    /// Ring-mode (geen chaining): de DMA loopt de descriptors met DSL-stap
    /// af en springt terug bij de descriptor met de ring-end-bit.
    fn rx_init(d: Pa, buf: Pa, last: bool) {
        dev::write32(d.add(DES_STATUS), DESC_OWN);
        dev::write32(d.add(DES_CNTL), rx_cntl(last));
        dev::write32(d.add(DES_BUF), lo(buf));
        dev::write32(d.add(DES_NEXT), 0);
    }

    /// RDES1 en RDES2 (buffergrootte, ring-end, adres) staan er nog van
    /// `rx_init`: in het normal format schrijft de DMA alleen RDES0 terug.
    /// Eigen line, dus deze clean raakt geen buurdescriptor.
    fn rx_give(d: Pa, _buf: Pa, _last: bool) {
        dev::write32(d.add(DES_STATUS), DESC_OWN);
        dev::push(d, DESC_SIZE as usize);
    }

    /// De line vers uit het geheugen: de DMA schreef hem buiten de cache
    /// om. De `pull` vóór de bufferlees in de kern is ook de barrière.
    fn rx_status(d: Pa) -> Option<u32> {
        dev::pull(d, DESC_SIZE as usize);
        let sts = dev::read32(d.add(DES_STATUS));
        (sts & DESC_OWN == 0).then_some(sts)
    }

    /// Een gesplitst frame kan niet: wat we als buffergrootte melden ligt
    /// boven de MTU.
    fn rx_whole(sts: u32) -> bool {
        sts & RX_STS_ERROR == 0 && sts & (RX_STS_FIRST | RX_STS_LAST) == RX_STS_FIRST | RX_STS_LAST
    }

    fn rx_raw_len(sts: u32) -> usize {
        ((sts >> RX_LEN_SHIFT) & RX_LEN_MASK) as usize
    }

    fn tx_init(d: Pa, buf: Pa, _last: bool) {
        dev::write32(d.add(DES_STATUS), 0);
        dev::write32(d.add(DES_CNTL), 0);
        dev::write32(d.add(DES_BUF), lo(buf));
        dev::write32(d.add(DES_NEXT), 0);
    }

    /// De DMA schrijft TDES0 buiten de cache om als hij klaar is.
    fn tx_free(d: Pa) -> bool {
        dev::pull(d, DESC_SIZE as usize);
        dev::read32(d.add(DES_STATUS)) & DESC_OWN == 0
    }

    /// TDES2 (het adres) staat er nog van `tx_init`. OWN als laatste; op
    /// een gecachete regio ziet de DMA hem pas na de push, en dan de hele
    /// line tegelijk.
    fn tx_give(d: Pa, _buf: Pa, len: usize, last: bool) {
        dev::write32(d.add(DES_CNTL), tx_cntl(len, last));
        dev::mb();
        dev::write32(d.add(DES_STATUS), DESC_OWN);
        dev::push(d, DESC_SIZE as usize);
    }
}

impl Dwmac1000 {
    /// Leest de Missed Frame and Buffer Overflow Counter en telt hem op bij
    /// de meetlat. Het register wist zichzelf bij lezen, dus élke lezing is
    /// "sinds de vorige" en niemand anders mag hem lezen; daarom `&mut`.
    /// Dit is het getal dat een RX-jacht nodig heeft: de RU-bit in
    /// DMA_STATUS is sticky en zegt alleen "ooit gebeurd".
    fn sample_missed(&mut self) {
        let (ring, fifo, ovf) = missed(self.regs().missed.read());
        self.stats.rx_missed_ring += ring;
        self.stats.rx_missed_fifo += fifo;
        self.stats.rx_missed_ovf += ovf;
    }

    /// Het meetinstrument voor een mislukte bring-up: één regel die zegt of
    /// de DMA liep, waar beide ringen staan en wat de MAC ervan vond. Het
    /// board drukt hem één keer na de start.
    pub fn diag(&mut self) -> Diag {
        let rx = self.ring.rx(self.rx_cur);
        let tx = self.ring.tx(self.tx_cur);
        dev::pull(rx, DESC_SIZE as usize);
        dev::pull(tx, DESC_SIZE as usize);
        self.sample_missed();
        let r = self.regs();
        Diag {
            dma_status: r.status.read(),
            op_mode: r.op_mode.read(),
            rx_cur: self.rx_cur,
            rx_rdes0: dev::read32(rx.add(DES_STATUS)),
            hw_rx: r.cur_rx_desc.read(),
            tx_cur: self.tx_cur,
            tx_tdes0: dev::read32(tx.add(DES_STATUS)),
            hw_tx: r.cur_tx_desc.read(),
            stats: self.stats,
        }
    }
}

/// De diagnose van [`Dwmac1000::diag`].
#[derive(Copy, Clone, Debug)]
pub struct Diag {
    dma_status: u32,
    op_mode: u32,
    rx_cur: u16,
    rx_rdes0: u32,
    hw_rx: u32,
    tx_cur: u16,
    tx_tdes0: u32,
    hw_tx: u32,
    stats: crate::Stats,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.stats;
        write!(
            f,
            "dma-status {:#010x} op-mode {:#010x} rx={}/{}(err)/{}(len) last-err {:#010x} tx={} \
             missed-ring={} missed-fifo={} ctr-ovf={} \
             rxdesc[{}] {:#010x} hw-rx {:#010x} txdesc[{}] {:#010x} hw-tx {:#010x}",
            self.dma_status,
            self.op_mode,
            s.rx_frames,
            s.rx_errors,
            s.rx_bad_len,
            s.rx_last_err,
            s.tx_frames,
            s.rx_missed_ring,
            s.rx_missed_fifo,
            s.rx_missed_ovf,
            self.rx_cur,
            self.rx_rdes0,
            self.hw_rx,
            self.tx_cur,
            self.tx_tdes0,
            self.hw_tx,
        )
    }
}
