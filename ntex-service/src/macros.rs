/// An implementation of [`ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_ready {
    ($field:ident) => {
        #[inline]
        async fn ready(
            &self,
        ) -> Option<impl ::std::future::Future<Output = Result<(), Self::Error>>> {
            self.$field.ready().await
        }
    };
}

/// An implementation of [`shutdown`] that forwards shutdown checks to a field.
#[macro_export]
macro_rules! forward_shutdown {
    ($field:ident) => {
        #[inline]
        async fn shutdown(&self) {
            self.$field.shutdown().await
        }
    };
}
