//! Canary service library: the closed exit-code contract, the bounded request
//! head parser and reply plan, the validated listener port parsed from the
//! single fatal external input, the dependency-free health probe, the bounded
//! log queue, and the fixed connection-slot table.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod exit;
pub mod http;
pub mod log;
pub mod port;
pub mod probe;
pub mod serve;
