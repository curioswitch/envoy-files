//! Pure HTTP/file-serving logic with no Envoy SDK dependency, so it can be
//! unit tested on any platform without an Envoy build. Uses `http` types
//! (`StatusCode`, `HeaderName`, `HeaderValue`) for the response plan.

pub mod encoding;
pub mod error;
pub mod listing;
pub mod mime;
pub mod path;
pub mod range;
pub mod response_plan;
pub mod validators;
