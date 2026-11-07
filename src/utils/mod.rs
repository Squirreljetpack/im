mod percentage;
pub use percentage::*;

cba::define_either! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Either<L, R = L> {
        Left,
        Right
    }
}
