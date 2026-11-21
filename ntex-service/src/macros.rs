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
        fn ready(
            &self,
            ctx: $crate::Ctx<'_, Self, $st>,
        ) -> impl ::core::future::Future<Output = Result<(), Self::Error>> {
            $crate::__private::map_output(ctx.ready(&self.$field), |res| res.map_err($err))
        }

        #[inline]
        fn shutdown(
            &self,
            ctx: $crate::Ctx<'_, Self, $st>,
        ) -> impl ::core::future::Future<Output = ()> {
            ctx.shutdown(&self.$field)
        }
    };
    (@go $st:ty; [$($field:tt)+]; map_err = $err:expr $(,)?) => {
        #[inline]
        fn bind(&self, ctx: $crate::BindCtx<'_>) {
            $(ctx.bind::<_, $st, _>(&self.$field);)+
        }

        fn ready(
            &self,
            ctx: $crate::Ctx<'_, Self, $st>,
        ) -> impl ::core::future::Future<Output = Result<(), Self::Error>> {
            $crate::__private::map_output(
                $crate::forward!(@ready ctx, self, $err; $($field)+),
                |res| res.map(|_| ()),
            )
        }

        fn shutdown(
            &self,
            ctx: $crate::Ctx<'_, Self, $st>,
        ) -> impl ::core::future::Future<Output = ()> {
            $crate::forward!(@shutdown ctx, self; $($field)+)
        }
    };
    (@go $st:ty; []; map_err $($rest:tt)*) => {
        ::core::compile_error!(
            "forward!: list at least one service field before `map_err`, e.g. `forward!(St, service, map_err = f)`"
        );
    };
    (@go $st:ty; [$($field:tt)*]; self . $($rest:tt)*) => {
        ::core::compile_error!(
            "forward!: write field names without `self.`, e.g. `forward!(St, service)`"
        );
    };
    (@go $st:ty; [$($field:tt)*]; $next:tt . $($rest:tt)*) => {
        ::core::compile_error!(
            "forward!: nested field paths are not supported, implement `bind`, `ready`, and `shutdown` manually"
        );
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
            move |$ctx| $crate::__private::ready_field($ctx, &$this.$field, $err),
            move |$ctx| $crate::forward!(@ready $ctx, $this, $err; $($rest)+),
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
    ($one:tt $(,)?) => {
        ::core::compile_error!(
            "forward!: expected a state type and at least one service field, e.g. `forward!(St, service)`"
        );
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
        $crate::forward_ready!($st, self.$field, ::core::convert::Into::into);
    };
    ($st:ty, $field:ident, $err:expr) => {
        $crate::forward_ready!($st, self.$field, $err);
    };
    ($st:ty, $this:ident.$field:tt) => {
        $crate::forward_ready!($st, $this.$field, ::core::convert::Into::into);
    };
    ($st:ty, $this:ident.$field:tt, $err:expr) => {
        #[inline]
        fn ready(
            &$this,
            ctx: $crate::Ctx<'_, Self, $st>,
        ) -> impl ::core::future::Future<Output = Result<(), Self::Error>> {
            $crate::__private::map_output(ctx.ready(&$this.$field), |res| res.map_err($err))
        }
    };
}

/// An implementation of [`crate::Service::shutdown`] that forwards shutdown checks to a field.
#[doc(hidden)]
#[macro_export]
macro_rules! forward_shutdown {
    ($st:ty, $field:ident) => {
        $crate::forward_shutdown!($st, self.$field);
    };
    ($st:ty, $this:ident.$field:tt) => {
        #[inline]
        fn shutdown(
            &$this,
            ctx: $crate::Ctx<'_, Self, $st>,
        ) -> impl ::core::future::Future<Output = ()> {
            ctx.shutdown(&$this.$field)
        }
    };
}
