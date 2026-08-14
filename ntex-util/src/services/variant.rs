//! Contains `Variant` service and related types and functions.
#![allow(non_snake_case)]
use std::{fmt, marker::PhantomData, task::Poll};

use ntex_service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

/// Construct `Variant` service factory.
///
/// Variant service allow to combine multiple different services into a single service.
pub fn variant<V1: ServiceFactory<V1R, V1C>, V1R, V1C>(
    factory: V1,
) -> Variant<V1, V1R, V1C> {
    Variant {
        factory,
        _t: PhantomData,
    }
}

/// Combine multiple different service types into a single service.
pub struct Variant<A, AR, AC> {
    factory: A,
    _t: PhantomData<(AR, AC)>,
}

impl<A, AR, AC> Variant<A, AR, AC>
where
    A: ServiceFactory<AR, AC>,
    AC: Clone,
{
    /// Convert to a Variant with two request types
    pub fn v2<B, BR, F>(self, factory: F) -> VariantFactory2<A, AC, B, AR, BR>
    where
        A::Response: Service<AR, Data = A::Data>,
        B: ServiceFactory<BR, AC, Data = A::Data, Error = A::Error>,
        B::Response: Service<
                BR,
                Response = <A::Response as Service<AR>>::Response,
                Error = <A::Response as Service<AR>>::Error,
                Data = A::Data,
            >,
        F: IntoServiceFactory<B, BR, AC>,
    {
        VariantFactory2 {
            V1: self.factory,
            V2: factory.into_factory(),
            _t: PhantomData,
        }
    }
}

impl<A, AR, AC> fmt::Debug for Variant<A, AR, AC>
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
    impl<V1, V1C, $($T,)+ V1R, $($R,)+> $fac1_type<V1, V1C, $($T,)+ V1R, $($R,)+>
        where
            V1: ServiceFactory<V1R, V1C>,
        {
            /// Convert to a Variant with more request types
            pub fn $m_name<$name, $r_name, F>(self, factory: F) -> $fac2_type<V1, V1C, $($T,)+ $name, V1R, $($R,)+ $r_name>
            where V1::Response: Service<V1R, Data = V1::Data>,
                  $name: ServiceFactory<$r_name, V1C, Data = V1::Data, Error = V1::Error>,
                  $name::Response: Service<$r_name,
                    Response = <V1::Response as Service<V1R>>::Response,
                    Error = <V1::Response as Service<V1R>>::Error,
                    Data = V1::Data>,
                  F: IntoServiceFactory<$name, $r_name, V1C>,
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
    pub struct $srv_type<V1, $($T,)+ V1R, $($R,)+> {
        V1: V1,
        $($T: $T,)+
        _t: PhantomData<(V1R, $($R),+)>,
    }

    impl<V1: Clone, $($T: Clone,)+ V1R, $($R,)+> Clone for $srv_type<V1, $($T,)+ V1R, $($R,)+> {
        fn clone(&self) -> Self {
            Self {
                _t: PhantomData,
                V1: self.V1.clone(),
                $($T: self.$T.clone(),)+
            }
        }
    }

    impl<V1: fmt::Debug, $($T: fmt::Debug,)+ V1R, $($R,)+> fmt::Debug for $srv_type<V1, $($T,)+ V1R, $($R,)+> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct(stringify!($srv_type))
                .field("V1", &self.V1)
                $(.field(stringify!($T), &self.$T))+
                .finish()
        }
    }

    impl<V1, $($T,)+ V1R, $($R,)+> Service<$enum_type<V1R, $($R,)+>> for $srv_type<V1, $($T,)+ V1R, $($R,)+>
    where
        V1: Service<V1R>,
        $($T: Service<$R, Response = V1::Response, Error = V1::Error, Data = V1::Data>),+
    {
        type Response = V1::Response;
        type Error = V1::Error;
        type Data = V1::Data;

        async fn ready(&self, data: &Self::Data, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            use std::{future::Future, pin::Pin};

            let mut fut1 = ::std::pin::pin!(ctx.ready(&self.V1, data));
            $(let mut $T = ::std::pin::pin!(ctx.ready(&self.$T, data));)+

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

        fn poll(&self, data: &Self::Data, cx: &mut std::task::Context<'_>) -> Result<(), Self::Error> {
            self.V1.poll(data, cx)?;
            $(self.$T.poll(data, cx)?;)+
            Ok(())
        }

        async fn shutdown(&self, data: &Self::Data) {
            self.V1.shutdown(data).await;
            $(self.$T.shutdown(data).await;)+
        }

        async fn call(&self, req: $enum_type<V1R, $($R,)+>, data: &Self::Data, ctx: ServiceCtx<'_, Self>) -> Result<Self::Response, Self::Error> {
            match req {
                $enum_type::V1(req) => ctx.call(&self.V1, req, data).await,
                $($enum_type::$T(req) => ctx.call(&self.$T, req, data).await,)+
            }
        }
    }

    #[allow(non_snake_case)]
    pub struct $fac_type<V1, V1C, $($T,)+ V1R, $($R,)+> {
        V1: V1,
        $($T: $T,)+
        _t: PhantomData<(V1C, V1R, $($R,)+)>,
    }

    impl<V1: Clone, V1C, $($T: Clone,)+ V1R, $($R,)+> Clone for $fac_type<V1, V1C, $($T,)+ V1R, $($R,)+> {
        fn clone(&self) -> Self {
            Self {
                _t: PhantomData,
                V1: self.V1.clone(),
                $($T: self.$T.clone(),)+
            }
        }
    }

    impl<V1: fmt::Debug, V1C, $($T: fmt::Debug,)+ V1R, $($R,)+> fmt::Debug for $fac_type<V1, V1C, $($T,)+ V1R, $($R,)+> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Variant")
                .field("V1", &self.V1)
                $(.field(stringify!($T), &self.$T))+
                .finish()
        }
    }

    impl<V1, V1C, $($T,)+ V1R, $($R,)+> Service<V1C> for $fac_type<V1, V1C, $($T,)+ V1R, $($R,)+>
    where
        V1: ServiceFactory<V1R, V1C>,
        V1::Response: Service<V1R, Data = V1::Data>,
        V1C: Clone,
        $($T: ServiceFactory<$R, V1C, Data = V1::Data, Error = V1::Error>,
          $T::Response: Service<$R,
            Response = <V1::Response as Service<V1R>>::Response,
            Error = <V1::Response as Service<V1R>>::Error,
            Data = V1::Data>),+
    {
        type Response = $srv_type<V1::Response, $($T::Response,)+ V1R, $($R,)+>;
        type Error = V1::Error;
        type Data = V1::Data;

        async fn call(
            &self,
            cfg: V1C,
            data: &Self::Data,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, Self::Error> {
            Ok($srv_type {
                V1: ctx.call(&self.V1, cfg.clone(), data).await?,
                $($T: ctx.call(&self.$T, cfg.clone(), data).await?,)+
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
    use ntex_service::{fn_factory, fn_service};

    use super::*;
    use crate::time;

    #[derive(Debug, Clone)]
    struct Srv1;

    impl Service<()> for Srv1 {
        type Response = usize;
        type Error = ();
        type Data = ();

        async fn ready(
            &self,
            _: &Self::Data,
            _c: ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn shutdown(&self, _: &Self::Data) {}

        async fn call(
            &self,
            _r: (),
            _: &Self::Data,
            _c: ServiceCtx<'_, Self>,
        ) -> Result<usize, ()> {
            Ok(1)
        }
    }

    #[derive(Debug, Clone)]
    struct Srv2;

    impl Service<()> for Srv2 {
        type Response = usize;
        type Error = ();
        type Data = ();

        async fn ready(
            &self,
            _: &Self::Data,
            _c: ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn shutdown(&self, _: &Self::Data) {}

        async fn call(
            &self,
            _r: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<usize, ()> {
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

        let service = factory.pipeline((), &()).await.unwrap().clone();
        assert!(format!("{service:?}").contains("Variant"));

        assert!(crate::future::lazy(|cx| service.poll(cx)).await.is_ok());
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

        impl Service<()> for Srv5 {
            type Response = usize;
            type Error = ();
            type Data = ();
            async fn ready(
                &self,
                _: &Self::Data,
                _: ServiceCtx<'_, Self>,
            ) -> Result<(), Self::Error> {
                time::sleep(time::Millis(50)).await;
                time::sleep(time::Millis(50)).await;
                time::sleep(time::Millis(50)).await;
                time::sleep(time::Millis(50)).await;
                Ok(())
            }
            async fn shutdown(&self, _: &Self::Data) {}
            async fn call(
                &self,
                _r: (),
                _: &Self::Data,
                _: ServiceCtx<'_, Self>,
            ) -> Result<usize, ()> {
                Ok(2)
            }
        }

        let factory = variant(fn_service(|()| async { Ok::<_, ()>(0) }))
            .v2(fn_factory(|| async { Ok::<_, ()>(Srv5) }))
            .v3(fn_service(|()| crate::future::Ready::Ok::<_, ()>(2)));
        assert!(format!("{factory:?}").contains("Variant"));

        let service = factory.clone().pipeline((), &()).await.unwrap().clone();
        assert!(format!("{service:?}").contains("Variant"));

        let service = factory.pipeline((), &()).await.unwrap().clone();
        assert!(service.ready().await.is_ok());
        assert!(format!("{service:?}").contains("Variant"));
    }
}
