type A = unsafe<'a> &'a ();
type B = unsafe<'a, 'b> &'a &'b ();
type C = unsafe<> u8;
