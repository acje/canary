//! Canary service library: the closed exit-code contract and the validated
//! listener port parsed from the single fatal external input.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod exit;
pub mod port;
