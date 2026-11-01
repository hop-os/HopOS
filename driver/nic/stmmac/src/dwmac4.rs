//! De 4.x/5.x-generatie (DWMAC4/EQOS): op de Radxa Zero 3E het GMAC1-blok
//! van de RK3566, dat zich meldt als VERSION 0x3051 (snpsver 5.10; gemeten
//! 05-08, de DTS noemt hem "snps,dwmac-4.20a").
//!
//! Hier zit de MDIO op 0x200/0x204, het MAC-adres op 0x300, de DMA per
//! kanaal op 0x1100 + n × 0x80 met een MTL-laag ertussen, en de descriptors
//! hebben een lees- en een schrijfvorm met een tail-pointer in plaats van
//! een poll-demand-register.
//!
//! REFERENTIE (opgehaald 05-08, nagerekend): Linux v6.13
//! `drivers/net/ethernet/stmicro/stmmac`: `dwmac4.h` en `dwmac4_dma.h`
//! (registers), `dwmac4_descs.h` (descriptorbits), `dwmac4_core.c`
//! (MDIO-velden, `GMAC_CORE_INIT`, de snelheidsbits), `dwmac4_dma.c` en
//! `dwmac4_lib.c` (de init-volgorde, MTL, ringlengte en tail-pointer).
//!
//! CACHE-COHERENTIE: de DMA-regio ligt buiten de kern-RAM. De descriptors
//! liggen ongecachet (Normal-NC): ze staan gewoon 16 bytes uit elkaar,
//! zonder onderhoud. Normal-NC is wel zwakker geordend dan Device: de CPU
//! mag loads herordenen, vandaar de `mb` tussen de OWN-lees en de
//! bufferlees (Linux: `dma_rmb()` op dezelfde plek).
//!
//! De framebuffers liggen in een eigen blok van 2 MB ([`BUF_OFF`]) dat het
//! board Normal-WB mapt; de kern veegt (`pull` vóór elke RX-lees, `push` na
//! elke TX-schrijf, één `pull` over alle buffers bij de start: de net-wb
//! van de O6N en de Altra, en Linux' `dma_sync_single_for_cpu`/
//! `_for_device`), en de kopieën zijn `memcpy` (de kern). Een lees
//! uit NC gaat elke keer naar het DRAM, en de in-order A55 wacht per woord
//! op die rondreis. GEMETEN 03-10 op de Radxa (een app die 1514 bytes
//! kopieert, 816 MHz): uit NC met vluchtige woorden van 8 bytes (zoals
//! `dev::copy_out`) 35,3 µs per frame, uit NC met `memcpy` 16,8 µs (wat Go
//! deed), uit WB na `dc civac` 1,9 µs. Over de draad met de O6N (X1, bench):
//! 32 MB/s in en 85 uit, waarvan de kopie bij 32 MB/s al 76% van de OS-core
//! was; Go haalde 56 in en 99 uit met `memmove` uit NC.

mod regs;
#[cfg(test)]
mod tests;

use crate::{Error, MMC_INT_ALL, Mii, Ops, Result, Rings, lo, set_mac_addr};
use core::fmt;
use dev::{Pa, Reg};
use netdev::Mac;
use regs::Regs;

pub use regs::CSR_100_150M;

/// Een draaiende DWMAC4.
pub type Dwmac4 = crate::Stmmac<Hw>;
/// Het blok vóór de ringen.
pub type Probe = crate::Probe<Hw>;
/// Het interrupt-pad.
pub type IrqAck = crate::IrqAck<Hw>;

// GMAC_CONFIG-bits (dwmac4.h).
/// Jabber disable.
const CFG_JD: u32 = 1 << 17;
/// Jumbo enable: bewust UIT, zie [`CORE_INIT`].
const CFG_JE: u32 = 1 << 16;
/// Port select: MII (10/100) in plaats van GMII (1000).
const CFG_PS: u32 = 1 << 15;
/// Fast ethernet speed: 100 in plaats van 10.
const CFG_FES: u32 = 1 << 14;
/// Duplex mode: full.
const CFG_DM: u32 = 1 << 13;
/// Carrier sense negeren tijdens TX (half duplex).
const CFG_DCRS: u32 = 1 << 9;
/// Packet burst enable (half duplex).
const CFG_BE: u32 = 1 << 18;
/// Automatic pad/CRC strip: bewust UIT, de kern trekt de FCS zelf af.
#[cfg(test)]
const CFG_ACS: u32 = 1 << 20;
/// Transmitter aan.
const CFG_TE: u32 = 1 << 1;
/// Receiver aan.
const CFG_RE: u32 = 1 << 0;

/// `GMAC_CORE_INIT` uit dwmac4.h is JD|PS|BE|DCRS|JE. Wij laten JE (jumbo)
/// eruit, en dat is een BEWUSTE afwijking: onze RX-buffers zijn
/// [`BUF_SIZE`] groot, en met jumbo aan zou de MAC frames tot 9018 bytes
/// aannemen die over meerdere descriptors binnenkomen. `receive` eist FD|LD
/// in één descriptor, dus zou zo'n frame een stille verliespost zijn in
/// plaats van een afgekeurd frame. PS hoort bij de snelheid en wordt in
/// [`mac_config`] gezet.
const CORE_INIT: u32 = CFG_JD | CFG_BE | CFG_DCRS;
/// De snelheidsbits (dwmac4_core.c `mac->link.*`): 10 = PS, 100 = PS|FES,
/// 1000 = geen van beide.
const SPEED_MASK: u32 = CFG_PS | CFG_FES;

/// GMAC_RXQ_CTRL0: RX-queue 0 aan in DCB-modus
/// (`GMAC_RX_DCB_QUEUE_ENABLE(0)`); zonder dit komt er geen frame uit de
/// MTL.
const RXQ0_DCB_ENABLE: u32 = 1 << 1;

// MTL (dwmac4.h): één queue, dus alleen kanaal 0.
/// TX store-and-forward.
const MTL_TSF: u32 = 1 << 1;
/// TXQEN (niet-AVB).
const MTL_TXQ_EN: u32 = 1 << 3;
/// RX store-and-forward.
const MTL_RSF: u32 = 1 << 5;
/// TQS in [24:16]: de TX-queue-maat in eenheden van 256 B, min één.
const MTL_TQS_SHIFT: u32 = 16;
/// RQS in [29:20].
const MTL_RQS_SHIFT: u32 = 20;

// DMA_SYS_BUS_MODE is óók het AXI-configregister. De waarden komen ÉÉN OP
// ÉÉN uit de DTS van dit silicium (rk356x-base.dtsi,
// gmac1_stmmac_axi_setup): snps,mixed-burst = MB; snps,blen = <0 0 0 0 16 8
// 4> = BLEN16|BLEN8|BLEN4; snps,rd_osr_lmt = <8>; snps,wr_osr_lmt = <4>.
// Wat er NIET staat is even belangrijk: geen snps,fixed-burst en geen
// snps,aal. FB zetten zou hier schadelijk zijn: "mixed burst has no effect
// when fb is set", dus een welgemeende extra bit zou de instelling die de
// vendor wél vraagt uitschakelen.
const SYS_BUS_MB: u32 = 1 << 14;
const SYS_BUS_BLEN16: u32 = 1 << 3;
const SYS_BUS_BLEN8: u32 = 1 << 2;
const SYS_BUS_BLEN4: u32 = 1 << 1;
const SYS_BUS_RD_OSR: u32 = 8 << 16;
const SYS_BUS_WR_OSR: u32 = 4 << 24;
/// De OSR-velden: na reset niet noodzakelijk nul, en er blind bij OR-en
/// geeft een groter getal dan de vendor toestaat.
const SYS_BUS_OSR_MASK: u32 = (0xF << 16) | (0xF << 24);
const SYS_BUS_MODE: u32 =
    SYS_BUS_MB | SYS_BUS_BLEN16 | SYS_BUS_BLEN8 | SYS_BUS_BLEN4 | SYS_BUS_RD_OSR | SYS_BUS_WR_OSR;

/// DMA_CHAN_CONTROL: PBL maal 8.
const CHAN_PBL_X8: u32 = 1 << 16;
/// TX: operate on second packet.
const CHAN_OSP: u32 = 1 << 4;
/// ST (TX) of SR (RX): zelfde bit, ander register.
const CHAN_START: u32 = 1 << 0;
/// De burstlengte; met PBLx8 effectief 64 beats.
const PBL: u32 = 8;
const TX_PBL_SHIFT: u32 = 16;
const RX_PBL_SHIFT: u32 = 16;
/// RBSZ is het veld [14:1]: de maat staat er maal twee in.
const RX_RBSZ_SHIFT: u32 = 1;
const RX_RBSZ_MASK: u32 = 0x7FFE;

// De interrupts van kanaal 0. Twee bit-indelingen bestaan er voor het
// enable-register en dát is de valkuil (dwmac4_dma.h): tot core 4.00 is NIE
// bit 16, vanaf 4.10 bit 15 (`DMA_CHAN_INTR_ENA_NIE_4_10`). Dit silicium is
// een 4.20a en dus de 4.10-indeling. GEMETEN 20-09 op de Radxa: met bit 16
// las het register 0x40 terug (alleen RIE bleef staan), stond de lijn nooit
// hoog en claimde de node 0 interrupts per seconde terwijl er 200 MB
// binnenkwam. Status is W1C met dezelfde nummering in beide indelingen.
/// Normal interrupt summary enable (4.10+).
const INTR_NIE: u32 = 1 << 15;
/// Receive interrupt enable.
const INTR_RIE: u32 = 1 << 6;
/// Transmit interrupt enable (`DMA_CHAN_INTR_ENA_TIE`).
const INTR_TIE: u32 = 1 << 0;
/// Status: receive interrupt.
const STAT_RI: u32 = 1 << 6;
/// Status: transmit interrupt (een descriptor met IOC is klaar).
const STAT_TI: u32 = 1 << 0;
/// Status: normal interrupt summary.
const STAT_NIS: u32 = 1 << 15;

/// Eén descriptor: vier woorden van 32 bits; ook de stap in de ring.
const DESC_SIZE: u64 = 16;
/// Het vierde woord: OWN en het pakket (TDES3) of de status (RDES3).
const DES3: u64 = 12;

// TX (dwmac4_descs.h): TDES0/1 = bufferadres, TDES2 = bufferlengte en IOC,
// TDES3 = pakket.
const TX_BUF_LEN_MASK: u32 = 0x3FFF;
/// TDES2: TI als deze descriptor klaar is (`TDES2_INTERRUPT_ON_COMPLETION`).
const TX_IOC: u32 = 1 << 31;
const TX_PKT_LEN_MASK: u32 = 0x7FFF;
const TX_LAST: u32 = 1 << 28;
const TX_FIRST: u32 = 1 << 29;
const TX_OWN: u32 = 1 << 31;

// RX: RDES0/1 = bufferadres, RDES3 = eigendom en status of lengte.
const RX_PKT_LEN_MASK: u32 = 0x7FFF;
const RX_ERR_SUMMARY: u32 = 1 << 15;
const RX_LAST: u32 = 1 << 28;
const RX_FIRST: u32 = 1 << 29;
/// Bij teruggeven: buffer 1 is geldig.
const RX_BUF1_VALID: u32 = 1 << 24;
/// Zonder dit bit vult de DMA de descriptor af zonder RI te zetten: de lijn
/// blijft dan stil terwijl het verkeer doorloopt (Radxa 20-09: 200 MB
/// binnen, 0 claims, status alleen ERI). Linux zet hem bij élke teruggave
/// (`dwmac4_set_rx_owner`).
const RX_IOC: u32 = 1 << 30;
const RX_OWN: u32 = 1 << 31;

/// RX-descriptors. Een gepolde ontvanger (300 µs) heeft de ring als buffer
/// tussen twee pomprondes, en 64 × 1,5 KB is 0,8 ms op 1 Gbit. De igb ging
/// van 5 naar 36 MB/s met 64 naar 256 (19-09); de Radxa zat op 15,9 MB/s
/// inbound tegen 118 op de borden met een grote ring (20-09).
pub const NUM_RX: u16 = 256;
/// TX-descriptors.
pub const NUM_TX: u16 = 64;
/// Eén buffer: een veelvoud van een cacheline, ruim boven de 1518 van een
/// MTU-1500-frame.
pub const BUF_SIZE: usize = 1536;
/// Het grootste frame dat wij versturen; de TX-lengtevelden zijn hier ruim
/// (14/15 bits), dus dit is een MTU-grens en geen veldgrens.
pub const MAX_FRAME: usize = 1518;

/// Waar de framebuffers beginnen: het tweede blok van 2 MB van de
/// DMA-regio, zodat het board ze Normal-WB mapt terwijl de descriptors
/// ervoor NC blijven (zoals `BUF_OFF` van de rtl8126 en de igb).
pub const BUF_OFF: u64 = 2 << 20;
/// De maat van dat blok.
pub const BUF_BLOCK: u64 = 2 << 20;

/// De DMA-regio die deze driver nodig heeft; het board reserveert dit in
/// zijn plan: de descriptors onderin, de 320 buffers (480 KB) vanaf
/// [`BUF_OFF`]. 2,5 MB van de 8 MB net-DMA van de RK3566.
pub const NEED_BYTES: u64 = <Hw as Ops>::NEED_BYTES;

const _: () = {
    assert!(BUF_SIZE >= MAX_FRAME + crate::FCS_LEN);
    // RBSZ [14:1] moet de maat dragen: de vorige generatie kreeg een
    // buffergrootte die door een te smal veld naar nul werd geveegd (30-07).
    assert!(((BUF_SIZE as u32) << RX_RBSZ_SHIFT) & !RX_RBSZ_MASK == 0);
    assert!(NUM_RX.is_power_of_two() && NUM_TX.is_power_of_two());
    // De descriptors vóór het bufferblok, de buffers erin, en elke buffer op
    // eigen cachelijnen: een veeg van de ene raakt de andere niet.
    assert!((NUM_RX as u64 + NUM_TX as u64) * DESC_SIZE <= BUF_OFF);
    assert!(NEED_BYTES - BUF_OFF <= BUF_BLOCK);
    assert!((BUF_SIZE as u64).is_multiple_of(dev::LINE));
};

/// Het GMAC_CONFIG-woord: de bestaande inhoud met de snelheids-, duplex- en
/// jumbobits gewist, dan de core-init-bits en de snelheid erin. Idempotent.
fn mac_config(cur: u32, mbps: u32, full_duplex: bool) -> u32 {
    let mut cfg = (cur & !(SPEED_MASK | CFG_DM | CFG_JE)) | CORE_INIT;
    match mbps {
        10 => cfg |= CFG_PS,
        100 => cfg |= CFG_PS | CFG_FES,
        _ => {} // 1000: PS en FES uit, GMII in plaats van MII.
    }
    if full_duplex {
        cfg |= CFG_DM;
    }
    cfg
}

/// Het RX_CONTROL-woord: RBSZ gewist, dan de burstlengte en de
/// buffergrootte erin (de maat past: een const-assertie bij [`BUF_SIZE`]).
const fn rx_control(cur: u32) -> u32 {
    (cur & !RX_RBSZ_MASK) | (PBL << RX_PBL_SHIFT) | ((BUF_SIZE as u32) << RX_RBSZ_SHIFT)
}

/// TDES2 en TDES3 voor één frame van `len` bytes (`1..=MAX_FRAME`): de
/// bufferlengte in [13:0] met IOC, en het pakket (eerste én laatste descriptor,
/// lengte in [14:0], OWN). Twee velden, dezelfde lengte: wie er één
/// vergeet, krijgt een MAC die de verkeerde hoeveelheid bytes verstuurt.
const fn tx_desc23(len: usize) -> (u32, u32) {
    let l = len as u32;
    (
        TX_IOC | (l & TX_BUF_LEN_MASK),
        TX_OWN | TX_FIRST | TX_LAST | (l & TX_PKT_LEN_MASK),
    )
}

/// De FIFO-maten uit HW_FEATURE1 (TXFIFOSIZE [10:6], RXFIFOSIZE [4:0], elk
/// 128 << n). Uit de hardware, niet uit een constante: TQS en RQS moeten
/// erop kloppen, en een verkeerde TQS is een MAC die frames in de FIFO laat
/// staan.
fn fifo_sizes(hw_feature1: u32) -> (u32, u32) {
    let tx = (hw_feature1 >> 6) & 0x1F;
    let rx = hw_feature1 & 0x1F;
    (
        128u32.checked_shl(tx).unwrap_or(0),
        128u32.checked_shl(rx).unwrap_or(0),
    )
}

/// De hoogste 32 bits van een DMA-adres.
fn hi(pa: Pa) -> u32 {
    (pa.0 >> 32) as u32
}

/// De ops-tabel van de DWMAC4.
#[derive(Clone, Copy)]
pub struct Hw;

impl Ops for Hw {
    type Regs = Regs;

    const NUM_RX: u16 = NUM_RX;
    const NUM_TX: u16 = NUM_TX;
    const DESC_STRIDE: u64 = DESC_SIZE;
    const BUF_SIZE: usize = BUF_SIZE;
    const BUF_OFF: u64 = BUF_OFF;
    const MAX_FRAME: usize = MAX_FRAME;
    const RX_LIMIT: usize = BUF_SIZE;
    // dwmac4_core.c `dwmac4_setup`: addr_shift 21, reg_shift 16,
    // clk_csr_shift 8; stmmac_mdio.c: `MII_GMAC4_READ` = 3 << 2,
    // `MII_GMAC4_WRITE` = 1 << 2.
    const MII: Mii = Mii {
        addr_shift: 21,
        reg_shift: 16,
        csr_shift: 8,
        read: 3 << 2,
        write: 1 << 2,
    };
    const INTR: u32 = INTR_NIE | INTR_RIE | INTR_TIE;
    const STAT: u32 = STAT_RI | STAT_TI | STAT_NIS;

    fn version(r: &Regs) -> &Reg<u32> {
        &r.version
    }

    fn bus_mode(r: &Regs) -> &Reg<u32> {
        &r.dma_bus_mode
    }

    fn mii(r: &Regs) -> (&Reg<u32>, &Reg<u32>) {
        (&r.mdio_addr, &r.mdio_data)
    }

    fn irq(r: &Regs) -> (&Reg<u32>, &Reg<u32>) {
        (&r.chan.intr_ena, &r.chan.status)
    }

    /// RGMII: 10, 100 of 1000.
    const GIGABIT: bool = true;

    /// De volgorde van Linux (`stmmac_hw_setup`).
    fn program(r: &Regs, ring: &Rings<Self>, mac: Mac) -> Result {
        let c = &r.chan;

        // 1. De AXI-kant, met de waarden uit de DTS van dit silicium.
        r.dma_sys_bus_mode
            .update(|v| (v & !SYS_BUS_OSR_MASK) | SYS_BUS_MODE);

        // 2. Kanaal 0: PBL, interrupts dicht (het board zet ze open), de
        //    ring-adressen en de lengtes: het AANTAL MIN ÉÉN.
        c.control.update(|v| v | CHAN_PBL_X8);
        c.intr_ena.write(0);
        c.tx_control
            .update(|v| v | (PBL << TX_PBL_SHIFT) | CHAN_OSP);
        c.tx_base_hi.write(hi(ring.tx_desc));
        c.tx_base.write(lo(ring.tx_desc));
        c.tx_ring_len.write(u32::from(NUM_TX) - 1);
        c.rx_control.update(rx_control);
        c.rx_base_hi.write(hi(ring.rx_desc));
        c.rx_base.write(lo(ring.rx_desc));
        c.rx_ring_len.write(u32::from(NUM_RX) - 1);

        // Tail-pointers: er is geen poll-demand in deze generatie, de DMA
        // werkt tot de tail en stopt. RX staat vol, dus de tail is één
        // voorbij het einde; TX is leeg, dus de tail is de basis.
        c.rx_end
            .write(lo(ring.rx(0).add(u64::from(NUM_RX) * DESC_SIZE)));
        c.tx_end.write(lo(ring.tx_desc));

        // 3. MTL: store-and-forward beide kanten, de queue-maten uit de
        //    hardware (TQS/RQS = fifo/256 - 1). Een nul-register betekent
        //    een blok zonder klok, en dan loopt fifo/256-1 om.
        let f1 = r.hw_feature1.read();
        let (tx, rx) = fifo_sizes(f1);
        if tx < 256 || rx < 256 {
            return Err(Error::Fifo {
                tx,
                rx,
                hw_feature1: f1,
            });
        }
        r.mtl_tx_op_mode
            .write(MTL_TSF | MTL_TXQ_EN | ((tx / 256 - 1) << MTL_TQS_SHIFT));
        r.mtl_rx_op_mode
            .write(MTL_RSF | ((rx / 256 - 1) << MTL_RQS_SHIFT));

        // 4. Het MAC-adres in de perfect-filter vóór RX aan gaat; filter 0
        //    is perfect match plus broadcast, geen promiscuous.
        set_mac_addr(&r.addr0_hi, &r.addr0_lo, mac);
        r.packet_filter.write(0);
        r.rxq_ctrl0.write(RXQ0_DCB_ENABLE);

        // 5. De MAC-config (core-init, snelheid en duplex) volgt bij de link
        //    (`set_link`), zoals `stmmac_mac_link_up`.

        // De MMC-interrupts dicht (Linux `stmmac_mmc_setup`). GEMETEN 03-10
        // op de Radxa: toen de RX-octetteller over 0x8000_0000 ging, stond
        // MMCRXIS in GMAC_INT_STATUS en MACIS in DMA_STATUS, claimde de
        // node 500.000 NIC-interrupts per seconde en kreeg Hop op de
        // OS-core geen beurt meer, tot de watchdog. GMAC_INT_EN blijft
        // zoals de reset hem laat: nul, wij handelen geen MAC-interrupt af.
        r.mmc_rx_mask.write(MMC_INT_ALL);
        r.mmc_tx_mask.write(MMC_INT_ALL);
        r.mmc_ipc_mask.write(MMC_INT_ALL);

        // 6. Lopen: sticky bits van vóór de reset weg, dan de DMA; de MAC
        //    bij de link.
        r.dma_status.update(|v| v);
        c.status.update(|v| v);
        c.tx_control.update(|v| v | CHAN_START);
        c.rx_control.update(|v| v | CHAN_START);
        dev::mb();
        Ok(())
    }

    /// Core-init plus snelheid en duplex, en zender en ontvanger aan.
    fn set_link(r: &Regs, mbps: u32, full_duplex: bool) {
        r.config
            .update(|v| mac_config(v, mbps, full_duplex) | CFG_TE | CFG_RE);
        dev::mb();
    }

    /// De RX-tail: de DMA vult tot hier.
    fn rx_doorbell(r: &Regs, tail: Pa) {
        r.chan.rx_end.write(lo(tail));
    }

    fn tx_doorbell(r: &Regs, tail: Pa) {
        r.chan.tx_end.write(lo(tail));
    }

    /// De leesvorm: bufferadres, geen tweede buffer, en OWN|BUF1V|IOC als
    /// laatste. De DMA schrijft bij afronding ALLE VIER de woorden met
    /// status vol: het bufferadres is dan weg en moet er bij elke teruggave
    /// opnieuw in.
    fn rx_give(d: Pa, buf: Pa, _last: bool) {
        dev::write32(d, lo(buf));
        dev::write32(d.add(4), hi(buf));
        dev::write32(d.add(8), 0);
        dev::mb();
        dev::write32(d.add(DES3), RX_OWN | RX_BUF1_VALID | RX_IOC);
    }

    /// OWN is vrij: pas nú de rest lezen. Onder Normal-NC mag de CPU de
    /// bufferloads vóór de OWN-load uitvoeren (een control dependency
    /// ordent load naar store, niet load naar load).
    fn rx_status(d: Pa) -> Option<u32> {
        let sts = dev::read32(d.add(DES3));
        if sts & RX_OWN != 0 {
            return None;
        }
        dev::mb();
        Some(sts)
    }

    fn rx_whole(sts: u32) -> bool {
        sts & RX_ERR_SUMMARY == 0 && sts & (RX_FIRST | RX_LAST) == RX_FIRST | RX_LAST
    }

    fn rx_raw_len(sts: u32) -> usize {
        (sts & RX_PKT_LEN_MASK) as usize
    }

    fn tx_init(d: Pa, _buf: Pa, _last: bool) {
        dev::clear(d, DESC_SIZE as usize);
    }

    /// Van de DMA als hij hem nog niet verzond, of de tail nog niet zag.
    fn tx_free(d: Pa) -> bool {
        dev::mb();
        dev::read32(d.add(DES3)) & TX_OWN == 0
    }

    /// De afgeronde descriptor draagt status in alle vier de woorden, dus
    /// het bufferadres moet er per frame opnieuw in. Dit gaat pas fout ná
    /// één ronde door de ring: het soort bug dat een korte test overleeft
    /// en een download niet. OWN als laatste.
    fn tx_give(d: Pa, buf: Pa, len: usize, _last: bool) {
        let (des2, des3) = tx_desc23(len);
        dev::write32(d, lo(buf));
        dev::write32(d.add(4), hi(buf));
        dev::write32(d.add(8), des2);
        dev::mb();
        dev::write32(d.add(DES3), des3);
    }
}

impl Dwmac4 {
    /// Eén regel voor een mislukte bring-up: liep de DMA, waar staan beide
    /// ringen, en wat vond de MAC ervan.
    #[must_use]
    pub fn diag(&self) -> Diag {
        let r = self.regs();
        let c = &r.chan;
        dev::mb();
        Diag {
            chan_status: c.status.read(),
            dma_status: r.dma_status.read(),
            dma_debug: r.dma_debug0.read(),
            mtl_tx: r.mtl_tx_debug.read(),
            mtl_rx: r.mtl_rx_debug.read(),
            mac_cfg: r.config.read(),
            rx_cur: self.rx_cur,
            rx_rdes3: dev::read32(self.ring.rx(self.rx_cur).add(DES3)),
            hw_rx: c.cur_rx_desc.read(),
            tx_cur: self.tx_cur,
            tx_tdes3: dev::read32(self.ring.tx(self.tx_cur).add(DES3)),
            hw_tx: c.cur_tx_desc.read(),
            stats: self.stats,
        }
    }
}

/// De diagnose van [`Dwmac4::diag`].
#[derive(Copy, Clone, Debug)]
pub struct Diag {
    chan_status: u32,
    dma_status: u32,
    dma_debug: u32,
    mtl_tx: u32,
    mtl_rx: u32,
    mac_cfg: u32,
    rx_cur: u16,
    rx_rdes3: u32,
    hw_rx: u32,
    tx_cur: u16,
    tx_tdes3: u32,
    hw_tx: u32,
    stats: crate::Stats,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "chan-status {:#010x} dma-status {:#010x} dbg {:#010x} mtl tx {:#010x} rx {:#010x} \
             mac-cfg {:#010x} rx={}/{}(err) last-err {:#010x} tx={} \
             rxdesc[{}] {:#010x} hw-rx {:#010x} txdesc[{}] {:#010x} hw-tx {:#010x}",
            self.chan_status,
            self.dma_status,
            self.dma_debug,
            self.mtl_tx,
            self.mtl_rx,
            self.mac_cfg,
            self.stats.rx_frames,
            self.stats.rx_errors,
            self.stats.rx_last_err,
            self.stats.tx_frames,
            self.rx_cur,
            self.rx_rdes3,
            self.hw_rx,
            self.tx_cur,
            self.tx_tdes3,
            self.hw_tx,
        )
    }
}
