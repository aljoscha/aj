//! Enrollment: what a gateway remembers about its hosts (spec 7.1).
//!
//! The state file is the gateway's own memory of the hosts it was told to keep,
//! so it is typed: an address it cannot parse back is a corrupt file, not a
//! request to tolerate. What a client sends and reads about those hosts is the
//! protocol's own ([`aj_wire::EnrollHostRequest`], [`aj_wire::HostList`]).
//!
//! It records two different things, and the difference is the point. A dynamic
//! enrollment exists *because* this file says so, so an entry removed from it
//! unenrolls that host. A configured host exists because the configuration file
//! says so, and what this one keeps for it is only the id it answered to, so
//! that a host which is down when the gateway starts is still named by the id
//! its sessions are namespaced under. Recording a configured host's existence
//! here too would resurrect one the operator deleted from that file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gateway::config::HostAddress;

/// One host as the gateway's state records it: where to dial it, and the id it
/// answered to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EnrolledHost {
    pub(crate) address: HostAddress,
    /// The id the host reported, which is the namespace its sessions appear
    /// under.
    ///
    /// Recorded rather than re-learned so that a restarted gateway can route and
    /// label a host's sessions from the first instant, including while that host
    /// is down. An id names a session store (spec 4), so it is a stable identity
    /// and caching it has no staleness hazard.
    pub(crate) host_id: String,
}

/// The gateway's enrollment state, as one file under `~/.aj/gateway/`.
pub(crate) struct EnrollmentFile {
    path: PathBuf,
}

/// What one gateway state file holds. Named so the JSON has keys rather than a
/// bare array, which is what leaves room for another field later.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Recorded {
    /// The dynamic enrollments, which this file is the record of: an entry here
    /// is what makes that host enrolled after a restart.
    #[serde(default)]
    pub(crate) hosts: Vec<EnrolledHost>,
    /// The ids learned for hosts the configuration file enrolls. Identity only:
    /// an entry here enrolls nothing, so it cannot bring back a host the
    /// operator removed from that file, and one whose address is no longer
    /// configured is simply dropped.
    #[serde(default)]
    pub(crate) configured_ids: Vec<EnrolledHost>,
}

impl EnrollmentFile {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("hosts.json"),
        }
    }

    /// What is recorded here, empty when there is no file yet.
    ///
    /// A file that exists and cannot be read is an error rather than an empty
    /// set: answering "no hosts" would make the next write erase the operator's
    /// enrollments, and a gateway that quietly serves nothing is the worst
    /// outcome of a bad byte on disk.
    pub(crate) fn load(&self) -> Result<Recorded, EnrollmentError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Recorded::default());
            }
            Err(source) => {
                return Err(EnrollmentError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        serde_json::from_str(&text).map_err(|source| EnrollmentError::Parse {
            path: self.path.clone(),
            source,
        })
    }

    /// Replaces what is recorded with `recorded`.
    ///
    /// Written through a temp file in the same directory and renamed, so a
    /// reader sees the old set or the new one and never a torn one.
    pub(crate) fn save(&self, recorded: &Recorded) -> Result<(), EnrollmentError> {
        let parent = self.path.parent().ok_or_else(|| EnrollmentError::Write {
            path: self.path.clone(),
            reason: "the state file has no parent directory".to_string(),
        })?;
        std::fs::create_dir_all(parent).map_err(|err| EnrollmentError::Write {
            path: parent.to_path_buf(),
            reason: err.to_string(),
        })?;
        let body = serde_json::to_vec_pretty(recorded).map_err(|err| EnrollmentError::Write {
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
        assert_eq!(
            file.load().expect("an absent file is an empty record"),
            Recorded::default(),
            "a gateway that has enrolled nothing has no file to read",
        );

        let recorded = Recorded {
            hosts: vec![
                enrolled("127.0.0.1:6161", "aaa"),
                enrolled("100.64.0.2:6161", "bbb"),
            ],
            configured_ids: vec![enrolled("100.64.0.3:6161", "ccc")],
        };
        file.save(&recorded).expect("save");
        assert_eq!(file.load().expect("load"), recorded);

        // A save replaces the record rather than appending to it, which is what
        // makes a withdrawal outlive the process that served it.
        let shrunk = Recorded {
            hosts: recorded.hosts[..1].to_vec(),
            configured_ids: Vec::new(),
        };
        file.save(&shrunk).expect("save");
        assert_eq!(file.load().expect("load"), shrunk);
    }

    /// A file written before the gateway kept configured hosts' ids reads as
    /// having none, rather than failing the start of a gateway that has
    /// enrollments in it.
    #[test]
    fn a_file_with_no_configured_ids_reads_as_naming_none() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = EnrollmentFile::new(dir.path());
        std::fs::write(
            dir.path().join("hosts.json"),
            r#"{"hosts":[{"address":"http://127.0.0.1:6161","host_id":"aaa"}]}"#,
        )
        .expect("write");

        assert_eq!(
            file.load().expect("load"),
            Recorded {
                hosts: vec![enrolled("127.0.0.1:6161", "aaa")],
                configured_ids: Vec::new(),
            },
        );
    }

    #[test]
    fn the_state_file_is_created_under_a_directory_that_does_not_exist_yet() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = EnrollmentFile::new(&dir.path().join("gateway"));
        file.save(&Recorded {
            hosts: vec![enrolled("127.0.0.1:6161", "aaa")],
            configured_ids: Vec::new(),
        })
        .expect("the directory is created on the way");
        assert_eq!(file.load().expect("load").hosts.len(), 1);
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

        std::fs::write(
            dir.path().join("hosts.json"),
            r#"{"configured_ids":[{"address":"nope nope","host_id":"aaa"}]}"#,
        )
        .expect("write");
        assert!(
            matches!(file.load(), Err(EnrollmentError::Parse { .. })),
            "and so is one in the record that is only an id",
        );
    }
}
