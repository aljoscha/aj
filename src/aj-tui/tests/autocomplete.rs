//! Integration tests for `aj_tui::autocomplete::CombinedAutocompleteProvider`.
//!
//! These cover the three completion modes of the provider:
//!
//! - **Prefix extraction** (no filesystem): slash-command handling, forced
//!   extraction, and absolute-path detection within command arguments.
//! - **`@`-prefixed fuzzy file search**: walks a temp directory and
//!   verifies ranking, hidden-file handling, `.git` exclusion, quoted
//!   paths, and scoped sub-tree search. Uses `ignore::WalkBuilder`
//!   in-process so every test runs unconditionally — no external
//!   binaries, no skip predicates.
//! - **Direct path completion**: `./` and bare-name completion, including
//!   space-containing paths that get quoted.
//!
//! Every test that touches the filesystem uses a fresh `TempDir` and does
//! not depend on the layout of the host's `/tmp`.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use aj_tui::autocomplete::{
    AutocompleteProvider, CombinedAutocompleteProvider, CompletionApplied, SuggestOpts,
};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Seed a base directory with a given shape. Directories in `dirs`
/// are created (recursive) and files in `files` are written with their
/// parent directories auto-created.
#[derive(Default)]
struct FolderShape<'a> {
    dirs: &'a [&'a str],
    files: &'a [(&'a str, &'a str)],
}

fn setup_folder(base: &Path, shape: FolderShape<'_>) {
    for dir in shape.dirs {
        fs::create_dir_all(base.join(dir)).expect("mkdir");
    }
    for (rel_path, contents) in shape.files {
        let full = base.join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(&full, contents).expect("write file");
    }
}

fn suggest(
    provider: &CombinedAutocompleteProvider,
    line: &str,
    force: bool,
) -> Option<aj_tui::autocomplete::AutocompleteSuggestions> {
    suggest_at(provider, line, line.chars().count(), force)
}

fn suggest_at(
    provider: &CombinedAutocompleteProvider,
    line: &str,
    cursor_col: usize,
    force: bool,
) -> Option<aj_tui::autocomplete::AutocompleteSuggestions> {
    // Build a throw-away tokio runtime for each call. Every integration
    // test in this file ultimately routes through the async provider
    // trait; blocking at the edge keeps the tests themselves sync and
    // avoids attaching `#[tokio::test]` to every case.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    rt.block_on(async {
        provider
            .get_suggestions(
                &[line.to_string()],
                0,
                cursor_col,
                SuggestOpts {
                    cancel: tokio_util::sync::CancellationToken::new(),
                    force,
                },
            )
            .await
    })
}

fn values(suggestions: &aj_tui::autocomplete::AutocompleteSuggestions) -> Vec<String> {
    suggestions
        .items
        .iter()
        .map(|item| item.value.clone())
        .collect()
}

fn sorted_values(suggestions: &aj_tui::autocomplete::AutocompleteSuggestions) -> Vec<String> {
    let mut v = values(suggestions);
    v.sort();
    v
}

fn base_dir(provider_root: &TempDir, sub: &str) -> PathBuf {
    let path = provider_root.path().join(sub);
    fs::create_dir_all(&path).expect("mkdir");
    path
}

// ---------------------------------------------------------------------------
// should_trigger_file_completion
// ---------------------------------------------------------------------------

#[test]
fn should_trigger_file_completion_returns_false_for_top_level_slash_command() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["/mo".to_string()];
    assert!(
        !provider.should_trigger_file_completion(&lines, 0, 3),
        "Tab inside a slash-command name must not open the file picker",
    );
}

#[test]
fn should_trigger_file_completion_returns_true_in_normal_contexts() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    // Empty buffer.
    assert!(provider.should_trigger_file_completion(&[String::new()], 0, 0));

    // Plain prose.
    let lines = vec!["hello world".to_string()];
    assert!(provider.should_trigger_file_completion(&lines, 0, 11));

    // Inside an `@`-attachment token.
    let lines = vec!["@src".to_string()];
    assert!(provider.should_trigger_file_completion(&lines, 0, 4));

    // Past the slash-command name (now in argument position).
    let lines = vec!["/cmd ".to_string()];
    assert!(provider.should_trigger_file_completion(&lines, 0, 5));
}

// ---------------------------------------------------------------------------
// extract_path_prefix
// ---------------------------------------------------------------------------

#[test]
fn extracts_root_slash_from_hey_slash_when_forced() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "hey /", true);
    assert!(
        result.is_some(),
        "forced extraction should yield suggestions"
    );
    assert_eq!(result.unwrap().prefix, "/");
}

#[test]
fn extracts_slash_a_from_plain_slash_a_when_forced() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "/A", true);
    // "/A" may return None if nothing matches, but when it does return,
    // the prefix is exactly what was typed.
    if let Some(r) = result {
        assert_eq!(r.prefix, "/A");
    }
}

#[test]
fn does_not_trigger_on_slash_command_when_forced() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "/model", true);
    assert!(
        result.is_none(),
        "forced extraction on a bare slash-command token should still suppress path suggestions",
    );
}

#[test]
fn triggers_absolute_path_inside_command_argument() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "/command /", true);
    assert!(
        result.is_some(),
        "absolute path after command arg should complete"
    );
    assert_eq!(result.unwrap().prefix, "/");
}

// ---------------------------------------------------------------------------
// @-prefixed fuzzy file suggestions
// ---------------------------------------------------------------------------

#[test]
fn at_prefix_returns_all_files_and_folders_for_empty_query() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &["src"],
            files: &[("README.md", "readme")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@", false).expect("suggestions");

    assert_eq!(sorted_values(&result), vec!["@README.md", "@src/"]);
}

#[test]
fn at_prefix_matches_file_with_extension_in_query() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[("file.txt", "content")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@file.txt", false).expect("suggestions");

    assert!(values(&result).iter().any(|v| v == "@file.txt"));
}

#[test]
fn at_prefix_filters_are_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &["src"],
            files: &[("README.md", "readme")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@re", false).expect("suggestions");

    assert_eq!(sorted_values(&result), vec!["@README.md"]);
}

#[test]
fn at_prefix_ranks_directories_before_files() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &["src"],
            files: &[("src.txt", "text")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@src", false).expect("suggestions");

    let vs = values(&result);
    assert_eq!(vs.first().map(String::as_str), Some("@src/"));
    assert!(vs.iter().any(|v| v == "@src.txt"));
}

#[test]
fn at_prefix_returns_nested_file_paths() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[("src/index.ts", "export {};\n")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@index", false).expect("suggestions");

    assert!(values(&result).iter().any(|v| v == "@src/index.ts"));
}

#[test]
fn at_prefix_matches_deeply_nested_paths() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[
                ("packages/tui/src/autocomplete.ts", "export {};"),
                ("packages/ai/src/autocomplete.ts", "export {};"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@tui/src/auto", false).expect("suggestions");
    let vs = values(&result);
    assert!(vs.iter().any(|v| v == "@packages/tui/src/autocomplete.ts"));
    assert!(!vs.iter().any(|v| v == "@packages/ai/src/autocomplete.ts"));
}

#[test]
fn at_prefix_matches_directory_in_middle_of_path() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[
                ("src/components/Button.tsx", "export {};"),
                ("src/utils/helpers.ts", "export {};"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@components/", false).expect("suggestions");
    let vs = values(&result);
    assert!(vs.iter().any(|v| v == "@src/components/Button.tsx"));
    assert!(!vs.iter().any(|v| v == "@src/utils/helpers.ts"));
}

#[test]
fn at_prefix_quotes_paths_with_spaces() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &["my folder"],
            files: &[("my folder/test.txt", "content")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@my", false).expect("suggestions");
    let vs = values(&result);
    assert!(vs.iter().any(|v| v == "@\"my folder/\""));
}

#[test]
fn at_prefix_includes_hidden_paths_but_excludes_dot_git() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[".aj", ".github", ".git"],
            files: &[
                (".aj/config.json", "{}"),
                (".github/workflows/ci.yml", "name: ci"),
                (".git/config", "[core]"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@", false).expect("suggestions");
    let vs = values(&result);
    assert!(vs.iter().any(|v| v == "@.aj/"));
    assert!(vs.iter().any(|v| v == "@.github/"));
    assert!(
        !vs.iter()
            .any(|v| v == "@.git" || v == "@.git/" || v.starts_with("@.git/")),
        "entries under .git must be excluded, got: {vs:?}",
    );
}

#[test]
fn at_prefix_explicit_dot_git_scope_returns_no_suggestions() {
    // `.git/` content is tooling state that the `@`-attachment
    // workflow has no business surfacing. A user typing
    // `@.git/HEAD` (or anything inside `.git/`) must come up empty
    // even though that path technically scopes the walker into a
    // real directory. Locks down the deliberately-strict
    // `path_has_git_component` exclusion.
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[".git"],
            files: &[
                (".git/HEAD", "ref: refs/heads/main"),
                (".git/config", "[core]"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@.git/HEAD", false);
    assert!(
        result.is_none(),
        "explicit `@.git/HEAD` must surface no suggestions, got: {:?}",
        result.map(|r| r.items.into_iter().map(|i| i.value).collect::<Vec<_>>()),
    );
    let result = suggest(&provider, "@.git/", false);
    assert!(
        result.is_none(),
        "explicit `@.git/` must surface no suggestions, got: {:?}",
        result.map(|r| r.items.into_iter().map(|i| i.value).collect::<Vec<_>>()),
    );
}

#[test]
fn at_prefix_returns_same_suggestions_when_cwd_path_contains_the_query() {
    // Regression: when the base directory's own path segments
    // coincidentally contain the query string (e.g. the provider was
    // created rooted at `.../cwd-plan-repro/` and the query is
    // `@plan`), the walker must not treat the containing path as a
    // match. The suggestions should be identical to what the same
    // folder structure produces under a neutral root.
    let tmp = TempDir::new().unwrap();
    let normal_base = base_dir(&tmp, "cwd-normal");
    let query_in_path_base = base_dir(&tmp, "cwd-plan-repro");

    let shape = FolderShape {
        dirs: &["packages/coding-agent/examples/extensions/plan-mode"],
        files: &[
            (
                "packages/coding-agent/examples/extensions/plan-mode/README.md",
                "readme",
            ),
            ("packages/pods/docs/plan.md", "plan"),
        ],
    };
    // The struct is intentionally not Clone; set up both roots with
    // the same shape literal rather than threading a borrow.
    setup_folder(&normal_base, shape);
    setup_folder(
        &query_in_path_base,
        FolderShape {
            dirs: &["packages/coding-agent/examples/extensions/plan-mode"],
            files: &[
                (
                    "packages/coding-agent/examples/extensions/plan-mode/README.md",
                    "readme",
                ),
                ("packages/pods/docs/plan.md", "plan"),
            ],
        },
    );

    let query = "@plan";
    let normal_provider = CombinedAutocompleteProvider::new(vec![], &normal_base);
    let query_in_path_provider = CombinedAutocompleteProvider::new(vec![], &query_in_path_base);

    let normal = suggest(&normal_provider, query, false).expect("suggestions");
    let query_in_path = suggest(&query_in_path_provider, query, false).expect("suggestions");

    let normalize = |s: &aj_tui::autocomplete::AutocompleteSuggestions| -> Vec<String> {
        let mut out: Vec<String> = s
            .items
            .iter()
            .map(|it| {
                format!(
                    "{} :: {}",
                    it.label,
                    it.description.as_deref().unwrap_or("")
                )
            })
            .collect();
        out.sort();
        out
    };

    let normal_norm = normalize(&normal);
    let query_in_path_norm = normalize(&query_in_path);
    assert_eq!(
        query_in_path_norm, normal_norm,
        "the query appearing in the base-dir path should not change suggestions",
    );
    assert!(
        normal_norm
            .iter()
            .any(|e| e == "plan-mode/ :: packages/coding-agent/examples/extensions/plan-mode"),
        "expected the plan-mode directory entry; got {normal_norm:?}",
    );
    assert!(
        normal_norm
            .iter()
            .any(|e| e == "plan.md :: packages/pods/docs/plan.md"),
        "expected the plan.md file entry; got {normal_norm:?}",
    );
}

#[test]
fn at_prefix_continues_autocomplete_inside_quoted_paths() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[
                ("my folder/test.txt", "content"),
                ("my folder/other.txt", "content"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let line = "@\"my folder/\"";
    let cursor = line.chars().count() - 1; // inside the closing quote
    let result = suggest_at(&provider, line, cursor, false).expect("suggestions");
    let vs = values(&result);
    assert!(vs.iter().any(|v| v == "@\"my folder/test.txt\""));
    assert!(vs.iter().any(|v| v == "@\"my folder/other.txt\""));
}

#[test]
fn at_prefix_applies_quoted_completion_without_duplicating_closing_quote() {
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[("my folder/test.txt", "content")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let line = "@\"my folder/te\"";
    let cursor = line.chars().count() - 1;
    let result = suggest_at(&provider, line, cursor, false).expect("suggestions");
    let target = result
        .items
        .iter()
        .find(|i| i.value == "@\"my folder/test.txt\"")
        .expect("target item");
    let CompletionApplied { lines, .. } =
        provider.apply_completion(&[line.to_string()], 0, cursor, target, &result.prefix);
    assert_eq!(lines[0], "@\"my folder/test.txt\" ");
}

#[test]
fn at_prefix_scopes_fuzzy_search_to_relative_directories_recursively() {
    // Walk a sibling subtree via a relative scoped prefix
    // (`@../outside/a`) and surface every entry whose filename
    // contains `a`, with paths re-anchored to the user's typed
    // prefix.
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    let outside = base_dir(&tmp, "outside");
    setup_folder(
        &outside,
        FolderShape {
            dirs: &[],
            files: &[
                ("nested/alpha.ts", "export {};"),
                ("nested/deeper/also-alpha.ts", "export {};"),
                ("nested/deeper/zzz.ts", "export {};"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@../outside/a", false).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "@../outside/nested/alpha.ts"),
        "expected @../outside/nested/alpha.ts in {vs:?}",
    );
    assert!(
        vs.iter()
            .any(|v| v == "@../outside/nested/deeper/also-alpha.ts"),
        "expected nested/deeper/also-alpha.ts in {vs:?}",
    );
    assert!(
        !vs.iter().any(|v| v == "@../outside/nested/deeper/zzz.ts"),
        "zzz.ts should not match the `a` query; got {vs:?}",
    );
}

#[cfg(unix)]
#[test]
fn at_prefix_follows_symlinked_directories_for_fuzzy_search() {
    // Symlinked directories must be descended into so files only
    // reachable via the symlink show up in fuzzy results.
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    let outside = base_dir(&tmp, "outside");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[("dir/some_file.txt", "real")],
        },
    );
    setup_folder(
        &outside,
        FolderShape {
            dirs: &[],
            files: &[("some_file.txt", "symlinked")],
        },
    );
    std::os::unix::fs::symlink("../outside", base.join("symlinked_dir")).expect("create symlink");

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@some", false).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "@dir/some_file.txt"),
        "expected real file; got {vs:?}",
    );
    assert!(
        vs.iter().any(|v| v == "@symlinked_dir/some_file.txt"),
        "expected file reached via symlinked dir; got {vs:?}",
    );
}

#[cfg(unix)]
#[test]
fn at_prefix_returns_symlinked_directories_when_matching_their_name() {
    // A symlinked directory should be reported as a directory entry
    // (label ends with `/`) when the query matches the symlink name.
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    let outside = base_dir(&tmp, "outside");
    setup_folder(
        &outside,
        FolderShape {
            dirs: &[],
            files: &[("nested/file.txt", "symlinked")],
        },
    );
    std::os::unix::fs::symlink("../outside", base.join("symlinked_dir")).expect("create symlink");

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@symlinked", false).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "@symlinked_dir/"),
        "expected symlinked dir entry with trailing slash; got {vs:?}",
    );
}

#[cfg(unix)]
#[test]
fn at_prefix_returns_symlinked_files_without_requiring_type_l() {
    // Symlinks-to-files must still be returned as completions: the
    // `ignore` walker with `follow_links(true)` reports them with a
    // regular file type, so they flow through unchanged.
    let tmp = TempDir::new().unwrap();
    let base = base_dir(&tmp, "cwd");
    setup_folder(
        &base,
        FolderShape {
            dirs: &[],
            files: &[("original.txt", "content")],
        },
    );
    std::os::unix::fs::symlink("original.txt", base.join("link.txt")).expect("create symlink");

    let provider = CombinedAutocompleteProvider::new(vec![], &base);
    let result = suggest(&provider, "@link", false).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "@link.txt"),
        "expected symlink-to-file in results; got {vs:?}",
    );
}

// ---------------------------------------------------------------------------
// ./ path completion
// ---------------------------------------------------------------------------

#[test]
fn dot_slash_prefix_is_preserved_when_completing_paths() {
    let tmp = TempDir::new().unwrap();
    setup_folder(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &[("update.sh", "#!/bin/bash"), ("utils.ts", "export {};")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "./up", true).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "./update.sh"),
        "expected ./update.sh in {vs:?}",
    );
}

#[test]
fn dot_slash_prefix_is_preserved_for_directory_completions() {
    let tmp = TempDir::new().unwrap();
    setup_folder(
        tmp.path(),
        FolderShape {
            dirs: &["src"],
            files: &[("src/index.ts", "export {};")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "./sr", true).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "./src/"),
        "expected ./src/ in {vs:?}",
    );
}

// ---------------------------------------------------------------------------
// Quoted direct path completion
// ---------------------------------------------------------------------------

#[test]
fn quotes_paths_with_spaces_for_direct_completion() {
    let tmp = TempDir::new().unwrap();
    setup_folder(
        tmp.path(),
        FolderShape {
            dirs: &["my folder"],
            files: &[("my folder/test.txt", "content")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let result = suggest(&provider, "my", true).expect("suggestions");
    let vs = values(&result);
    assert!(
        vs.iter().any(|v| v == "\"my folder/\""),
        "expected quoted \"my folder/\" in {vs:?}",
    );
}

#[test]
fn continues_completion_inside_quoted_paths() {
    let tmp = TempDir::new().unwrap();
    setup_folder(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &[
                ("my folder/test.txt", "content"),
                ("my folder/other.txt", "content"),
            ],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let line = "\"my folder/\"";
    let cursor = line.chars().count() - 1;
    let result = suggest_at(&provider, line, cursor, true).expect("suggestions");
    let vs = values(&result);
    assert!(vs.iter().any(|v| v == "\"my folder/test.txt\""));
    assert!(vs.iter().any(|v| v == "\"my folder/other.txt\""));
}

#[test]
fn applies_quoted_completion_without_duplicating_closing_quote() {
    let tmp = TempDir::new().unwrap();
    setup_folder(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &[("my folder/test.txt", "content")],
        },
    );

    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());
    let line = "\"my folder/te\"";
    let cursor = line.chars().count() - 1;
    let result = suggest_at(&provider, line, cursor, true).expect("suggestions");
    let target = result
        .items
        .iter()
        .find(|i| i.value == "\"my folder/test.txt\"")
        .expect("target item");
    let CompletionApplied { lines, .. } =
        provider.apply_completion(&[line.to_string()], 0, cursor, target, &result.prefix);
    assert_eq!(lines[0], "\"my folder/test.txt\"");
}
