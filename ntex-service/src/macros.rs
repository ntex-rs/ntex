/// An implementation of [`ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_ready {
    ($field:ident) => {
        #[inline]
        async fn ready(
            &self,
            ctx: $crate::ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            ctx.ready(&self.$field)
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($field:ident, $err:expr) => {
        #[inline]
        async fn ready(
            &self,
            ctx: $crate::ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            ctx.ready(&self.$field).await.map_err($err)
        }
    };
}

/// An implementation of [`unready`] that forwards unready checks to a field.
#[macro_export]
macro_rules! forward_unready {
    ($field:ident) => {
        #[inline]
        async fn unready(&self) -> Result<(), Self::Error> {
            self.$field
                .unready()
                .await
                .map_err(::core::convert::Into::into)
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
