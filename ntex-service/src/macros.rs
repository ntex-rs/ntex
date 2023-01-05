/// An implementation of [`poll_ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_poll_ready {
    ($field:ident) => {
        #[inline]
        fn poll_ready(
            &self,
            cx: &mut ::core::task::Context<'_>,
        ) -> ::core::task::Poll<Result<(), Self::Error>> {
            self.$field
                .poll_ready(cx)
                .map_err(::core::convert::Into::into)
        }
    };
    ($field:ident, $err:expr) => {
        #[inline]
        fn poll_ready(
            &self,
            cx: &mut ::core::task::Context<'_>,
        ) -> ::core::task::Poll<Result<(), Self::Error>> {
            self.$field.poll_ready(cx).map_err($err)
        }
    };
}

/// An implementation of [`poll_shutdown`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_poll_shutdown {
    ($field:ident) => {
        #[inline]
        fn poll_shutdown(
            &self,
            cx: &mut ::core::task::Context<'_>,
        ) -> ::core::task::Poll<()> {
            self.$field.poll_shutdown(cx)
        }
    };
}
