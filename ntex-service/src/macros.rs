/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a field.
///
/// Preserves the calling context and any wrapped [`crate::ServiceBinding`]'s
/// readiness. A raw service field is checked on each forwarded evaluation.
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

/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a
/// [`crate::Pipeline`] or [`crate::pipeline::PipelineBinding`] field.
///
/// Links the enclosing readiness scope to the field's shared readiness before
/// checking it, so changes also invalidate the enclosing service's cache.
///
/// ```
/// use ntex_service::{Ctx, Pipeline, Service, forward_pl_ready, forward_pl_shutdown};
///
/// struct Forward {
///     inner: Pipeline<usize, usize, ()>,
/// }
///
/// impl Service<(), usize> for Forward {
///     type Res = usize;
///     type Error = ();
///
///     forward_pl_ready!((), inner);
///     forward_pl_shutdown!((), inner);
///
///     async fn call(&self, req: usize, _: Ctx<'_, Self>) -> Result<usize, ()> {
///         self.inner.call(req).await
///     }
/// }
/// ```
#[macro_export]
macro_rules! forward_pl_ready {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn ready(&self, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            self.$field.__register_ready_dependency(ctx);
            self.$field
                .ready()
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($st:ty, $field:ident, $err:expr) => {
        #[inline]
        async fn ready(&self, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            self.$field.__register_ready_dependency(ctx);
            self.$field.ready().await.map_err($err)
        }
    };
}

/// An implementation of [`crate::Service::shutdown`] that shuts down a
/// [`crate::Pipeline`] field.
///
/// Retains the dependency on the field's readiness, including subscriptions
/// established during shutdown.
#[macro_export]
macro_rules! forward_pl_shutdown {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn shutdown(&self, ctx: $crate::Ctx<'_, Self, $st>) {
            self.$field.__register_ready_dependency(ctx);
            self.$field.shutdown().await
        }
    };
}
