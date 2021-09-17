use crate::Transform;

/// Use function as transform service
pub fn fn_transform<F>(f: F) -> FnTransform<F> {
    FnTransform(f)
}

pub struct FnTransform<F>(F);

impl<S, F, Out> Transform<S> for FnTransform<F>
where
    F: Fn(S) -> Out,
{
    type Service = Out;

    fn new_transform(&self, service: S) -> Self::Service {
        (self.0)(service)
    }
}

impl<F: Clone> Clone for FnTransform<F> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
