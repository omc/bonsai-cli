//! The mock's integration tests, one module per area, in a single test binary so
//! the dependencies link once instead of once per file.

mod actions;
mod admin;
mod catalogue;
mod clusters;
mod common;
mod coverage;
mod credentials;
mod login;
mod observability;
mod public_url;
