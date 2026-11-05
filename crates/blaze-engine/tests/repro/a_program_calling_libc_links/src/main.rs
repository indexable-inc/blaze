fn main() {
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    println!("{}", pid > 0);
}
