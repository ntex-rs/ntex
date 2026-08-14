use crate::{Pipeline, Service, boxed, chain_factory, dev};

pub(crate) type ServiceOf<F, Req, Cfg> = <F as ServiceFactory<Req, Cfg>>::Service;
pub(crate) type ResponseOf<F, Req, Cfg> = <F as ServiceFactory<Req, Cfg>>::Response;
pub(crate) type ErrorOf<F, Req, Cfg> = <F as ServiceFactory<Req, Cfg>>::Error;

/// A factory for creating [`Service`] values.
pub trait ServiceFactory<Req, Cfg = ()> {
    /// Responses given by the created services.
    type Response;

    /// Errors produced by the created services.
    type Error;

    /// The type of service produced by this factory.
    type Service: Service<Req, Response = Self::Response, Error = Self::Error>;

    /// Possible errors encountered during service construction or data mapping.
    type InitError;

    /// Data supplied by the outer pipeline that executes this factory.
    type Data;

    /// Creates a new service asynchronously.
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError>;

    /// Maps outer pipeline data to data for the generated service pipeline.
    async fn map_data(
        &self,
        cfg: &Cfg,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<Req>>::Data, Self::InitError>;

    /// Creates a new service and wraps it with its mapped execution data.
    #[inline]
    async fn pipeline(
        &self,
        cfg: Cfg,
        data: &Self::Data,
    ) -> Result<
        Pipeline<Self::Service, <Self::Service as Service<Req>>::Data>,
        Self::InitError,
    >
    where
        Self: Sized,
    {
        let svc_data = self.map_data(&cfg, data).await?;
        Ok(Pipeline::new(self.create(cfg).await?, svc_data))
    }

    #[inline]
    fn map<F, Res>(
        self,
        f: F,
    ) -> dev::ServiceChainFactory<dev::MapFactory<Self, F, Req, Res, Cfg>, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res + Clone,
    {
        chain_factory(dev::MapFactory::new(self, f))
    }

    #[inline]
    fn map_err<F, E>(
        self,
        f: F,
    ) -> dev::ServiceChainFactory<dev::MapErrFactory<Self, Req, Cfg, F, E>, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E + Clone,
    {
        chain_factory(dev::MapErrFactory::new(self, f))
    }

    #[inline]
    fn map_init_err<F, E>(
        self,
        f: F,
    ) -> dev::ServiceChainFactory<dev::MapInitErr<Self, Req, Cfg, F, E>, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E + Clone,
    {
        chain_factory(dev::MapInitErr::new(self, f))
    }

    /// Creates a boxed service factory.
    fn boxed(
        self,
    ) -> boxed::BoxServiceFactory<
        Cfg,
        Req,
        Self::Response,
        Self::Error,
        Self::InitError,
        Self::Data,
        <Self::Service as Service<Req>>::Data,
    >
    where
        Cfg: 'static,
        Req: 'static,
        Self: 'static + Sized,
        Self::Service: 'static,
        Self::Data: 'static,
        <Self::Service as Service<Req>>::Data: 'static,
    {
        boxed::factory(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceCtx;

    #[derive(Debug)]
    struct Factory;

    impl ServiceFactory<(), ()> for Factory {
        type Response = String;
        type Error = ();
        type Service = DataService;
        type InitError = ();
        type Data = usize;

        async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
            Ok(DataService)
        }

        async fn map_data(
            &self,
            _: &(),
            data: &Self::Data,
        ) -> Result<String, Self::InitError> {
            Ok(data.to_string())
        }
    }

    #[derive(Debug)]
    struct DataService;

    impl Service<()> for DataService {
        type Response = String;
        type Error = ();
        type Data = String;

        async fn call(
            &self,
            _: (),
            data: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, Self::Error> {
            Ok(data.clone())
        }
    }

    #[ntex::test]
    async fn maps_pipeline_data() {
        let pipeline = Factory.pipeline((), &42).await.unwrap();
        assert_eq!(pipeline.call(()).await.unwrap(), "42");
    }

    #[ntex::test]
    async fn preserves_distinct_factory_and_service_data() {
        let factory = chain_factory(Factory).map(|value| format!("{value}!"));
        let pipeline = factory.pipeline((), &42).await.unwrap();
        assert_eq!(pipeline.call(()).await.unwrap(), "42!");

        let factory = boxed::factory(Factory);
        let pipeline = factory.pipeline((), &42).await.unwrap();
        assert_eq!(pipeline.call(()).await.unwrap(), "42");
    }
}
