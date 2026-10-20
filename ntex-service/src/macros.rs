/// Forwards [`crate::Service::bind`] to one or more service fields.
///
/// Use alongside [`crate::forward_ready!`] and [`crate::forward_shutdown!`].
/// For additional sources or mixed pipeline/service children, implement `bind`
/// explicitly using [`crate::BindCtx`].
#[macro_export]
macro_rules! forward_bind {
    ($st:ty, $($field:tt),+ $(,)?) => {
        #[inline]
        fn bind(&self, ctx: $crate::BindCtx<'_>) {
            $(ctx.bind::<_, $st, _>(&self.$field);)+
        }
    };
}

/// Forwards [`crate::Service::bind`] to pipeline or pipeline-binding fields,
/// including state-taking pipelines.
///
/// Calls each field's [`bind_to`](crate::Pipeline::bind_to) method.
#[macro_export]
macro_rules! forward_pl_bind {
    ($st:ty, $($field:tt),+ $(,)?) => {
        #[inline]
        fn bind(&self, ctx: $crate::BindCtx<'_>) {
            $(self.$field.bind_to(&ctx);)+
        }
    };
}

/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a field.
///
/// Preserves the calling context and any wrapped [`crate::ServiceBinding`]'s
/// readiness. A raw service field is checked on each forwarded evaluation.
/// The field must also be wired in `bind`, for example with [`crate::forward_bind!`].
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
/// Use [`crate::forward_pl_bind!`] to wire the dependency during initialization.
///
/// ```
/// use ntex_service::{Ctx, Pipeline, Service, forward_pl_bind, forward_pl_ready, forward_pl_shutdown};
///
/// struct Forward {
///     inner: Pipeline<usize, usize, ()>,
/// }
///
/// impl Service<(), usize> for Forward {
///     type Res = usize;
///     type Error = ();
///
///     forward_pl_bind!((), inner);
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

/// An implementation of [`crate::Service::shutdown`] that shuts down a
/// [`crate::Pipeline`] field.
///
/// Wire the field with [`crate::forward_pl_bind!`], even for shutdown-only use.
#[macro_export]
macro_rules! forward_pl_shutdown {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn shutdown(&self, _: $crate::Ctx<'_, Self, $st>) {
            self.$field.shutdown().await
        }
    };
}
