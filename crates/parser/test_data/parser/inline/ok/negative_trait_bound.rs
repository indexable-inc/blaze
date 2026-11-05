fn f<T: !Copy>() where T: ?Sized + !Send {}
