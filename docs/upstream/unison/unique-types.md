# Unique types · Unison programming language
URL: https://www.unison-lang.org/docs/language-reference/unique-types/

A type declaration gives a name to a type, but Unison does not uniquely identify a type by its name. Rather, the hash of a type's definition identifies the type. The hash is based on the structure of the type definition, with all identifiers removed.

For example, Unison considers these type declarations to declare the exact same type, even though they give different names to both the type constructor and the data constructors:

```
structural type Optional a = Some a | None

structural type Maybe a = Just a | Nothing
```

[...]

This is not always what you want. Sometimes you want to give meaning to a type that is more than just its structure. For example, it might be confusing that these two types are identical:

```
structural type Suit = Hearts | Spades | Diamonds | Clubs

structural type Direction = North | South | East | West
```

Unison will consider every unary type constructor with four nullary data constructors as identical to these declarations. So Unison will not stop us providing a Direction where a Suit is expected.

The unique keyword solves this problem:

[...]

When compiling these declarations, Unison will generate a universally unique identifier for the type and use that identifier when generating the hash for the type. As a result, the type gets a hash that is universally unique.

# Structural types · Unison programming language
URL: https://www.unison-lang.org/docs/language-reference/structural-types/

The structural keyword specifies that the type being defined is a structural type. Structural types may have different names but are equivalent if they share the same data constructor and type constructor structure.
