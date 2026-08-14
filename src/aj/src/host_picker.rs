//! The host-picker overlay, and the resolution of the `--host` flag beside it:
//! the two ways a client says which of a peer's hosts a created session is for
//! (spec 6.6, 9.2).
//!
//! A create runs an agent in a working directory, so which host it lands on is
//! never guessed. A peer with one answer gives it itself, which is what an
//! absent host field on the wire asks it to do, so a plain host and a
//! single-host gateway keep their one-gesture create. From two hosts up the
//! answer is the user's to give: the overlay asks for it at a terminal, and
//! [`resolve_host`] takes it from the command line for a run that has none.
//!
//! # Nothing is chosen on the operator's behalf
//!
//! The overlay opens with no host selected. Its first row is a sentinel that
//! names none: the band starts there, a confirm on it does nothing at all, and
//! its empty filter key drops it out of the visible set as soon as anything is
//! typed. So a host is reached only by an affirmative act, a cursor move or a
//! filter, and a reflexive confirm cannot mint a session anywhere.
//! Pre-selecting a plausible row would put the create back on a host nobody
//! named, which is the failure this overlay exists to remove.
//!
//! Rows that name no host this create can be for are inert by one mechanism:
//! their filter key is absent from the id map the confirm resolves through, so
//! the callback returns having done nothing and the overlay stays open. Besides
//! the sentinel that is a host the gateway has never spoken to, which has no id
//! and so nothing a create could name it by (spec 7.1). It gets a row anyway,
//! saying why: a pickable row would be a lie, and no row at all would be a
//! different one in a list of the peer's hosts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aj_app::session::SessionRequest;
use aj_wire::DirectoryHost;
use vaxis::vxfw::{FilterableSelect, SelectItem, to_widget_ref};

use crate::interactive::OverlayHandles;
use crate::overlay::{OverlayPlacement, close_all, close_top, subtitle_confirm_close};
use crate::settings_ui::push_window;

/// What a row shows for a host that carries neither of the two names a gateway
/// can give one, which is a shape no peer sends.
const UNNAMED: &str = "an unnamed host";

/// A name with something in it, or `None`.
///
/// An empty id is not an id: no peer can resolve a create against it, and a row
/// keyed on it would collide with the sentinel's own empty key and make that row
/// confirmable. So one is treated exactly like an absent name, here and
/// everywhere either name is read.
fn named(name: &Option<String>) -> Option<&str> {
    name.as_deref().filter(|name| !name.is_empty())
}

/// What a row calls `host`, for display only: its id, or the address a gateway
/// has only ever known it by (spec 7.1).
///
/// The address is a label and never something a create can name (spec 6.8), so
/// a row labelled with one is not a row that can be picked. See [`rows`].
fn host_label(host: &DirectoryHost) -> &str {
    named(&host.id)
        .or_else(|| named(&host.address))
        .unwrap_or(UNNAMED)
}

/// Whether a create has to be told which host it is for.
///
/// True from two hosts up, which is exactly where a gateway stops defaulting
/// the absent host field and refuses instead (spec 6.6). Below that the peer
/// answers the question itself, whether by having one host to default to or by
/// having none and saying so.
pub(crate) fn choice_is_ambiguous(hosts: &[DirectoryHost]) -> bool {
    hosts.len() > 1
}

/// Why a `--host` value did not name one host.
///
/// Every arm lists what could have been said instead, which is the vocabulary
/// the gateway's own refusals use. Neither list is ever empty: a match set that
/// disambiguates nothing holds at least two, and [`resolve_host`] is not called
/// with no candidates.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HostQueryError {
    #[error("an empty value names no host: give an id, or a prefix only one host answers to")]
    Blank,
    #[error("{query:?} names no host here: {}", candidates.join(", "))]
    NoMatch {
        query: String,
        candidates: Vec<String>,
    },
    #[error("{query:?} names {} hosts here: {}", matched.len(), matched.join(", "))]
    Ambiguous { query: String, matched: Vec<String> },
}

/// The id of the one host in `hosts` that `query` names, for the `--host` flag.
///
/// An exact id wins outright. Failing that, a value that is a prefix of exactly
/// one id resolves to it, so an operator types as much of a 32-hex id as it
/// takes to be unique. What comes back is always the full id, because that is
/// the only form a peer resolves a create against (spec 6.6).
///
/// A blank value is refused rather than read as a prefix, which every id starts
/// with: a script whose host variable came out empty has named nothing, and
/// resolving that against a peer that happens to have one host would be the
/// guess this whole path exists to avoid.
///
/// A host with no id is no candidate, because a create cannot name it (spec
/// 7.1). It is still listed among the candidates a refusal names, so an
/// operator who typed its address is shown what they are looking at rather than
/// told it does not exist.
///
/// `hosts` is expected to hold at least one entry. A peer with no hosts has
/// nothing to resolve against and nothing to create on either, which is the
/// peer's own refusal to word.
pub(crate) fn resolve_host(hosts: &[DirectoryHost], query: &str) -> Result<String, HostQueryError> {
    if query.is_empty() {
        return Err(HostQueryError::Blank);
    }
    let ids = || hosts.iter().filter_map(|host| named(&host.id));
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
/// [`SessionRequest::New`] naming it in `handles.session_request`. The sentinel
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
        // A gateway may hold more hosts than the overlay has rows for.
        sel.set_show_scrollbar(true);
        let ids_c = Rc::clone(&ids);
        let request_c = Rc::clone(&handles.session_request);
        let stack_c = Rc::clone(&handles.stack);
        let editor_c = Rc::clone(&handles.editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            let Some(host) = ids_c.get(&item.filter_key) else {
                return;
            };
            *request_c.borrow_mut() = Some(SessionRequest::New {
                host: Some(host.clone()),
            });
            // A confirmed pick is terminal: tear the whole stack down back to
            // the transcript, matching the other selectors.
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
        "New session",
        subtitle_confirm_close(),
        to_widget_ref(Rc::clone(&select)),
        focus,
        OverlayPlacement::Small,
    );
}

/// The picker's rows, sentinel first, and the filter-key -> host-id map its
/// confirm resolves a row through (the callback sees only the filter key).
///
/// A row's filter key is its label, so typing any part of an id or an address
/// finds it. Only a host with an id reaches the map.
fn rows(hosts: &[DirectoryHost]) -> (Vec<SelectItem>, HashMap<String, String>) {
    // The sentinel's filter key is empty, which is both what keeps it out of the
    // map and what makes any query at all drop it from the list.
    let mut items =
        vec![SelectItem::new("no host chosen", "").with_description("move to one, then confirm")];
    let mut ids = HashMap::new();
    for host in hosts {
        let label = host_label(host);
        let mut item = SelectItem::new(label, label);
        match named(&host.id) {
            Some(id) => {
                ids.insert(label.to_string(), id.to_string());
                if host.unreachable {
                    item = item.with_description("unreachable");
                }
            }
            None => item = item.with_description("never reached, nothing to create on yet"),
        }
        items.push(item);
    }
    (items, ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned(id: &str) -> DirectoryHost {
        DirectoryHost {
            id: Some(id.to_string()),
            address: None,
            name: None,
            unreachable: false,
        }
    }

    fn unreachable(id: &str) -> DirectoryHost {
        DirectoryHost {
            name: None,
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
            name: None,
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
    /// confirm lands on.
    #[test]
    fn the_first_row_names_no_host() {
        let (items, ids) = rows(&[learned("aaa"), learned("bbb")]);
        assert_eq!(
            ids.get(&items[0].filter_key),
            None,
            "the row the band opens on must resolve to no host at all",
        );
        assert!(
            items[0].filter_key.is_empty(),
            "and its key has to be empty, which is what drops the row from the \
             list as soon as a query is typed: {:?}",
            items[0].filter_key,
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

    /// An id with nothing in it is not a name: it cannot be picked, and it
    /// cannot make the sentinel row confirmable by colliding with its key.
    ///
    /// The client does not police the id grammar a gateway enforces at
    /// enrollment, so this is the peer's word for a host, taken as it arrives.
    #[test]
    fn an_empty_id_leaves_the_sentinel_inert() {
        let (items, ids) = rows(&[
            DirectoryHost {
                id: Some(String::new()),
                address: None,
                name: None,
                unreachable: false,
            },
            learned("bbb"),
        ]);
        assert_eq!(
            ids.get(&items[0].filter_key),
            None,
            "a bare confirm on the first row still names no host",
        );
        assert_eq!(
            items[1].label, UNNAMED,
            "and the host with the empty id is listed as one that cannot be named",
        );
        assert_eq!(ids.len(), 1, "only the real host is pickable: {ids:?}");
    }

    /// An exact id resolves, and it wins over a prefix relation: an id that is
    /// a prefix of a longer one is still that host's own name.
    #[test]
    fn an_exact_id_wins_over_a_prefix_of_another() {
        let hosts = [learned("abc"), learned("abcdef")];
        assert_eq!(resolve_host(&hosts, "abc").expect("exact"), "abc");
        assert_eq!(resolve_host(&hosts, "abcd").expect("prefix"), "abcdef");
    }

    /// Only a prefix resolves, never an interior run of characters. A value
    /// that appears in the middle of an id names it no more than a value that
    /// appears in none of them.
    #[test]
    fn an_interior_substring_is_not_a_prefix() {
        let hosts = [learned("abcdef")];
        let err = resolve_host(&hosts, "cde").expect_err("not a prefix");
        assert!(matches!(err, HostQueryError::NoMatch { .. }), "{err:?}");
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

    /// A blank value is refused wherever it lands, including against the one
    /// host a peer that has exactly one would otherwise resolve it to. A script
    /// whose host variable came out empty named nothing.
    #[test]
    fn a_blank_value_names_no_host() {
        for hosts in [vec![learned("abc")], vec![learned("abc"), learned("bcd")]] {
            let err = resolve_host(&hosts, "").expect_err("a blank value resolves to nothing");
            assert!(
                matches!(err, HostQueryError::Blank),
                "{err:?} against {} host(s)",
                hosts.len(),
            );
        }
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
