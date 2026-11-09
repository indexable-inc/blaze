//! A capability the host withholds is missing at the root, never halfway through an op: each
//! root accessor of `Io` is `Some` exactly when the host offers it, and its `need_` form is
//! `Ok` then and otherwise a `Missing` naming it, whatever is offered. Every flag of `Offers`
//! is covered: one with no accessor here fails the test, naming it.
//!
//! Two capabilities are reached through another: `procs` from `host` and `audio_in` from
//! `audio`. Each is there exactly when both are offered, and its `need_` chain names the
//! first of the two that is missing.
//!
//! Plant: [`Plant::OffersIgnored`], accessors that hand out what the host does not offer.

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control};
use crate::{
    Plant,
    io::{Io, Missing, Offers, SimWorld},
};

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// One bit a capability, in the order of [`Offers::NAMES`].
    offered: u64,
}

fn plan(d: &mut impl Draw) -> Plan {
    Plan { seed: d.int(0, u64::MAX), offered: d.int(0, (1 << Offers::NAMES.len()) - 1) }
}

/// The capability `name` is reached through, if it is not a root of `Io` itself.
fn through(name: &str) -> Option<&'static str> {
    match name {
        "procs" => Some("host"),
        "audio_in" => Some("audio"),
        _ => None,
    }
}

/// Whether `io` hands out the capability `name`, and what its `need_` form says; `None` for a
/// name this test has no accessor for.
fn asked(io: &Io, name: &str) -> Option<(bool, Result<(), Missing>)> {
    fn need<T>(got: Result<T, Missing>) -> Result<(), Missing> {
        got.map(|_| ())
    }
    Some(match name {
        "fs" => (io.fs().is_some(), need(io.need_fs())),
        "net" => (io.net().is_some(), need(io.need_net())),
        "env" => (io.env().is_some(), need(io.need_env())),
        "args" => (io.args().is_some(), need(io.need_args())),
        "clock" => (io.clock().is_some(), need(io.need_clock())),
        "rng" => (io.rng().is_some(), need(io.need_rng())),
        "mem" => (io.mem().is_some(), need(io.need_mem())),
        "host" => (io.host().is_some(), need(io.need_host())),
        "term" => (io.term().is_some(), need(io.need_term())),
        "keys" => (io.keys().is_some(), need(io.need_keys())),
        "roots" => (io.roots().is_some(), need(io.need_roots())),
        "locale" => (io.locale().is_some(), need(io.need_locale())),
        "user" => (io.user().is_some(), need(io.need_user())),
        "procs" => (
            io.host().and_then(|host| host.procs()).is_some(),
            need(io.need_host().and_then(|host| host.need_procs())),
        ),
        "code" => (io.code().is_some(), need(io.need_code())),
        "windows" => (io.windows().is_some(), need(io.need_windows())),
        "automation" => (io.automation().is_some(), need(io.need_automation())),
        "opener" => (io.opener().is_some(), need(io.need_opener())),
        "power" => (io.power().is_some(), need(io.need_power())),
        "audio" => (io.audio().is_some(), need(io.need_audio())),
        "audio_in" => (
            io.audio().and_then(|audio| audio.input()).is_some(),
            need(io.need_audio().and_then(|audio| audio.need_input())),
        ),
        "camera" => (io.camera().is_some(), need(io.need_camera())),
        "screen" => (io.screen().is_some(), need(io.need_screen())),
        "location" => (io.location().is_some(), need(io.need_location())),
        "bluetooth" => (io.bluetooth().is_some(), need(io.need_bluetooth())),
        "usb" => (io.usb().is_some(), need(io.need_usb())),
        "serial" => (io.serial().is_some(), need(io.need_serial())),
        "clipboard" => (io.clipboard().is_some(), need(io.need_clipboard())),
        "notify" => (io.notify().is_some(), need(io.need_notify())),
        _ => return None,
    })
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let offers = Offers::from_bits(plan.offered);
    if offers.bits() != plan.offered {
        return Err(format!("Offers::from_bits({:#x}) reads back as {:#x}", plan.offered, offers.bits()));
    }
    let mut world = SimWorld::new(plan.seed);
    world.offers = offers;
    let io = Io::planted(world, plant);
    if io.offers() != offers {
        return Err(format!("the world offers {:?}, not the {offers:?} it was made with", io.offers()));
    }
    for &name in Offers::NAMES {
        let own = offers.offers(name).ok_or_else(|| format!("harness: Offers has no flag {name}"))?;
        let outer = match through(name) {
            Some(outer) => offers.offers(outer).ok_or_else(|| format!("harness: Offers has no flag {outer}"))?,
            None => true,
        };
        let offered = own && outer;
        // The first of the chain that is missing is the one a `need_` names.
        let missing = match (through(name), outer) {
            (Some(outer), false) => outer,
            _ => name,
        };
        let Some((handed, needed)) = asked(&io, name) else {
            return Err(format!("Offers has the flag {name}, and this test knows no accessor for it"));
        };
        if handed != offered {
            return Err(format!(
                "io.{name}() is {} though the host {} it",
                if handed { "Some" } else { "None" },
                if offered { "offers" } else { "withholds" }
            ));
        }
        match (offered, needed) {
            (true, Ok(())) => {}
            (false, Err(Missing { capability })) if capability == missing => {}
            (_, needed) => return Err(format!("io.need_{name}() gave {needed:?} with it offered {offered}")),
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 100)]
fn a_withheld_capability_is_missing_and_named(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_accessors_that_ignore_the_offers() {
    control(Plant::OffersIgnored, |seeds: &mut Seeds| plan(seeds), run);
}

#[test]
fn sim_offers_everything_and_sim_with_withholds() {
    assert_eq!(Io::sim(1).offers(), Offers::ALL);
    let io = Io::sim_with(1, Offers { net: false, ..Offers::ALL });
    assert!(io.net().is_none());
    assert!(io.fs().is_some());
    let missing = io.need_net().expect_err("net is withheld");
    assert_eq!(missing, Missing { capability: "net" });
    assert!(missing.to_string().contains("`net`"), "{missing}");
}

#[test]
fn a_native_world_offers_only_what_blaze_core_has_effects_for() {
    let native = crate::host::OFFERS;
    for name in ["fs", "env", "args", "clock", "rng", "mem", "host"] {
        assert_eq!(native.offers(name), Some(true), "{name}");
    }
    for name in &Offers::NAMES[8..] {
        assert_eq!(native.offers(name), Some(false), "a native world has no provider for {name}");
    }
}
