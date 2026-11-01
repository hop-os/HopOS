//! De vlakken die deze build niet heeft, als stub met dezelfde signatuur
//! (handboek §7: de `cfg` op module-niveau; Linux zet ze als lege
//! `static inline` in de header). Een headless kern heeft alleen deze: lege
//! functies, geen regel scherm, media of USB (docs/sequence-hopos.md
//! §14.3). `main.rs` neemt per vlak de echte module (gui.rs, usb.rs,
//! codec.rs, optical.rs) of de stub hier.

/// Zonder `gui`: geen glas, en het gaat nooit weg.
#[cfg(not(feature = "gui"))]
pub(crate) mod gui {
    use executor::Executor;

    /// Geen glas achter de console-tee.
    pub(crate) fn glass(_b: &[u8]) {}

    /// Geen framebuffer-console.
    pub(crate) fn init_framebuffer_console(_board: &'static crate::Machine) {}

    /// Geen meetregels.
    pub(crate) fn start_screen_status(_exec: &'static Executor) {}

    /// De grant-aanbieder van de lifecycle: geen. Een job die om het glas
    /// vraagt, draait headless.
    pub(crate) fn slot_grants() -> kern::grants::NoGrants {
        kern::grants::NoGrants
    }

    /// De USB zonder glas (media): de toetsen gaan nergens heen.
    #[cfg(feature = "usb")]
    pub(crate) struct UsbInput;

    #[cfg(feature = "usb")]
    impl UsbInput {
        pub(crate) fn start(_exec: &'static Executor) -> Option<UsbInput> {
            Some(UsbInput)
        }
        pub(crate) fn withdraw() {}
    }
}

/// Zonder de bouwsteen `usb`: geen bus.
#[cfg(not(feature = "usb"))]
pub(crate) mod usb {
    use executor::Executor;

    /// Geen controllers.
    pub(crate) fn start(_exec: &'static Executor) {}

    /// Niets stil te leggen voor een flip.
    pub(crate) async fn quiesce(_exec: &'static Executor) {}
}

/// Zonder `media`: geen VPU-driver en geen dienst; de codec-ops antwoorden
/// "onbekende op", zoals op een board zonder codec-ijzer.
#[cfg(not(feature = "media"))]
pub(crate) mod codec {
    use executor::Executor;

    /// Niets op te brengen.
    pub(crate) fn up(_exec: &'static Executor) {}

    /// De codec reserveert niets uit de pool.
    pub(crate) fn planned_bytes() -> u64 {
        0
    }
}

/// De USB zonder `media` (gui): geen optische schijven, de opslaghaken van
/// de USB-taak doen niets.
#[cfg(all(feature = "usb", not(feature = "media")))]
pub(crate) mod optical {
    use usb::storage::{BulkError, BulkId, BulkInfo, BulkReq};

    /// Wekt de USB-taak voor een optische transfer: hier nooit.
    pub(crate) static WAKE: sync::Signal = sync::Signal::new();

    /// De brug naar de optische eigenaar: zonder eigenaar. Met accolades,
    /// zodat `Bridge::default()` dezelfde regel is als met `media`.
    #[derive(Default)]
    pub(crate) struct Bridge {}

    impl Bridge {
        pub(crate) fn attached(&mut self, _: &BulkInfo) {}
        pub(crate) fn gone(&mut self, _: BulkId) {}
        pub(crate) fn next(&mut self) -> Option<BulkReq> {
            None
        }
        pub(crate) fn output(&self, _: &BulkReq) -> &[u8] {
            &[]
        }
        pub(crate) fn input(&mut self, _: &BulkReq) -> &mut [u8] {
            &mut []
        }
        pub(crate) fn done(&mut self, _: &BulkReq, _: Result<usize, BulkError>) {}
    }
}
