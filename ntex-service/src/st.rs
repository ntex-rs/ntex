#[derive(Debug)]
pub enum FromStResult<'a, T> {
    Ref(&'a T),
    Owned(T),
}

pub trait FromSt<T>: Sized {
    fn from_state(st: &T) -> FromStResult<'_, Self>;
}

impl<T> FromSt<T> for T {
    #[inline]
    fn from_state(st: &T) -> FromStResult<'_, T> {
        FromStResult::Ref(st)
    }
}
