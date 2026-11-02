// A test fixture, not a module: `dropped_bound_tests` borrow checks it.
//
// `lifetime_expansion` is cve-rs's `src/lifetime_expansion.rs` as published (cve-rs 0.7.0,
// https://github.com/Speykious/cve-rs at 54ba036d, under its GLWTS Public License, which lets
// anyone copy and modify it), its code unchanged and its doc comments shortened. Below it, the
// lines of cve-rs's `transmute`, `use_after_free` and `buffer_overflow` modules that reach
// rust-lang/rust#25860, each through `expand` or `expand_mut`, cut down to what minicore has
// (no `Box`, `std::io` or `black_box`). Those three borrow check on their own, as they trust
// `expand_mut`'s signature; what refuses them is the error in `expand` and `expand_mut`.

mod lifetime_expansion {
	//! Domain expansion: `'static` lifetime
	//!
	//! See <https://github.com/rust-lang/rust/issues/25860> for this bug's bug report.

	/// Converts lifetime `'b` to lifetime `'a`.
	#[inline(never)]
	pub const fn lifetime_translator<'a, 'b, T: ?Sized>(_val_a: &'a &'b (), val_b: &'b T) -> &'a T {
		val_b
	}

	/// This does the same thing as [`lifetime_translator`], just for mutable refs.
	#[inline(never)]
	pub fn lifetime_translator_mut<'a, 'b, T: ?Sized>(
		_val_a: &'a &'b (),
		val_b: &'b mut T,
	) -> &'a mut T {
		val_b
	}

	/// Expands the domain of `'a` to `'b`.
	pub fn expand<'a, 'b, T: ?Sized>(x: &'a T) -> &'b T {
		let f: for<'x> fn(_, &'x T) -> &'b T = lifetime_translator;
		f(STATIC_UNIT, x)
	}

	/// This does the same thing as [`expand`] for mutable references.
	pub fn expand_mut<'a, 'b, T: ?Sized>(x: &'a mut T) -> &'b mut T {
		let f: for<'x> fn(_, &'x mut T) -> &'b mut T = lifetime_translator_mut;
		f(STATIC_UNIT, x)
	}

	/// A unit with a static lifetime.
	pub const STATIC_UNIT: &&() = &&();
}

mod transmute {
	#[allow(dead_code)]
	enum DummyEnum<A, B> {
		A(Option<A>),
		B(Option<B>),
	}

	#[inline(never)]
	fn transmute_inner<A, B>(dummy: &mut DummyEnum<A, B>, obj: A) -> &mut Option<B> {
		let DummyEnum::B(ref_to_b) = dummy else { loop {} };
		let ref_to_b = crate::lifetime_expansion::expand_mut(ref_to_b);
		*dummy = DummyEnum::A(Some(obj));
		ref_to_b
	}
}

mod use_after_free {
	const HORRIBLE_LEN: usize = 64;

	/// Get a static reference to a dropped buffer.
	#[inline(never)]
	fn get_horrible_buffer_mut() -> &'static mut [u8; HORRIBLE_LEN] {
		let mut buffer = [0; HORRIBLE_LEN];
		crate::lifetime_expansion::expand_mut(&mut buffer)
	}
}

mod buffer_overflow {
	/// `construct_fake_string`'s sentinel, a borrow of a dropped local made `'static`.
	fn sentinel() -> &'static [usize; 3] {
		let sentinel = [0usize, 1usize, 2usize];
		crate::lifetime_expansion::expand(&sentinel)
	}
}
