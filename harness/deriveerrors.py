#!/usr/bin/env python3
"""Built-in derive refusals fail check and build with the error each case names (#751).

Each invalid item is unused, so walking reachable functions cannot find its error. The
same daemon sees valid/invalid/valid source and a restart with its store intact. Ordinary
proc-macro failures keep their own diagnostic classification in the front end.
"""
import json, os, shutil, subprocess, tempfile

from blaze_cli import base_env as env, write

CASES = [
    ("default", "#[derive(Default)] enum Mode { Fast, Slow }", "enum with no", "E0665"),
    *[("union-" + trait, f"#[derive({trait})] union Value {{ value: u32 }}", "cannot be derived for unions", None)
      for trait in ("Debug", "Default", "Hash", "Ord", "PartialOrd", "PartialEq")],
    ("from-enum", "#[derive(From)] enum Either { A(u8) }", "used on an enum", None),
    ("from-union", "#[derive(From)] union Value { value: u32 }", "used on a union", None),
    ("from-empty", "#[derive(From)] struct Empty;", "with no fields", None),
    ("from-fields", "#[derive(From)] struct Pair(u8, u8);", "with multiple fields", None),
    ("coerce-enum", "#[derive(std::marker::CoercePointee)] enum Value<T: ?Sized> { V(Box<T>) }", "can only be derived on `struct`s", None),
    ("reborrow-enum", "#[derive(std::marker::Reborrow)] enum Value { V }", "can only be derived on `struct`s", None),
    ("coerce-shared-enum", "#[derive(std::marker::CoerceShared)] enum Value { V }", "can only be derived on `struct`s", None),
    ("coerce-repr", "#[derive(std::marker::CoercePointee)] struct Value<T: ?Sized>(Box<T>);", "#[repr(transparent)]", None),
    ("coerce-empty", "#[repr(transparent)] #[derive(std::marker::CoercePointee)] struct Empty;", "with at least one field", None),
    ("coerce-generic", "#[repr(transparent)] #[derive(std::marker::CoercePointee)] struct Value(u32);", "generic over at least one type", None),
    ("coerce-sized", "#[repr(transparent)] #[derive(std::marker::CoercePointee)] struct Value<T>(Box<T>);", "to be marked `?Sized`", None),
    ("coerce-shared-target", "#[derive(std::marker::CoerceShared)] struct Shared<'a>(&'a u32);", "requires exactly one `#[coerce_shared(Target)]` attribute", None),
    ("pointee-none", "#[repr(transparent)] #[derive(std::marker::CoercePointee)] struct Pointer<T: ?Sized, U: ?Sized>(Box<T>, std::marker::PhantomData<U>);", "exactly one generic type parameter must be marked", None),
    ("pointee-two", "#[repr(transparent)] #[derive(std::marker::CoercePointee)] struct Pointer<#[pointee] T: ?Sized, #[pointee] U: ?Sized>(Box<T>, std::marker::PhantomData<U>);", "only one type parameter can be marked", None),
]
PREFIX = '#![feature(derive_from, derive_coerce_pointee, reborrow)]\n#![allow(unused)]\nuse std::from::From;\n'
GOOD = '#[derive(Default, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]\nenum Mode { #[default] Fast, Slow }\n#[derive(From)] struct Wrapper(u32);\n#[derive(Copy, Clone, Eq)] union Bits { value: u32 }\nimpl PartialEq for Bits { fn eq(&self, _: &Self) -> bool { true } }\n#[repr(transparent)] #[derive(std::marker::CoercePointee)] struct Pointer<T: ?Sized>(Box<T>);\n'


def main():
    work = tempfile.mkdtemp(prefix="derive-errors-", dir="/tmp" if os.name != "nt" else None)
    print(f"work: {work}", flush=True)
    at, home = os.path.join(work, "workspace"), os.path.join(work, "home")
    write(at, "Cargo.toml", '[package]\nname = "derive-errors"\nversion = "0.1.0"\nedition = "2024"\n')
    base = env(home)
    passed = False

    def source(item):
        write(at, "src/main.rs", PREFIX + item + "\nfn main() {}\n")

    def run(command):
        ran = subprocess.run(["blaze", command, *(["--message-format=json"] if command == "check" else [])], cwd=at, env=base,
                             capture_output=True, text=True, timeout=600)
        errors = [json.loads(line)["message"] for line in ran.stdout.splitlines()
                  if line.startswith("{") and json.loads(line).get("reason") == "compiler-message"
                  and json.loads(line)["message"]["level"] == "error"]
        return ran, errors

    def check(item, message=None, code=None):
        source(item)
        for command in ("check", "build"):
            ours, errors = run(command)
            expected = 101 if message else 0
            assert ours.returncode == expected, f"blaze {command}: {ours.stdout}\n{ours.stderr}"
            if message:
                said = "\n".join(it["message"] for it in errors) + ours.stderr
                assert message in said, f"missing {message!r}: {said}"
                if code and command == "check":
                    assert any(it["code"] and it["code"]["code"] == code for it in errors), errors

    try:
        check(GOOD)
        for name, item, message, code in CASES:
            check(item, message, code)
            check(GOOD)
            print(f"  {name}: check/build refuse, valid edit succeeds", flush=True)
        source(CASES[0][1])
        subprocess.run(["blaze", "stop"], cwd=at, env=base, capture_output=True, check=True)
        check(CASES[0][1], CASES[0][2], CASES[0][3])
        passed = True
    finally:
        subprocess.run(["blaze", "stop"], cwd=at, env=base, capture_output=True)
        if passed:
            shutil.rmtree(work)
    print("PASS: built-in derive errors, edits and restart", flush=True)


if __name__ == "__main__":
    main()
