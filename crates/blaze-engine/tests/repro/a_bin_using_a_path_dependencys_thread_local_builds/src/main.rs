fn main() {
    let guard = blaze_reclaim::enter();
    println!("{}", blaze_reclaim::in_task());
    drop(guard);
}
