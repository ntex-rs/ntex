/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a field.
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

/// An implementation of [`crate::Service::poll`] that forwards poll call to a field.
#[macro_export]
macro_rules! forward_poll {
    ($field:ident) => {
        #[inline]
        fn poll(&self, cx: &mut std::task::Context<'_>) -> Result<(), Self::Error> {
            self.$field.poll(cx).map_err(From::from)
        }
    };
    ($field:ident, $err:expr) => {
        #[inline]
        fn poll(&self, cx: &mut std::task::Context<'_>) -> Result<(), Self::Error> {
            self.$field.poll(cx).map_err($err)
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
