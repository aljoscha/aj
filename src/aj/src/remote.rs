//! The remote-control protocol over HTTP (spec section 6).
//!
//! Three pieces: [`server`] serves a [`aj_app::host::SessionHost`] on a
//! control port, [`client`] speaks to one, and [`identity`] decides which
//! peers may connect at all. Everything the protocol calls correctness lives
//! in the host and in the client fold, so this module is transport and
//! nothing else.
//!
//! The `serve` and `connect` command surfaces that drive these live with the
//! rest of the CLI.

// Part of the protocol surface below has no caller above the transport: the
// tree read (whose view is phase 3), and two diagnostics accessors (the bound
// address and the resolver's socket path). The stream-silence and
// stream-open-timeout overrides and the error-code accessor are reached by
// this module's own tests only. All of it belongs to the protocol rather than
// to one frontend's wiring, so the lint is silenced here rather than the
// surface trimmed to what today's TUI happens to use.
#![allow(dead_code)]

mod client;
mod identity;
mod server;

#[cfg(test)]
mod tests;

pub(crate) use client::{RemoteClient, RemoteCommand, RemoteError, RemoteEvents};
pub(crate) use identity::{IdentityGate, IdentityMode, TailscaleWhois};
pub(crate) use server::RemoteServer;
