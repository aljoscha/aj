//! `aj version` names the build it runs from.

use std::process::Command;

fn stdout(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).expect("utf-8 output"))
}

/// The binary reports the cargo version and the revision of the checkout it
/// was built from, the same through the subcommand and the flag. A stale
/// baked-in revision is the failure this guards: cargo builds the binary
/// before running the test, so it must name the current `HEAD`.
#[test]
fn version_names_the_cargo_version_and_the_current_revision() {
    let aj = env!("CARGO_BIN_EXE_aj");
    let reported = stdout(Command::new(aj).arg("version")).expect("aj version succeeds");
    let flag = stdout(Command::new(aj).arg("--version")).expect("aj --version succeeds");
    assert_eq!(reported, flag);

    let revision = reported
        .trim_end()
        .strip_prefix(concat!("aj ", env!("CARGO_PKG_VERSION"), " ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected version line: {reported:?}"));

    let head = stdout(
        Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR")),
    );
    match head {
        Some(head) => assert_eq!(revision.trim_end_matches("-dirty"), head.trim()),
        // Built outside a git checkout, where there is no revision to name.
        None => assert_eq!(revision, "unknown"),
    }
}
