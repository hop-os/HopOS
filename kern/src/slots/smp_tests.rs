//! De SMP- en sharegroup-paden van de lifecycle-actor, over de nep-kooi van
//! [`super::tests`]: het startschot van elke secundaire, de stop over de
//! hele span (E9: pas vrij na de bevestiging van élke core), `fatal`
//! als één core niet bevestigt, en leden van een sharegroup die hun core
//! delen en alleen stoppen.

use super::tests::{Actor, FakeConsole, Obey, actor, s, start, stop};
use super::*;
use crate::pool::GroupName;
use crate::testutil::block_on;
use std::vec;

fn group(name: &str) -> Placement {
    let mut g = GroupName::new();
    for b in name.bytes() {
        g.push(b).unwrap();
    }
    Placement {
        group: Some(g),
        pool_cores: 1,
        cores: 1,
        class: None,
        prefer: None,
        pin: None,
    }
}

/// Een lid van sharegroup `name`, zoals Hop hem start.
fn start_member(a: &mut Actor<'_>, slot: usize, name: &str) -> Result {
    let g = block_on(a.claim(StartSpec::new(s(slot), 8 << 20, group(name))))?;
    block_on(a.arm(g, 0x4001_0000))?;
    a.svc.ctl(s(slot)).unwrap().gone.set();
    Ok(())
}

// De app vraagt zijn cores één voor één (CTRL_SMP_REQ = slot + k, zoals
// Go's `task`); de actor dispatcht elke secundaire op de fysieke core van
// zijn eigen span, en beantwoordt elk verzoek.
#[test]
fn every_secondary_of_the_span_is_dispatched_in_the_cage() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 6);
    start(&mut a, 2, 8, 1).unwrap(); // Core 1.
    start(&mut a, 4, 8, 3).unwrap(); // Cores 2..4.
    assert_eq!(a.status(s(4)).core.map(|(c, n)| (c.get(), n)), Some((2, 3)));
    for vcpu in [5, 6] {
        a.cage.smp_req[4] = vcpu;
        block_on(a.handle(Request::Smp(s(4))));
        assert_eq!(a.cage.smp_req[4], 0, "request {vcpu} left unanswered");
    }
    assert_eq!(a.cage.secondaries, [(4, 3), (4, 4)]);
    assert!(con.saw("slot 4: SMP core 3 dispatched HOPOS_SMP_DISPATCH_OK"));
    assert!(con.saw("slot 4: SMP core 4 dispatched HOPOS_SMP_DISPATCH_OK"));
    // De primaire zelf is geen secundaire, en de buurman nooit.
    a.cage.smp_req[4] = 4;
    a.smp(s(4));
    a.cage.smp_req[4] = 7;
    a.smp(s(4));
    assert_eq!(a.cage.secondaries.len(), 2);
    assert!(con.saw("HOPOS_SMP_REJECT"));
}

// E9: de stop vraagt élke core van de span, en geeft pas vrij als ze
// allemaal stil zijn.
#[test]
fn stop_confirms_every_core_of_the_span() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 1, 16, 3).unwrap();
    for vcpu in [2, 3] {
        a.cage.smp_req[1] = vcpu;
        a.smp(s(1));
    }
    stop(&mut a, 1).unwrap();
    let asked = a.cage.asked_quiet.borrow().clone();
    for c in 1..=3 {
        assert!(asked.contains(&(1, c)), "stop never asked core {c}");
    }
    assert!(
        !asked.iter().any(|(_, c)| *c == 4),
        "stop touched a foreign core"
    );
    assert_eq!(a.status(s(1)).occupancy, Occupancy::Empty);
    // De hele span is terug: een nieuwe app van drie cores past.
    start(&mut a, 2, 16, 3).unwrap();
}

// Eén secundaire bevestigt niet, ook niet na de intrekking: de kern kan de
// isolatie van de hele eigenaar niet bevestigen, dus `fatal` (§13.5).
#[test]
#[should_panic(expected = "HOPOS_FATAL slot 1: not stopped after revocation")]
fn one_unconfirmed_core_is_fatal() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 1, 16, 3).unwrap();
    a.cage.smp_req[1] = 3;
    a.smp(s(1));
    a.cage.stuck[3] = true;
    let _ = stop(&mut a, 1);
}

// Een startschot van een secundaire met een onbekende uitkomst: de core kan
// alsnog aangaan, dus `fatal`, zoals in `arm`.
#[test]
#[should_panic(expected = "HOPOS_FATAL slot 1: the SMP start on core 2 failed")]
fn a_failed_secondary_dispatch_is_fatal() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 1, 16, 2).unwrap();
    a.cage.fail_secondary = true;
    a.cage.smp_req[1] = 2;
    a.smp(s(1));
}

// Sharegroups op app-cores: leden delen de core van de groep, houden hun
// eigen partitie, en de stop van één lid laat de rest en de
// groepsreservering staan.
#[test]
fn members_share_the_group_core_and_stop_alone() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 3);
    start_member(&mut a, 2, "demo").unwrap();
    start_member(&mut a, 3, "demo").unwrap();
    let (c2, c3) = (a.status(s(2)).core, a.status(s(3)).core);
    assert_eq!(c2, c3, "members did not share the group core");
    let core = c2.unwrap().0;
    // Elk lid zijn eigen kooi en partitie, op dezelfde core.
    assert_eq!(a.cage.dispatched, [(2, core.get()), (3, core.get())]);
    let (p2, p3) = (a.parts.partition_of(s(2)), a.parts.partition_of(s(3)));
    assert!(p2.is_some() && p3.is_some() && p2 != p3);
    // Een dedicated job komt nooit op de groepscore.
    start(&mut a, 4, 8, 1).unwrap();
    assert_ne!(a.status(s(4)).core.unwrap().0, core);

    stop(&mut a, 2).unwrap();
    assert_eq!(a.status(s(2)).occupancy, Occupancy::Empty);
    assert_eq!(a.status(s(3)).occupancy, Occupancy::Running);
    assert!(!a.cage.exit_asked[3], "the neighbour was asked to stop");
    assert!(!a.places.core_free(core.get()), "the group lost its core");
    // Het nieuwe lid komt weer bij de groep.
    start_member(&mut a, 5, "demo").unwrap();
    assert_eq!(a.status(s(5)).core, c3);
    stop(&mut a, 3).unwrap();
    stop(&mut a, 5).unwrap();
    assert!(
        a.places.core_free(core.get()),
        "the last member kept the pool"
    );
}

// share.go `bootPendingDispatch`: een buur die nooit yieldt, houdt de
// gedeelde core vast. Na RECLAIM_WAIT wordt hij geofferd (zijn kooi
// ingetrokken), en dan krijgt het nieuwe lid zijn beurt.
#[test]
fn a_neighbour_that_never_yields_is_sacrificed_for_the_new_member() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 2);
    start_member(&mut a, 2, "demo").unwrap();
    a.cage.hog = Some(2);
    a.cage.boot_pending[3] = true;
    start_member(&mut a, 3, "demo").unwrap();
    assert!(a.cage.revoked[2], "the hog kept the core");
    assert!(!a.cage.revoked[3]);
    assert!(con.saw(
        "slot 3: core 1 never yielded in 2 s, sacrificing resident slot 2 HOPOS_CORE_RECLAIM"
    ));
    assert_eq!(a.status(s(3)).occupancy, Occupancy::Running);
}

// Niets op te offeren (de core zegt niet wie hem houdt): een start met een
// onbekende uitkomst, dus `fatal`, in plaats van een start die gelukt heet en
// nooit draait.
#[test]
#[should_panic(expected = "HOPOS_FATAL slot 3: core 1 never picked up the start")]
fn a_member_that_never_gets_a_turn_is_fatal() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 2);
    start_member(&mut a, 2, "demo").unwrap();
    a.cage.boot_pending[3] = true;
    let _ = start_member(&mut a, 3, "demo");
}

// Hop is nooit het slachtoffer: zonder Hop herstart niemand iets.
#[test]
fn hop_is_never_sacrificed() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 2);
    start_member(&mut a, 2, "hop").unwrap();
    a.cage.hog = Some(2);
    a.cage.boot_pending[3] = true;
    // Zonder slachtoffer een start met onbekende uitkomst: `fatal`.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        start_member(&mut a, 3, "hop")
    }));
    assert!(r.is_err(), "a start that never ran was not fatal");
    assert!(!a.cage.revoked[2], "Hop was sacrificed");
    assert!(con.saw("HOPOS_CORE_RECLAIM_FAILED"));
}

// De OS-core als groep `system` (03-10): met één app-core krijgt de eerste
// dedicated job die core, de tweede gaat luid naar de OS-core in plaats
// van te falen, een lid met de tag `system` komt erbij, en een job met een
// eigen groep blijft bij zijn groep (en vindt hier geen core).
#[test]
fn a_job_without_a_free_core_joins_the_system_group() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 1);
    a.places.share_os_core(crate::pool::SYSTEM_GROUP).unwrap();
    start(&mut a, 2, 8, 1).unwrap();
    assert_eq!(a.status(s(2)).core.map(|(c, _)| c.get()), Some(1));
    start(&mut a, 3, 8, 1).unwrap();
    assert_eq!(a.status(s(3)).core.map(|(c, _)| c), Some(Core::OS));
    assert!(con.saw(
        "slot 3: no free app core, joining the system group on the OS core HOPOS_PLACE_SYSTEM"
    ));
    start_member(&mut a, 4, "system").unwrap();
    assert_eq!(a.status(s(4)).core.map(|(c, _)| c), Some(Core::OS));
    assert!(start_member(&mut a, 5, "web").is_err());
    assert!(
        start(&mut a, 6, 8, 2).is_err(),
        "an SMP job took the OS core"
    );
    stop(&mut a, 3).unwrap();
    assert_eq!(a.status(s(4)).occupancy, Occupancy::Running);
}

/// Eén echt venster ([`crate::grants::DeviceGrant`]) achter de haakjes,
/// zoals de gui-smaak: een grant die bij de stop niet terugkomt, weigert de
/// volgende houder.
struct OneWindow(crate::grants::DeviceGrant);

impl Grants for OneWindow {
    fn env(&mut self, slot: Slot, env: &[u8], out: &mut Vec<u8>) {
        if crate::grants::env_get(env, "FB") == Some(b"1") && self.0.claim(slot).is_ok() {
            out.extend_from_slice(b"FB_BASE=0x20000000\n");
        }
    }
    fn arm(&mut self, _: Slot) -> Result {
        Ok(())
    }
    fn adopt(&mut self, _: Slot) -> Result {
        Ok(())
    }
    fn release(&mut self, slot: Slot) {
        self.0.release(slot);
    }
}

/// Een Lumen-start zoals de system-listener hem doet (claim, env langs de
/// actor, arm), en daarna elke secundaire van de span: de env die op de
/// control-page zou gaan.
fn start_wide(a: &mut Actor<'_, OneWindow>, slot: usize, cores: usize) -> Result<Vec<u8>> {
    let mut spec = StartSpec::new(s(slot), 768 << 20, super::tests::ded(cores));
    spec.mounts = vec![Mount {
        local: b"/mounts".to_vec(),
        shared: b"/devices".to_vec(),
    }];
    let g = block_on(a.claim(spec))?;
    let env = match block_on(a.handle(Request::Env {
        slot: s(slot),
        env: b"FB=1\n".to_vec(),
    })) {
        Response::Env(e) => e,
        other => panic!("the env step answered {other:?}"),
    };
    block_on(a.arm(g, 0x4001_0000))?;
    a.svc.ctl(s(slot)).unwrap().gone.set();
    for k in 1..cores {
        a.cage.smp_req[slot] = (slot + k) as u64;
        a.smp(s(slot));
    }
    Ok(env)
}

// De vorm van Lumen op de O6N (30-09: na de DELETE weigerde elke
// plaatsing tot een koude boot): Hop op de OS-core, één app op core 1, en
// een job over de andere tien app-cores met de grootste partitie, een
// volume en een device-grant, die pas op de intrekking stopt. Na de stop is
// alles terug: dezelfde span met hetzelfde venster, de partitie, een
// gewone job en een plaatsing in de groep van Hop (een flipbundel).
#[test]
fn a_wide_job_with_devices_leaves_everything_placeable() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut fb = crate::grants::DeviceGrant::new("fb");
    fb.offer(crate::grants::Window {
        pa: 0x1_bc7a_0000,
        size: 4 << 20,
    })
    .unwrap();
    let mut a = super::tests::actor_with(&svc, &con, Obey::Revoke, 1024, 11, OneWindow(fb));
    a.places.share_os_core(crate::pool::SYSTEM_GROUP).unwrap();
    let hop = StartSpec::new(s(1), 8 << 20, group("system"));
    let g = block_on(a.claim(hop)).unwrap();
    block_on(a.arm(g, 0x4001_0000)).unwrap();
    start(&mut a, 2, 8, 1).unwrap();
    let env = start_wide(&mut a, 3, 10).unwrap();
    assert!(env.ends_with(b"FB_BASE=0x20000000\n"), "no grant: {env:?}");
    assert_eq!(
        a.status(s(3)).core.map(|(c, n)| (c.get(), n)),
        Some((2, 10))
    );
    assert_eq!(a.cage.secondaries.len(), 9);
    stop(&mut a, 3).unwrap();
    assert!(a.cage.revoked[3], "the stop never revoked");
    assert_eq!(a.status(s(3)).occupancy, Occupancy::Empty);
    assert_eq!(a.parts.partition_of(s(3)), None);
    assert_eq!(a.grants.0.holder(), None, "the window stayed with slot 3");
    for c in 2..=11 {
        assert!(a.places.core_free(c), "core {c} stayed claimed");
    }
    // Een flipbundel (de groep van Hop) en een gewone job.
    let flip = StartSpec::new(s(4), 64 << 20, group("system"));
    let g = block_on(a.claim(flip)).unwrap();
    a.abort(g);
    start(&mut a, 4, 8, 1).unwrap();
    assert_eq!(a.status(s(4)).core.map(|(c, _)| c.get()), Some(2));
    stop(&mut a, 4).unwrap();
    // En Lumen zelf weer, in hetzelfde slot, met zijn venster.
    let env = start_wide(&mut a, 3, 10).unwrap();
    assert!(
        env.ends_with(b"FB_BASE=0x20000000\n"),
        "no grant again: {env:?}"
    );
    assert_eq!(
        a.status(s(3)).core.map(|(c, n)| (c.get(), n)),
        Some((2, 10))
    );
}
