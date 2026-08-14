use crate::{Pipeline, Service, ServiceCtx, ctx::WaitersRef};

pub(crate) type ServiceOf<F, Cfg> = <F as Service<Cfg>>::Response;
pub(crate) type ResponseOf<F, Req, Cfg> = <ServiceOf<F, Cfg> as Service<Req>>::Response;
pub(crate) type ErrorOf<F, Req, Cfg> = <ServiceOf<F, Cfg> as Service<Req>>::Error;

/// Service factories are [`Service`] implementations that create other services.
/// They are also responsible for mapping data from outer pipelines
/// into data for inner pipelines.
///
/// Service factories are used to create dynamic service pipelines.
/// For instance, a connect service may create a service for handling the http requests
/// on the created connection, passing in the connection as the data for the new pipeline.
pub trait ServiceFactory<Req, Cfg>: Service<Cfg, Response: Service<Req>> {
    /// Asynchronously creates a new service and wraps it in a container.
    #[inline]
    async fn pipeline(
        &self,
        cfg: Cfg,
        data: &Self::Data,
    ) -> Result<Pipeline<Self::Response, <Self::Response as Service<Req>>::Data>, Self::Error>
    where
        Self: Sized,
    {
        let (idx, waiters) = WaitersRef::new();
        let ctx = ServiceCtx::new(idx, &waiters);
        let svc_data = self.map_data(&cfg, data).await?;
        Ok(Pipeline::new(self.call(cfg, data, ctx).await?, svc_data))
    }

    /// Maps the outer pipeline data to the data stored by the produced pipeline.
    async fn map_data(
        &self,
        cfg: &Cfg,
        data: &Self::Data,
    ) -> Result<<Self::Response as Service<Req>>::Data, Self::Error>
    where
        Self: Sized;
}

impl<SF, Req, Cfg> ServiceFactory<Req, Cfg> for SF
where
    SF: Service<Cfg>,
    SF::Data: Clone,
    SF::Response: Service<Req, Data = SF::Data>,
{
    async fn map_data(
        &self,
        _: &Cfg,
        data: &Self::Data,
    ) -> Result<<Self::Response as Service<Req>>::Data, Self::Error>
    where
        Self: Sized,
    {
        Ok(data.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Factory;

    impl Service<()> for Factory {
        type Response = DataService;
        type Error = ();
        type Data = usize;

        async fn call(
            &self,
            _: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, Self::Error> {
            Ok(DataService)
        }
    }

    impl ServiceFactory<(), ()> for Factory {
        async fn map_data(&self, _: &(), data: &Self::Data) -> Result<String, Self::Error> {
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
}
