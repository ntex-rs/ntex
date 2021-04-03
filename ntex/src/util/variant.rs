//! Contains `Variant` service and related types and functions.
use std::{future::Future, pin::Pin, task::Context, task::Poll};

use crate::service::{IntoServiceFactory, Service, ServiceFactory};

/// Construct `Variant` service factory.
///
/// Variant service allow to combine multiple different services into a single service.
pub fn variant<A: ServiceFactory>(factory: A) -> Variant<A> {
    Variant { factory }
}

/// Combine multiple different service types into a single service.
pub struct Variant<A> {
    factory: A,
}

impl<A> Variant<A>
where
    A: ServiceFactory,
    A::Config: Clone,
{
    /// Convert to a Variant with two request types
    pub fn and<B, F>(self, factory: F) -> VariantFactory2<A, B>
    where
        B: ServiceFactory<
            Config = A::Config,
            Response = A::Response,
            Error = A::Error,
            InitError = A::InitError,
        >,
        F: IntoServiceFactory<B>,
    {
        VariantFactory2 {
            A: self.factory,
            V2: factory.into_factory(),
        }
    }

    /// Convert to a Variant with two request types
    pub fn v2<B, F>(self, factory: F) -> VariantFactory2<A, B>
    where
        B: ServiceFactory<
            Config = A::Config,
            Response = A::Response,
            Error = A::Error,
            InitError = A::InitError,
        >,
        F: IntoServiceFactory<B>,
    {
        VariantFactory2 {
            A: self.factory,
            V2: factory.into_factory(),
        }
    }
}

macro_rules! variant_impl_and ({$fac1_type:ident, $fac2_type:ident, $name:ident, $m_name:ident, ($($T:ident),+)} => {

    impl<V1, $($T),+> $fac1_type<V1, $($T),+>
        where
            V1: ServiceFactory,
            V1::Config: Clone,
        {
            #[doc(hidden)]
            /// Convert to a Variant with more request types
            pub fn and<$name, F>(self, factory: F) -> $fac2_type<V1, $($T,)+ $name>
            where $name: ServiceFactory<
                    Config = V1::Config,
                    Response = V1::Response,
                    Error = V1::Error,
                    InitError = V1::InitError>,
                F: IntoServiceFactory<$name>,
            {
                $fac2_type {
                    A: self.A,
                    $($T: self.$T,)+
                    $name: factory.into_factory(),
                }
            }

            /// Convert to a Variant with more request types
            pub fn $m_name<$name, F>(self, factory: F) -> $fac2_type<V1, $($T,)+ $name>
            where $name: ServiceFactory<
                    Config = V1::Config,
                    Response = V1::Response,
                    Error = V1::Error,
                    InitError = V1::InitError>,
                F: IntoServiceFactory<$name>,
            {
                $fac2_type {
                    A: self.A,
                    $($T: self.$T,)+
                    $name: factory.into_factory(),
                }
            }
    }
});

macro_rules! variant_impl ({$mod_name:ident, $enum_type:ident, $srv_type:ident, $fac_type:ident, $(($n:tt, $T:ident)),+} => {

    #[allow(non_snake_case)]
    pub enum $enum_type<V1, $($T),+> {
        V1(V1),
        $($T($T),)+
    }

    #[allow(non_snake_case)]
    pub struct $srv_type<V1, $($T),+> {
        a: V1,
        $($T: $T,)+
    }

    impl<V1: Clone, $($T: Clone),+> Clone for $srv_type<V1, $($T),+> {
        fn clone(&self) -> Self {
            Self {
                a: self.a.clone(),
                $($T: self.$T.clone(),)+
            }
        }
    }

    impl<V1, $($T),+> Service for $srv_type<V1, $($T),+>
    where
        V1: Service,
        $($T: Service<Response = V1::Response, Error = V1::Error>),+
    {
        type Request = $enum_type<V1::Request, $($T::Request),+>;
        type Response = V1::Response;
        type Error = V1::Error;
        type Future = $mod_name::ServiceResponse<V1::Future, $($T::Future),+>;

        #[inline]
        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            let mut ready = self.a.poll_ready(cx)?.is_ready();
            $(ready = self.$T.poll_ready(cx)?.is_ready() && ready;)+

            if ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        #[inline]
        fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
            let mut ready = self.a.poll_shutdown(cx, is_error).is_ready();
        $(ready = self.$T.poll_shutdown(cx, is_error).is_ready() && ready;)+

        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

        #[inline]
        fn call(&self, req: Self::Request) -> Self::Future {
            match req {
                $enum_type::V1(req) => $mod_name::ServiceResponse::V1 { fut: self.a.call(req) },
                $($enum_type::$T(req) => $mod_name::ServiceResponse::$T { fut: self.$T.call(req) },)+
            }
        }
    }

    #[allow(non_snake_case)]
    pub struct $fac_type<V1, $($T),+> {
        A: V1,
        $($T: $T,)+
    }

    impl<V1: Clone, $($T: Clone),+> Clone for $fac_type<V1, $($T),+> {
        fn clone(&self) -> Self {
            Self {
                A: self.A.clone(),
                $($T: self.$T.clone(),)+
            }
        }
    }

    impl<V1, $($T),+> ServiceFactory for $fac_type<V1, $($T),+>
    where
        V1: ServiceFactory,
        V1::Config: Clone,
        $($T: ServiceFactory<Config = V1::Config, Response = V1::Response, Error = V1::Error, InitError = V1::InitError>),+
    {
        type Request = $enum_type<V1::Request, $($T::Request),+>;
        type Response = V1::Response;
        type Error = V1::Error;
        type Config = V1::Config;
        type InitError = V1::InitError;
        type Service = $srv_type<V1::Service, $($T::Service,)+>;
        type Future = $mod_name::ServiceFactoryResponse<V1, $($T,)+>;

        fn new_service(&self, cfg: Self::Config) -> Self::Future {
            $mod_name::ServiceFactoryResponse {
                a: None,
                items: Default::default(),
                $($T: self.$T.new_service(cfg.clone()),)+
                a_fut: self.A.new_service(cfg),
            }
        }
    }

    #[doc(hidden)]
    #[allow(non_snake_case)]
    pub mod $mod_name {
        use super::*;

        pin_project_lite::pin_project! {
            #[project = ServiceResponseProject]
            pub enum ServiceResponse<A: Future, $($T: Future),+> {
                V1{ #[pin] fut: A },
                $($T{ #[pin] fut: $T },)+
            }
        }

        impl<A, $($T),+> Future for ServiceResponse<A, $($T),+>
        where
            A: Future,
            $($T: Future<Output = A::Output>),+
        {
            type Output = A::Output;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                match self.project() {
                    ServiceResponseProject::V1{fut} => fut.poll(cx),
                    $(ServiceResponseProject::$T{fut} => fut.poll(cx),)+
                }
            }
        }


        #[doc(hidden)]
        pin_project_lite::pin_project! {
            pub struct ServiceFactoryResponse<A: ServiceFactory, $($T: ServiceFactory),+> {
                pub(super) a: Option<A::Service>,
                pub(super) items: ($(Option<$T::Service>,)+),
                #[pin] pub(super) a_fut: A::Future,
                $(#[pin] pub(super) $T: $T::Future),+
            }
        }

        impl<A, $($T),+> Future for ServiceFactoryResponse<A, $($T),+>
        where
            A: ServiceFactory,
        $($T: ServiceFactory<Response = A::Response, Error = A::Error, InitError = A::InitError,>),+
        {
            type Output = Result<$srv_type<A::Service, $($T::Service),+>, A::InitError>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let this = self.project();
                let mut ready = true;

                if this.a.is_none() {
                    match this.a_fut.poll(cx) {
                        Poll::Ready(Ok(item)) => {
                            *this.a = Some(item);
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
                            a: this.a.take().unwrap(),
                            $($T: this.items.$n.take().unwrap(),)+
                        }))
                    } else {
                        Poll::Pending
                    }
            }
        }
    }

});

#[rustfmt::skip]
variant_impl!(v2, Variant2, VariantService2, VariantFactory2, (0, V2));
#[rustfmt::skip]
variant_impl!(v3, Variant3, VariantService3, VariantFactory3, (0, V2), (1, V3));
#[rustfmt::skip]
variant_impl!(v4, Variant4, VariantService4, VariantFactory4, (0, V2), (1, V3), (2, V4));
#[rustfmt::skip]
variant_impl!(v5, Variant5, VariantService5, VariantFactory5, (0, V2), (1, V3), (2, V4), (3, V5));
#[rustfmt::skip]
variant_impl!(v6, Variant6, VariantService6, VariantFactory6, (0, V2), (1, V3), (2, V4), (3, V5), (4, V6));
#[rustfmt::skip]
variant_impl!(v7, Variant7, VariantService7, VariantFactory7, (0, V2), (1, V3), (2, V4), (3, V5), (4, V6), (5, V7));
#[rustfmt::skip]
variant_impl!(v8, Variant8, VariantService8, VariantFactory8, (0, V2), (1, V3), (2, V4), (3, V5), (4, V6), (5, V7), (6, V8));

variant_impl_and!(VariantFactory2, VariantFactory3, V3, v3, (V2));
variant_impl_and!(VariantFactory3, VariantFactory4, V4, v4, (V2, V3));
variant_impl_and!(VariantFactory4, VariantFactory5, V5, v5, (V2, V3, V4));
variant_impl_and!(VariantFactory5, VariantFactory6, V6, v6, (V2, V3, V4, V5));
variant_impl_and!(
    VariantFactory6,
    VariantFactory7,
    V7,
    v7,
    (V2, V3, V4, V5, V6)
);
#[rustfmt::skip]
variant_impl_and!(VariantFactory7, VariantFactory8, V8, v8, (V2, V3, V4, V5, V6, V7));

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use super::*;
    use crate::service::{fn_factory, Service, ServiceFactory};
    use crate::util::{lazy, Ready};

    #[derive(Clone)]
    struct Srv1;

    impl Service for Srv1 {
        type Request = ();
        type Response = usize;
        type Error = ();
        type Future = Ready<usize, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
            Poll::Ready(())
        }

        fn call(&self, _: ()) -> Self::Future {
            Ready::<_, ()>::Ok(1)
        }
    }

    #[derive(Clone)]
    struct Srv2;

    impl Service for Srv2 {
        type Request = ();
        type Response = usize;
        type Error = ();
        type Future = Ready<usize, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
            Poll::Ready(())
        }

        fn call(&self, _: ()) -> Self::Future {
            Ready::<_, ()>::Ok(2)
        }
    }

    #[crate::rt_test]
    async fn test_variant() {
        let factory = variant(fn_factory(|| async { Ok::<_, ()>(Srv1) }))
            .and(fn_factory(|| async { Ok::<_, ()>(Srv2) }))
            .and(fn_factory(|| async { Ok::<_, ()>(Srv2) }))
            .clone();
        let service = factory.new_service(&()).await.clone().unwrap();

        assert!(lazy(|cx| service.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| service.poll_shutdown(cx, true)).await.is_ready());

        assert_eq!(service.call(Variant3::V1(())).await, Ok(1));
        assert_eq!(service.call(Variant3::V2(())).await, Ok(2));
        assert_eq!(service.call(Variant3::V3(())).await, Ok(2));
    }
}
