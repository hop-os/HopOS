//! Het videocodec-blok van de Cix P1 aanzetten: een Arm China Linlon V8 met
//! vier cores (Go: `OLD/metal/board/o6n/hop/vpu.go` en `vpu_recovery.go`).
//! Alleen met de feature `media`.
//!
//! Dit module draagt precies wat geen driver weet: waar het blok zit en hoe
//! je het aan krijgt. Registers, firmware en protocol zijn board-vrij en
//! wonen in `media-mve`; board importeert media niet (handboek §7), de knoop
//! zit in de binary (`hopos::codec`).
//!
//! De adressen staan in de DSDT: device `_HID "CIXH3010"`, twee
//! Memory32Fixed-vensters (0x14230000 en 0x14240000, elk 64 KB), één
//! Extended Interrupt (SPI 326, INTID 358) en `_CCA = 0`. Dat laatste is geen
//! detail: de VPU is NIET cache-coherent, dus de arena moet ongecached zijn
//! (gemeten 22-09: met gecachte arena start de firmware maar antwoordt hij
//! nooit). [`scan_dsdt`] leest ze; de echte toets blijft HARDWARE_ID in de
//! driver.
//!
//! De volgorde op ijzer is van veilig naar riskant: eerst het SCMI-kanaal
//! naar de TF-A een versie vragen (puur lezen), dan de stroomdomeinen, dan
//! terugvragen of ze aan staan. Pas daarna leest iemand een VPU-register:
//! zonder die volgorde is de eerste lees een SError die geen handler
//! opvangt, en weet je alleen dát de node weg is.

use board::Board as _;
use board_uefi::map_device;
use cpu::println;
use dev::Pa;
use driver_scmi::{Channel, POWER_OFF, POWER_ON, proto};

/// Het RCSU-venster naast het blok (reset/strap), als de DSDT zwijgt.
pub const VPU_RCSU: u64 = 0x1423_0000;
/// Het codec-blok zelf, als de DSDT zwijgt.
pub const VPU_BASE: u64 = 0x1424_0000;
/// De maat van elk venster.
pub const VPU_SIZE: u64 = 0x1_0000;
/// SPI 326.
pub const VPU_IRQ: u32 = 358;

/// Wat de DSDT over de VPU zegt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VpuWindows {
    /// Het registerblok.
    pub base: u64,
    /// Het RCSU-venster.
    pub rcsu: u64,
    /// De interrupt (GIC-INTID), 0 als de DSDT hem niet noemt.
    pub intid: u32,
    /// `_CCA`: `Some(false)` is niet-coherent (de O6N), `None` onbekend.
    pub coherent: Option<bool>,
}

/// De `_HID` van de Linlon V8 in de Cix-DSDT.
const HID: &[u8] = b"CIXH3010";
/// Hoe ver na de `_HID` de resources van hetzelfde device hoogstens staan.
const SCOPE: usize = 1024;

/// Zoekt de VPU in de AML van de DSDT: de `_HID`-string, en daarachter,
/// vóór het volgende device, de Memory32Fixed-descriptors (0x86), de
/// Extended Interrupt (0x89) en `_CCA`. Geen AML-interpreter: de Cix-tabel
/// is statisch en dit patroon deterministisch te lezen (zoals de `_CPC` in
/// `fw::aml::cpc`). Het lagere venster is het RCSU, het hogere het blok.
#[must_use]
pub fn scan_dsdt(aml: &[u8]) -> Option<VpuWindows> {
    let at = aml.windows(HID.len()).position(|w| w == HID)?;
    let from = at + HID.len();
    let rest = aml.get(from..)?;
    // Het volgende device begint bij de volgende `_HID`.
    let end = rest
        .windows(4)
        .position(|w| w == b"_HID")
        .unwrap_or(rest.len())
        .min(SCOPE);
    let scope = rest.get(..end)?;
    let mut mem = [0u64; 2];
    let mut n = 0;
    let mut intid = 0;
    let mut coherent = None;
    let mut i = 0;
    while i < scope.len() {
        match scope.get(i..) {
            // Memory32Fixed: 86 09 00 rw base[4] len[4].
            Some([0x86, 0x09, 0x00, _, b0, b1, b2, b3, ..]) if n < 2 => {
                mem[n] = u64::from(u32::from_le_bytes([*b0, *b1, *b2, *b3]));
                n += 1;
                i += 12;
            }
            // Extended Interrupt: 89 len[2] flags count intid[4]...
            Some([0x89, _, _, _, c, i0, i1, i2, i3, ..]) if *c >= 1 && intid == 0 => {
                intid = u32::from_le_bytes([*i0, *i1, *i2, *i3]);
                i += 9;
            }
            // Name(_CCA, Zero|One): 08 5F 43 43 41 00|01, of een ByteConst.
            Some([0x08, b'_', b'C', b'C', b'A', v, rest @ ..]) => {
                coherent = match (*v, rest.first()) {
                    (0x00, _) => Some(false),
                    (0x01, _) => Some(true),
                    (0x0a, Some(b)) => Some(*b != 0),
                    _ => None,
                };
                i += 6;
            }
            _ => i += 1,
        }
    }
    if n < 2 {
        return None;
    }
    let (rcsu, base) = (mem[0].min(mem[1]), mem[0].max(mem[1]));
    Some(VpuWindows {
        base,
        rcsu,
        intid,
        coherent,
    })
}

// De stroomdomeinen (SKY1_PD_*). De VPU is de top plus vier cores, en hij
// hangt aan de multimedia-hub: staat die uit, dan komt er geen bus bij het
// blok en leest élk register nul, ook met stroom op het blok zelf.
const PD_MM_HUB: u32 = 4;
const PD_MM_HUB_SMMU: u32 = 5;
const PD_VPU_TOP: u32 = 11;
const PD_VPU_CORE0: u32 = 12;
const PD_VPU_CORES: u32 = 4;

/// Het DVFS-domein van de VPU: op niveau nul krijgt de core geen frequentie.
const PERF_VPU_DOMAIN: u32 = 9;
/// De APB-klok van het blok: stroom zonder klok leest ook overal nul.
const CLK_VPU_APB: u32 = 67;
/// De klok van de NI-700-interconnect waar de VPU aan hangt.
const CLK_MM_NI700: u32 = 80;

/// Het SCMI-kanaal van de SCP zelf (Linux: "ap2pm"): het draagt de klokken.
const PM_SCMI_SHMEM: u64 = 0x0659_0000;
/// Het SCMI-kanaal naar de TF-A: de bel is een SMC, en dit kanaal schakelt
/// de stroomdomeinen van GPU/VPU/NPU plus de interconnect-permissies.
const TFA_SCMI_SHMEM: u64 = 0x8438_0000;
const TFA_SCMI_SIZE: u64 = 0x1000;
const TFA_SMC_FUNC: u32 = 0xc200_0001;

/// De system reset controller: de terugval als het blok na de power-on van
/// de TF-A toch stil blijft. Alle bits actief-laag (1 = uit reset).
const SRC_BASE: u64 = 0x1600_0000;
const SRC_SIZE: u64 = 0x1000;
const SRC_RESETS: [(u64, u32); 4] = [
    (0x800, 1 << 31), // NI700_MMHUB_RCSU_RESET_N
    (0x804, 1 << 5),  // RCSU_SMMU_MMHUB_RESET_N
    (0x804, 1 << 19), // het RCSU
    (0x400, 1 << 11), // de VPU
];

/// ACPI `VPU0.PPRS._ON` zet deze bits op RCSU+0x21c. De logische ON van
/// SCMI alleen bewijst niet dat de gates open staan (gemeten 27-09: PGCTRL
/// ging van 0x07cef000 naar 0x07cefffc na de stroomcyclus).
const VPU_POWER_GATES: u32 = 0x1ffc;
/// PGCTRL in het RCSU.
const RCSU_PGCTRL: u64 = 0x21c;
/// TERMINATE van LSID 0 (0x200 + 0x18) en de stap per LSID.
const LSID_TERMINATE: u64 = 0x218;
const LSID_STRIDE: u64 = 0x40;

/// Moet de VPU door de stroomcyclus? Gesloten gates, of een sessie die nog
/// afbreekt van een vorige kern: het beeld van 27-09 ("session slot 0 will
/// not terminate"), dat alleen de cyclus oploste.
#[must_use]
pub fn needs_recovery(pgctrl: u32, terminate: [u32; 4]) -> bool {
    pgctrl & VPU_POWER_GATES != VPU_POWER_GATES || terminate.iter().any(|&t| t != 0)
}

/// Hoeveel stroomcycli de start hoogstens probeert. Op 27-09 was één genoeg;
/// op 04-10 (3.0.11) kwam het blok na één cyclus als UP door zonder ooit een
/// event te geven, en zat daarna slot 0 vast ("will not terminate").
pub const RECOVER_CYCLES: u32 = 3;
/// Rust na een cyclus vóór de registers opnieuw gelezen worden.
const RECOVER_SETTLE_NS: u64 = 10_000_000;

/// Herstelt tot `read` een gezond blok geeft: hoogstens [`RECOVER_CYCLES`]
/// keer `cycle`, met na elke cyclus een nieuwe blik. Geeft het aantal
/// cycli, of [`PowerError::Stuck`] met de laatste waarden. Zonder deze blik
/// meldde de kern de codec als UP terwijl het blok niet opkwam.
pub fn recover(
    mut read: impl FnMut() -> (u32, [u32; 4]),
    mut cycle: impl FnMut(u32, u32, [u32; 4]) -> Result<(), PowerError>,
) -> Result<u32, PowerError> {
    let (mut pgctrl, mut terminate) = read();
    let mut n = 0;
    while needs_recovery(pgctrl, terminate) {
        if n == RECOVER_CYCLES {
            return Err(PowerError::Stuck { pgctrl, terminate });
        }
        n += 1;
        cycle(n, pgctrl, terminate)?;
        (pgctrl, terminate) = read();
    }
    Ok(n)
}

/// De stroomcyclus: VPU-domeinen 15..11 uit, dan 11..15 aan. De gedeelde
/// hub (4 en 5) nooit: daar hangt meer aan. Stopt bij de eerste fout. Alleen
/// vóór de engine geregistreerd is, als deze kern geen sessies heeft.
pub fn cycle_domains<E>(mut power: impl FnMut(u32, u32) -> Result<(), E>) -> Result<(), (u32, E)> {
    for d in (PD_VPU_TOP..PD_VPU_CORE0 + PD_VPU_CORES).rev() {
        power(d, POWER_OFF).map_err(|e| (d, e))?;
    }
    for d in PD_VPU_TOP..PD_VPU_CORE0 + PD_VPU_CORES {
        power(d, POWER_ON).map_err(|e| (d, e))?;
    }
    Ok(())
}

/// De bel van het TF-A-kanaal: een SMC met het shmem-adres.
fn tfa_ring() {
    let _ = cpu::psci::smc(
        TFA_SMC_FUNC,
        TFA_SCMI_SHMEM >> 12,
        TFA_SCMI_SHMEM & 0xfff,
        0,
    );
}

fn tfa() -> Option<Channel> {
    if !map_device(TFA_SCMI_SHMEM, TFA_SCMI_SIZE) {
        return None;
    }
    // SAFETY: 0x84380000 is het SCMI-shmem naar de TF-A dat de Cix-firmware
    // zelf gebruikt (gemapt hierboven); de SMC luidt alleen de bel, en na
    // ExitBootServices schrijft niemand anders erin.
    Some(unsafe { Channel::with_ring(Pa(TFA_SCMI_SHMEM), cpu::idle::now, tfa_ring) })
}

/// Waarom het blok niet aan kwam.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerError {
    /// Geen Cix P1, of geen VPU in de DSDT.
    NotHere,
    /// De arena is te klein (minimaal 16 MB).
    Arena {
        /// De maat in MB.
        mb: u64,
    },
    /// Een venster of kanaal is niet te mappen.
    Map {
        /// Het adres.
        pa: u64,
    },
    /// De arena ongecached maken lukte niet.
    Uncached,
    /// Het SCMI-kanaal naar de TF-A antwoordt niet.
    Scmi,
    /// Een stroomdomein ging niet aan (of de cyclus faalde erop).
    Domain {
        /// Het domein.
        domain: u32,
    },
    /// Na [`RECOVER_CYCLES`] stroomcycli zit het blok nog vast.
    Stuck {
        /// PGCTRL na de laatste cyclus.
        pgctrl: u32,
        /// TERMINATE per LSID na de laatste cyclus.
        terminate: [u32; 4],
    },
}

impl core::fmt::Display for PowerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            PowerError::NotHere => f.write_str("vpu: no CIXH3010 in the DSDT of a Cix P1"),
            PowerError::Arena { mb } => write!(f, "vpu: arena of {mb} MB is too small"),
            PowerError::Map { pa } => write!(f, "vpu: cannot map {pa:#x}"),
            PowerError::Uncached => f.write_str("vpu: cannot make the arena uncached"),
            PowerError::Scmi => f.write_str("vpu: TF-A SCMI power protocol not answering"),
            PowerError::Domain { domain } => write!(f, "vpu: power domain {domain} not on"),
            PowerError::Stuck { pgctrl, terminate } => write!(
                f,
                "vpu: still stuck after {RECOVER_CYCLES} power cycles, pgctrl={pgctrl:#x} terminate={terminate:?}"
            ),
        }
    }
}

/// Brengt de VPU zo ver dat een driver ermee kan praten: vensters uit de
/// DSDT, arena ongecached, stroom, klok, perf-domein en reset, en de
/// stroomcyclus als het blok vastzit. Eén keer bij het opstarten.
pub fn power_vpu(arena: u64, size: u64) -> Result<VpuWindows, PowerError> {
    if !crate::is_cix() {
        return Err(PowerError::NotHere);
    }
    let w = match board_uefi::Uefi::new()
        .acpi_table(b"DSDT")
        .and_then(scan_dsdt)
    {
        Some(w) => w,
        None => {
            println!(
                "vpu: no CIXH3010 windows in the DSDT, using the SoC constants {VPU_BASE:#x}/{VPU_RCSU:#x}"
            );
            VpuWindows {
                base: VPU_BASE,
                rcsu: VPU_RCSU,
                intid: VPU_IRQ,
                coherent: Some(false),
            }
        }
    };
    if size < 16 << 20 {
        return Err(PowerError::Arena { mb: size >> 20 });
    }
    for pa in [w.base, w.rcsu] {
        if !map_device(pa, VPU_SIZE) {
            return Err(PowerError::Map { pa });
        }
    }
    // Via het board, niet `cpu::memattr`: de map van de stub overleeft een
    // flip, een tabel uit de heap niet (`board::Board::remap`).
    board_uefi::Uefi::new()
        .remap(arena, size, cpu::memattr::Attr::NormalNc)
        .map_err(|_| PowerError::Uncached)?;
    let mut ch = tfa().ok_or(PowerError::Map { pa: TFA_SCMI_SHMEM })?;
    power_on(&mut ch)?;
    if !map_device(PM_SCMI_SHMEM, 0x1000) {
        println!("vpu: cannot map the SCP SCMI channel");
    }
    clocks();
    perf();
    unreset()?;
    let read = || {
        let term: [u32; 4] = core::array::from_fn(|i| {
            dev::read32(Pa(w.base + LSID_TERMINATE + i as u64 * LSID_STRIDE))
        });
        (dev::read32(Pa(w.rcsu + RCSU_PGCTRL)), term)
    };
    let cycles = recover(read, |n, pgctrl, term| {
        println!(
            "vpu: incomplete power state pgctrl={pgctrl:#x} terminate={term:?}; cycling VPU domains ({n} of {RECOVER_CYCLES}) HOPOS_VPU_RECOVER"
        );
        cycle_domains(|d, s| ch.power_set(d, s))
            .map_err(|(domain, _)| PowerError::Domain { domain })?;
        clocks();
        perf();
        unreset()?;
        dev::delay(cpu::idle::now, RECOVER_SETTLE_NS);
        Ok(())
    })?;
    if cycles > 0 {
        let (pgctrl, term) = read();
        println!(
            "vpu: recovered after {cycles} power cycle(s), pgctrl={pgctrl:#x} terminate={term:?} HOPOS_VPU_RECOVERED"
        );
    }
    println!(
        "vpu: id {:#x} rcsu {:#x} (windows {:#x}/{:#x}, intid {}, cca {:?})",
        dev::read32(Pa(w.base)),
        dev::read32(Pa(w.rcsu)),
        w.base,
        w.rcsu,
        w.intid,
        w.coherent
    );
    Ok(w)
}

/// Topdomein, vier cores en de hub aan, en terugvragen. Alle vijf VPU-
/// domeinen: de scheduler telt cores met stroom, en een blok dat zich als
/// viercore meldt met drie uit loopt vast op zijn eerste job.
fn power_on(ch: &mut Channel) -> Result<(), PowerError> {
    let v = ch.version(proto::POWER).map_err(|_| PowerError::Scmi)?;
    println!(
        "vpu: TF-A SCMI channel alive, power protocol v{}.{}",
        v >> 16,
        v & 0xffff
    );
    let domains = [PD_MM_HUB, PD_MM_HUB_SMMU, PD_VPU_TOP]
        .into_iter()
        .chain(PD_VPU_CORE0..PD_VPU_CORE0 + PD_VPU_CORES);
    for d in domains {
        ch.power_set(d, POWER_ON)
            .map_err(|_| PowerError::Domain { domain: d })?;
        if !matches!(ch.power_state(d), Ok(POWER_ON)) {
            return Err(PowerError::Domain { domain: d });
        }
    }
    println!("vpu: power domains 4 5 11-15 on (confirmed by the firmware)");
    Ok(())
}

/// Heeft dit kanaal het clock-protocol?
fn offers_clock(ch: &mut Channel) -> bool {
    let mut list = [0u8; 16];
    ch.protocols(&mut list)
        .is_ok_and(|n| list.iter().take(n).any(|&p| p == proto::CLOCK))
}

/// De klokken van blok en interconnect aan, op het eerste kanaal dat het
/// clock-protocol draagt (gemeten: alleen ap2pm). Een andere indeling geeft
/// een regel, geen stil blok.
fn clocks() {
    let mbox = crate::thermal::SCMI_CHANNEL;
    for (name, base) in [("SCP mailbox", mbox), ("SCP ap2pm", PM_SCMI_SHMEM)] {
        // SAFETY: twee SCMI-shmem-kanalen van de Cix-firmware (de mailbox van
        // de AML en ap2pm), gemapt als Device; de beurt is synchroon en de
        // thermometer gebruikt de mailbox alleen tussen twee beurten.
        let mut ch = unsafe { Channel::new(Pa(base), cpu::idle::now) };
        if !offers_clock(&mut ch) {
            continue;
        }
        for (id, what) in [(CLK_MM_NI700, "mm ni700"), (CLK_VPU_APB, "vpu apb")] {
            match ch.clock_enable(id, true) {
                Ok(()) => println!(
                    "vpu: {what} clock on via the {name} channel, {} MHz",
                    ch.clock_rate(id).unwrap_or(0) / 1_000_000
                ),
                Err(e) => println!("vpu: {what} clock ({id}) on the {name} channel: {e}"),
            }
        }
        return;
    }
    println!(
        "vpu: no SCMI channel offers the clock protocol - clocks left as the firmware set them"
    );
}

/// Het perf-domein van de VPU uit stand nul. Zonder niveaulijst is een
/// bescheiden waarde de veiligste zet; de firmware klemt.
fn perf() {
    // SAFETY: de mailbox van de AML; zie `clocks`.
    let mut ch = unsafe { Channel::new(Pa(crate::thermal::SCMI_CHANNEL), cpu::idle::now) };
    match ch.perf_level(PERF_VPU_DOMAIN) {
        Ok(0) => {
            let _ = ch.set_perf_level(PERF_VPU_DOMAIN, 1);
            println!(
                "vpu: perf domain {PERF_VPU_DOMAIN} raised to level {}",
                ch.perf_level(PERF_VPU_DOMAIN).unwrap_or(0)
            );
        }
        Ok(l) => println!("vpu: perf domain {PERF_VPU_DOMAIN} is at level {l}"),
        Err(e) => println!("vpu: perf domain {PERF_VPU_DOMAIN}: {e}"),
    }
}

/// Blok, RCSU, hub en SMMU uit reset; idempotent.
fn unreset() -> Result<(), PowerError> {
    if !map_device(SRC_BASE, SRC_SIZE) {
        return Err(PowerError::Map { pa: SRC_BASE });
    }
    for (off, bit) in SRC_RESETS {
        let at = Pa(SRC_BASE + off);
        dev::write32(at, dev::read32(at) | bit);
    }
    dev::mb();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registerwaarden van de O6N vóór en na het herstel (27-09).
    #[test]
    fn vpu_recovery_conditions() {
        assert!(
            needs_recovery(0x07ce_f000, [0; 4]),
            "koude boot met dichte gates"
        );
        assert!(
            !needs_recovery(0x07ce_fffc, [0; 4]),
            "gezonde VPU zou resetten"
        );
        for i in 0..4 {
            let mut t = [0; 4];
            t[i] = 1;
            assert!(needs_recovery(0x07ce_fffc, t), "vast slot {i} gemist");
        }
    }

    /// Eén cyclus die helpt, een blok dat pas na de tweede opkomt, en een
    /// blok dat blijft hangen: dat laatste is een fout, geen UP.
    #[test]
    fn vpu_recover_checks_after_every_cycle() {
        let healthy = (0x07ce_fffc, [0; 4]);
        let stuck = (0x07ce_f000, [0; 4]);
        assert_eq!(recover(|| healthy, |_, _, _| unreachable!()), Ok(0));
        for after in 1..=RECOVER_CYCLES {
            // Lezen en cycleren delen de teller: een Cell, geen twee leningen.
            let cycles = core::cell::Cell::new(0);
            let r = recover(
                || {
                    if cycles.get() >= after {
                        healthy
                    } else {
                        stuck
                    }
                },
                |_, _, _| {
                    cycles.set(cycles.get() + 1);
                    Ok(())
                },
            );
            assert_eq!(r, Ok(after), "gezond na {after} cycli");
        }
        let mut cycles = 0;
        let r = recover(
            || (0x07ce_fffc, [1, 0, 0, 0]),
            |_, _, _| {
                cycles += 1;
                Ok(())
            },
        );
        assert_eq!(
            r,
            Err(PowerError::Stuck {
                pgctrl: 0x07ce_fffc,
                terminate: [1, 0, 0, 0]
            })
        );
        assert_eq!(cycles, RECOVER_CYCLES);
        let r = recover(|| stuck, |_, _, _| Err(PowerError::Domain { domain: 13 }));
        assert_eq!(r, Err(PowerError::Domain { domain: 13 }));
    }

    #[test]
    fn vpu_power_cycle() {
        let mut calls = Vec::new();
        cycle_domains::<()>(|d, s| {
            calls.push((d, s));
            Ok(())
        })
        .unwrap();
        let want = [
            (15, POWER_OFF),
            (14, POWER_OFF),
            (13, POWER_OFF),
            (12, POWER_OFF),
            (11, POWER_OFF),
            (11, POWER_ON),
            (12, POWER_ON),
            (13, POWER_ON),
            (14, POWER_ON),
            (15, POWER_ON),
        ];
        assert_eq!(calls, want, "onveilig domein of volgorde");
    }

    #[test]
    fn vpu_power_cycle_stops_on_failure() {
        for fail_at in 0..10 {
            let mut calls = 0;
            let r = cycle_domains(|_, _| {
                calls += 1;
                if calls == fail_at + 1 {
                    Err("SCMI failed")
                } else {
                    Ok(())
                }
            });
            assert!(matches!(r, Err((_, "SCMI failed"))), "fout {fail_at}");
            assert_eq!(calls, fail_at + 1);
        }
    }

    /// Een stuk AML zoals de Cix-DSDT het draagt: Device(VPU0) met _HID,
    /// _CCA en een _CRS-buffer; daarna het volgende device.
    fn aml() -> Vec<u8> {
        let mut b = b"\x5b\x82\x40\x08VPU0\x08_HID\x0dCIXH3010\x00".to_vec();
        b.extend_from_slice(b"\x08_CCA\x00");
        b.extend_from_slice(b"\x08_CRS\x11\x30\x0a\x2d");
        for base in [0x1424_0000u32, 0x1423_0000] {
            b.extend_from_slice(&[0x86, 0x09, 0x00, 0x01]);
            b.extend_from_slice(&base.to_le_bytes());
            b.extend_from_slice(&0x1_0000u32.to_le_bytes());
        }
        b.extend_from_slice(&[0x89, 0x06, 0x00, 0x0d, 0x01]);
        b.extend_from_slice(&358u32.to_le_bytes());
        b.extend_from_slice(&[0x79, 0x00]);
        // Het volgende device: zijn venster hoort niet bij de VPU.
        b.extend_from_slice(b"\x5b\x82\x20\x08NPU0\x08_HID\x0dCIXH4000\x00");
        b.extend_from_slice(&[0x86, 0x09, 0x00, 0x01, 0, 0, 0, 0x20, 0, 0, 1, 0]);
        b
    }

    #[test]
    fn the_dsdt_gives_the_windows_the_interrupt_and_cca() {
        let w = scan_dsdt(&aml()).unwrap();
        assert_eq!(
            w,
            VpuWindows {
                base: VPU_BASE,
                rcsu: VPU_RCSU,
                intid: VPU_IRQ,
                coherent: Some(false),
            }
        );
        assert_eq!(scan_dsdt(b"no vpu here"), None);
        // Eén venster is geen VPU: de driver krijgt geen half adres.
        let mut half = b"\x08_HID\x0dCIXH3010\x00".to_vec();
        half.extend_from_slice(&[0x86, 0x09, 0x00, 0x01, 0, 0, 0x24, 0x14, 0, 0, 1, 0]);
        assert_eq!(scan_dsdt(&half), None);
    }
}
