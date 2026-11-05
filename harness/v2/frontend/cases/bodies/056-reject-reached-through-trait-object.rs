//@ expect: reject E0382
trait Run {
    fn run(&self) -> usize;
}

struct Job;

impl Run for Job {
    fn run(&self) -> usize {
        let s = String::from("job");
        drop(s);
        s.len() //~ ERROR E0382
    }
}

fn main() {
    let job: Box<dyn Run> = Box::new(Job);
    println!("{}", job.run());
}
