//! HTTP handlers, grouped as the contract's tags.

pub mod audit;
pub mod listeners;
pub mod system;

use axum::extract::{FromRequest, FromRequestParts};

/// JSON body whose parse failures become the contract's `Error` body.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(crate::error::ApiError))]
pub struct ApiJson<T>(pub T);

/// Query string whose parse failures become the contract's `Error` body.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(crate::error::ApiError))]
pub struct ApiQuery<T>(pub T);

/// A display name: trimmed, 1 to `max` characters, no control characters.
pub fn clean_name(raw: &str, what: &str, max: usize) -> Result<String, crate::error::ApiError> {
    let name = raw.trim();
    let n = name.chars().count();
    if !(1..=max).contains(&n) || name.chars().any(|c| c.is_control()) {
        return Err(crate::error::ApiError::invalid(
            "name_invalid",
            format!("The {what} name must be 1 to {max} characters."),
        ));
    }
    Ok(name.to_string())
}
