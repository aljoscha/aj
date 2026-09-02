//! One label rule at every site.
//!
//! The rule lives in three sibling crates with no edges between them, stated
//! three times on purpose (see `normalize_host_name`'s doc). This table is
//! what keeps the statements from drifting: every case runs against every
//! site, so a site that grows stricter or laxer than its siblings fails here
//! by name. The fourth label site, a peer name arriving through the gateway,
//! is `normalize_host_name` applied at ingress and is pinned by the gateway's
//! own tests.

use aj_models::auth::{AccountLabelError, MAX_ACCOUNT_LABEL_BYTES, normalize_account_label};
use aj_session::{MAX_TAG_BYTES, TagError, normalize_tag};
use aj_wire::{HostNameError, MAX_HOST_NAME_BYTES, normalize_host_name};

/// What the shared rule says about one input.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Kept(&'static str),
    Blank,
    Control,
}

/// One site, adapted to a common shape: its name, its byte bound, and the
/// rule mapped onto the shared verdict vocabulary.
struct Site {
    name: &'static str,
    max_bytes: usize,
    apply: fn(&str) -> SiteResult,
}

#[derive(Debug, PartialEq, Eq)]
enum SiteResult {
    Kept(String),
    Blank,
    Control,
    TooLong,
}

fn sites() -> Vec<Site> {
    vec![
        Site {
            name: "session tag (aj-session)",
            max_bytes: MAX_TAG_BYTES,
            apply: |input| match normalize_tag(input) {
                Ok(Some(kept)) => SiteResult::Kept(kept),
                Ok(None) => SiteResult::Blank,
                Err(TagError::Control) => SiteResult::Control,
                Err(TagError::TooLong { .. }) => SiteResult::TooLong,
            },
        },
        Site {
            name: "host name (aj-wire)",
            max_bytes: MAX_HOST_NAME_BYTES,
            apply: |input| match normalize_host_name(input) {
                Ok(Some(kept)) => SiteResult::Kept(kept),
                Ok(None) => SiteResult::Blank,
                Err(HostNameError::Control) => SiteResult::Control,
                Err(HostNameError::TooLong { .. }) => SiteResult::TooLong,
            },
        },
        Site {
            name: "account label (aj-models)",
            max_bytes: MAX_ACCOUNT_LABEL_BYTES,
            apply: |input| match normalize_account_label(input) {
                Ok(Some(kept)) => SiteResult::Kept(kept),
                Ok(None) => SiteResult::Blank,
                Err(AccountLabelError::Control) => SiteResult::Control,
                Err(AccountLabelError::TooLong { .. }) => SiteResult::TooLong,
            },
        },
    ]
}

/// The shared cases. Allow UTF-8 and be done with it: a multi-scalar emoji, a
/// combining mark, a right-to-left string, a zero-width joiner, and even a
/// bidi control are all labels. Control characters and over-long inputs are
/// the only refusals, and blank input names nothing.
fn cases() -> Vec<(&'static str, &'static str, Verdict)> {
    vec![
        ("plain", "fix-auth", Verdict::Kept("fix-auth")),
        ("padding trims", "  fix-auth  ", Verdict::Kept("fix-auth")),
        ("empty is blank", "", Verdict::Blank),
        ("all-space is blank", "   ", Verdict::Blank),
        (
            "multi-scalar emoji",
            "\u{1F469}\u{200D}\u{1F33E} farmer",
            Verdict::Kept("\u{1F469}\u{200D}\u{1F33E} farmer"),
        ),
        (
            "combining mark",
            "cafe\u{301}",
            Verdict::Kept("cafe\u{301}"),
        ),
        (
            "right-to-left",
            "\u{5E9}\u{5DC}\u{5D5}\u{5DD}",
            Verdict::Kept("\u{5E9}\u{5DC}\u{5D5}\u{5DD}"),
        ),
        (
            "zero-width joiner",
            "a\u{200D}b",
            Verdict::Kept("a\u{200D}b"),
        ),
        (
            "bidi control is a renderer question, not a label one",
            "\u{202E}abc",
            Verdict::Kept("\u{202E}abc"),
        ),
        ("newline is control", "two\nlines", Verdict::Control),
        ("escape is control", "\u{1B}[31mred", Verdict::Control),
        ("interior bell is control", "di\u{7}ng", Verdict::Control),
    ]
}

#[test]
fn every_label_site_applies_the_one_rule() {
    for site in sites() {
        for (case, input, expected) in cases() {
            let got = (site.apply)(input);
            let want = match &expected {
                Verdict::Kept(kept) => SiteResult::Kept((*kept).to_string()),
                Verdict::Blank => SiteResult::Blank,
                Verdict::Control => SiteResult::Control,
            };
            assert_eq!(got, want, "{}: case {case:?} diverged", site.name);
        }
    }
}

#[test]
fn every_label_site_bounds_bytes_at_its_own_limit() {
    for site in sites() {
        let exact = "a".repeat(site.max_bytes);
        assert_eq!(
            (site.apply)(&exact),
            SiteResult::Kept(exact.clone()),
            "{}: the exact bound is within the rule",
            site.name
        );
        let over = "a".repeat(site.max_bytes + 1);
        assert_eq!(
            (site.apply)(&over),
            SiteResult::TooLong,
            "{}: one byte past the bound is refused",
            site.name
        );
        // The bound applies to the trimmed label, so padding cannot push a
        // fitting label over it.
        let padded = format!("  {exact}  ");
        assert_eq!(
            (site.apply)(&padded),
            SiteResult::Kept(exact),
            "{}: padding does not count against the bound",
            site.name
        );
    }
}
