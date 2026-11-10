//! The app's side of the world api (src/world_api.rs): each call answered on the main thread against the live world,
//! and the journal of saved edits replayed at launch.
use super::*;
use crate::world_api::{self, Call};
use serde_json::Value;

impl App {
    /// Serve the world to agents (the window's only: a shot or a test serves nothing) and replay the saved edits.
    pub fn serve_world(&mut self) {
        match world_api::serve(self.link.waker()) {
            Ok(rx) => self.world_rx = Some(rx),
            Err(e) => eprintln!("harness-native: world api: {e}"),
        }
        self.replay_world();
    }

    /// Apply the journal's edits in order, as a launch does after the scene loads. One that no longer applies (its
    /// entity or token is gone from the scene) is said, naming it, and the rest go on.
    pub fn replay_world(&mut self) {
        let edits = match world_api::journal() {
            Ok(e) => e,
            Err(e) => return eprintln!("harness-native: world journal: {e}"),
        };
        for (i, edit) in edits.into_iter().enumerate() {
            let edit = match edit {
                Call::Set { path, component, value, .. } => Call::Set { path, component, value, save: false },
                Call::Token { name, value, .. } => Call::Token { name, value, save: false },
                Call::Script { code, .. } => Call::Script { code, save: false },
                other => other,
            };
            if let Err(e) = self.world_call(edit) {
                eprintln!("harness-native: world journal: edit {i}: {e}");
            }
        }
    }

    /// Answer the calls that came in since the last frame.
    pub(super) fn step_world(&mut self) {
        let mut any = false;
        while let Some(p) = self.world_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            // A caller that stopped waiting gets nothing: its answer is dropped.
            let _ = p.answer.try_send(self.world_call(p.call));
            any = true;
        }
        if any {
            self.poke();
        }
    }

    /// One call against the app's world ([`call`]).
    ///
    /// # Errors
    /// What refused it and why.
    pub fn world_call(&mut self, call: Call) -> Result<Value, String> {
        self::call(&self.world, &mut self.world_scripts, call)
    }
}

/// One call against `world`; `scripts` counts the scripts run, each a script entity of its own.
///
/// # Errors
/// What refused it and why, naming the entity, component, token or script.
pub(super) fn call(world: &World, scripts: &mut u32, call: Call) -> Result<Value, String> {
    let parse = |what: &str, s: String| serde_json::from_str::<Value>(&s).map_err(|e| format!("{what}: flecs wrote JSON this app cannot read: {e}"));
    let w = world.world_ref();
    let no_var = |name: &str| format!("the scene has no variable `{name}` (scene/00-tokens.flecs)");
    match call {
        Call::Query { expr } => parse(&expr, w.query_json(&expr)?),
        Call::Get { path } => parse(&path, w.entity_json(&path)?),
        Call::Set { path, component, value, save } => {
            let now = parse(&path, w.set_json(&path, &component, &value.to_string())?)?;
            if save {
                world_api::keep(Call::Set { path, component, value: now.clone(), save: true })?;
            }
            Ok(now)
        }
        Call::Token { name, value: None, .. } => parse(&name, w.var_json(&name).ok_or_else(|| no_var(&name))?),
        Call::Token { name, value: Some(value), save } => {
            let mut whole = parse(&name, w.var_json(&name).ok_or_else(|| no_var(&name))?)?;
            world_api::merge(&mut whole, value);
            // A colour goes down every copy of it (ghostty.rs `retint`); anything else is the variable alone.
            let set = match ghostty::fill_of(&whole) {
                Some(f) => ghostty::retint(w, &name, f),
                None => w.set_mut_var(&name, &whole.to_string()),
            };
            if !set {
                return Err(format!("`{name}` did not take {whole}: only an `export mut` variable changes while the app runs, and the value must fit its type"));
            }
            let now = parse(&name, w.var_json(&name).ok_or_else(|| no_var(&name))?)?;
            if save {
                world_api::keep(Call::Token { name, value: Some(now.clone()), save: true })?;
            }
            Ok(now)
        }
        Call::Script { code, save } => {
            *scripts += 1;
            let e = world.script_named(&format!("world::edit{scripts}")).build_from_code(&code);
            if let Some(err) = script_error(*e) {
                return Err(format!("the script did not run: {err}"));
            }
            if save {
                world_api::keep(Call::Script { code, save: true })?;
            }
            Ok(Value::Null)
        }
        Call::Schema { component: None } => parse("the schema", w.schema_json()),
        Call::Schema { component: Some(c) } => parse(&c, w.type_info_json(&c).ok_or_else(|| format!("the world has no component `{c}` with a type"))?),
        Call::Saved => serde_json::to_value(world_api::journal()?).map_err(|e| format!("the journal: {e}")),
        Call::Forget { index } => Ok(serde_json::json!({"left": world_api::forget(index)?})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The refinement: a world call does to the live world what its words say. A token written is read back with only
    /// the named members changed and reaches what the scene made from it; a component set on an entity is that
    /// entity's value; a query finds it; what cannot be done is refused naming it.
    #[test]
    fn world_calls_read_and_edit_the_live_world() {
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let mut n = 0;
        let mut ask = |c: Call| call(&world, &mut n, c);
        let was = ask(Call::Token { name: "tone_accent".into(), value: None, save: false }).unwrap();
        let now = ask(Call::Token { name: "tone_accent".into(), value: Some(json!({"dark": {"r": 1.0, "g": 0.25, "b": 0.5}})), save: false })
            .unwrap();
        assert_eq!((now["dark"]["r"].as_f64(), now["dark"]["g"].as_f64()), (Some(1.0), Some(0.25)));
        assert_eq!(now["light"], was["light"], "the members not named changed");
        // The theme's role made from the token follows it.
        let role = ask(Call::Get { path: "hn.Theme.accent".into() }).unwrap();
        assert_eq!(role["components"]["Fill"]["dark"]["g"].as_f64(), Some(0.25), "{role}");
        // A component set on an entity, then found by a query.
        let set = ask(Call::Set { path: "hn.Theme.card".into(), component: "Fill".into(), value: json!({"dark": {"r": 0.5}}), save: false })
            .unwrap();
        assert_eq!(set["dark"]["r"].as_f64(), Some(0.5));
        let rows = ask(Call::Query { expr: "Fill, (ChildOf, hn.Theme), Prefab".into() }).unwrap();
        assert!(rows["results"].as_array().is_some_and(|r| r.iter().any(|e| e["name"] == "card")), "{rows}");
        // Script makes entities; a bad one says why.
        ask(Call::Script { code: "world_test_made { }".into(), save: false }).unwrap();
        assert!(ask(Call::Get { path: "world_test_made".into() }).is_ok());
        assert!(ask(Call::Script { code: "a { NoSuchComponent: {x: 1} }".into(), save: false }).is_err());
        for bad in [
            Call::Get { path: "no.such.entity".into() },
            Call::Set { path: "hn.Theme.card".into(), component: "NoSuch".into(), value: json!({}), save: false },
            Call::Token { name: "no_such_token".into(), value: Some(json!(1)), save: false },
        ] {
            assert!(ask(bad.clone()).is_err(), "{bad:?} was not refused");
        }
    }
}
