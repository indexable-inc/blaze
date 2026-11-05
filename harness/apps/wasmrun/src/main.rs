//! wasmtime as a library: a module is compiled by its Cranelift, then run with calls into the
//! host and back, a trap, memory growth, and fuel that runs out.

use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Store, Trap, Val};

const MODULE: &str = r#"
(module
  (import "host" "log" (func $log (param i32 i32)))
  (import "host" "mix" (func $mix (param i64) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 16) "hello from wasm")
  (global $calls (mut i32) (i32.const 0))
  (func $fib (export "fib") (param $n i32) (result i64)
    (if (result i64) (i32.lt_u (local.get $n) (i32.const 2))
      (then (i64.extend_i32_u (local.get $n)))
      (else (i64.add (call $fib (i32.sub (local.get $n) (i32.const 1)))
                     (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
  (func (export "greet")
    (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
    (call $log (i32.const 16) (i32.const 15)))
  (func (export "calls") (result i32) (global.get $calls))
  (func (export "checksum") (param $len i32) (result i64)
    (local $at i32) (local $sum i64)
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $at) (local.get $len)))
        (i32.store8 (local.get $at) (i32.mul (local.get $at) (i32.const 7)))
        (local.set $sum (call $mix (i64.add (local.get $sum) (i64.load8_u (local.get $at)))))
        (local.set $at (i32.add (local.get $at) (i32.const 1)))
        (br $next)))
    (local.get $sum))
  (func (export "grow") (param $pages i32) (result i32) (memory.grow (local.get $pages)))
  (func (export "divide") (param i32 i32) (result i32) (i32.div_s (local.get 0) (local.get 1)))
  (func (export "floats") (param f64 f32) (result f64 f32)
    (f64.sqrt (f64.mul (local.get 0) (f64.const 3.5)))
    (f32.nearest (f32.div (local.get 1) (f32.const 0.3))))
  (func (export "unreachable") unreachable)
  (func $spin (export "spin") (loop $again (br $again))))
"#;

struct Host {
    logged: Vec<String>,
    mixed: u64,
}

fn linker(engine: &Engine) -> wasmtime::Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap("host", "log", |mut caller: Caller<'_, Host>, at: i32, len: i32| {
        let memory = caller.get_export("memory").and_then(|it| it.into_memory());
        let Some(memory) = memory else { return };
        let (bytes, host) = memory.data_and_store_mut(&mut caller);
        let text = &bytes[at as usize..(at + len) as usize];
        host.logged.push(String::from_utf8_lossy(text).into_owned());
    })?;
    linker.func_wrap("host", "mix", |mut caller: Caller<'_, Host>, value: i64| -> i64 {
        caller.data_mut().mixed += 1;
        (value as u64).wrapping_mul(0x9e3779b97f4a7c15).rotate_left(7) as i64
    })?;
    Ok(linker)
}

fn trap(result: wasmtime::Result<impl std::fmt::Debug>) -> String {
    match result {
        Ok(value) => format!("returned {value:?}"),
        Err(error) => match error.downcast_ref::<Trap>() {
            Some(trap) => format!("trap: {trap}"),
            None => format!("error: {}", error.root_cause()),
        },
    }
}

fn run(instance: &Instance, store: &mut Store<Host>, rounds: u32) -> wasmtime::Result<()> {
    let fib = instance.get_typed_func::<i32, i64>(&mut *store, "fib")?;
    for n in [0, 1, 10, 24 + rounds as i32] {
        println!("fib({n}) = {}", fib.call(&mut *store, n)?);
    }
    let greet = instance.get_typed_func::<(), ()>(&mut *store, "greet")?;
    for _ in 0..3 {
        greet.call(&mut *store, ())?;
    }
    let calls = instance.get_typed_func::<(), i32>(&mut *store, "calls")?.call(&mut *store, ())?;
    println!("greeted {calls} times: {:?}", store.data().logged);
    let checksum = instance.get_typed_func::<i32, i64>(&mut *store, "checksum")?;
    for _ in 0..rounds {
        println!("checksum {:x}, host called {} times", checksum.call(&mut *store, 60_000)?, store.data().mixed);
    }
    let grow = instance.get_typed_func::<i32, i32>(&mut *store, "grow")?;
    println!("grow: {} then {} then {}", grow.call(&mut *store, 3)?, grow.call(&mut *store, 1)?, grow.call(&mut *store, 70_000)?);
    let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| wasmtime::Error::msg("no memory"))?;
    println!("memory is {} bytes, byte 5 is {}", memory.data_size(&*store), memory.data(&*store)[5]);
    let divide = instance.get_typed_func::<(i32, i32), i32>(&mut *store, "divide")?;
    println!("divide: {}", trap(divide.call(&mut *store, (7, -2))));
    println!("divide by zero: {}", trap(divide.call(&mut *store, (7, 0))));
    println!("divide overflow: {}", trap(divide.call(&mut *store, (i32::MIN, -1))));
    println!("out of bounds: {}", trap(checksum.call(&mut *store, 90_000_000)));
    let unreachable = instance.get_typed_func::<(), ()>(&mut *store, "unreachable")?;
    println!("unreachable: {}", trap(unreachable.call(&mut *store, ())));
    let floats = instance.get_func(&mut *store, "floats").ok_or_else(|| wasmtime::Error::msg("no floats"))?;
    let mut results = [Val::I32(0), Val::I32(0)];
    floats.call(&mut *store, &[Val::F64(2.0f64.to_bits()), Val::F32(1.0f32.to_bits())], &mut results)?;
    println!("floats: {:?} {:?}", results[0].f64(), results[1].f32());
    println!("deep recursion: {}", trap(fib.call(&mut *store, -1)).len() > 0);
    let spin = instance.get_typed_func::<(), ()>(&mut *store, "spin")?;
    store.set_fuel(1_000_000)?;
    println!("spin: {}", trap(spin.call(&mut *store, ())));
    Ok(())
}

fn main() -> wasmtime::Result<()> {
    let rounds: u32 = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(1);
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, MODULE)?;
    println!("exports: {:?}", module.exports().map(|it| it.name().to_owned()).collect::<Vec<_>>());
    let linker = linker(&engine)?;
    let mut store = Store::new(&engine, Host { logged: Vec::new(), mixed: 0 });
    store.set_fuel(u64::MAX)?;
    let instance = linker.instantiate(&mut store, &module)?;
    run(&instance, &mut store, rounds)?;
    println!("not a module: {}", Module::new(&engine, "(module (func (result i32)))").is_err());
    Ok(())
}
