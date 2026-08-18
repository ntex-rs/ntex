//! Contains `Variant` service and related types and functions.
#![allow(non_snake_case)]
use std::{fmt, marker::PhantomData, task::Poll};

use ntex_service::{Ctx, IntoServiceFactory, ReadyCtx, Service, ServiceFactory};

/// Construct `Variant` service factory.
///
/// Variant service allow to combine multiple different services into a single service.
pub fn variant<V1: ServiceFactory<V1R, St>, St, V1R>(
    f: impl IntoServiceFactory<V1, St, V1R>,
) -> Variant<V1, St, V1R> {
    Variant {
        factory: f.into_factory(),
        _t: PhantomData,
    }
}

/// Combine multiple different service types into a single service.
pub struct Variant<A, St, AR> {
    factory: A,
    _t: PhantomData<(St, AR)>,
}

impl<A, St, AR> Variant<A, St, AR>
where
    A: ServiceFactory<AR, St>,
{
    /// Convert to a Variant with two request types
    pub fn v2<B, BR>(
        self,
        f: impl IntoServiceFactory<B, St, BR>,
    ) -> VariantFactory2<St, A, B, AR, BR>
    where
        B: ServiceFactory<
                BR,
                St,
                Res = A::Res,
                Error = A::Error,
                InitCfg = A::InitCfg,
                InitError = A::InitError,
            >,
    {
        VariantFactory2 {
            V1: self.factory,
            V2: f.into_factory(),
            _t: PhantomData,
        }
    }
}

impl<A, St, AR> fmt::Debug for Variant<A, St, AR>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Variant")
            .field("V1", &self.factory)
            .finish()
    }
}

macro_rules! variant_impl_and ({$fac1_type:ident, $fac2_type:ident, $name:ident, $r_name:ident, $m_name:ident, ($($T:ident),+), ($($R:ident),+)} => {

    #[allow(non_snake_case)]
    impl<St, V1, $($T,)+ V1R, $($R,)+> $fac1_type<St, V1, $($T,)+ V1R, $($R,)+>
        where
            V1: ServiceFactory<V1R, St>,
        {
            /// Convert to a Variant with more request types
            pub fn $m_name<$name, $r_name, F>(self, factory: F) -> $fac2_type<St, V1, $($T,)+ $name, V1R, $($R,)+ $r_name>
            where $name: ServiceFactory<$r_name,
                    St,
                    Res = V1::Res,
                    Error = V1::Error,
                    InitCfg = V1::InitCfg,
                    InitError = V1::InitError>,
                  F: IntoServiceFactory<$name, St, $r_name>,
            {
                $fac2_type {
                    V1: self.V1,
                    $($T: self.$T,)+
                    $name: factory.into_factory(),
                    _t: PhantomData
                }
            }
    }
});

macro_rules! variant_impl ({$mod_name:ident, $enum_type:ident, $srv_type:ident, $fac_type:ident, $num:literal, $(($n:tt, $T:ident, $R:ident)),+} => {

    #[allow(non_snake_case, missing_debug_implementations)]
    pub enum $enum_type<V1R, $($R),+> {
        V1(V1R),
        $($T($R),)+
    }

    #[allow(non_snake_case)]
    pub struct $srv_type<St, V1, $($T,)+ V1R, $($R,)+> {
        V1: V1,
        $($T: $T,)+
        _t: PhantomData<(St, V1R, $($R),+)>,
    }

    impl<St, V1: Clone, $($T: Clone,)+ V1R, $($R,)+> Clone for $srv_type<St, V1, $($T,)+ V1R, $($R,)+> {
        fn clone(&self) -> Self {
            Self {
                _t: PhantomData,
                V1: self.V1.clone(),
                $($T: self.$T.clone(),)+
            }
        }
    }

    impl<St, V1: fmt::Debug, $($T: fmt::Debug,)+ V1R, $($R,)+> fmt::Debug for $srv_type<St, V1, $($T,)+ V1R, $($R,)+> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct(stringify!($srv_type))
                .field("V1", &self.V1)
                $(.field(stringify!($T), &self.$T))+
                .finish()
        }
    }

    impl<St, V1, $($T,)+ V1R, $($R,)+> Service<St> for $srv_type<St, V1, $($T,)+ V1R, $($R,)+>
    where
        V1: Service<St, Req = V1R>,
        $($T: Service<St, Req = $R, Res = V1::Res, Error = V1::Error>),+
    {
        type Req = $enum_type<V1R, $($R,)+>;
        type Res = V1::Res;
        type Error = V1::Error;

        async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
            use std::{future::Future, pin::Pin};

            let mut fut1 = ::std::pin::pin!(ctx.ready(&self.V1));
            $(let mut $T = ::std::pin::pin!(ctx.ready(&self.$T));)+

            let mut ready: [bool; $num] = [false; $num];

            ::std::future::poll_fn(|cx| {
                if !ready[$num-1] {
                    ready[$num-1] = Pin::new(&mut fut1).poll(cx)?.is_ready();
                }
                $(if !ready[$n] {
                    ready[$n] = Pin::new(&mut $T).poll(cx)?.is_ready();
                })+;

                for v in &ready[..] {
                    if !v {
                        return Poll::Pending
                    }
                }
                Poll::Ready(Ok(()))
            }).await
        }

        async fn call(&self, req: $enum_type<V1R, $($R,)+>, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
            match req {
                $enum_type::V1(req) => ctx.call(&self.V1, req).await,
                $($enum_type::$T(req) => ctx.call(&self.$T, req).await,)+
            }
        }

        async fn shutdown(&self) {
            self.V1.shutdown().await;
            $(self.$T.shutdown().await;)+
        }
    }

    #[allow(non_snake_case)]
    pub struct $fac_type<St, V1, $($T,)+ V1R, $($R,)+> {
        V1: V1,
        $($T: $T,)+
        _t: PhantomData<(St, V1R, $($R,)+)>,
    }

    impl<St, V1: Clone, $($T: Clone,)+ V1R, $($R,)+> Clone for $fac_type<St, V1, $($T,)+ V1R, $($R,)+> {
        fn clone(&self) -> Self {
            Self {
                _t: PhantomData,
                V1: self.V1.clone(),
                $($T: self.$T.clone(),)+
            }
        }
    }

    impl<St, V1: fmt::Debug, $($T: fmt::Debug,)+ V1R, $($R,)+> fmt::Debug for $fac_type<St, V1, $($T,)+ V1R, $($R,)+> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Variant")
                .field("V1", &self.V1)
                $(.field(stringify!($T), &self.$T))+
                .finish()
        }
    }

    impl<St, V1, $($T,)+ V1R, $($R,)+> ServiceFactory<$enum_type<V1R, $($R),+>, St> for $fac_type<St, V1, $($T,)+ V1R, $($R,)+>
    where
        V1: ServiceFactory<V1R, St>,
        $($T: ServiceFactory<$R, St, Res = V1::Res, Error = V1::Error, InitCfg = V1::InitCfg, InitError = V1::InitError>),+
    {
        type Res = V1::Res;
        type Error = V1::Error;
        type Service = $srv_type<St, V1::Service, $($T::Service,)+ V1R, $($R,)+>;
        type InitCfg = V1::InitCfg;
        type InitError = V1::InitError;

        async fn create(&self, cfg: &V1::InitCfg) -> Result<Self::Service, Self::InitError> {
            Ok($srv_type {
                V1: self.V1.create(cfg).await?,
                $($T: self.$T.create(cfg).await?,)+
                _t: PhantomData
            })
        }
    }
});

#[rustfmt::skip]
variant_impl!(v2, Variant2, VariantService2, VariantFactory2, 2, (0, V2, V2R));
#[rustfmt::skip]
variant_impl!(v3, Variant3, VariantService3, VariantFactory3, 3, (0, V2, V2R), (1, V3, V3R));
#[rustfmt::skip]
variant_impl!(v4, Variant4, VariantService4, VariantFactory4, 4, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R));
#[rustfmt::skip]
variant_impl!(v5, Variant5, VariantService5, VariantFactory5, 5, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R));
#[rustfmt::skip]
variant_impl!(v6, Variant6, VariantService6, VariantFactory6, 6, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R), (4, V6, V6R));
#[rustfmt::skip]
variant_impl!(v7, Variant7, VariantService7, VariantFactory7, 7, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R), (4, V6, V6R), (5, V7, V7R));
#[rustfmt::skip]
variant_impl!(v8, Variant8, VariantService8, VariantFactory8, 8, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R), (4, V6, V6R), (5, V7, V7R), (6, V8, V8R));

#[rustfmt::skip]
variant_impl_and!(VariantFactory2, VariantFactory3, V3, V3R, v3, (V2), (V2R));
#[rustfmt::skip]
variant_impl_and!(VariantFactory3, VariantFactory4, V4, V4R, v4, (V2, V3), (V2R, V3R));
#[rustfmt::skip]
variant_impl_and!(VariantFactory4, VariantFactory5, V5, V5R, v5, (V2, V3, V4), (V2R, V3R, V4R));
#[rustfmt::skip]
variant_impl_and!(VariantFactory5, VariantFactory6, V6, V6R, v6, (V2, V3, V4, V5), (V2R, V3R, V4R, V5R));
#[rustfmt::skip]
variant_impl_and!(VariantFactory6, VariantFactory7, V7, V7R, v7, (V2, V3, V4, V5, V6), (V2R, V3R, V4R, V5R, V6R));
#[rustfmt::skip]
variant_impl_and!(VariantFactory7, VariantFactory8, V8, V8R, v8, (V2, V3, V4, V5, V6, V7), (V2R, V3R, V4R, V5R, V6R, V7R));

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use ntex_service::{Pipeline, fn_factory, fn_service};

    use super::*;
    use crate::time;

    #[derive(Debug, Clone)]
    struct Srv1;

    impl Service for Srv1 {
        type Req = ();
        type Res = usize;
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn shutdown(&self) {}

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<usize, ()> {
            Ok(1)
        }
    }

    #[derive(Debug, Clone)]
    struct Srv2;

    impl Service for Srv2 {
        type Req = ();
        type Res = usize;
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn shutdown(&self) {}

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<usize, ()> {
            Ok(2)
        }
    }

    #[ntex::test]
    async fn test_variant() {
        let factory = variant(fn_factory(|| async { Ok::<_, ()>(Srv1) }));
        assert!(format!("{factory:?}").contains("Variant"));

        let factory = factory
            .v2(fn_factory(|| async { Ok::<_, ()>(Srv2) }))
            .clone()
            .v3(fn_factory(|| async { Ok::<_, ()>(Srv2) }))
            .clone();

        let service = factory.pipeline(&()).await.unwrap();
        assert!(format!("{service:?}").contains("Variant"));

        assert!(service.ready().await.is_ok());
        service.shutdown().await;

        assert_eq!(service.call(Variant3::V1(())).await, Ok(1));
        assert_eq!(service.call(Variant3::V2(())).await, Ok(2));
        assert_eq!(service.call(Variant3::V3(())).await, Ok(2));
    }

    #[ntex::test]
    async fn test_variant_readiness() {
        #[derive(Debug, Clone)]
        struct Srv5;

        impl Service for Srv5 {
            type Req = ();
            type Res = usize;
            type Error = ();
            async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
                time::sleep(time::Millis(50)).await;
                time::sleep(time::Millis(50)).await;
                time::sleep(time::Millis(50)).await;
                time::sleep(time::Millis(50)).await;
                Ok(())
            }
            async fn shutdown(&self) {}
            async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<usize, ()> {
                Ok(2)
            }
        }

        let factory = variant(fn_service(async |()| Ok::<_, ()>(0)))
            .v2(fn_factory(async || Ok::<_, ()>(Srv5)).map_init_err(|()| unreachable!()))
            .v3(fn_service(async |()| Ok::<_, ()>(2)));
        assert!(format!("{factory:?}").contains("Variant"));

        let service = factory.clone().create(&()).await.unwrap().clone();
        assert!(format!("{service:?}").contains("Variant"));

        let service = Pipeline::new(factory.create(&()).await.unwrap());
        assert!(service.ready().await.is_ok());
        assert!(format!("{service:?}").contains("Variant"));
    }
}
