//! [`in_key_order!`](crate::in_key_order): types whose CBOR is written in the order its map
//! keys sort in, so the encoder never puts a map in order.

/// Defines a struct, or an enum whose every variant has named fields, with a `Serialize` that
/// writes each one's fields in the order `keys [..]` lists when the format is not
/// human-readable (CBOR): the order of their keys' encoding, shorter names first and then
/// bytewise. [`crate::cbor`] then writes every entry where it goes, where a struct declared in
/// another order has its entries found again, sorted and copied twice at its end
/// ([`crate::cbor::Writer::permuted`]). A human-readable format (JSON) gets the fields in the
/// order declared, as serde's derive writes them, so its bytes are unchanged; the CBOR is the
/// same bytes either way, since the encoding sorts the keys.
///
/// Derive everything else (`Deserialize` too) as usual. A field takes no `#[serde]`
/// attribute: this `Serialize` does not read them. The variants' indices are their places in
/// the declaration, as the derive's are.
#[macro_export]
macro_rules! in_key_order {
    (
        $(#[$meta:meta])*
        $vis:vis struct $ty:ident $(<$($param:ident),+>)? {
            $($(#[$field_meta:meta])* $field_vis:vis $field:ident: $field_ty:ty),+ $(,)?
        }
        keys [$($key:ident),+ $(,)?]
    ) => {
        $(#[$meta])*
        $vis struct $ty $(<$($param),+>)? {
            $($(#[$field_meta])* $field_vis $field: $field_ty),+
        }

        impl $(<$($param: $crate::__serde::Serialize),+>)? $crate::__serde::Serialize
            for $ty $(<$($param),+>)?
        {
            fn serialize<S: $crate::__serde::Serializer>(
                &self,
                to: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                use $crate::__serde::ser::SerializeStruct as _;
                let $ty { $($field),+ } = self;
                let declared = to.is_human_readable();
                let len = <[&str]>::len(&[$(stringify!($field)),+]);
                let mut out = to.serialize_struct(stringify!($ty), len)?;
                if declared {
                    $(out.serialize_field(stringify!($field), $field)?;)+
                } else {
                    $(out.serialize_field(stringify!($key), $key)?;)+
                }
                out.end()
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $ty:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident {
                    $($(#[$field_meta:meta])* $field:ident: $field_ty:ty),* $(,)?
                } keys [$($key:ident),* $(,)?]
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis enum $ty {
            $($(#[$variant_meta])* $variant { $($(#[$field_meta])* $field: $field_ty),* }),+
        }

        impl $crate::__serde::Serialize for $ty {
            fn serialize<S: $crate::__serde::Serializer>(
                &self,
                to: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                use $crate::__serde::ser::SerializeStructVariant as _;
                // The variants in the order declared, so each one's discriminant is its index.
                #[allow(dead_code)]
                enum Index {
                    $($variant),+
                }
                let declared = to.is_human_readable();
                match self {
                    $($ty::$variant { $($field),* } => {
                        let len = <[&str]>::len(&[$(stringify!($field)),*]);
                        let mut out = to.serialize_struct_variant(
                            stringify!($ty),
                            Index::$variant as u32,
                            stringify!($variant),
                            len,
                        )?;
                        if declared {
                            $(out.serialize_field(stringify!($field), $field)?;)*
                        } else {
                            $(out.serialize_field(stringify!($key), $key)?;)*
                        }
                        out.end()
                    })+
                }
            }
        }
    };
}
