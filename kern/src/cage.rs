//! De naad tussen de kern en de architectuur: wat de lifecycle van het ijzer
//! nodig heeft, als traits.
//!
//! Dit is de Rust-vorm van `cage.go` (de kooi-naad) en `board.Cores`. De
//! lifecycle rekent met logische cores en slots; hoe een kooi gebouwd wordt
//! (stage-2 plus VMID op ARM, PMP plus Sv39 op RISC-V), hoe een core start
//! (mailbox plus SEV, PSCI, reset) en hoe een slot hard gestopt wordt
//! (stage-2 intrekken, reset) is van `cpu` en het board. De kern bezit
//! niets hiervan; hij krijgt een `&mut impl Cage` van zijn eigenaar.
//!
//! De tabel uit `docs/technical/isolation.md` blijft leidend:
//!
//! ```text
//!                        ARM                            RISC-V
//! het niveau van HOP     EL2                            machine mode
//! het niveau van de app  EL1                            supervisor mode
//! wat de app begrenst    stage-2-tabel + VMID           PMP-whitelist
//! hoe een core start     mailbox + SEV; koud: PSCI      reset of boot-pending
//! hoe HOP een slot stopt stage-2 intrekken, parkeert    reset, of de kill-tick
//! ```

use crate::cageheap::CageMem;
use crate::slots::ServicerCtl;
use crate::{Core, Region, Slot};
use alloc::rc::Rc;
use core::future::Future;
use core::time::Duration;

/// De klasse van een core, zoals een jobspec hem vraagt.
///
/// Een board met één soort core zegt overal [`CoreClass::Mid`] of `None`;
/// een big.LITTLE-board zegt per core wat hij is. Hop-de-bewoner vraagt
/// geen klasse ([`crate::pool::Placement::hop`]): op de OS-core kiest de
/// bootparameter `hopos.oscore` de soort (PORT.md beslissing 2), en op een
/// app-core is `hopos.hop.core-class` een voorkeur, geen eis.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CoreClass {
    /// Zuinig (A55, E-core).
    Small,
    /// Midden (A78, of een board zonder klassen).
    Mid,
    /// Snel (X-core, P-core).
    Big,
}

/// De toestand van een core volgens het silicium of de switcher.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Power {
    /// Uit, of geparkeerd in de lus van de switcher: vrij om te starten.
    Off,
    /// Aan en aan het werk.
    On,
    /// Onderweg (PSCI `ON_PENDING`).
    Pending,
}

/// De cores van het board: nummering, klasse, aan/uit/affiniteit.
///
/// Logische nummers zijn `1..=app_cores()`; alles wat de kern over een
/// core vraagt, gaat hierlangs. `cpu` en het board vullen hem in (PSCI,
/// Apple's IPI, een hart-lijst op RISC-V).
pub trait Cores {
    /// Het aantal logische app-cores (de hoogste logische core).
    fn app_cores(&self) -> usize;
    /// De klasse van `core`; `None` = het board kent geen klassen.
    fn class(&self, core: Core) -> Option<CoreClass>;
    /// De toestand van `core`.
    fn power(&self, core: Core) -> Power;
    /// Wek `core` uit een WFE/WFI (SEV of IPI).
    fn kick(&mut self, core: Core);
}

/// Waarom de kooi iets weigerde; de getallen gaan mee in
/// [`crate::Error::Cage`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CageError {
    /// Een code die de architectuur zelf documenteert.
    pub code: u32,
}

/// Waarom een poort van een jobspec niet doorgezet werd.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PortError {
    /// De poort staat al voor een ander slot open.
    Taken {
        /// De poort.
        port: u16,
        /// Het slot dat hem heeft.
        owner: usize,
    },
    /// De switch nam hem niet aan (vol, of er is geen switch).
    Refused {
        /// De poort.
        port: u16,
    },
}

/// Wat een kooi over een slot meldt: de control-page en het fault-rapport.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Status {
    /// Draait de primaire core van het slot?
    pub core_on: bool,
    /// De app-status (`layout.Status*`).
    pub app: u64,
    /// De exit-code.
    pub exit_code: u64,
    /// De heartbeat-teller.
    pub heartbeat: u64,
    /// De door de app gemelde RAM-maat.
    pub ram_size: u64,
    /// Het geheugen dat de app zelf in gebruik meldt (`CTRL_MEM_SYS`).
    pub mem_sys: u64,
    /// De meetlat van docs/apps.md, rauw: de idle-tijd van de app in
    /// nanoseconden (alle cores bij elkaar, `CTRL_IDLE` omgerekend), het
    /// aantal wekken (`CTRL_WAKES`), zijn cores (`CTRL_CORES`) en de klok
    /// van de kern bij het lezen. Wie twee standen vergelijkt heeft idle-
    /// en wek-tempo; de kern zelf bewaart geen vorige stand.
    pub idle_ns: u64,
    /// Het aantal wekken van de slaper van de app.
    pub wakes: u64,
    /// De cores van de app, zoals hij ze zelf telt.
    pub cores: u64,
    /// De klok van de kern (ns) bij het lezen van deze stand.
    pub at_ns: u64,
    /// De vectorindex van een fault (0 = geen).
    pub fault_vec: u64,
    /// Het syndroom van die fault.
    pub fault_esr: u64,
    /// Het fault-adres.
    pub fault_far: u64,
    /// De vectorindex plus 1 van een exception die de app op EL1 zelf ving
    /// (`CTRL_APP_FAULT_VEC`, de vectortabel van applib; 0 = geen). Het
    /// rapport hierboven is van EL2 en ziet alleen wat naar EL2 trapt.
    pub app_fault_vec: u64,
    /// ESR_EL1 van die exception.
    pub app_fault_esr: u64,
    /// ELR_EL1: de PC waar de app viel.
    pub app_fault_elr: u64,
    /// FAR_EL1: het adres dat hij raakte.
    pub app_fault_far: u64,
}

/// De klasse van een ESR (ARM ARM D24.2.40, het EC-veld en voor een abort
/// de fault-status), als korte Engelse naam voor een fault-regel: zodat de
/// console van ijzer in één regel zegt wat er gebeurde, niet alleen een
/// getal. Les van 30-09 (de eerste Pi 5-boot): `esr=0x82000005` moest met
/// de hand gedecodeerd worden tot "instruction abort, translation fault
/// level 1".
#[must_use]
pub const fn esr_class(esr: u64) -> &'static str {
    let ec = (esr >> 26) & 0x3f;
    let fsc = esr & 0x3f;
    match ec {
        0x00 => "unknown or undefined instruction",
        0x01 => "trapped WFI/WFE",
        0x07 => "trapped FP/SIMD",
        0x0e => "illegal execution state",
        0x15 => "SVC",
        0x16 => "HVC",
        0x17 => "SMC",
        0x18 => "trapped system register",
        0x20 | 0x21 => match fsc {
            0x04..=0x07 => "instruction abort, translation fault",
            0x09..=0x0b => "instruction abort, access flag fault",
            0x0d..=0x0f => "instruction abort, permission fault",
            _ => "instruction abort",
        },
        0x22 => "PC alignment fault",
        0x24 | 0x25 => match fsc {
            0x21 => "data abort, alignment fault",
            0x04..=0x07 => "data abort, translation fault",
            0x09..=0x0b => "data abort, access flag fault",
            0x0d..=0x0f => "data abort, permission fault",
            0x10 => "data abort, synchronous external abort",
            _ => "data abort",
        },
        0x26 => "SP alignment fault",
        0x2f => "SError",
        0x3c => "BRK",
        _ => "other exception",
    }
}

/// De kooi: bouwen, dispatchen, intrekken, wekken, en de waarheid over de
/// context van een slot.
///
/// Eén implementatie per architectuur, in `cpu`. De kern roept hem alleen
/// vanuit de lifecycle-actor aan, dus `&mut self` is de eigendomsregel: er
/// is precies één aanroeper.
pub trait Cage {
    /// Zet een verse chunk van het kooi-geheugen op het attribuut dat de
    /// app-cores met de MMU uit zien (`on`: Device in de map van de kern), of
    /// een lege terug op dat van de pool (`kern::cageheap`). Een functie en
    /// geen methode: de lifecycle roept hem midden in een call van de kooi.
    /// `false` = het lukte niet. Standaard niets: een board dat de pool al
    /// Device mapt, of de toetsen.
    const MAP_CAGE: fn(Region, bool) -> bool = |_, _| true;
    /// Wis `len` bytes op `base` en publiceer de writes (één brok van de
    /// scrub; de lifecycle yieldt ertussen).
    fn clear(&mut self, base: u64, len: u64);
    /// Bouw de complete context en de beschermingsgrens van `slot` over
    /// `part`, vóór publicatie (E5). `cores` is de vertrouwde SMP-breedte,
    /// `first` de eerste fysieke core van de span. Wat de kooi buiten de
    /// partitie nodig heeft (zijn kooi-blok, de VMID, groei van een
    /// bewonerslijst), neemt hij hier, uit `mem`; past het niet, dan is het
    /// een weigering en nam hij niets.
    fn build(
        &mut self,
        slot: Slot,
        part: Region,
        entry: u64,
        first: Core,
        cores: usize,
        mem: &mut dyn CageMem,
    ) -> Result<(), CageError>;
    /// Geeft wat [`Cage::build`] voor `slot` nam terug (het kooi-blok en de
    /// VMID): na de bevestigde stop, of na een start die nooit draaide. Een
    /// slot zonder bouw: niets.
    fn release(&mut self, slot: Slot, mem: &mut dyn CageMem) {
        let _ = (slot, mem);
    }
    /// Wat de kooi van `slot` de volgende kern meegeeft (de flip): de PA van
    /// zijn kooi-blok en zijn VMID (op riscv de ASID).
    fn carry(&self, slot: Slot) -> (u64, u32) {
        let _ = slot;
        (0, 0)
    }
    /// Geef de levensduur van `slot` zijn servicer-taak
    /// ([`crate::slots::servicer_task`] met `ctl`), na de bouw en vóór het
    /// startschot. Een `Err` (de taak kan er niet bij) is een weigering: er
    /// draait nog niets. Zonder executor (de toetsen) geen taak.
    fn serve(&mut self, slot: Slot, ctl: Rc<ServicerCtl>) -> Result<(), CageError> {
        let _ = (slot, ctl);
        Ok(())
    }
    /// Het startschot van de primaire context op `core`. Een `Err` is een
    /// ONBEKENDE uitkomst: de core kan alsnog aangaan (de lifecycle maakt er
    /// `fatal` van).
    fn dispatch(&mut self, slot: Slot, core: Core) -> Result<(), CageError>;
    /// Het startschot van een secundaire SMP-context op `core`.
    fn dispatch_secondary(&mut self, slot: Slot, core: Core) -> Result<(), CageError>;
    /// Vraag de app coöperatief te stoppen: de kill-vlag op de control-page,
    /// met `grace` erin als de termijn die hij krijgt vóór de intrekking.
    fn request_exit(&mut self, slot: Slot, grace: Duration);
    /// Doet de context van `slot` op `core` niets meer (dood, leeg, of de
    /// core staat stil)?
    fn quiet(&self, slot: Slot, core: Core) -> bool;
    /// Leeft de context van `slot` (ctx-staat live of saved)?
    fn live(&self, slot: Slot) -> bool;
    /// De hard-kill: trek de vertaling van `slot` in, voor al zijn cores.
    fn revoke(&mut self, slot: Slot);
    /// Wacht `slot` op een gedeelde app-core nog op zijn eerste beurt
    /// (boot-pending)? Zonder sharegroups, of met een rotatie die zelf een
    /// tijdschijf heeft, nooit lang: dan de standaard.
    fn pending(&self, slot: Slot) -> bool {
        let _ = slot;
        false
    }
    /// Het slot dat app-core `core` nu draait (`SCHED_CURRENT`, share.go
    /// `coreHog`), of `None` als de core niemand draait of het niet zegt.
    fn holder(&self, core: Core) -> Option<Slot> {
        let _ = core;
        None
    }
    /// Het onbeantwoorde SMP-verzoek van `slot` (0 = geen). De waarde komt
    /// van een app-schrijfbare page en wordt nooit vertrouwd.
    fn smp_request(&self, slot: Slot) -> u64;
    /// Beantwoord het SMP-verzoek (zet het op 0).
    fn clear_smp_request(&mut self, slot: Slot);
    /// De status van `slot` voor HOP.
    fn status(&self, slot: Slot) -> Status;
    /// Zet elke poort in `ports` van de uplink door naar dezelfde poort in
    /// `slot` (DNAT in de switch, tcp en udp, zoals Go's `armSlot`), en
    /// wacht op de bevestiging: een poort die al van een ander slot is, laat
    /// de start falen in plaats van een app die niemand bereikt. Bij een
    /// fout staat er niets meer van `slot` open (alles of niets).
    ///
    /// Het slot-LAN hoort bij de kooi: wie de ringen aan de switch hangt
    /// ([`Cage::build`]), zet ook de deuren open. Zonder netwerk (de tests,
    /// een board zonder NIC) is er niets door te zetten.
    fn publish(
        &mut self,
        slot: Slot,
        ports: &[u16],
    ) -> impl Future<Output = Result<(), PortError>> {
        let _ = (slot, ports);
        core::future::ready(Ok(()))
    }
    /// Trekt elke publicatie van `slot` in, en de flows die erbij horen.
    /// Bij elke stop en na een mislukte start; zonder publicaties een no-op.
    fn unpublish(&mut self, slot: Slot) {
        let _ = slot;
    }
    /// Haalt de frame-ringen van `slot` van de switch (die [`Cage::build`]
    /// eraan hing) en wacht tot de switch ze losliet: daarna raakt de switch
    /// de staart van de partitie niet meer aan. De lifecycle roept dit bij
    /// elke stop nadat de app stil is (tijdens zijn gratie houdt hij zijn
    /// net), en na een start die na de bouw toch niet doorging, dus altijd
    /// vóór de partitie terug kan. Zonder netwerk is er niets los te halen.
    fn detach(&mut self, slot: Slot) -> impl Future<Output = ()> {
        let _ = slot;
        core::future::ready(())
    }
}

/// De console van de kern: markerregels en de logregels van apps.
///
/// Eén regel per aanroep, Engels, met marker en getallen (handboek §6).
pub trait Console {
    /// Een kernregel.
    fn log(&self, args: core::fmt::Arguments<'_>);
    /// Een logregel van de app in `slot` (vol = droppen, zoals de
    /// `Channel<LogLine, 64>` van de Go-servicer).
    fn app_line(&self, slot: Slot, line: &[u8]) {
        let _ = (slot, line);
    }
    /// Hard falen (docs/sequence-hopos.md §13.2): de kern kan een invariant
    /// niet meer bevestigen (een slot dat niet stil wordt, een schijf die
    /// faalt). De binary geeft `HOPOS_FATAL` en parkeert, de watchdog
    /// reset; op de host is het een panic, zodat een toets hem ziet.
    #[expect(
        clippy::panic,
        reason = "de host-vorm van fatal: een toets vangt hem met should_panic"
    )]
    fn fatal(&self, args: core::fmt::Arguments<'_>) -> ! {
        panic!("HOPOS_FATAL {args}")
    }
}

/// Fysiek geheugen buiten de eigen heap, woordgewijs: de boot-scratch, de
/// feitenpagina en de handoff van de flip.
///
/// Het board vult hem met `dev::read64`/`write64` op adressen uit `layout`;
/// de tests met een ijle tabel. Zo blijft de rekenkunde van kernflip puur
/// en zonder `unsafe`.
pub trait PhysMem {
    /// Lees het 64-bit woord op `pa` (8-uitgelijnd).
    fn read64(&self, pa: u64) -> u64;
    /// Schrijf het 64-bit woord op `pa` (8-uitgelijnd).
    fn write64(&mut self, pa: u64, v: u64);
    /// Wis `len` bytes op `pa` (beide 8-uitgelijnd).
    fn clear(&mut self, pa: u64, len: u64) {
        let mut off = 0;
        while off < len {
            self.write64(pa.wrapping_add(off), 0);
            off += 8;
        }
    }
    /// Veeg `[pa, pa+len)` naar het punt van coherentie (clean+invalidate).
    fn clean_inv(&mut self, pa: u64, len: u64);
    /// Kopieer `src` naar `pa` (woordgewijs; het board mag `dev::copy_in`
    /// gebruiken).
    fn copy_in(&mut self, pa: u64, src: &[u8]) {
        let mut i = 0;
        while i < src.len() {
            let a = pa.wrapping_add(i as u64);
            let (word, lead) = (a & !7, (a & 7) as usize);
            let mut w = self.read64(word).to_le_bytes();
            let n = (8 - lead).min(src.len() - i);
            if let (Some(d), Some(s)) = (w.get_mut(lead..lead + n), src.get(i..i + n)) {
                d.copy_from_slice(s);
            }
            self.write64(word, u64::from_le_bytes(w));
            i += n;
        }
    }
    /// Kopieer bytes uit `pa` naar `dst`.
    fn copy_out(&self, dst: &mut [u8], pa: u64) {
        let mut i = 0;
        while i < dst.len() {
            let w = self.read64(pa.wrapping_add(i as u64)).to_le_bytes();
            let n = (dst.len() - i).min(8);
            if let (Some(d), Some(s)) = (dst.get_mut(i..i + n), w.get(..n)) {
                d.copy_from_slice(s);
            }
            i += 8;
        }
    }
}

/// De tijd van de executor: slapen en de monotone klok ([`sync::Timer`],
/// dezelfde trait als de drivers en applib).
pub use sync::Timer;

#[cfg(test)]
pub(crate) mod tests {
    use super::{PhysMem, esr_class};
    use std::collections::HashMap;

    /// IJl fysiek geheugen: alleen wat geschreven is, bestaat.
    #[derive(Default, Clone, PartialEq, Debug)]
    pub(crate) struct SparseMem(pub(crate) HashMap<u64, u64>);

    impl PhysMem for SparseMem {
        fn read64(&self, pa: u64) -> u64 {
            self.0.get(&pa).copied().unwrap_or(0)
        }
        fn write64(&mut self, pa: u64, v: u64) {
            if v == 0 {
                self.0.remove(&pa);
            } else {
                self.0.insert(pa, v);
            }
        }
        fn clear(&mut self, pa: u64, len: u64) {
            self.0.retain(|a, _| *a < pa || *a >= pa + len);
        }
        fn clean_inv(&mut self, _: u64, _: u64) {}
    }

    // De ESR's van de eerste Pi 5-boot (30-09) en wat een app op EL1 het
    // vaakst doet, in de woorden van de fault-regel.
    #[test]
    fn esr_classes_name_the_fault() {
        assert_eq!(
            esr_class(0x8200_0005),
            "instruction abort, translation fault"
        );
        assert_eq!(esr_class(0x9600_0021), "data abort, alignment fault");
        assert_eq!(esr_class(0x9200_0047), "data abort, translation fault");
        assert_eq!(esr_class(0x0200_0000), "unknown or undefined instruction");
        assert_eq!(esr_class(0x5a00_0001), "HVC");
        assert_eq!(esr_class(0xbe00_0000), "SError");
        assert_eq!(esr_class(0xfc00_0000), "other exception");
    }
}
