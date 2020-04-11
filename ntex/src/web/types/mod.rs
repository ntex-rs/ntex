//! Extractor types

pub(in crate::web) mod data;
pub(in crate::web) mod form;
pub(in crate::web) mod json;
mod path;
pub(in crate::web) mod payload;
mod query;

pub use self::data::Data;
pub use self::form::{Form, FormConfig};
pub use self::json::{Json, JsonConfig};
pub use self::path::Path;
pub use self::payload::{Payload, PayloadConfig};
pub use self::query::Query;
