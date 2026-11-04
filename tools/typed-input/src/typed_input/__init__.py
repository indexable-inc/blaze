"""A script's one typed input (AGENTS.md rule scripts-take-typed-input).

A script takes a single JSON object, given inline as its one argument, as the path of a file, or
as `-` for standard input, and reads it into a frozen dataclass whose every field is named and
typed. `parse` refuses a field the class does not have (never dropping it), one it needs and was
not given, and a value of another type, naming the field, the class and where in the JSON it is.

    @dataclass(frozen=True)
    class Run:
        suite: Literal["ruff-and-ty", "uv"]
        timeout_minutes: int = 300

    run = parse(Run, sys.argv[1], "downstream.py")
"""

import dataclasses
import json
import sys
import types
import typing
from pathlib import Path
from typing import Any, Literal, NoReturn


class InputError(Exception):
    """What is wrong with the input, naming the field."""


def read(text: str) -> object:
    """The JSON value of `text`: inline, the path of a file, or `-` for standard input."""
    if text == "-":
        text = sys.stdin.read()
    elif not text.lstrip().startswith(("{", "[")):
        text = Path(text).read_text()
    try:
        return json.loads(text)
    except json.JSONDecodeError as e:
        raise InputError(f"the input is not JSON: {e}") from e


def _check(where: str, hint: Any, value: object) -> object:
    """`value` as of type `hint`, or an InputError naming `where`."""
    if isinstance(hint, typing.TypeAliasType):
        return _check(where, hint.__value__, value)
    origin = typing.get_origin(hint)
    if hint is Any:
        return value
    if origin is Literal:
        if value not in typing.get_args(hint) or isinstance(value, bool) != any(
            isinstance(it, bool) for it in typing.get_args(hint)
        ):
            allowed = ", ".join(json.dumps(it) for it in typing.get_args(hint))
            raise InputError(f"{where} is one of {allowed}, not {json.dumps(value)}")
        return value
    if origin in (typing.Union, types.UnionType):
        errors: list[str] = []
        for option in typing.get_args(hint):
            try:
                return _check(where, option, value)
            except InputError as e:
                errors.append(str(e))
        raise InputError(" or ".join(errors))
    if hint is type(None):
        if value is not None:
            raise InputError(f"{where} is null, not {json.dumps(value)}")
        return None
    if origin is tuple:
        args = typing.get_args(hint)
        if not isinstance(value, list) or len(args) != 2 or args[1] is not Ellipsis:
            raise InputError(f"{where} is a list, not {json.dumps(value)}")
        items = typing.cast(list[object], value)
        return tuple(_check(f"{where}[{i}]", args[0], item) for i, item in enumerate(items))
    if dataclasses.is_dataclass(hint) and isinstance(hint, type):
        return _of(hint, value, where)
    if hint in (str, int, float, bool):
        # A bool is an int to isinstance, and JSON's 1 is no float's 1.0 to anyone reading it.
        if not isinstance(value, hint) or (hint is not bool and isinstance(value, bool)):
            raise InputError(f"{where} is a {hint.__name__}, not {json.dumps(value)}")
        return value
    raise TypeError(f"typed_input: {where} has a type it does not read: {hint!r}")


def _at(where: str) -> str:
    return f"at {where}" if where else "at the top level"


def _of[T](cls: type[T], value: object, where: str) -> T:
    if not isinstance(value, dict):
        raise InputError(f"{where or 'the input'} is a JSON object ({cls.__name__}), not {json.dumps(value)}")
    given = typing.cast(dict[str, object], value)
    fields = [f.name for f in dataclasses.fields(typing.cast(Any, cls))]
    prefix = f"{where}." if where else ""
    if unknown := sorted(set(given) - set(fields)):
        names = ", ".join(unknown)
        raise InputError(
            f"{cls.__name__} {_at(where)} has no field {names} (its fields are {', '.join(sorted(fields))})"
        )
    defaulted = {
        f.name
        for f in dataclasses.fields(typing.cast(Any, cls))
        if f.default is not dataclasses.MISSING or f.default_factory is not dataclasses.MISSING
    }
    if missing := [it for it in fields if it not in given and it not in defaulted]:
        names = ", ".join(missing)
        raise InputError(f"{cls.__name__} {_at(where)} needs field {names}")
    hints = typing.get_type_hints(cls)
    return cls(**{name: _check(prefix + name, hints[name], given[name]) for name in fields if name in given})


def parse[T](cls: type[T], text: str, script: str) -> T:
    """`cls` read from the JSON `text` (inline, a path, or `-`); exits naming what is wrong."""
    try:
        return _of(cls, read(text), "")
    except (InputError, OSError) as e:
        fail(script, str(e))


def fail(script: str, message: str) -> NoReturn:
    sys.exit(f"{script}: {message}")
