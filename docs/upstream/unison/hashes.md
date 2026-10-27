# Hashes · Unison programming language
URL: https://www.unison-lang.org/docs/language-reference/hashes/

A hash in Unison is a 512-bit SHA3 digest of a term or a type's internal structure, excluding all names. The textual representation of a hash is its base32Hex Unicode encoding.

Unison attributes a hash to every term and type declaration, and the hash may be used to unambiguously refer to that term or type in all contexts. As far as Unison is concerned, the hash of a term or type is its true name.

## Literal Hash References

A term, type, data constructor, or ability constructor may be unambiguously referenced by hash. Literal hash references have the following structure:

- A term definition has a hash of the form `#x` where `x` is the base32Hex encoding of the hash of the term. For example `#a0v829`.
- A term or type definition that’s part of a cycle of mutually recursive definitions hashes to the form `#x.n` where `x` is the hash of the cycle and `n` is the term or type’s index in its cycle. A cycle has a canonical order determined by sorting all the members of the cycle by their individual hashes (with the cycle removed).
- A data constructor hashes to the form `#x#c` where `x` is the hash of the data type definition and `c` is the index of that data constructor in the type definition.
- A data constructor in a cyclic type definition hashes to the form `#x.n#c` where `#x.n` is the hash of the data type and `c` is the data constructor’s index in the type definition.
- A built-in reference to a Unison built-in term or type `n` has a hash of the form `##n`. `##Nat` is an example of a built-in reference.
