/// Forwards [`crate::Service::bind`], [`crate::Service::ready`], and
/// [`crate::Service::shutdown`] to inner service fields.
///
/// This is the recommended way to write a wrapper service. For a
/// [`crate::Pipeline`] field implement `bind` with [`crate::Pipeline::bind_to`].
///
/// - `forward!(St, field)`: forwards to a service field, readiness errors are
///   converted with [`Into`].
/// - `forward!(St, a, b, ...)`: forwards to several service fields. Readiness
///   completes once all fields are ready, and shutdown waits for all of them.
/// - `forward!(St, field, ..., map_err = f)`: converts readiness errors with `f`.
///
/// `St` is the state type of the `Service<St, Req>` impl, and each field is a
/// named or tuple field such as `inner` or `0`. Call the inner services with
/// [`crate::Ctx::call`].
///
/// ```
/// use ntex_service::{Ctx, Service, forward};
///
/// struct Log<S>(S);
///
/// impl<S, St, Req> Service<St, Req> for Log<S>
/// where
///     S: Service<St, Req>,
/// {
///     type Res = S::Res;
///     type Error = S::Error;
///
///     forward!(St, 0);
///
///     async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
///         println!("call");
///         ctx.call(&self.0, req).await
///     }
/// }
///
/// struct Both<A, B> {
///     first: A,
///     second: B,
/// }
///
/// impl<A, B, St, Req> Service<St, Req> for Both<A, B>
/// where
///     A: Service<St, Req, Res = Req>,
///     B: Service<St, Req, Error = A::Error>,
/// {
///     type Res = B::Res;
///     type Error = String;
///
///     forward!(St, first, second, map_err = |_| "not ready".to_string());
///
///     async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<B::Res, String> {
///         let req = ctx.call(&self.first, req).await.map_err(|_| "first".to_string())?;
///         ctx.call(&self.second, req).await.map_err(|_| "second".to_string())
///     }
/// }
/// ```
#[macro_export]
macro_rules! forward {
    (@go $st:ty; [$field:tt]; map_err = $err:expr $(,)?) => {
        #[inline]
        fn bind(&self, ctx: $crate::BindCtx<'_>) {
            ctx.bind::<_, $st, _>(&self.$field);
        }

        #[inline]
        async fn ready(&self, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&self.$field).await.map_err($err)
        }

        #[inline]
        async fn shutdown(&self, ctx: $crate::Ctx<'_, Self, $st>) {
            ctx.shutdown(&self.$field).await
        }
    };
    (@go $st:ty; [$($field:tt)+]; map_err = $err:expr $(,)?) => {
        #[inline]
        fn bind(&self, ctx: $crate::BindCtx<'_>) {
            $(ctx.bind::<_, $st, _>(&self.$field);)+
        }

        async fn ready(&self, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            $crate::forward!(@ready ctx, self, $err; $($field)+)
                .await
                .map(|_| ())
        }

        async fn shutdown(&self, ctx: $crate::Ctx<'_, Self, $st>) {
            $crate::forward!(@shutdown ctx, self; $($field)+).await
        }
    };
    (@go $st:ty; [$($field:tt)+]; ) => {
        $crate::forward!(@go $st; [$($field)+]; map_err = ::core::convert::Into::into);
    };
    (@go $st:ty; [$($field:tt)*]; $next:tt $(, $($rest:tt)*)?) => {
        $crate::forward!(@go $st; [$($field)* $next]; $($($rest)*)?);
    };
    (@ready $ctx:ident, $this:tt, $err:expr; $field:tt) => {
        $crate::__private::ready_field($ctx, &$this.$field, $err)
    };
    (@ready $ctx:ident, $this:tt, $err:expr; $field:tt $($rest:tt)+) => {
        $crate::__private::ready_join(
            $ctx,
            || $crate::__private::ready_field($ctx, &$this.$field, $err),
            || $crate::forward!(@ready $ctx, $this, $err; $($rest)+),
        )
    };
    (@shutdown $ctx:ident, $this:tt; $field:tt) => {
        $ctx.shutdown(&$this.$field)
    };
    (@shutdown $ctx:ident, $this:tt; $field:tt $($rest:tt)+) => {
        $crate::__private::join(
            $ctx.shutdown(&$this.$field),
            $crate::forward!(@shutdown $ctx, $this; $($rest)+),
        )
    };
    ($st:ty, $($rest:tt)+) => {
        $crate::forward!(@go $st; []; $($rest)+);
    };
}

/// Forwards [`crate::Service::bind`] to one or more service fields.
///
/// For a single inner service prefer [`crate::forward!`].
/// For additional sources or mixed pipeline/service children, implement `bind`
/// explicitly using [`crate::BindCtx`].
#[doc(hidden)]
#[macro_export]
macro_rules! forward_bind {
    ($st:ty, $($field:tt),+ $(,)?) => {
        #[inline]
        fn bind(&self, ctx: $crate::BindCtx<'_>) {
            $(ctx.bind::<_, $st, _>(&self.$field);)+
        }
    };
    ($st:ty, $this:ident.$field:tt) => {
        #[inline]
        fn bind(&$this, ctx: $crate::BindCtx<'_>) {
            ctx.bind::<_, $st, _>(&$this.$field);
        }
    };
}

/// An implementation of [`crate::Service::ready`] that forwards readiness checks to a field.
///
/// Preserves the calling context and any wrapped [`crate::ReadyBinding`]'s
/// readiness. A raw service field is checked on each forwarded evaluation.
/// The field must also be wired in `bind`.
#[doc(hidden)]
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
    ($st:ty, $this:ident.$field:tt) => {
        #[inline]
        async fn ready(&$this, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&$this.$field)
                .await
                .map_err(::core::convert::Into::into)
        }
    };
    ($st:ty, $this:ident.$field:tt, $err:expr) => {
        #[inline]
        async fn ready(&$this, ctx: $crate::Ctx<'_, Self, $st>) -> Result<(), Self::Error> {
            ctx.ready(&$this.$field).await.map_err($err)
        }
    };
}

/// An implementation of [`crate::Service::shutdown`] that forwards shutdown checks to a field.
#[doc(hidden)]
#[macro_export]
macro_rules! forward_shutdown {
    ($st:ty, $field:ident) => {
        #[inline]
        async fn shutdown(&self, ctx: $crate::Ctx<'_, Self, $st>) {
            ctx.shutdown(&self.$field).await
        }
    };
    ($st:ty, $this:ident.$field:tt) => {
        #[inline]
        async fn shutdown(&$this, ctx: $crate::Ctx<'_, Self, $st>) {
            ctx.shutdown(&$this.$field).await
        }
    };
}
