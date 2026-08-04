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

// The CLI and TUI wiring that reaches these lives outside this module, so
// within the binary only this module's own tests call them today. Both lints
// come off with that wiring.
#![allow(dead_code, unused_imports)]

mod client;
mod identity;
mod server;

#[cfg(test)]
mod tests;

pub(crate) use client::{RemoteClient, RemoteCommand, RemoteError, RemoteEvents};
pub(crate) use identity::{
    AJ_CONTROL_CAPABILITY, IdentityError, IdentityGate, IdentityMode, PeerIdentity, TailscaleWhois,
    WhoisResolver,
};
pub(crate) use server::{RemoteServer, ServerError};
