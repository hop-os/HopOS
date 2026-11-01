//! De grant-haakjes van de lifecycle-actor (`kern::grants`), over de
//! nep-kooi van [`super::tests`] en een nep-aanbieder die telt: de env na
//! de claim, `arm` na de bouw en vóór de dispatch, `release` na een
//! bevestigde stop en bij elke abort, `adopt` bij de adoptie. Plus de
//! partitie van een levende bewoner in de servicer-tabel (de codec-grant).

use super::tests::{Actor, FakeConsole, MIB, Obey, actor, actor_with, ded, s, start, stop};
use super::*;
use crate::testutil::block_on;
use std::vec;

/// De regels die de nep-aanbieder bij een aanvraag toevoegt.
const EXTRA: &[u8] = b"FB_BASE=0x20000000\nFB_WIDTH=1024\n";

/// Een aanbieder die telt: welk slot op welke haak kwam, en bij
/// `GUI=display` een grant met [`EXTRA`] in de env.
#[derive(Default)]
struct CountGrants {
    env: Vec<usize>,
    arm: Vec<usize>,
    adopt: Vec<usize>,
    release: Vec<usize>,
    /// Wat `env` toevoegt; leeg = [`EXTRA`].
    extra: Vec<u8>,
    fail_arm: bool,
    fail_adopt: bool,
}

impl Grants for CountGrants {
    fn env(&mut self, slot: Slot, env: &[u8], out: &mut Vec<u8>) {
        self.env.push(slot.get());
        if crate::grants::env_get(env, "GUI") == Some(b"display") {
            out.extend_from_slice(if self.extra.is_empty() {
                EXTRA
            } else {
                &self.extra
            });
        }
    }
    fn arm(&mut self, slot: Slot) -> Result {
        self.arm.push(slot.get());
        if self.fail_arm {
            return Err(Error::Range { base: 1, size: 2 });
        }
        Ok(())
    }
    fn adopt(&mut self, slot: Slot) -> Result {
        self.adopt.push(slot.get());
        if self.fail_adopt {
            return Err(Error::StillOwned { slot: 9 });
        }
        Ok(())
    }
    fn release(&mut self, slot: Slot) {
        self.release.push(slot.get());
    }
}

type Counted<'s> = Actor<'s, CountGrants>;

fn counted<'s>(svc: &'s Servicers, con: &'s FakeConsole, obey: Obey) -> Counted<'s> {
    actor_with(svc, con, obey, 64, 4, CountGrants::default())
}

/// Een start zoals de system-listener hem doet: claim, de env langs de
/// actor, arm. Geeft de env die op de control-page zou gaan.
fn start_env(a: &mut Counted<'_>, slot: usize, env: &[u8], ports: &[u16]) -> Result<Vec<u8>> {
    let mut spec = StartSpec::new(s(slot), 16 * MIB, ded(1));
    spec.ports = ports.to_vec();
    let g = block_on(a.claim(spec))?;
    let env = match block_on(a.handle(Request::Env {
        slot: s(slot),
        env: env.to_vec(),
    })) {
        Response::Env(e) => e,
        Response::Failed(e) => {
            a.abort(g);
            return Err(e);
        }
        other => panic!("the env step answered {other:?}"),
    };
    block_on(a.arm(g, 0x4001_0000))?;
    a.svc.ctl(s(slot)).unwrap().gone.set();
    Ok(env)
}

// De gewone levensloop: de env krijgt de regels van de aanbieder (met een
// slotregel achter een env zonder), arm na de bouw, en de grant gaat pas
// terug na de bevestigde stop.
#[test]
fn a_grant_lives_from_the_env_to_the_confirmed_stop() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    let env = start_env(&mut a, 2, b"BUCKET=hop\nGUI=display", &[]).unwrap();
    assert_eq!(
        env,
        [&b"BUCKET=hop\nGUI=display\n"[..], EXTRA].concat(),
        "the provider's lines follow a line break"
    );
    assert_eq!(a.grants.env, [2]);
    assert_eq!(a.grants.arm, [2]);
    assert_eq!(a.cage.built.len(), 1, "armed after the build");
    assert_eq!(a.cage.dispatched, [(2, 1)]);
    assert!(a.grants.release.is_empty(), "no release while it runs");
    stop(&mut a, 2).unwrap();
    assert_eq!(a.grants.release, [2]);
    // Een env zonder aanvraag blijft precies wat hij was, ook zonder
    // slotregel.
    let env = start_env(&mut a, 3, b"BUCKET=hop", &[]).unwrap();
    assert_eq!(env, b"BUCKET=hop");
    assert_eq!(a.grants.arm, [2, 3], "arm is asked for every start");
}

// Een abort geeft de grant terug: er draaide nooit iets.
#[test]
fn an_abort_releases_the_grant() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    let g = block_on(a.claim(StartSpec::new(s(2), 16 * MIB, ded(1)))).unwrap();
    let r = block_on(a.handle(Request::Env {
        slot: s(2),
        env: b"GUI=display\n".to_vec(),
    }));
    assert!(matches!(r, Response::Env(ref e) if e.ends_with(EXTRA)));
    a.abort(g);
    assert_eq!(a.grants.release, [2]);
    assert!(a.grants.arm.is_empty());
    assert_eq!(a.status(s(2)).occupancy, Occupancy::Empty);
}

// Een mislukte bouw is een abort, dus ook een release.
#[test]
fn a_failed_build_releases_the_grant() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    a.cage.fail_build = true;
    assert!(start_env(&mut a, 2, b"GUI=display\n", &[]).is_err());
    assert!(a.grants.arm.is_empty(), "no arm without a cage");
    assert_eq!(a.grants.release, [2]);
}

// Faalt `arm`, dan is dat een startfout zoals een mislukte bouw: niets
// gedispatcht, geen servicer, poorten dicht, partitie en grant terug.
#[test]
fn a_failed_arm_refuses_the_start_and_leaves_nothing() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    a.grants.fail_arm = true;
    let e = start_env(&mut a, 2, b"GUI=display\n", &[80]).unwrap_err();
    assert_eq!(e, Error::Range { base: 1, size: 2 });
    assert!(con.saw("slot 2: device grant not armed"));
    assert!(con.saw("HOPOS_GRANT_ARM_FAIL"));
    assert_eq!(a.cage.built.len(), 1);
    assert!(a.cage.dispatched.is_empty());
    assert_eq!(a.cage.unpublished, [2]);
    assert_eq!(a.grants.release, [2]);
    assert_eq!(svc.current(s(2)), None, "no servicer registered");
    assert_eq!(a.status(s(2)).occupancy, Occupancy::Empty);
    assert!(a.parts.partition_of(s(2)).is_none());
    // Het slot is daarna gewoon weer te starten.
    a.grants.fail_arm = false;
    start_env(&mut a, 2, b"", &[]).unwrap();
}

// De env-haak hoort bij een stroom: zonder claim is er niemand die hem
// terug kan geven, en een lopend slot heeft zijn env al.
#[test]
fn the_env_step_needs_a_streaming_slot() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    let env = || b"GUI=display\n".to_vec();
    let r = block_on(a.handle(Request::Env {
        slot: s(2),
        env: env(),
    }));
    assert!(matches!(r, Response::Failed(Error::NotOwned { slot: 2 })));
    start(&mut a, 2, 16, 1).unwrap();
    let r = block_on(a.handle(Request::Env {
        slot: s(2),
        env: env(),
    }));
    assert!(matches!(r, Response::Failed(Error::NotOwned { slot: 2 })));
    assert!(a.grants.env.is_empty(), "the provider was never asked");
}

// Te lang voor de control-page is een weigering van de grant, geen paniek
// en geen fout van de start: de env blijft wat hij was, de grant gaat terug.
#[test]
fn an_env_over_the_control_page_refuses_the_grant() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    let max = abi::hopabi::CTRL_ENV_MAX as usize;
    a.grants.extra = vec![b'x'; max];
    let env = start_env(&mut a, 2, b"GUI=display\n", &[]).unwrap();
    assert_eq!(env, b"GUI=display\n");
    assert_eq!(a.grants.release, [2]);
    assert!(con.saw(&std::format!(
        "slot 2: device grant refused: env length {} exceeds {max}",
        max + 12
    )));
    assert!(con.saw("HOPOS_GRANT_ENV"));
    assert_eq!(a.status(s(2)).occupancy, Occupancy::Running, "the app runs");
    // Precies passend is geen weigering.
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = counted(&svc, &con, Obey::Exit);
    a.grants.extra = vec![b'x'; max - 12];
    let env = start_env(&mut a, 2, b"GUI=display\n", &[]).unwrap();
    assert_eq!(env.len(), max);
    assert!(a.grants.release.is_empty());
}

// De adoptie herstelt de grant van elke bewoner vóór zijn servicer
// terugkomt; een aanbieder die weigert, laat geen dienst starten.
#[test]
fn adoption_restores_the_grants_before_the_services() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut old = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut old, 2, 16, 1).unwrap();
    start(&mut old, 3, 16, 1).unwrap();
    let states = old.snapshot().unwrap();

    let (svc2, con2) = (Servicers::new(), FakeConsole::default());
    let mut b = counted(&svc2, &con2, Obey::Exit);
    assert_eq!(b.adopt(&states), Ok(2));
    assert_eq!(b.grants.adopt, [2, 3]);
    assert!(svc2.current(s(2)).is_some() && svc2.current(s(3)).is_some());
    assert!(b.grants.release.is_empty());

    let (svc3, con3) = (Servicers::new(), FakeConsole::default());
    let mut c = counted(&svc3, &con3, Obey::Exit);
    c.grants.fail_adopt = true;
    assert_eq!(c.adopt(&states), Err(Error::StillOwned { slot: 9 }));
    assert!(con3.saw("slot 2: device grant not restored"));
    assert!(con3.saw("HOPOS_GRANT_ADOPT_FAIL"));
    assert_eq!(svc3.current(s(2)), None, "no service after a refused grant");
    assert_eq!(svc3.current(s(3)), None);
}

// De partitie van een levende bewoner, zoals de codec-grant hem vraagt:
// alleen zolang de servicer leeft, en dezelfde als die van het grootboek.
#[test]
fn the_partition_of_a_live_resident() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    assert_eq!(svc.partition(s(2)), None, "nobody lives there");
    start(&mut a, 2, 16, 1).unwrap();
    let p = svc.partition(s(2)).unwrap();
    assert_eq!(Some(p), a.status(s(2)).partition);
    assert_eq!(p.size, 16 * MIB);
    assert_eq!(svc.partition(s(3)), None, "another slot is not live");
    stop(&mut a, 2).unwrap();
    assert_eq!(svc.partition(s(2)), None, "gone with the servicer");
}

// De codec-grant rekent in die partitie, en weigert buiten de levensduur.
#[cfg(feature = "media")]
#[test]
fn the_codec_grant_uses_the_live_partition() {
    use crate::codecabi::codec_grant;
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = actor(&svc, &con, Obey::Exit, 64, 4);
    start(&mut a, 2, 16, 1).unwrap();
    let p = svc.partition(s(2)).unwrap();
    let b = codec_grant(p, 0x1000, 0x2000).unwrap();
    assert_eq!((b.pa, b.size), (p.base + 0x1000, 0x2000));
    assert!(codec_grant(p, p.size - 0x1000, 0x2000).is_err());
    stop(&mut a, 2).unwrap();
    assert!(svc.partition(s(2)).is_none());
}
