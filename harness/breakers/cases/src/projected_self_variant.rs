//! rkyv implements deserialization for a projection that normalizes to the archived enum.
trait Archive {
    type Archived;
}
struct Source;
enum Archived {
    Tuple(u8),
    Record { count: u8 },
    Unit,
}
impl Archive for Source {
    type Archived = Archived;
}
type ArchivedOf<T> = <T as Archive>::Archived;

trait Decode {
    fn value(&self) -> u8;
}
impl Decode for ArchivedOf<Source> {
    fn value(&self) -> u8 {
        match self {
            Self::Tuple(value) => *value,
            Self::Record { count } => *count + 1,
            Self::Unit => 0,
        }
    }
}
pub fn run() -> [u8; 3] {
    [Archived::Tuple(17).value(), Archived::Record { count: 23 }.value(), Archived::Unit.value()]
}
