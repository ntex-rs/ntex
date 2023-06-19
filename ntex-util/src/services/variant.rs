//! Contains `Variant` service and related types and functions.
use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use ntex_service::{IntoServiceFactory, Service, ServiceCall, ServiceCtx, ServiceFactory};

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
        B: ServiceFactory<
            BR,
            AC,
            Response = A::Response,
            Error = A::Error,
            InitError = A::InitError,
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

macro_rules! variant_impl_and ({$fac1_type:ident, $fac2_type:ident, $name:ident, $r_name:ident, $m_name:ident, ($($T:ident),+), ($($R:ident),+)} => {

    #[allow(non_snake_case)]
    impl<V1, V1C, $($T,)+ V1R, $($R,)+> $fac1_type<V1, V1C, $($T,)+ V1R, $($R,)+>
        where
            V1: ServiceFactory<V1R, V1C>,
        {
            /// Convert to a Variant with more request types
            pub fn $m_name<$name, $r_name, F>(self, factory: F) -> $fac2_type<V1, V1C, $($T,)+ $name, V1R, $($R,)+ $r_name>
            where $name: ServiceFactory<$r_name, V1C,
                    Response = V1::Response,
                    Error = V1::Error,
                    InitError = V1::InitError>,
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

macro_rules! variant_impl ({$mod_name:ident, $enum_type:ident, $srv_type:ident, $fac_type:ident, $(($n:tt, $T:ident, $R:ident)),+} => {

    #[allow(non_snake_case)]
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

    impl<V1, $($T,)+ V1R, $($R,)+> Service<$enum_type<V1R, $($R,)+>> for $srv_type<V1, $($T,)+ V1R, $($R,)+>
    where
        V1: Service<V1R>,
        $($T: Service<$R, Response = V1::Response, Error = V1::Error>),+
    {
        type Response = V1::Response;
        type Error = V1::Error;
        type Future<'f> = $mod_name::ServiceResponse<
            ServiceCall<'f, V1, V1R>, $(ServiceCall<'f, $T, $R>),+> where Self: 'f, V1: 'f;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            let mut ready = self.V1.poll_ready(cx)?.is_ready();
            $(ready = self.$T.poll_ready(cx)?.is_ready() && ready;)+

            if ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
            let mut ready = self.V1.poll_shutdown(cx).is_ready();
            $(ready = self.$T.poll_shutdown(cx).is_ready() && ready;)+

            if ready {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }

        fn call<'a>(&'a self, req: $enum_type<V1R, $($R,)+>, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a>
        {
            match req {
                $enum_type::V1(req) => $mod_name::ServiceResponse::V1 { fut: ctx.call(&self.V1, req) },
                $($enum_type::$T(req) => $mod_name::ServiceResponse::$T { fut: ctx.call(&self.$T, req) },)+
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

    impl<V1, V1C, $($T,)+ V1R, $($R,)+> ServiceFactory<$enum_type<V1R, $($R),+>, V1C> for $fac_type<V1, V1C, $($T,)+ V1R, $($R,)+>
    where
        V1: ServiceFactory<V1R, V1C>,
        V1C: Clone,
        $($T: ServiceFactory< $R, V1C, Response = V1::Response, Error = V1::Error, InitError = V1::InitError>),+
    {
        type Response = V1::Response;
        type Error = V1::Error;
        type InitError = V1::InitError;
        type Service = $srv_type<V1::Service, $($T::Service,)+ V1R, $($R,)+>;
        type Future<'f> = $mod_name::ServiceFactoryResponse<'f, V1, V1C, $($T,)+ V1R, $($R,)+> where Self: 'f, V1C: 'f;

        fn create(&self, cfg: V1C) -> Self::Future<'_> {
            $mod_name::ServiceFactoryResponse {
                V1: None,
                items: Default::default(),
                $($T: self.$T.create(cfg.clone()),)+
                V1_fut: self.V1.create(cfg),
            }
        }
    }

    #[doc(hidden)]
    #[allow(non_snake_case)]
    pub mod $mod_name {
        use super::*;

        pin_project_lite::pin_project! {
            #[project = ServiceResponseProject]
            pub enum ServiceResponse<V1: Future, $($T: Future),+>
            {
                V1{ #[pin] fut: V1 },
                $($T{ #[pin] fut: $T },)+
            }
        }

        impl<V1, $($T),+> Future for ServiceResponse<V1, $($T),+>
        where
            V1: Future,
            $($T: Future<Output = V1::Output>),+
        {
            type Output = V1::Output;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                match self.project() {
                    ServiceResponseProject::V1{fut} => fut.poll(cx),
                    $(ServiceResponseProject::$T{fut} => fut.poll(cx),)+
                }
            }
        }


        pin_project_lite::pin_project! {
            #[doc(hidden)]
            pub struct ServiceFactoryResponse<'f, V1: ServiceFactory<V1R, V1C>, V1C, $($T: ServiceFactory<$R, V1C>,)+ V1R, $($R,)+>
            where
                V1C: 'f,
                V1: 'f,
              $($T: 'f,)+
            {
                pub(super) V1: Option<V1::Service>,
                pub(super) items: ($(Option<$T::Service>,)+),
                #[pin] pub(super) V1_fut: V1::Future<'f>,
                $(#[pin] pub(super) $T: $T::Future<'f>),+
            }
        }

        impl<'f, V1, V1C, $($T,)+ V1R, $($R,)+> Future for ServiceFactoryResponse<'f, V1, V1C, $($T,)+ V1R, $($R,)+>
        where
            V1: ServiceFactory<V1R, V1C> + 'f,
        $($T: ServiceFactory<$R, V1C, Response = V1::Response, Error = V1::Error, InitError = V1::InitError,> + 'f),+
        {
            type Output = Result<$srv_type<V1::Service, $($T::Service,)+ V1R, $($R),+>, V1::InitError>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let this = self.project();
                let mut ready = true;

                if this.V1.is_none() {
                    match this.V1_fut.poll(cx) {
                        Poll::Ready(Ok(item)) => {
                            *this.V1 = Some(item);
                        }
                        Poll::Pending => ready = false,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                    }
                }

                $(
                    if this.items.$n.is_none() {
                        match this.$T.poll(cx) {
                            Poll::Ready(Ok(item)) => {
                                this.items.$n = Some(item);
                            }
                            Poll::Pending => ready = false,
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        }
                    }
                )+

                    if ready {
                        Poll::Ready(Ok($srv_type {
                            V1: this.V1.take().unwrap(),
                            $($T: this.items.$n.take().unwrap(),)+
                            _t: PhantomData
                        }))
                    } else {
                        Poll::Pending
                    }
            }
        }
    }

});

#[rustfmt::skip]
variant_impl!(v2, Variant2, VariantService2, VariantFactory2, (0, V2, V2R));
#[rustfmt::skip]
variant_impl!(v3, Variant3, VariantService3, VariantFactory3, (0, V2, V2R), (1, V3, V3R));
#[rustfmt::skip]
variant_impl!(v4, Variant4, VariantService4, VariantFactory4, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R));
#[rustfmt::skip]
variant_impl!(v5, Variant5, VariantService5, VariantFactory5, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R));
#[rustfmt::skip]
variant_impl!(v6, Variant6, VariantService6, VariantFactory6, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R), (4, V6, V6R));
#[rustfmt::skip]
variant_impl!(v7, Variant7, VariantService7, VariantFactory7, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R), (4, V6, V6R), (5, V7, V7R));
#[rustfmt::skip]
variant_impl!(v8, Variant8, VariantService8, VariantFactory8, (0, V2, V2R), (1, V3, V3R), (2, V4, V4R), (3, V5, V5R), (4, V6, V6R), (5, V7, V7R), (6, V8, V8R));

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
    use ntex_service::{fn_factory, Service, ServiceFactory};
    use std::task::{Context, Poll};

    use super::*;
    use crate::future::{lazy, Ready};

    #[derive(Clone)]
    struct Srv1;

    impl Service<()> for Srv1 {
        type Response = usize;
        type Error = ();
        type Future<'f> = Ready<usize, ()> where Self: 'f;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }

        fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            Ready::<_, ()>::Ok(1)
        }
    }

    #[derive(Clone)]
    struct Srv2;

    impl Service<()> for Srv2 {
        type Response = usize;
        type Error = ();
        type Future<'f> = Ready<usize, ()> where Self: 'f;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }

        fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            Ready::<_, ()>::Ok(2)
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_variant() {
        let factory = variant(fn_factory(|| async { Ok::<_, ()>(Srv1) }))
            .v2(fn_factory(|| async { Ok::<_, ()>(Srv2) }))
            .clone()
            .v3(fn_factory(|| async { Ok::<_, ()>(Srv2) }))
            .clone();
        let service = factory.container(&()).await.unwrap().clone();

        assert!(lazy(|cx| service.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| service.poll_shutdown(cx)).await.is_ready());

        assert_eq!(service.call(Variant3::V1(())).await, Ok(1));
        assert_eq!(service.call(Variant3::V2(())).await, Ok(2));
        assert_eq!(service.call(Variant3::V3(())).await, Ok(2));
    }
}
