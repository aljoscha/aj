//! The host-picker overlay, and the resolution of the `--host` flag beside it:
//! the two ways a client says which of a peer's hosts a created session is for
//! (spec 6.6, 9.2).
//!
//! A create runs an agent in a working directory, so which host it lands on is
//! never guessed. A plain host serves one directory and a gateway with a single
//! enrolled host has only one answer, so nothing is asked there and the create
//! stays one gesture. From two hosts up the answer is the user's to give:
//! [`choice_is_ambiguous`] says when it has to be asked for, the overlay asks
//! at a terminal, and [`resolve_host`] answers for a run that has none.
//!
//! # Nothing is chosen on the operator's behalf
//!
//! The overlay opens with no host selected. Its first row is a sentinel that
//! names no host: the band sits there, Enter on it does nothing at all (its
//! filter key is absent from the id map, exactly as the session selector's
//! loading row is), and the empty filter key drops it out of the visible set as
//! soon as anything is typed. So a host is reached only by an affirmative act, a
//! cursor move or a filter, and a reflexive Enter cannot mint a session
//! anywhere. Pre-selecting a plausible row would put the create back on a host
//! nobody named, which is the failure this overlay exists to remove.
//!
//! A host a gateway has never spoken to has no id, and an id is the only thing
//! a create can name one by (spec 7.1). Such a host gets a row all the same,
//! inert by the same mechanism and saying why: a pickable row would be a lie,
//! and no row at all would be a different one in a list of the peer's hosts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aj_app::session::SessionRequest;
use aj_wire::DirectoryHost;
use vaxis::vxfw::{FilterableSelect, SelectItem, to_widget_ref};

use crate::interactive::OverlayHandles;
use crate::overlay::{OverlayPlacement, close_all, close_key_label, close_top, confirm_key_label};
use crate::settings_ui::push_window;

/// The sentinel row's filter key. Empty, so it matches only the empty query and
/// is absent from the id map, which is what makes the row inert.
const NOTHING_CHOSEN: &str = "";

/// What a client calls `host`: its id, or the address a gateway has only ever
/// known it by (spec 7.1).
///
/// A gateway names every enrolled host by exactly one of the two, so the
/// fallback covers a shape no peer sends.
fn host_label(host: &DirectoryHost) -> &str {
    host.id
        .as_deref()
        .or(host.address.as_deref())
        .unwrap_or("an unnamed host")
}

/// Whether a create has to be told which host it is for.
///
/// True from two hosts up. A peer that publishes one host or none has exactly
/// one answer and gives it itself, which is what an absent host field on the
/// wire asks it to do (spec 6.6).
pub(crate) fn choice_is_ambiguous(hosts: &[DirectoryHost]) -> bool {
    hosts.len() > 1
}

/// Why a `--host` value did not name one host.
///
/// Both arms list what the user could have said, which is the vocabulary the
/// gateway's own refusal uses. Neither list is ever empty: a match set that
/// disambiguates nothing holds at least two, and [`resolve_host`] is not called
/// with no candidates.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HostQueryError {
    #[error("{query:?} names none of this peer's hosts: {}", candidates.join(", "))]
    NoMatch {
        query: String,
        candidates: Vec<String>,
    },
    #[error(
        "{query:?} names {} of this peer's hosts: {}",
        matched.len(),
        matched.join(", ")
    )]
    Ambiguous { query: String, matched: Vec<String> },
}

/// The id of the one host in `hosts` that `query` names, for the `--host` flag.
///
/// An exact id wins outright. Failing that, a value that is a prefix of exactly
/// one id resolves to it, so an operator types as much of a 32-hex id as it
/// takes to be unique. What comes back is always the full id, because that is
/// the only form a peer resolves a create against (spec 6.6).
///
/// A host with no id is no candidate: a create cannot name it (spec 7.1). It is
/// still listed among the candidates a refusal names, so an operator who typed
/// its address is told what they are looking at rather than that it does not
/// exist.
///
/// `hosts` is expected to hold at least one entry. A peer with no hosts has
/// nothing to resolve against and nothing to create on either, which is the
/// peer's own refusal to word.
pub(crate) fn resolve_host(hosts: &[DirectoryHost], query: &str) -> Result<String, HostQueryError> {
    let ids = || hosts.iter().filter_map(|host| host.id.as_deref());
    if let Some(exact) = ids().find(|id| *id == query) {
        return Ok(exact.to_string());
    }
    let matched: Vec<&str> = ids().filter(|id| id.starts_with(query)).collect();
    match matched.as_slice() {
        [sole] => Ok((*sole).to_string()),
        [] => Err(HostQueryError::NoMatch {
            query: query.to_string(),
            candidates: hosts
                .iter()
                .map(|host| host_label(host).to_string())
                .collect(),
        }),
        _ => Err(HostQueryError::Ambiguous {
            query: query.to_string(),
            matched: matched.iter().map(|id| (*id).to_string()).collect(),
        }),
    }
}

/// Open the host picker over `hosts`. A confirmed host row parks a
/// [`SessionRequest::New`] naming it in `handles.session_request`; the sentinel
/// row, a host with no id, and Esc park nothing, so nothing is minted. Does not
/// move focus: the caller posts the refocus event.
pub(crate) fn open_host_picker(handles: &OverlayHandles, hosts: &[DirectoryHost]) {
    let (items, ids) = rows(hosts);
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        items,
        handles.chrome.select.clone(),
    )));
    let focus = select.borrow().focus_target();
    let ids = Rc::new(ids);
    {
        let mut sel = select.borrow_mut();
        let ids_c = Rc::clone(&ids);
        let request_c = Rc::clone(&handles.session_request);
        let stack_c = Rc::clone(&handles.stack);
        let editor_c = Rc::clone(&handles.editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            // A row that names no host this create can be for: the sentinel the
            // picker opens on, or a host the gateway has never reached. Leave
            // the overlay open, having done nothing.
            let Some(host) = ids_c.get(&item.filter_key) else {
                return;
            };
            *request_c.borrow_mut() = Some(SessionRequest::New {
                host: Some(host.clone()),
            });
            close_all(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(&handles.stack);
        let editor_cancel = Rc::clone(&handles.editor);
        sel.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    push_window(
        &handles.stack,
        &handles.chrome,
        "New session on",
        subtitle(),
        to_widget_ref(Rc::clone(&select)),
        focus,
        OverlayPlacement::Small,
    );
}

/// The picker's rows, sentinel first, and the filter-key -> host-id map its
/// confirm resolves a row through (the callback sees only the filter key).
///
/// A row's filter key is its label, so typing any part of an id or an address
/// finds it. Only a host with an id reaches the map, which is what leaves every
/// other row inert.
fn rows(hosts: &[DirectoryHost]) -> (Vec<SelectItem>, HashMap<String, String>) {
    let mut items = vec![
        SelectItem::new("no host chosen", NOTHING_CHOSEN)
            .with_description("move to a host below, then confirm"),
    ];
    let mut ids = HashMap::new();
    for host in hosts {
        let label = host_label(host);
        let mut item = SelectItem::new(label, label);
        match (&host.id, host.unreachable) {
            (Some(id), unreachable) => {
                ids.insert(label.to_string(), id.clone());
                if unreachable {
                    item = item.with_description("unreachable");
                }
            }
            // No id, so nothing a create can name: the gateway has never
            // spoken to this host and does not invent an id for it (spec 7.1).
            (None, _) => item = item.with_description("never reached, nothing to create on yet"),
        }
        items.push(item);
    }
    (items, ids)
}

/// The window subtitle: what confirming does, and how to leave.
fn subtitle() -> String {
    format!(
        "{} to create the session there  \u{2022}  {} to cancel",
        confirm_key_label(),
        close_key_label(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned(id: &str) -> DirectoryHost {
        DirectoryHost {
            id: Some(id.to_string()),
            address: None,
            unreachable: false,
        }
    }

    fn unreachable(id: &str) -> DirectoryHost {
        DirectoryHost {
            unreachable: true,
            ..learned(id)
        }
    }

    /// A host a gateway has never spoken to: an address and no id, which is
    /// also always unreachable (spec 7.1).
    fn unseen(address: &str) -> DirectoryHost {
        DirectoryHost {
            id: None,
            address: Some(address.to_string()),
            unreachable: true,
        }
    }

    /// One host, or none, is not a choice: the peer answers it itself, so the
    /// create stays one gesture and no overlay appears.
    #[test]
    fn one_host_or_none_is_not_ambiguous() {
        assert!(!choice_is_ambiguous(&[]));
        assert!(!choice_is_ambiguous(&[learned("a")]));
        assert!(choice_is_ambiguous(&[learned("a"), learned("b")]));
    }

    /// The row the picker opens on names no host, so it is absent from the id
    /// map and a confirm on it can resolve to nothing. This is what a bare
    /// Enter lands on.
    #[test]
    fn the_first_row_names_no_host() {
        let (items, ids) = rows(&[learned("aaa"), learned("bbb")]);
        assert_eq!(items[0].filter_key, NOTHING_CHOSEN);
        assert_eq!(
            ids.get(&items[0].filter_key),
            None,
            "the row the band opens on must resolve to no host at all",
        );
        assert_eq!(items.len(), 3, "the sentinel plus one row per host");
    }

    /// Every host is listed, and only one with an id can be confirmed.
    #[test]
    fn only_a_host_with_an_id_resolves() {
        let (items, ids) = rows(&[learned("aaa"), unseen("127.0.0.1:9"), unreachable("ccc")]);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["no host chosen", "aaa", "127.0.0.1:9", "ccc"],
            "an unreached host is named by its address rather than left out",
        );
        assert_eq!(ids.get("aaa"), Some(&"aaa".to_string()));
        assert_eq!(ids.get("ccc"), Some(&"ccc".to_string()));
        assert_eq!(
            ids.get("127.0.0.1:9"),
            None,
            "an address is a label, never something a create can name",
        );
        let descriptions: Vec<Option<&str>> = items
            .iter()
            .map(|item| item.description.as_deref())
            .collect();
        assert_eq!(
            descriptions[2],
            Some("never reached, nothing to create on yet"),
            "the row says why it cannot be picked: {descriptions:?}",
        );
        assert_eq!(descriptions[3], Some("unreachable"));
    }

    /// An exact id resolves, and it wins over a prefix relation: an id that is
    /// a prefix of a longer one is still that host's own name.
    #[test]
    fn an_exact_id_wins_over_a_prefix_of_another() {
        let hosts = [learned("abc"), learned("abcdef")];
        assert_eq!(resolve_host(&hosts, "abc").expect("exact"), "abc");
        assert_eq!(resolve_host(&hosts, "abcd").expect("prefix"), "abcdef");
    }

    /// A prefix that fits several hosts is refused with those hosts named, not
    /// resolved to the first of them.
    #[test]
    fn an_ambiguous_prefix_is_refused_with_the_candidates() {
        let err = resolve_host(&[learned("abc"), learned("abd")], "ab").expect_err("ambiguous");
        assert!(
            matches!(&err, HostQueryError::Ambiguous { matched, .. } if matched.len() == 2),
            "{err:?}",
        );
        let message = err.to_string();
        assert!(
            message.contains("abc") && message.contains("abd"),
            "the refusal names what could have been meant: {message}",
        );
    }

    /// A value nothing answers to is refused with every host listed, including
    /// the ones no create could have named.
    #[test]
    fn no_match_lists_every_host() {
        let err =
            resolve_host(&[learned("abc"), unseen("127.0.0.1:9")], "zz").expect_err("no match");
        let message = err.to_string();
        assert!(
            message.contains("abc") && message.contains("127.0.0.1:9"),
            "an operator who named an unreached host is shown it: {message}",
        );
    }

    /// A host with no id is not resolvable even by its own address: a create
    /// names hosts by id, and the address is only ever a label.
    #[test]
    fn an_address_is_not_a_create_target() {
        let hosts = [learned("abc"), unseen("127.0.0.1:9")];
        assert!(resolve_host(&hosts, "127.0.0.1:9").is_err());
    }
}
