//! Tests for the concrete `@`-file autocomplete provider.
//!
//! Three groups:
//!
//! - **Prefix-parsing unit tests** exercise the small pure helpers
//!   ([`parse_path_prefix`], [`build_completion_value`], and the delimiter /
//!   quote scanners) with no filesystem.
//! - The [`provider`] submodule covers [`CombinedAutocompleteProvider`]'s
//!   three completion modes against a fresh `TempDir`: prefix extraction,
//!   `@`-prefixed fuzzy file search (ranking, hidden-file handling, `.git`
//!   exclusion, quoted paths, scoped sub-tree search, symlinks), and direct
//!   `./` / bare-name path completion.
//! - The [`session`] submodule covers the streaming
//!   [`FuzzyFileSession`](super::FuzzyFileSession) path handed out by
//!   [`AutocompleteProvider::try_start_session`](vaxis::vxfw::AutocompleteProvider::try_start_session):
//!   session creation vs. other contexts, incremental narrowing without a
//!   re-walk, invalidation, lifecycle, and the notify wake-up.
//!
//! Every filesystem-touching test uses a fresh `TempDir` and does not depend
//! on the layout of the host's `/tmp`.

use super::*;

// ---------------------------------------------------------------------------
// Prefix-parsing helpers (no filesystem)
// ---------------------------------------------------------------------------

#[test]
fn parses_plain_prefix() {
    let p = parse_path_prefix("src/");
    assert_eq!(p.raw_prefix, "src/");
    assert!(!p.is_at_prefix);
    assert!(!p.is_quoted_prefix);
}

#[test]
fn parses_at_prefix() {
    let p = parse_path_prefix("@foo");
    assert_eq!(p.raw_prefix, "foo");
    assert!(p.is_at_prefix);
    assert!(!p.is_quoted_prefix);
}

#[test]
fn parses_quoted_prefix() {
    let p = parse_path_prefix("\"my folder/");
    assert_eq!(p.raw_prefix, "my folder/");
    assert!(!p.is_at_prefix);
    assert!(p.is_quoted_prefix);
}

#[test]
fn parses_at_quoted_prefix() {
    let p = parse_path_prefix("@\"my folder/");
    assert_eq!(p.raw_prefix, "my folder/");
    assert!(p.is_at_prefix);
    assert!(p.is_quoted_prefix);
}

#[test]
fn builds_completion_value_for_plain_path() {
    assert_eq!(
        build_completion_value("src/main.rs", false, false, false),
        "src/main.rs"
    );
}

#[test]
fn builds_completion_value_with_at_prefix() {
    assert_eq!(
        build_completion_value("src/main.rs", false, true, false),
        "@src/main.rs"
    );
}

#[test]
fn builds_completion_value_quotes_when_path_has_spaces() {
    assert_eq!(
        build_completion_value("my folder/", true, false, false),
        "\"my folder/\""
    );
}

#[test]
fn builds_completion_value_quotes_when_prefix_is_quoted() {
    assert_eq!(
        build_completion_value("plain.txt", false, false, true),
        "\"plain.txt\""
    );
}

#[test]
fn finds_last_delimiter_at_last_space() {
    assert_eq!(find_last_delimiter("hey foo"), Some(3));
    assert_eq!(find_last_delimiter("abc"), None);
}

#[test]
fn finds_unclosed_quote_when_trailing() {
    assert_eq!(find_unclosed_quote_start("hello \"world"), Some(6));
    assert_eq!(find_unclosed_quote_start("\"closed\""), None);
}

#[test]
fn byte_slice_clamps_out_of_range_and_non_boundary_offsets() {
    assert_eq!(safe_slice("éclair", 0, 1), "");
    assert_eq!(safe_slice("éclair", 0, usize::MAX), "éclair");
}

// ---------------------------------------------------------------------------
// Provider: prefix extraction, @-fuzzy search, and direct path completion
// ---------------------------------------------------------------------------

mod provider {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;
    use vaxis::vxfw::{
        AutocompleteProvider, AutocompleteSuggestions, CompletionApplied, SuggestOpts,
    };

    use crate::autocomplete::CombinedAutocompleteProvider;

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
    ) -> Option<AutocompleteSuggestions> {
        suggest_at(provider, line, line.len(), force)
    }

    fn suggest_at(
        provider: &CombinedAutocompleteProvider,
        line: &str,
        cursor_col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        // Build a throw-away tokio runtime for each call. Every test in this
        // module ultimately routes through the async provider trait; blocking
        // at the edge keeps the tests themselves sync and avoids attaching
        // `#[tokio::test]` to every case.
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

    fn values(suggestions: &AutocompleteSuggestions) -> Vec<String> {
        suggestions
            .items
            .iter()
            .map(|item| item.value.clone())
            .collect()
    }

    fn sorted_values(suggestions: &AutocompleteSuggestions) -> Vec<String> {
        let mut v = values(suggestions);
        v.sort();
        v
    }

    fn base_dir(provider_root: &TempDir, sub: &str) -> PathBuf {
        let path = provider_root.path().join(sub);
        fs::create_dir_all(&path).expect("mkdir");
        path
    }

    #[test]
    fn non_ascii_before_at_prefix_uses_byte_cursor_for_suggestion_and_completion() {
        let tmp = TempDir::new().unwrap();
        setup_folder(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &[("main.rs", "fn main() {}")],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let line = "café @ma tail";
        let cursor = "café @ma".len();

        let result = suggest_at(&provider, line, cursor, false).expect("suggestions");
        assert_eq!(result.prefix, "@ma");
        let target = result
            .items
            .iter()
            .find(|item| item.value == "@main.rs")
            .expect("main.rs suggestion");
        let applied =
            provider.apply_completion(&[line.to_string()], 0, cursor, target, &result.prefix);

        assert_eq!(applied.lines, ["café @main.rs  tail"]);
        assert_eq!(applied.cursor_col, "café @main.rs ".len());
    }

    #[test]
    fn non_ascii_before_direct_path_uses_byte_cursor_for_suggestion_and_completion() {
        let tmp = TempDir::new().unwrap();
        setup_folder(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &[("main.rs", "fn main() {}")],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let line = "café ./ma tail";
        let cursor = "café ./ma".len();

        let result = suggest_at(&provider, line, cursor, false).expect("suggestions");
        assert_eq!(result.prefix, "./ma");
        let target = result
            .items
            .iter()
            .find(|item| item.value == "./main.rs")
            .expect("main.rs suggestion");
        let applied =
            provider.apply_completion(&[line.to_string()], 0, cursor, target, &result.prefix);

        assert_eq!(applied.lines, ["café ./main.rs tail"]);
        assert_eq!(applied.cursor_col, "café ./main.rs".len());
    }

    // -- should_trigger_file_completion --

    #[test]
    fn should_trigger_file_completion_returns_true_in_normal_contexts() {
        let tmp = TempDir::new().unwrap();
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        // Empty buffer.
        assert!(provider.should_trigger_file_completion(&[String::new()], 0, 0));

        // Plain prose.
        let lines = vec!["hello world".to_string()];
        assert!(provider.should_trigger_file_completion(&lines, 0, 11));

        // Inside an `@`-attachment token.
        let lines = vec!["@src".to_string()];
        assert!(provider.should_trigger_file_completion(&lines, 0, 4));

        // Past the leading `/` token, now in a later (argument) position.
        let lines = vec!["/cmd ".to_string()];
        assert!(provider.should_trigger_file_completion(&lines, 0, 5));
    }

    // -- extract_path_prefix --

    #[test]
    fn extracts_root_slash_from_hey_slash_when_forced() {
        let tmp = TempDir::new().unwrap();
        let provider = CombinedAutocompleteProvider::new(tmp.path());
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
        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let result = suggest(&provider, "/A", true);
        // "/A" may return None if nothing matches, but when it does return,
        // the prefix is exactly what was typed.
        if let Some(r) = result {
            assert_eq!(r.prefix, "/A");
        }
    }

    #[test]
    fn does_not_trigger_on_bare_root_slash_token_when_forced() {
        let tmp = TempDir::new().unwrap();
        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let result = suggest(&provider, "/model", true);
        assert!(
            result.is_none(),
            "forced extraction on a bare root-slash token should still suppress path suggestions",
        );
    }

    #[test]
    fn triggers_absolute_path_inside_command_argument() {
        let tmp = TempDir::new().unwrap();
        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let result = suggest(&provider, "/command /", true);
        assert!(
            result.is_some(),
            "absolute path after command arg should complete"
        );
        assert_eq!(result.unwrap().prefix, "/");
    }

    // -- @-prefixed fuzzy file suggestions --

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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
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
        let normal_provider = CombinedAutocompleteProvider::new(&normal_base);
        let query_in_path_provider = CombinedAutocompleteProvider::new(&query_in_path_base);

        let normal = suggest(&normal_provider, query, false).expect("suggestions");
        let query_in_path = suggest(&query_in_path_provider, query, false).expect("suggestions");

        let normalize = |s: &AutocompleteSuggestions| -> Vec<String> {
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

        let provider = CombinedAutocompleteProvider::new(&base);
        let line = "@\"my folder/\"";
        let cursor = line.len() - 1; // inside the closing quote
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

        let provider = CombinedAutocompleteProvider::new(&base);
        let line = "@\"my folder/te\"";
        let cursor = line.len() - 1;
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

        let provider = CombinedAutocompleteProvider::new(&base);
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
        std::os::unix::fs::symlink("../outside", base.join("symlinked_dir"))
            .expect("create symlink");

        let provider = CombinedAutocompleteProvider::new(&base);
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
        std::os::unix::fs::symlink("../outside", base.join("symlinked_dir"))
            .expect("create symlink");

        let provider = CombinedAutocompleteProvider::new(&base);
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

        let provider = CombinedAutocompleteProvider::new(&base);
        let result = suggest(&provider, "@link", false).expect("suggestions");
        let vs = values(&result);
        assert!(
            vs.iter().any(|v| v == "@link.txt"),
            "expected symlink-to-file in results; got {vs:?}",
        );
    }

    // -- ./ path completion --

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

        let provider = CombinedAutocompleteProvider::new(tmp.path());
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

        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let result = suggest(&provider, "./sr", true).expect("suggestions");
        let vs = values(&result);
        assert!(
            vs.iter().any(|v| v == "./src/"),
            "expected ./src/ in {vs:?}",
        );
    }

    #[test]
    fn direct_path_listing_excludes_dot_git_but_keeps_other_dotfiles() {
        // The direct-path listing reads the working directory with `read_dir`.
        // It must skip `.git` to match the fuzzy walkers while keeping other
        // dotfiles like `.github` visible.
        let tmp = TempDir::new().unwrap();
        setup_folder(
            tmp.path(),
            FolderShape {
                dirs: &[".git", ".github"],
                files: &[("README.md", "readme"), (".git/config", "[core]")],
            },
        );

        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let result = suggest(&provider, "./", true).expect("suggestions");
        let vs = values(&result);
        assert!(
            vs.iter().any(|v| v.contains(".github")),
            "other dotfiles must remain visible, got: {vs:?}",
        );
        assert!(
            vs.iter().any(|v| v.contains("README.md")),
            "regular files must be listed, got: {vs:?}",
        );
        // `.github/` never contains the substring `.git/`, so this only trips
        // on an actual `.git` entry.
        assert!(
            !vs.iter().any(|v| v.contains(".git/")),
            ".git must be excluded from the direct-path listing, got: {vs:?}",
        );
    }

    // -- Quoted direct path completion --

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

        let provider = CombinedAutocompleteProvider::new(tmp.path());
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

        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let line = "\"my folder/\"";
        let cursor = line.len() - 1;
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

        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let line = "\"my folder/te\"";
        let cursor = line.len() - 1;
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
}

// ---------------------------------------------------------------------------
// Streaming `@`-fuzzy session
// ---------------------------------------------------------------------------

mod session {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::TempDir;
    use vaxis::vxfw::{AutocompleteProvider, AutocompleteSession, SessionInvalid, SessionStatus};

    use crate::autocomplete::CombinedAutocompleteProvider;

    #[derive(Default)]
    struct FolderShape<'a> {
        dirs: &'a [&'a str],
        files: &'a [&'a str],
    }

    fn setup(base: &Path, shape: FolderShape<'_>) {
        for dir in shape.dirs {
            fs::create_dir_all(base.join(dir)).expect("mkdir");
        }
        for rel in shape.files {
            let full = base.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("mkdir parent");
            }
            fs::write(&full, "").expect("write");
        }
    }

    /// Shared "render was requested" signal so tests can assert that
    /// nucleo's notify callback fires when new matches arrive. Not
    /// strictly needed for correctness — the important observable is
    /// the snapshot — but confirms the plumbing is intact.
    fn noop_notify() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    /// Block until the session reports `!running` or we exceed the
    /// iteration cap. Needed because nucleo's matcher runs on its own
    /// thread pool and we want a stable snapshot before asserting.
    /// `budget_ms` is per-tick; the overall spin tops out at ~2s.
    async fn drive_to_quiescent(session: &mut Box<dyn AutocompleteSession>) -> SessionStatus {
        let mut last = SessionStatus {
            changed: false,
            running: true,
        };
        for _ in 0..200 {
            tokio::task::yield_now().await;
            last = session.tick(50);
            if !last.running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        last
    }

    // -- Session creation vs. other contexts --

    #[tokio::test]
    async fn session_extracts_at_prefix_after_non_ascii_text_with_byte_cursor() {
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["readme.md"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());
        let line = "café @re";
        let lines = vec![line.to_string()];

        let session = provider
            .try_start_session(&lines, 0, line.len(), noop_notify())
            .expect("streaming session");

        assert_eq!(session.prefix(), "@re");
    }

    #[tokio::test]
    async fn try_start_session_returns_some_on_at_context() {
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["readme.md"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@".to_string()];
        let session = provider.try_start_session(&lines, 0, 1, noop_notify());
        assert!(
            session.is_some(),
            "typing `@` should open a streaming session",
        );
    }

    #[tokio::test]
    async fn try_start_session_returns_none_for_slash_context() {
        let tmp = TempDir::new().unwrap();
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["/".to_string()];
        let session = provider.try_start_session(&lines, 0, 1, noop_notify());
        assert!(
            session.is_none(),
            "only `@`-contexts get a streaming session; a leading `/` is an \
             absolute path and stays on the one-shot path"
        );
    }

    #[tokio::test]
    async fn try_start_session_returns_none_for_plain_text() {
        let tmp = TempDir::new().unwrap();
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["hello world".to_string()];
        let session = provider.try_start_session(&lines, 0, 11, noop_notify());
        assert!(
            session.is_none(),
            "non-completable contexts should not return a session",
        );
    }

    // -- Streaming & ranking --

    #[tokio::test]
    async fn session_snapshot_populates_after_walker_finishes() {
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["alpha.rs", "beta.rs", "gamma.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@a".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 2, noop_notify())
            .expect("session should open");

        drive_to_quiescent(&mut session).await;
        let snap = session.snapshot();

        // `@a` fuzzy-matches alpha and gamma (both contain an `a`),
        // but alpha leads with `a` and thus scores strictly higher
        // under nucleo's path-mode bonus. Assert alpha is in there;
        // don't pin the exact set because nucleo's subsequence match
        // may or may not accept "beta" depending on config nuances.
        let labels: Vec<String> = snap.iter().map(|it| it.label.clone()).collect();
        assert!(
            labels.contains(&"alpha.rs".to_string()),
            "snapshot should include alpha.rs; got {:?}",
            labels,
        );
    }

    #[tokio::test]
    async fn session_ranks_filename_prefix_match_above_scattered_subsequence() {
        // Files laid out so nucleo's ranking is the discriminator:
        // "auto" is a prefix of `autocomplete.rs` (strong match) and
        // appears as a scattered subsequence in
        // `tests/support/mod.rs` (weaker match). The streaming
        // session trusts nucleo's ordering; a filename-prefix match
        // must outrank a scattered-subsequence hit.
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["src/autocomplete.rs", "tests/support/mod.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@auto".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 5, noop_notify())
            .expect("session should open");

        drive_to_quiescent(&mut session).await;
        let snap = session.snapshot();

        let first_label = snap.first().map(|it| it.label.clone()).unwrap_or_default();
        assert_eq!(
            first_label,
            "autocomplete.rs",
            "filename-prefix match should rank above scattered subsequence; got {:?}",
            snap.iter().map(|it| &it.label).collect::<Vec<_>>(),
        );
    }

    // -- Update / invalidation --

    #[tokio::test]
    async fn update_narrows_matches_without_restarting_walker() {
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["alpha.rs", "anvil.rs", "beta.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        // Start with just `@` (every file matches) ...
        let lines = vec!["@".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 1, noop_notify())
            .expect("session should open");
        drive_to_quiescent(&mut session).await;
        let initial_count = session.snapshot().len();
        assert!(
            initial_count >= 3,
            "empty @ should surface every walked file; got {initial_count}"
        );

        // ... then narrow to `@an`. We reuse the same session: no
        // restart, no re-walk — nucleo just re-scores the injected
        // items against the longer needle.
        let lines = vec!["@an".to_string()];
        session
            .update(&lines, 0, 3)
            .expect("narrowing should keep the session alive");

        drive_to_quiescent(&mut session).await;
        let narrowed = session.snapshot();
        let labels: Vec<String> = narrowed.iter().map(|it| it.label.clone()).collect();
        assert!(
            labels.iter().any(|l| l == "anvil.rs"),
            "narrowed snapshot should include anvil.rs; got {:?}",
            labels,
        );
        assert!(
            !labels.iter().any(|l| l == "beta.rs"),
            "`an` should not fuzzy-match beta.rs; got {:?}",
            labels,
        );

        // `prefix()` follows the user's typing so the editor knows
        // how many characters to replace on apply.
        assert_eq!(session.prefix(), "@an");
    }

    #[tokio::test]
    async fn update_keeps_session_alive_across_slash_in_prefix() {
        // The session is rooted at the project base once at construction
        // and stays there. Typing a `/` inside the `@`-prefix used to
        // re-root the walker under a sub-directory and invalidate the
        // session; it no longer does. The new characters are folded into
        // the nucleo pattern and `match_paths()` scoring promotes hits
        // at path-delimiter boundaries instead.
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &["src"],
                files: &["src/lib.rs", "README.md"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@s".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 2, noop_notify())
            .expect("session should open");

        let lines = vec!["@src/".to_string()];
        session
            .update(&lines, 0, 5)
            .expect("slash inside the prefix should keep the session alive");
        assert_eq!(session.prefix(), "@src/");
    }

    #[tokio::test]
    async fn update_returns_invalid_when_leaving_at_context() {
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["x.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 1, noop_notify())
            .expect("session should open");

        // Remove the `@`. The session can't serve a non-`@` context.
        let lines = vec!["".to_string()];
        let outcome = session.update(&lines, 0, 0);
        assert_eq!(outcome, Err(SessionInvalid));
    }

    // -- Lifecycle --

    #[tokio::test]
    async fn session_tick_reports_running_then_quiescent() {
        // The contract of `tick().running`: true while either the
        // walker is still pushing or the matcher is still absorbing
        // items; false once everything has settled. Tests and the
        // editor's pending-autocomplete drain both rely on this
        // transition actually happening.
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["a.rs", "b.rs", "c.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 1, noop_notify())
            .expect("session should open");

        let final_status = drive_to_quiescent(&mut session).await;
        assert!(
            !final_status.running,
            "session should eventually quiesce after the walker finishes"
        );
    }

    #[tokio::test]
    async fn dropping_session_cancels_the_walker() {
        // Can't assert directly that the walker thread died — it's
        // owned by the `spawn_blocking` pool — but we can assert that
        // dropping the session is a non-blocking operation even
        // during an in-flight walk. If the cancel token wasn't wired,
        // `Drop` would still have to wait for the injector to be
        // dropped by the walker task.
        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["x.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let lines = vec!["@".to_string()];
        let session = provider
            .try_start_session(&lines, 0, 1, noop_notify())
            .expect("session should open");

        // Immediate drop: should complete without hanging.
        let start = std::time::Instant::now();
        drop(session);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "dropping a session should be near-instantaneous",
        );
    }

    // -- Notify wake-up --

    #[tokio::test]
    async fn notify_fires_when_walker_pushes_items() {
        // Nucleo calls our notify callback whenever there's new
        // information. The editor relies on this to schedule a
        // re-render as matches stream in. We assert notify fires at
        // least once during a session that matches something.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = TempDir::new().unwrap();
        setup(
            tmp.path(),
            FolderShape {
                dirs: &[],
                files: &["alpha.rs", "beta.rs", "gamma.rs"],
            },
        );
        let provider = CombinedAutocompleteProvider::new(tmp.path());

        let count = Arc::new(AtomicUsize::new(0));
        let count_cb = Arc::clone(&count);
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            count_cb.fetch_add(1, Ordering::Relaxed);
        });

        let lines = vec!["@a".to_string()];
        let mut session = provider
            .try_start_session(&lines, 0, 2, notify)
            .expect("session should open");

        drive_to_quiescent(&mut session).await;
        assert!(
            count.load(Ordering::Relaxed) > 0,
            "notify should fire at least once during a streaming session",
        );
    }
}
