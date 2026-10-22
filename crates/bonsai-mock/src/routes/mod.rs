//! One module per group of `/v1` operations, plus the `/_mock` admin API and
//! pages (`admin`). Each exposes `router()`, merged into [`crate::app`].

pub mod account;
pub mod actions;
pub mod admin;
pub mod catalogue;
pub mod clusters;
pub mod credentials;
pub mod login;
pub mod observability;
pub mod root;
pub mod token;
