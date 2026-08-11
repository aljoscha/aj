//! Enrollment: what a gateway remembers about its hosts (spec 7.1).
//!
//! The state file is the gateway's own memory of the hosts it was told to keep,
//! so it is typed: an address it cannot parse back is a corrupt file, not a
//! request to tolerate. What a client sends and reads about those hosts is the
//! protocol's own ([`aj_wire::EnrollHostRequest`], [`aj_wire::HostList`]).
//!
//! Only dynamic enrollments are written down. A static one comes back from the
//! configuration file on every start (see [`super::config`]), and persisting it
//! too would resurrect a host the operator deleted from that file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gateway::config::HostAddress;

/// One dynamically enrolled host as the gateway's state records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EnrolledHost {
    pub(crate) address: HostAddress,
    /// The id the host reported when it was enrolled, which is the namespace its
    /// sessions appear under.
    ///
    /// Recorded rather than re-learned so that a restarted gateway can route and
    /// label a host's sessions from the first instant, including while that host
    /// is down. It is fixed for the life of the enrollment: re-namespacing a
    /// host's sessions under a live client's feet would invalidate every id the
    /// client holds.
    pub(crate) host_id: String,
}

/// The gateway's enrollment state, as one file under `~/.aj/gateway/`.
pub(crate) struct EnrollmentFile {
    path: PathBuf,
}

/// The file's contents. Named so the JSON has a key rather than a bare array,
/// which is what leaves room for another field later.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    hosts: Vec<EnrolledHost>,
}

impl EnrollmentFile {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("hosts.json"),
        }
    }

    /// The enrollments recorded here, empty when there is no file yet.
    ///
    /// A file that exists and cannot be read is an error rather than an empty
    /// set: answering "no hosts" would make the next write erase the operator's
    /// enrollments, and a gateway that quietly serves nothing is the worst
    /// outcome of a bad byte on disk.
    pub(crate) fn load(&self) -> Result<Vec<EnrolledHost>, EnrollmentError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(EnrollmentError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let persisted: Persisted =
            serde_json::from_str(&text).map_err(|source| EnrollmentError::Parse {
                path: self.path.clone(),
                source,
            })?;
        Ok(persisted.hosts)
    }

    /// Replaces the recorded set with `hosts`.
    ///
    /// Written through a temp file in the same directory and renamed, so a
    /// reader sees the old set or the new one and never a torn one.
    pub(crate) fn save(&self, hosts: &[EnrolledHost]) -> Result<(), EnrollmentError> {
        let parent = self.path.parent().ok_or_else(|| EnrollmentError::Write {
            path: self.path.clone(),
            reason: "the state file has no parent directory".to_string(),
        })?;
        std::fs::create_dir_all(parent).map_err(|err| EnrollmentError::Write {
            path: parent.to_path_buf(),
            reason: err.to_string(),
        })?;
        let body = serde_json::to_vec_pretty(&Persisted {
            hosts: hosts.to_vec(),
        })
        .map_err(|err| EnrollmentError::Write {
            path: self.path.clone(),
            reason: err.to_string(),
        })?;
        let write = || -> Result<(), String> {
            let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut temp, &body).map_err(|e| e.to_string())?;
            std::io::Write::flush(&mut temp).map_err(|e| e.to_string())?;
            temp.persist(&self.path).map_err(|e| e.to_string())?;
            Ok(())
        };
        write().map_err(|reason| EnrollmentError::Write {
            path: self.path.clone(),
            reason,
        })
    }
}

/// Why the enrollment state could not be read or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EnrollmentError {
    #[error("could not read the gateway state at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not readable gateway state: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write the gateway state at {path}: {reason}")]
    Write { path: PathBuf, reason: String },
    /// A file in the state directory that exists and cannot be used as it is.
    /// Left for the operator, because the alternative is overwriting state whose
    /// meaning is unclear.
    #[error("{path} is not usable gateway state: {reason}")]
    Unusable { path: PathBuf, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enrolled(address: &str, host_id: &str) -> EnrolledHost {
        EnrolledHost {
            address: HostAddress::parse(address).expect("an address"),
            host_id: host_id.to_string(),
        }
    }

    #[test]
    fn the_state_file_round_trips() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = EnrollmentFile::new(dir.path());
        assert!(
            file.load()
                .expect("an absent file is an empty set")
                .is_empty(),
            "a gateway that has enrolled nothing has no file to read",
        );

        let hosts = vec![
            enrolled("127.0.0.1:6161", "aaa"),
            enrolled("100.64.0.2:6161", "bbb"),
        ];
        file.save(&hosts).expect("save");
        assert_eq!(file.load().expect("load"), hosts);

        // A save replaces the set rather than appending to it, which is what
        // makes a withdrawal outlive the process that served it.
        file.save(&hosts[..1]).expect("save");
        assert_eq!(file.load().expect("load"), hosts[..1].to_vec());
    }

    #[test]
    fn the_state_file_is_created_under_a_directory_that_does_not_exist_yet() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = EnrollmentFile::new(&dir.path().join("gateway"));
        file.save(&[enrolled("127.0.0.1:6161", "aaa")])
            .expect("the directory is created on the way");
        assert_eq!(file.load().expect("load").len(), 1);
    }

    /// A file that exists and does not parse is an error. Reading it as "no
    /// hosts" would let the next save erase what it could not read.
    #[test]
    fn unreadable_state_is_an_error_rather_than_an_empty_set() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = EnrollmentFile::new(dir.path());
        std::fs::write(dir.path().join("hosts.json"), "{\"hosts\": [").expect("write");
        assert!(matches!(file.load(), Err(EnrollmentError::Parse { .. })));

        std::fs::write(
            dir.path().join("hosts.json"),
            r#"{"hosts":[{"address":"nope nope","host_id":"aaa"}]}"#,
        )
        .expect("write");
        assert!(
            matches!(file.load(), Err(EnrollmentError::Parse { .. })),
            "an address the gateway could not dial is corrupt state",
        );
    }
}
