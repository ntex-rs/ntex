/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_ready {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn ready(&self, ctx: $crate::ReadyCtx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&self.$field)
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($st:ty, $field:ident, $err:expr) => {
        #[inline]
        async fn ready(&self, ctx: $crate::ReadyCtx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&self.$field).await.map_err($err)
        }
    };
}

/// An implementation of [`crate::Service::shutdown`] that forwards shutdown checks to a field.
#[macro_export]
macro_rules! forward_shutdown {
    ($field:ident) => {
        #[inline]
        async fn shutdown(&self) {
            self.$field.shutdown().await
        }
    };
}
