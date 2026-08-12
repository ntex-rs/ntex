use crate::Service;

#[derive(Debug)]
pub enum FromStResult<'a, T> {
    Ref(&'a T),
    Owned(T),
}

pub trait FromSt<T: Service>: Sized
where
    Self: Service,
{
    fn from_state(st: &T::St) -> FromStResult<'_, Self::St>;
}

impl<T: Service, U: Service<St = T::St>> FromSt<T> for U {
    #[inline]
    fn from_state(st: &T::St) -> FromStResult<'_, U::St> {
        FromStResult::Ref(st)
    }
}
