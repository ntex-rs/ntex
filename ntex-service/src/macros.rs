/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_ready {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn ready(&self, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&self.$field)
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($st:ty, $field:ident, $err:expr) => {
        #[inline]
        async fn ready(&self, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&self.$field).await.map_err($err)
        }
    };
}

/// An implementation of [`crate::Service::shutdown`] that forwards shutdown checks to a field.
#[macro_export]
macro_rules! forward_shutdown {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn shutdown(&self, ctx: $crate::Ctx<'_, Self, $st>) {
            ctx.shutdown(&self.$field).await
        }
    };
}

#[macro_export]
macro_rules! forward_pl_ready {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn ready(&self, _: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            self.$field
                .ready()
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($st:ty, $field:ident, $err:expr) => {
        #[inline]
        async fn ready(&self, _: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            self.$field.ready().await.map_err($err)
        }
    };
}

#[macro_export]
macro_rules! forward_pl_shutdown {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn shutdown(&self, _: $crate::Ctx<'_, Self, $st>) {
            self.$field.shutdown().await
        }
    };
}
