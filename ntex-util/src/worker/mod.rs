use ntex_service::ServiceFactory;

mod wrk;

pub use self::wrk::Worker;
use crate::time::Millis;

#[non_exhaustive]
#[derive(Debug)]
/// Worker message
pub enum WorkerMessage<T> {
    /// New item received
    New(T),
    /// Graceful shutdown in millis
    Shutdown(Millis),
    /// Force shutdown
    ForceShutdown,
}

pub trait WorkerServiceFactory<T>: Send + 'static {
    type Factory: ServiceFactory<WorkerMessage<T>> + 'static;

    fn create(&self) -> Self::Factory;
}
