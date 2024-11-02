/// An implementation of [`ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_state {
    ($field:ident) => {
        #[inline]
        async fn state(
            &self,
            st: $crate::ServiceState,
            ctx: $crate::ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            ctx.state(&self.$field, st)
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($field:ident, $err:expr) => {
        #[inline]
        async fn state(
            &self,
            st: $crate::ServiceState,
            ctx: $crate::ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            ctx.state(&self.$field, st).await.map_err($err)
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
