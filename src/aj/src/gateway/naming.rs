//! Namespaced session ids (spec 6.2).
//!
//! A gateway addresses a session as `<host_id>:<session_id>` and treats the
//! whole string as opaque in its own API. This is the one place that grammar is
//! written down, format and parse together, so no handler reaches for
//! `split_once(':')` on its own and no two of them disagree about what the
//! halves mean.

use std::fmt;

/// What separates the two halves of a namespaced id. A colon, which is valid
/// in a URL path segment (spec 6.2).
const SEPARATOR: char = ':';

/// Longest host id a gateway will namespace with. Generous next to the 32
/// hexadecimal characters a host mints, and short enough that a namespaced id
/// stays a comfortable path segment.
const MAX_HOST_ID: usize = 128;

/// One session as a gateway addresses it: the enrolled host that owns it, and
/// the id that host knows it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionAddress {
    pub(crate) host: String,
    pub(crate) session: String,
}

impl SessionAddress {
    pub(crate) fn new(host: &str, session: &str) -> Self {
        Self {
            host: host.to_string(),
            session: session.to_string(),
        }
    }

    /// Splits a namespaced id at the **first** separator.
    ///
    /// The first and not the last: a session id cannot contain a colon (the
    /// store's grammar admits only alphanumerics, `-` and `_`) and neither can
    /// a host id this gateway accepted ([`validate_host_id`]), so there is only
    /// ever one. Splitting at the last would differ only for input neither side
    /// can produce, and would mis-route it silently instead of refusing it.
    ///
    /// The session half is judged by [`addressable_session`] and by nothing
    /// else here.
    pub(crate) fn parse(raw: &str) -> Result<Self, AddressError> {
        let (host, session) = raw
            .split_once(SEPARATOR)
            .ok_or(AddressError::NotNamespaced)?;
        validate_host_id(host).map_err(AddressError::Host)?;
        addressable_session(session)?;
        Ok(Self::new(host, session))
    }
}

/// Whether `session` can be the session half of an id addressed here.
///
/// Deliberately not the store's grammar. The half belongs to the host, which
/// validates its own ids and answers 404 for one it could never hold (spec
/// 6.2), and a gateway that judged it too would have to be upgraded whenever a
/// host's grammar grew. Two exceptions, both about the half staying a path
/// segment of its own when a proxied URL is built from it: an empty one, and a
/// dot segment. `.` and `..` disappear from a URL path, so forwarding one would
/// send the host a request for a *different route* than the client named, which
/// is how a request about one session would become a create.
///
/// Read by whatever puts a session id on this gateway's wire as well as by
/// [`SessionAddress::parse`], so what a gateway publishes and what it resolves
/// cannot drift.
pub(crate) fn addressable_session(session: &str) -> Result<(), AddressError> {
    if session.is_empty() {
        return Err(AddressError::EmptySession);
    }
    if session == "." || session == ".." {
        return Err(AddressError::DotSession);
    }
    Ok(())
}

impl fmt::Display for SessionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{SEPARATOR}{}", self.host, self.session)
    }
}

/// Why a string is not an id this gateway addresses a session by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AddressError {
    /// No separator at all: a host-local id, which is what a client of a host
    /// holds and never what a client of a gateway does.
    NotNamespaced,
    Host(HostIdError),
    EmptySession,
    /// A session half of `.` or `..`, which a URL path would swallow.
    DotSession,
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotNamespaced => {
                write!(f, "a session id here is <host_id>{SEPARATOR}<session_id>")
            }
            Self::Host(err) => write!(f, "{err}"),
            Self::EmptySession => write!(f, "the session half of the id is empty"),
            Self::DotSession => write!(
                f,
                "the session half of the id is a path segment a URL would drop",
            ),
        }
    }
}

impl std::error::Error for AddressError {}

/// Why a string is not a host id this gateway will use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostIdError {
    Empty,
    TooLong {
        len: usize,
    },
    /// A character outside the grammar, quoted so a message can name it.
    Illegal {
        found: char,
    },
}

impl fmt::Display for HostIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a host id must not be empty"),
            Self::TooLong { len } => {
                write!(
                    f,
                    "a host id is at most {MAX_HOST_ID} bytes, this one is {len}"
                )
            }
            Self::Illegal { found } => write!(
                f,
                "a host id holds alphanumerics, {:?} and {:?} only, this one holds {found:?}",
                '-', '_'
            ),
        }
    }
}

impl std::error::Error for HostIdError {}

/// Whether `id` is a host id a gateway will namespace and address hosts by.
///
/// Non-empty, at most [`MAX_HOST_ID`] bytes, ASCII alphanumerics plus `-` and
/// `_`. The grammar follows from what the id is used *as*, not from how a host
/// mints it: it is the half before the separator in a [`SessionAddress`], so a
/// colon in it would make every id of that host ambiguous, and it is a path
/// segment in `/v1/hosts/{id}`, so a slash or a `.` in it would make the
/// enrollment unaddressable or unsafe.
///
/// A host mints 32 hexadecimal characters, which this admits, but the id is
/// read back from a file a person can edit and reaches a gateway over the wire.
/// So it is checked at the boundary rather than trusted, exactly as a session id
/// is (spec 6.2).
pub(crate) fn validate_host_id(id: &str) -> Result<(), HostIdError> {
    if id.is_empty() {
        return Err(HostIdError::Empty);
    }
    if id.len() > MAX_HOST_ID {
        return Err(HostIdError::TooLong { len: id.len() });
    }
    match id
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        Some(found) => Err(HostIdError::Illegal { found }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespaced_id_round_trips() {
        let address = SessionAddress::new("0123456789abcdef", "2026-08-06-10-15-30-123");
        assert_eq!(
            address.to_string(),
            "0123456789abcdef:2026-08-06-10-15-30-123"
        );
        assert_eq!(
            SessionAddress::parse(&address.to_string()).expect("parses"),
            address,
        );
    }

    /// The first colon is the separator, and the session half keeps the rest.
    /// Splitting at the last would read the host as `host:a`, which is not a
    /// host id at all, so a mis-split would have mis-routed rather than refused.
    #[test]
    fn the_split_is_at_the_first_separator() {
        let address = SessionAddress::parse("host:a:b").expect("the first colon separates");
        assert_eq!(address.host, "host");
        assert_eq!(address.session, "a:b");
        assert_eq!(
            validate_host_id("host:a"),
            Err(HostIdError::Illegal { found: ':' }),
            "which is why no enrolled host can answer to that name",
        );
    }

    #[test]
    fn an_id_that_is_not_namespaced_is_refused() {
        assert_eq!(
            SessionAddress::parse("2026-08-06-10-15-30-123"),
            Err(AddressError::NotNamespaced),
        );
        assert_eq!(SessionAddress::parse(""), Err(AddressError::NotNamespaced),);
        assert_eq!(
            SessionAddress::parse(":session"),
            Err(AddressError::Host(HostIdError::Empty)),
        );
        assert_eq!(
            SessionAddress::parse("host:"),
            Err(AddressError::EmptySession)
        );
    }

    /// The session half is the host's business, so anything that survives as one
    /// path segment travels. The host answers 404 for what its own grammar
    /// refuses.
    #[test]
    fn the_session_half_is_not_judged_here() {
        for session in ["../../etc/passwd", "not a session id", "%2e%2e", "a.b"] {
            let address = SessionAddress::parse(&format!("host:{session}")).expect("parses");
            assert_eq!(address.session, session);
        }
    }

    /// A dot segment is the one shape that would not stay put: a URL path drops
    /// it, so `host:..` plus no route would address `/v1/sessions` on the host,
    /// which is its create route rather than anything about a session.
    #[test]
    fn a_dot_segment_is_not_a_session() {
        for session in [".", ".."] {
            assert_eq!(
                SessionAddress::parse(&format!("host:{session}")),
                Err(AddressError::DotSession),
                "{session:?}",
            );
        }
    }

    #[test]
    fn a_host_id_is_checked_against_the_grammar() {
        for id in ["a", "0123456789abcdef", "a-b_c", &"x".repeat(MAX_HOST_ID)] {
            validate_host_id(id).unwrap_or_else(|err| panic!("{id:?} should pass: {err}"));
        }
        assert_eq!(validate_host_id(""), Err(HostIdError::Empty));
        assert_eq!(
            validate_host_id(&"x".repeat(MAX_HOST_ID + 1)),
            Err(HostIdError::TooLong {
                len: MAX_HOST_ID + 1
            }),
        );
        for (id, found) in [
            ("with:colon", ':'),
            ("with/slash", '/'),
            ("with.dot", '.'),
            ("with space", ' '),
            ("..", '.'),
            ("héllo", 'é'),
        ] {
            assert_eq!(
                validate_host_id(id),
                Err(HostIdError::Illegal { found }),
                "{id:?}",
            );
        }
    }
}
