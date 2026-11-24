//! Canary service library: the closed exit-code contract, the bounded request
//! head parser and reply plan, the validated listener port parsed from the
//! single fatal external input, and the bounded log queue.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod exit;
pub mod http;
pub mod log;
pub mod port;
