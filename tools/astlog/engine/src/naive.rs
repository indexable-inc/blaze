//! The evaluator astlog started with, kept as the reference model for the semi-naive one: naive
//! iteration per stratum, nested-loop joins over every row, variables in a map by name. It knows
//! relations, source atoms, negation, count and the `lt` builtin, which is what the property test
//! generates.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};

use blaze_hash::HashMap;

use crate::eval::{Facts, Row, Value};
use crate::program::{AppAtom, BodyAtom, Language, Program, Term};

type Env<N> = HashMap<String, Value<N>, Global>;

/// Every relation's rows as a set.
pub(crate) type Sets<N> = BTreeMap<String, BTreeSet<Row<N>>>;

pub(crate) fn fixpoint<F: Facts>(program: &Program<F::Lang>, facts: &F) -> Sets<F::Node>
where
    <F::Lang as Language>::Error: std::fmt::Debug,
{
    let mut db: Sets<F::Node> = BTreeMap::new();
    for rule in &program.rules {
        db.entry(rule.name.clone()).or_default();
    }
    let strata = program.strata().unwrap_or_else(|error| panic!("unstratifiable: {error}"));
    for stratum in strata {
        loop {
            let mut staged = Vec::new();
            for &index in &stratum {
                let rule = &program.rules[index];
                let mut envs = Vec::new();
                solve(facts, &db, &rule.body, Env::new_in(Global), &mut envs);
                for env in envs {
                    let row: Row<F::Node> =
                        rule.head_vars.iter().map(|var| env[var].clone()).collect();
                    staged.push((rule.name.clone(), row));
                }
            }
            let mut grew = false;
            for (name, row) in staged {
                grew |= db.entry(name).or_default().insert(row);
            }
            if !grew {
                break;
            }
        }
    }
    db
}

fn solve<F: Facts>(
    facts: &F,
    db: &Sets<F::Node>,
    atoms: &[BodyAtom<<F::Lang as Language>::Atom>],
    env: Env<F::Node>,
    out: &mut Vec<Env<F::Node>>,
) where
    <F::Lang as Language>::Error: std::fmt::Debug,
{
    let Some((first, rest)) = atoms.split_first() else {
        out.push(env);
        return;
    };
    match first {
        BodyAtom::Source(source) => {
            let inputs: Vec<Option<Value<F::Node>>> =
                source.args.iter().map(|term| bound(term, &env)).collect();
            let mut rows = Vec::new();
            facts
                .solve(&source.value, source.line, &inputs, &mut rows)
                .unwrap_or_else(|error| panic!("source atom failed: {error}"));
            for row in rows {
                if let Some(next) = unify_all(&env, &source.args, &row) {
                    solve(facts, db, rest, next, out);
                }
            }
        }
        BodyAtom::App(app) if app.name == "lt" => {
            let (Some(Value::Int(a)), Some(Value::Int(b))) =
                (bound(&app.args[0], &env), bound(&app.args[1], &env))
            else {
                panic!("lt on unbound or non-number values");
            };
            if a < b {
                solve(facts, db, rest, env, out);
            }
        }
        BodyAtom::App(app) => {
            for row in &db[&app.name] {
                if let Some(next) = unify_all(&env, &app.args, row) {
                    solve(facts, db, rest, next, out);
                }
            }
        }
        BodyAtom::Negation(neg) => {
            if !matches(db, neg, &env).next().is_some() {
                solve(facts, db, rest, env, out);
            }
        }
        BodyAtom::Count(count) => {
            let n = matches(db, &count.inner, &env).count();
            let n = Value::Int(i64::try_from(n).unwrap_or(i64::MAX));
            if let Some(next) = unify(&env, &count.result, &n) {
                solve(facts, db, rest, next, out);
            }
        }
    }
}

fn matches<'d, N: Clone + Ord>(
    db: &'d Sets<N>,
    app: &'d AppAtom,
    env: &'d Env<N>,
) -> impl Iterator<Item = &'d Row<N>> {
    db[&app.name].iter().filter(move |row| unify_all(env, &app.args, row).is_some())
}

fn bound<N: Clone>(term: &Term, env: &Env<N>) -> Option<Value<N>> {
    match term {
        Term::Var(var) => env.get(var).cloned(),
        Term::Text(text) => Some(Value::Text(std::sync::Arc::from(text.as_str()))),
        Term::Int(int) => Some(Value::Int(*int)),
    }
}

fn unify_all<N: Clone + PartialEq>(
    env: &Env<N>,
    terms: &[Term],
    row: &[Value<N>],
) -> Option<Env<N>> {
    let mut env = Some(env.clone());
    for (term, value) in terms.iter().zip(row) {
        env = env.and_then(|e| unify(&e, term, value));
    }
    env
}

fn unify<N: Clone + PartialEq>(env: &Env<N>, term: &Term, value: &Value<N>) -> Option<Env<N>> {
    match term {
        Term::Var(var) => match env.get(var) {
            Some(bound) if bound == value => Some(env.clone()),
            Some(_) => None,
            None => {
                let mut next = env.clone();
                next.insert(var.clone(), value.clone());
                Some(next)
            }
        },
        Term::Text(_) | Term::Int(_) => {
            (bound(term, env).as_ref() == Some(value)).then(|| env.clone())
        }
    }
}
