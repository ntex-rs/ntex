//! Extractor types

pub(in crate::web) mod form;
pub(in crate::web) mod json;
mod path;
pub(in crate::web) mod payload;
mod query;
pub(in crate::web) mod state;

pub use self::form::{Form, FormConfig};
pub use self::json::{Json, JsonConfig};
pub use self::path::Path;
pub use self::payload::{Payload, PayloadConfig};
pub use self::query::Query;
pub use self::state::State;
