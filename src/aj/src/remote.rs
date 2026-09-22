//! The remote-control protocol over HTTP.
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
pub(crate) mod history;
mod identity;
mod server;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use client::{RemoteClient, RemoteCommand, RemoteError, RemoteEvents, SILENCE};
pub(crate) use identity::{IdentityError, IdentityGate, IdentityMode, TailscaleWhois};
pub(crate) use server::RemoteServer;

/// An endpoint label that cannot expose URL credentials.
pub(crate) fn endpoint_label(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "remote host".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod endpoint_tests {
    #[test]
    fn endpoint_labels_omit_credentials_and_private_query_or_fragment_data() {
        assert_eq!(
            super::endpoint_label(
                "https://operator:password@example.test:8443/agent/?token=query-secret#fragment-secret"
            ),
            "https://example.test:8443/agent"
        );
    }
}
