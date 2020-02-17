//! Middlewares

#[cfg(feature = "compress")]
mod compress;
#[cfg(feature = "compress")]
pub use self::compress::Compress;

mod logger;
pub use self::logger::Logger;

mod defaultheaders;
pub use self::defaultheaders::DefaultHeaders;
