pub mod small {
    pub struct Small(pub u8);
}

#[cfg(feature = "big")]
pub mod big {
    pub struct Big(pub u64);
}

pub enum Kind {
    Small(small::Small),
    #[cfg(feature = "big")]
    Big(big::Big),
}

macro_rules! with_inner {
    ($inner:expr, |$var:ident| $body:expr) => {
        match $inner {
            Kind::Small(ref $var) => $body,
            #[cfg(feature = "big")]
            Kind::Big(ref $var) => $body,
        }
    };
}

impl Kind {
    pub fn size(&self) -> usize {
        with_inner!(self, |it| std::mem::size_of_val(&it.0))
    }
}
