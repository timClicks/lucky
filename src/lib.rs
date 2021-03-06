//! The Lucky CLI
//!
//! Lucky is a charm writing framework for [Juju](https://jaas.ai).

// Set compiler warning settings
#![warn(missing_docs)]
#![warn(future_incompatible)]
#![warn(clippy::pedantic)]
#![allow(clippy::default_trait_access)]
#![allow(clippy::use_self)]
#![allow(clippy::too_many_lines)]
// TODO: This is simply because of the `function_name` macro and we might want to fix it there
// instead of ignoring the warning here.
#![allow(clippy::empty_enum)]
// Ignore dead code warnings when building without the daemon
#![cfg_attr(not(feature = "daemon"), allow(unused))]

#[macro_use]
pub(crate) mod macros;

pub mod cli;
pub(crate) mod config;
pub(crate) mod log;
pub(crate) mod rpc;
pub(crate) mod types;

// Daemon only modules
#[cfg(feature = "daemon")]
pub(crate) mod daemon;
#[cfg(feature = "daemon")]
pub(crate) mod docker;
#[cfg(feature = "daemon")]
pub(crate) mod juju;
#[cfg(feature = "daemon")]
pub(crate) mod process;
#[cfg(feature = "daemon")]
pub(crate) mod rt;

/// Lucky version from environment var
///
/// This env var will be set by the build.rs script to the git version if not present at build time.
const LUCKY_VERSION: &str = env!("LUCKY_VERSION");

const VOLUME_DIR: &str = "volumes";
