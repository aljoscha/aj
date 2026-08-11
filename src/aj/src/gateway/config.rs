//! The gateway's static configuration (spec 7.1).
//!
//! `~/.aj/gateway.toml` by default, `--config <file>` to name another. It holds
//! the addresses of hosts the operator wants enrolled for as long as the file
//! says so, which is exactly why those are never written to the gateway's own
//! state: this file is their record, and persisting them too would resurrect a
//! host the operator removed from it.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One host's base URL, normalized so that two spellings of one host are one
/// address.
///
/// Normalization is what makes the "no duplicates" rule of spec 7.1 checkable:
/// the configuration's `100.64.0.2:6161` and a later
/// `POST /v1/hosts {"address": "http://100.64.0.2:6161/"}` are the same host,
/// and comparing the raw strings would enroll it twice.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct HostAddress(String);

impl HostAddress {
    /// Accepts `<host>:<port>` as well as a full `http(s)://` URL.
    ///
    /// The two are told apart by whether the raw string carries a scheme at all,
    /// rather than by trying a URL parse first: `localhost:6161` parses happily
    /// as a URL whose scheme is `localhost`, so a parse-first rule would accept
    /// it and then dial nothing. A bare address is the spelling `--listen` and
    /// the provisioner use, so it has to work here too.
    pub(crate) fn parse(raw: &str) -> Result<Self, AddressError> {
        let raw = raw.trim();
        let refuse = |reason: &str| AddressError {
            address: raw.to_string(),
            reason: reason.to_string(),
        };
        let candidate = match raw.split_once("://") {
            Some((scheme, _)) => {
                if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                    // Refused rather than prefixed: `http://ftp://host` parses,
                    // and dialing the host `ftp` is not what was asked for.
                    return Err(refuse("only http and https addresses are dialable"));
                }
                raw.to_string()
            }
            None => format!("http://{raw}"),
        };
        let url = reqwest::Url::parse(&candidate).map_err(|err| refuse(&err.to_string()))?;
        if url.host_str().is_none_or(str::is_empty) {
            return Err(refuse("no host in the address"));
        }
        if url.cannot_be_a_base() {
            return Err(refuse("not a base URL"));
        }
        // A trailing slash is what `Url` normalizes an empty path to, and every
        // route this address is used for appends `/v1/...`.
        Ok(Self(url.as_str().trim_end_matches('/').to_string()))
    }

    /// The base URL a client dials.
    pub(crate) fn url(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for HostAddress {
    type Error = AddressError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<HostAddress> for String {
    fn from(address: HostAddress) -> Self {
        address.0
    }
}

/// A string is not an address a host could be dialed at.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{address:?} is not a host address: {reason}")]
pub(crate) struct AddressError {
    pub(crate) address: String,
    reason: String,
}

/// What a gateway reads from its configuration file.
///
/// Unknown keys are ignored, which is what lets the provisioner section of spec
/// 7.1 land in an operator's file before this build knows what to do with it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct GatewayConfig {
    #[serde(default)]
    hosts: Vec<HostAddress>,
}

impl GatewayConfig {
    /// Reads the configuration at `path`.
    ///
    /// A malformed file fails the read rather than degrading to an empty
    /// configuration: a gateway that serves no hosts because of a typo looks
    /// exactly like one that was configured with none.
    pub(crate) fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|err| ConfigError::Parse {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })
    }

    /// The static host addresses, deduplicated.
    ///
    /// One host listed twice is one host: the normalized address makes two
    /// spellings comparable, and a repeat is a slip in a hand-edited file rather
    /// than a request to open two links to one store.
    pub(crate) fn hosts(&self) -> Vec<HostAddress> {
        let mut hosts: Vec<HostAddress> = Vec::with_capacity(self.hosts.len());
        for address in &self.hosts {
            if hosts.contains(address) {
                tracing::warn!("the gateway configuration lists {address} twice");
                continue;
            }
            hosts.push(address.clone());
        }
        hosts
    }
}

/// Why a configuration file could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("could not read the gateway configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse the gateway configuration at {path}: {reason}")]
    Parse { path: PathBuf, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_normalizes_to_one_base_url() {
        for raw in [
            "127.0.0.1:6161",
            "http://127.0.0.1:6161",
            "http://127.0.0.1:6161/",
            "HTTP://127.0.0.1:6161",
            "  127.0.0.1:6161  ",
        ] {
            assert_eq!(
                HostAddress::parse(raw).expect(raw).url(),
                "http://127.0.0.1:6161",
                "{raw:?}",
            );
        }
        // A name is not a scheme, which is the whole reason the prefix decides.
        assert_eq!(
            HostAddress::parse("localhost:6161")
                .expect("a bare address")
                .url(),
            "http://localhost:6161",
        );
        assert_eq!(
            HostAddress::parse("https://gateway.example/aj")
                .expect("a path prefix survives")
                .url(),
            "https://gateway.example/aj",
        );
    }

    #[test]
    fn something_that_is_not_an_address_is_refused() {
        for raw in ["", "   ", "http://", "not an address", "ftp://host:21"] {
            assert!(
                HostAddress::parse(raw).is_err(),
                "{raw:?} should be refused"
            );
        }
    }

    #[test]
    fn a_configuration_reads_its_hosts_and_ignores_what_it_does_not_know() {
        let config: GatewayConfig = toml::from_str(
            r#"
            hosts = ["127.0.0.1:6161", "http://127.0.0.1:6161/", "100.64.0.2:6161"]

            [provisioner]
            backend = "ember"
            golden = "aj-golden"
            "#,
        )
        .expect("the configuration parses");

        assert_eq!(
            config.hosts(),
            vec![
                HostAddress::parse("127.0.0.1:6161").expect("an address"),
                HostAddress::parse("100.64.0.2:6161").expect("an address"),
            ],
            "one host listed twice is one host",
        );
    }

    #[test]
    fn an_empty_configuration_is_a_configuration() {
        let config: GatewayConfig = toml::from_str("").expect("parses");
        assert!(config.hosts().is_empty());
    }

    #[test]
    fn a_malformed_address_fails_the_read() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("gateway.toml");
        std::fs::write(&path, "hosts = [\"nope nope\"]\n").expect("write");
        assert!(matches!(
            GatewayConfig::load(&path),
            Err(ConfigError::Parse { .. }),
        ));

        std::fs::write(&path, "hosts = 3\n").expect("write");
        assert!(matches!(
            GatewayConfig::load(&path),
            Err(ConfigError::Parse { .. }),
        ));

        assert!(matches!(
            GatewayConfig::load(&dir.path().join("absent.toml")),
            Err(ConfigError::Read { .. }),
        ));
    }
}
