//! Deterministic generated repositories and authoritative task verification.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::schedule::{TaskInstance, validate_task_instance_identity};
use crate::snapshot::{ChangeKind, IgnorePrefixes, capture, delta};
use crate::suite::{
    ArchetypeManifest, TaskParameters, UncommonTextLane, committed_manifest, suite_revision,
};

const CHECK_PATH: &str = "check.py";

/// Error returned when a fixture cannot be generated or inspected safely.
#[derive(Debug)]
pub struct FixtureError(pub String);

impl fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FixtureError {}

impl From<std::io::Error> for FixtureError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

/// An argv-only command that the runtime may execute in its verifier guest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandRequest {
    pub argv: Vec<String>,
}

/// Captured result of a command run by the verifier guest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    /// Constructs a successful result for pure verifier tests.
    pub fn success() -> Self {
        Self {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Materialized model-visible fixture metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GeneratedFixture {
    pub task_id: String,
    pub instance_hash: String,
    pub prompt: String,
    pub allowed_changed_paths: Vec<String>,
    pub visible_check: Option<CommandRequest>,
    pub baseline_revision: String,
    pub multiple_valid_diffs: bool,
}

/// Changed-path allowlist decision and the paths that produced it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedPathAllowlistResult {
    pub passed: bool,
    pub allowed_paths: Vec<String>,
    pub changed_paths: Vec<String>,
    pub disallowed_paths: Vec<String>,
}

/// Outcome of the optional model-visible check in the verifier guest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VisibleCheckOutcome {
    NotRequired,
    Missing,
    Passed,
    Failed,
}

/// Metadata recorded for the model-visible check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VisibleCheckMetadata {
    pub request: Option<CommandRequest>,
    pub outcome: VisibleCheckOutcome,
    pub result: Option<CommandResult>,
}

/// Independent authoritative verifier decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationReport {
    pub passed: bool,
    pub reasons: Vec<String>,
    pub changed_path_allowlist: ChangedPathAllowlistResult,
    pub visible_check: VisibleCheckMetadata,
    pub hidden_check: HiddenCheckMetadata,
}

/// Parent-owned authoritative checks that are not written into the fixture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HiddenCheckMetadata {
    pub contract_passed: bool,
    pub behavior_result: Option<CommandResult>,
}

#[derive(Debug)]
struct FixturePlan {
    baseline: BTreeMap<String, Vec<u8>>,
    canonical: BTreeMap<String, Vec<u8>>,
    empty_directories: BTreeSet<String>,
    prompt: String,
    allowed_paths: Vec<String>,
    visible_check: Option<CommandRequest>,
    multiple_valid_diffs: bool,
}

impl FixturePlan {
    fn new(prompt: String, allowed_paths: Vec<String>, multiple_valid_diffs: bool) -> Self {
        Self {
            baseline: BTreeMap::new(),
            canonical: BTreeMap::new(),
            empty_directories: BTreeSet::new(),
            prompt,
            allowed_paths,
            visible_check: None,
            multiple_valid_diffs,
        }
    }

    fn file(&mut self, path: &str, baseline: impl Into<Vec<u8>>, canonical: impl Into<Vec<u8>>) {
        self.baseline.insert(path.into(), baseline.into());
        self.canonical.insert(path.into(), canonical.into());
    }

    fn unchanged_file(&mut self, path: &str, content: impl Into<Vec<u8>>) {
        let content = content.into();
        self.baseline.insert(path.into(), content.clone());
        self.canonical.insert(path.into(), content);
    }

    fn checked(&mut self, script: String) {
        self.prompt
            .push_str("\n\nRun `python3 check.py` to check the result.");
        self.unchanged_file(CHECK_PATH, script);
        self.visible_check = Some(CommandRequest {
            argv: vec!["python3".into(), CHECK_PATH.into()],
        });
    }
}

/// Generates a repository at `root` without invoking external commands.
pub fn materialize(instance: &TaskInstance, root: &Path) -> Result<GeneratedFixture, FixtureError> {
    let (manifest, archetype) = validate_instance(instance)?;
    prepare_empty_root(root)?;
    let plan = build_plan(instance, &archetype)?;
    validate_plan(instance, &archetype, &plan)?;
    write_tree(root, &plan.baseline, &plan.empty_directories)?;
    let ignores = IgnorePrefixes::from_manifest(&manifest)
        .map_err(|error| FixtureError(error.to_string()))?;
    let baseline_revision = capture(root, &ignores)
        .map_err(|error| FixtureError(error.to_string()))?
        .root_hash;
    Ok(GeneratedFixture {
        task_id: instance.task_id.clone(),
        instance_hash: instance.instance_hash.clone(),
        prompt: plan.prompt,
        allowed_changed_paths: plan.allowed_paths,
        visible_check: plan.visible_check,
        baseline_revision,
        multiple_valid_diffs: plan.multiple_valid_diffs,
    })
}

/// Applies the generator's canonical valid change without external commands.
///
/// This is intended for harness preflight and fixture tests, not evaluation
/// trials.
pub fn apply_canonical_change(instance: &TaskInstance, root: &Path) -> Result<(), FixtureError> {
    let (_, archetype) = validate_instance(instance)?;
    let plan = build_plan(instance, &archetype)?;
    for path in plan.baseline.keys() {
        if !plan.canonical.contains_key(path) {
            let absolute = safe_join(root, path)?;
            if absolute.exists() {
                fs::remove_file(absolute)?;
            }
        }
    }
    for (path, content) in &plan.canonical {
        if plan.baseline.get(path) != Some(content) {
            write_file(root, path, content)?;
        }
    }
    Ok(())
}

/// Checks a candidate tree without executing code or mutating either tree.
///
/// The caller runs `GeneratedFixture::visible_check`, when present, in its
/// separate verifier guest and supplies the immutable captured result here.
pub fn verify_candidate(
    instance: &TaskInstance,
    fixture: &GeneratedFixture,
    authoritative_root: &Path,
    candidate_root: &Path,
    check_result: Option<&CommandResult>,
    hidden_result: Option<&CommandResult>,
) -> Result<VerificationReport, FixtureError> {
    let (manifest, archetype) = validate_instance(instance)?;
    if fixture.task_id != instance.task_id || fixture.instance_hash != instance.instance_hash {
        return Err(FixtureError(
            "fixture metadata does not belong to the task instance".into(),
        ));
    }
    let plan = build_plan(instance, &archetype)?;
    validate_plan(instance, &archetype, &plan)?;
    if fixture.allowed_changed_paths != plan.allowed_paths
        || fixture.visible_check != plan.visible_check
        || fixture.multiple_valid_diffs != plan.multiple_valid_diffs
    {
        return Err(FixtureError(
            "fixture metadata does not match the generated contract".into(),
        ));
    }

    let ignores = IgnorePrefixes::from_manifest(&manifest)
        .map_err(|error| FixtureError(error.to_string()))?;
    let baseline =
        capture(authoritative_root, &ignores).map_err(|error| FixtureError(error.to_string()))?;
    if baseline.root_hash != fixture.baseline_revision {
        return Err(FixtureError(
            "authoritative fixture no longer matches its baseline revision".into(),
        ));
    }
    let candidate =
        capture(candidate_root, &ignores).map_err(|error| FixtureError(error.to_string()))?;
    let changes = delta(&baseline, &candidate);
    let changed_paths = changes
        .paths
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    let allowed = plan.allowed_paths.iter().cloned().collect::<BTreeSet<_>>();
    let disallowed_paths = changed_paths
        .iter()
        .filter(|path| !allowed.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    let allowlist = ChangedPathAllowlistResult {
        passed: !changed_paths.is_empty() && disallowed_paths.is_empty(),
        allowed_paths: plan.allowed_paths.clone(),
        changed_paths,
        disallowed_paths,
    };

    let visible_check = check_metadata(plan.visible_check.clone(), check_result);
    let mut reasons = Vec::new();
    if allowlist.changed_paths.is_empty() {
        reasons.push("the candidate is identical to the untouched baseline".into());
    }
    if !allowlist.disallowed_paths.is_empty() {
        reasons.push(format!(
            "changed paths are outside the allowlist: {}",
            allowlist.disallowed_paths.join(", ")
        ));
    }
    for change in &changes.paths {
        if change.changes.contains(&ChangeKind::Mode) {
            reasons.push(format!("file mode changed at {}", change.path));
        }
    }
    match visible_check.outcome {
        VisibleCheckOutcome::Missing => {
            reasons.push("the required visible check was not run".into())
        }
        VisibleCheckOutcome::Failed => reasons.push("the visible check failed".into()),
        VisibleCheckOutcome::NotRequired | VisibleCheckOutcome::Passed => {}
    }
    let reasons_before_contract = reasons.len();
    check_contract(instance, &plan, candidate_root, &mut reasons)?;
    let contract_passed = reasons.len() == reasons_before_contract;
    if hidden_behavior_script(instance)?.is_some()
        && !hidden_result.is_some_and(|result| result.exit_code == Some(0))
    {
        reasons.push("the hidden authoritative behavior check failed".into());
    }
    Ok(VerificationReport {
        passed: reasons.is_empty(),
        reasons,
        changed_path_allowlist: allowlist,
        visible_check,
        hidden_check: HiddenCheckMetadata {
            contract_passed,
            behavior_result: hidden_result.cloned(),
        },
    })
}

/// Generates the broad authoritative behavior check without exposing it in the fixture.
pub fn hidden_behavior_script(instance: &TaskInstance) -> Result<Option<String>, FixtureError> {
    validate_instance(instance)?;
    let script = match &instance.parameters {
        TaskParameters::MultilineEdit {
            path,
            symbol,
            boundary,
            increment,
        } => Some(format!(
            "import runpy\nm=runpy.run_path({path:?})\nf=m[{symbol:?}]\nfor value in range(-257,258):\n assert f(value)==(max(value,{boundary})+{increment})*2\n"
        )),
        TaskParameters::IndentationSensitive {
            path,
            section,
            timeout,
        } => Some(format!(
            "from pathlib import Path\nlines=Path({path:?}).read_text().splitlines()\nassert lines.count('  {section}:')==1\ns=lines.index('  {section}:')\ne=next((i for i in range(s+1,len(lines)) if lines[i] and not lines[i].startswith('    ')),len(lines))\nassert lines[s+1:e].count('    timeout: {timeout}')==1\nassert lines.count('  neighbor:')==1 and lines[-1]=='    enabled: false'\n"
        )),
        TaskParameters::NearbyChanges {
            path,
            first,
            second,
            amount,
        } => Some(format!(
            "import runpy\nm=runpy.run_path({path:?})\nfor value in range(-257,258):\n assert m[{first:?}](value)==value*{amount}\n assert m[{second:?}](value)==value+{amount}\n assert m['stable_helper'](value)==value\n"
        )),
        TaskParameters::TwoRelatedSourceFiles {
            model_path,
            view_path,
            symbol,
            default_limit,
        } => Some(format!(
            "import runpy\nmodel=runpy.run_path({model_path:?}); view=runpy.run_path({view_path:?})\nfor i in range(-64,65):\n name='name-'+str(i)+'-\\u2603'\n r=model[{symbol:?}](name)\n assert r.name==name and r.limit=={default_limit}\n assert view['format_record'](r)==f'name={{name}} limit={default_limit}'\n r=model[{symbol:?}](name,i)\n assert view['format_record'](r)==f'name={{name}} limit={{i}}'\n"
        )),
        TaskParameters::SourcePlusTest {
            source_path,
            symbol,
            boundary,
            ..
        } => Some(format!(
            "import runpy\nf=runpy.run_path({source_path:?})[{symbol:?}]\nfor value in range({low},{high}):\n try:\n  result=f(value)\n  assert value>={boundary} and result==value*2\n except ValueError:\n  assert value<{boundary}\n",
            low = boundary - 257,
            high = boundary + 258,
        )),
        TaskParameters::ThreeFileConfiguration { paths, key, values } => Some(format!(
            "from pathlib import Path\nfor path,value,lane in zip({paths:?},{values:?},range(3)):\n lines=Path(path).read_text().splitlines()\n assert lines==[f'name = \"lane-{{lane}}\"',f'{key} = {{value}}','stable = true']\n"
        )),
        TaskParameters::RenameWithContent {
            old_path,
            new_path,
            old_symbol,
            symbol,
            multiplier,
        } => Some(format!(
            "from pathlib import Path\nimport runpy\nassert not Path({old_path:?}).exists()\nm=runpy.run_path({new_path:?}); assert {old_symbol:?} not in m\nf=m[{symbol:?}]\nfor value in range(-257,258): assert f(value)==value*{multiplier}\n"
        )),
        TaskParameters::RepeatedBlocks {
            path,
            target_label,
            old_limit,
            new_limit,
        } => Some(block_check_script(
            path,
            target_label,
            *old_limit,
            *new_limit,
        )),
        TaskParameters::RepeatedMethods {
            path,
            target_type,
            method,
            suffix,
        } => Some(format!(
            "import runpy\nm=runpy.run_path({path:?}); peer=m['Peer'](); target=m[{target_type:?}]()\nfor value in [str(i) for i in range(-128,129)]+['','a:b','\\u2603','line\\nfeed']:\n assert peer.{method}(value)==f'peer:{{value}}'\n assert target.{method}(value)==f'target:{{value}}:{suffix}'\n"
        )),
        _ => None,
    };
    Ok(script)
}

fn check_metadata(
    request: Option<CommandRequest>,
    result: Option<&CommandResult>,
) -> VisibleCheckMetadata {
    let outcome = match (&request, result) {
        (None, _) => VisibleCheckOutcome::NotRequired,
        (Some(_), None) => VisibleCheckOutcome::Missing,
        (Some(_), Some(result)) if result.exit_code == Some(0) => VisibleCheckOutcome::Passed,
        (Some(_), Some(_)) => VisibleCheckOutcome::Failed,
    };
    VisibleCheckMetadata {
        request,
        outcome,
        result: result.cloned(),
    }
}

fn validate_instance(
    instance: &TaskInstance,
) -> Result<(crate::suite::SuiteManifest, ArchetypeManifest), FixtureError> {
    let manifest = committed_manifest().map_err(|error| FixtureError(error.to_string()))?;
    validate_task_instance_identity(instance).map_err(|error| FixtureError(error.to_string()))?;
    let revision = suite_revision(&manifest).map_err(|error| FixtureError(error.to_string()))?;
    if instance.suite_revision != revision {
        return Err(FixtureError(
            "task instance has a stale suite revision".into(),
        ));
    }
    let archetype = manifest
        .archetypes
        .iter()
        .find(|item| item.id == instance.archetype_id)
        .ok_or_else(|| FixtureError("task instance has an unknown archetype".into()))?;
    if instance.parameters.kind() != archetype.parameter_kind
        || !instance.task_id.starts_with(&format!("{}-", archetype.id))
    {
        return Err(FixtureError(
            "task parameters do not match the archetype".into(),
        ));
    }
    let archetype = archetype.clone();
    Ok((manifest, archetype))
}

fn validate_plan(
    instance: &TaskInstance,
    archetype: &ArchetypeManifest,
    plan: &FixturePlan,
) -> Result<(), FixtureError> {
    if plan.allowed_paths.is_empty() {
        return Err(FixtureError("generated allowlist is empty".into()));
    }
    for path in plan
        .baseline
        .keys()
        .chain(plan.canonical.keys())
        .chain(plan.empty_directories.iter())
        .chain(plan.allowed_paths.iter())
    {
        validate_relative(path)?;
        if path == "AGENTS.md" || path.ends_with("/AGENTS.md") {
            return Err(FixtureError("fixtures may not contain AGENTS.md".into()));
        }
    }
    let token = instance
        .task_seed
        .get(..12)
        .ok_or_else(|| FixtureError("task seed is too short".into()))?;
    let mut expected = archetype
        .allowlist_templates
        .iter()
        .map(|template| template.replace("{id}", token))
        .collect::<Vec<_>>();
    let mut actual = plan.allowed_paths.clone();
    expected.sort();
    actual.sort();
    if actual != expected {
        return Err(FixtureError(format!(
            "generated allowlist does not match {} manifest templates",
            archetype.id
        )));
    }
    if archetype.visible_check != plan.visible_check.is_some()
        || archetype.multiple_valid_diffs != plan.multiple_valid_diffs
    {
        return Err(FixtureError(
            "generated fixture metadata disagrees with the manifest".into(),
        ));
    }
    if plan.prompt.to_ascii_lowercase().contains("apply_patch") {
        return Err(FixtureError(
            "task prompt names the editing tool or its syntax".into(),
        ));
    }
    Ok(())
}

fn prepare_empty_root(root: &Path) -> Result<(), FixtureError> {
    if root.exists() {
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(FixtureError("fixture root must be a real directory".into()));
        }
        if fs::read_dir(root)?.next().is_some() {
            return Err(FixtureError("fixture root must be empty".into()));
        }
    } else {
        fs::create_dir_all(root)?;
    }
    Ok(())
}

fn write_tree(
    root: &Path,
    files: &BTreeMap<String, Vec<u8>>,
    empty_directories: &BTreeSet<String>,
) -> Result<(), FixtureError> {
    for directory in empty_directories {
        create_directory(root, directory)?;
    }
    for (path, content) in files {
        write_file(root, path, content)?;
    }
    Ok(())
}

fn create_directory(root: &Path, path: &str) -> Result<(), FixtureError> {
    let absolute = safe_join(root, path)?;
    fs::create_dir_all(&absolute)?;
    set_directory_modes(root, &absolute)?;
    Ok(())
}

fn set_directory_modes(root: &Path, directory: &Path) -> Result<(), FixtureError> {
    let mut current = directory.to_path_buf();
    while current != root {
        fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
        current = current
            .parent()
            .ok_or_else(|| FixtureError("generated directory escaped fixture root".into()))?
            .to_path_buf();
    }
    Ok(())
}

fn write_file(root: &Path, path: &str, content: &[u8]) -> Result<(), FixtureError> {
    let absolute = safe_join(root, path)?;
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent)?;
        set_directory_modes(root, parent)?;
    }
    fs::write(&absolute, content)?;
    fs::set_permissions(absolute, fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn safe_join(root: &Path, path: &str) -> Result<PathBuf, FixtureError> {
    validate_relative(path)?;
    Ok(root.join(path))
}

fn validate_relative(path: &str) -> Result<(), FixtureError> {
    let candidate = Path::new(path);
    if path.is_empty()
        || path.contains('\\')
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(FixtureError(format!(
            "generated path is not normalized and relative: {path}"
        )));
    }
    Ok(())
}

fn build_plan(
    instance: &TaskInstance,
    archetype: &ArchetypeManifest,
) -> Result<FixturePlan, FixtureError> {
    let multiple = archetype.multiple_valid_diffs;
    let plan = match &instance.parameters {
        TaskParameters::UniqueReplacement {
            path,
            old,
            new,
            retry_count,
        } => {
            let baseline = format!(
                "# generated record\nowner=team_{}\nstatus={old}\nretry={retry_count}\nnote=keep this line\n",
                token(instance)?
            );
            let canonical = baseline.replace(&format!("status={old}"), &format!("status={new}"));
            let mut plan = FixturePlan::new(
                format!(
                    "In `{path}`, change the record status from `{old}` to `{new}`. Keep every other byte unchanged."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan
        }
        TaskParameters::MultilineEdit {
            path,
            symbol,
            boundary,
            increment,
        } => {
            let baseline = format!(
                "def {symbol}(value):\n    adjusted = value + {increment}\n    return adjusted * 2\n"
            );
            let canonical = format!(
                "def {symbol}(value):\n    adjusted = max(value, {boundary}) + {increment}\n    return adjusted * 2\n"
            );
            let mut plan = FixturePlan::new(
                format!(
                    "Update `{symbol}` in `{path}` so inputs below {boundary} are treated as {boundary} before adding {increment}. Preserve the final multiplication by 2 and behavior at or above the boundary."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan.checked(format!(
                "import runpy\nm = runpy.run_path({path:?})\nf = m[{symbol:?}]\nfor value in ({low}, {boundary}, {high}):\n    assert f(value) == (max(value, {boundary}) + {increment}) * 2\n",
                low = boundary - 2,
                high = boundary + 3,
            ));
            plan
        }
        TaskParameters::Insertion {
            path,
            anchor,
            value,
        } => {
            let baseline = format!("queue-start\n{anchor}\nkeep-after-anchor\nqueue-end\n");
            let canonical =
                format!("queue-start\n{anchor}\n{value}\nkeep-after-anchor\nqueue-end\n");
            let mut plan = FixturePlan::new(
                format!(
                    "Insert `{value}` in `{path}` immediately after `{anchor}` and before `keep-after-anchor`. Keep the existing entries in their current order."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan
        }
        TaskParameters::Removal {
            path,
            key,
            retained_value,
        } => {
            let baseline = format!(
                "enabled = true\n{key} = \"remove-me\"\nworkers = {retained_value}\nlabel = \"stable\"\n"
            );
            let canonical =
                format!("enabled = true\nworkers = {retained_value}\nlabel = \"stable\"\n");
            let mut plan = FixturePlan::new(
                format!(
                    "Remove the `{key}` setting from `{path}`. Do not change the remaining configuration."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan
        }
        TaskParameters::IndentationSensitive {
            path,
            section,
            timeout,
        } => {
            let baseline = format!(
                "services:\n  {section}:\n    enabled: true\n    retries: 2\n  neighbor:\n    enabled: false\n"
            );
            let canonical = format!(
                "services:\n  {section}:\n    enabled: true\n    retries: 2\n    timeout: {timeout}\n  neighbor:\n    enabled: false\n"
            );
            let mut plan = FixturePlan::new(
                format!(
                    "Add `timeout: {timeout}` to the `{section}` mapping in `{path}`. It must be nested alongside `enabled` and `retries`, not under `services` or `neighbor`. Preserve the neighboring mapping."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan.checked(format!(
                "from pathlib import Path\nlines = Path({path:?}).read_text().splitlines()\nstart = lines.index('  {section}:')\nend = next((i for i in range(start + 1, len(lines)) if lines[i] and not lines[i].startswith('    ')), len(lines))\nassert '    timeout: {timeout}' in lines[start + 1:end]\nassert '  neighbor:' in lines and '    enabled: false' in lines\n"
            ));
            plan
        }
        TaskParameters::NearbyChanges {
            path,
            first,
            second,
            amount,
        } => {
            let baseline = format!(
                "def {first}(value):\n    return value + {amount}\n\ndef stable_helper(value):\n    return value\n\ndef {second}(value):\n    return value - {amount}\n"
            );
            let canonical = format!(
                "def {first}(value):\n    return value * {amount}\n\ndef stable_helper(value):\n    return value\n\ndef {second}(value):\n    return value + {amount}\n"
            );
            let mut plan = FixturePlan::new(
                format!(
                    "In `{path}`, change `{first}` to multiply its input by {amount}, and change `{second}` to add {amount}. Keep `stable_helper` unchanged."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan.checked(format!(
                "import runpy\nm = runpy.run_path({path:?})\nfor value in (-2, 0, 5):\n    assert m[{first:?}](value) == value * {amount}\n    assert m[{second:?}](value) == value + {amount}\n    assert m['stable_helper'](value) == value\n"
            ));
            plan
        }
        TaskParameters::TwoRelatedSourceFiles {
            model_path,
            view_path,
            symbol,
            default_limit,
        } => {
            let model_baseline = format!(
                "class {symbol}:\n    def __init__(self, name):\n        self.name = name\n"
            );
            let model_canonical = format!(
                "class {symbol}:\n    def __init__(self, name, limit={default_limit}):\n        self.name = name\n        self.limit = limit\n"
            );
            let view_baseline =
                "def format_record(record):\n    return f\"name={record.name}\"\n".to_string();
            let view_canonical = "def format_record(record):\n    return f\"name={record.name} limit={record.limit}\"\n".to_string();
            let mut plan = FixturePlan::new(
                format!(
                    "Extend `{symbol}` in `{model_path}` with a `limit` field that defaults to {default_limit}. Update `format_record` in `{view_path}` to include `limit=<value>` after the name. Existing one-argument construction must keep working."
                ),
                vec![model_path.clone(), view_path.clone()],
                multiple,
            );
            plan.file(model_path, model_baseline, model_canonical);
            plan.file(view_path, view_baseline, view_canonical);
            plan.checked(format!(
                "import runpy\nmodel = runpy.run_path({model_path:?})\nview = runpy.run_path({view_path:?})\nr = model[{symbol:?}]('sample')\nassert r.limit == {default_limit}\nassert view['format_record'](r) == 'name=sample limit={default_limit}'\nr2 = model[{symbol:?}]('other', 99)\nassert view['format_record'](r2) == 'name=other limit=99'\n"
            ));
            plan
        }
        TaskParameters::SourcePlusTest {
            source_path,
            test_path,
            symbol,
            boundary,
        } => {
            let source_baseline = format!("def {symbol}(value):\n    return value * 2\n");
            let source_canonical = format!(
                "def {symbol}(value):\n    if value < {boundary}:\n        raise ValueError('value below {boundary}')\n    return value * 2\n"
            );
            let test_baseline = format!(
                "import runpy\nm = runpy.run_path({source_path:?})\nassert m[{symbol:?}]({boundary}) == {result}\n",
                result = boundary * 2
            );
            let test_canonical = format!(
                "import runpy\nm = runpy.run_path({source_path:?})\nassert m[{symbol:?}]({boundary}) == {result}\ntry:\n    m[{symbol:?}]({below})\n    raise AssertionError('expected ValueError')\nexcept ValueError:\n    pass\n",
                result = boundary * 2,
                below = boundary - 1,
            );
            let mut plan = FixturePlan::new(
                format!(
                    "Update `{symbol}` in `{source_path}` to raise `ValueError` for values below {boundary}, while retaining the doubled result at and above {boundary}. Extend `{test_path}` with a regression test that exercises the generated boundary and proves the below-boundary error."
                ),
                vec![source_path.clone(), test_path.clone()],
                multiple,
            );
            plan.file(source_path, source_baseline, source_canonical);
            plan.file(test_path, test_baseline, test_canonical);
            plan.checked(format!(
                "import runpy\nm = runpy.run_path({source_path:?})\nf = m[{symbol:?}]\nassert f({boundary}) == {result}\ntry:\n    f({below})\n    raise AssertionError('expected ValueError')\nexcept ValueError:\n    pass\nrunpy.run_path({test_path:?})\n",
                result = boundary * 2,
                below = boundary - 1,
            ));
            plan
        }
        TaskParameters::ThreeFileConfiguration { paths, key, values } => {
            let mut plan = FixturePlan::new(
                format!(
                    "Set `{key}` to {}, {}, and {} in `{}`, `{}`, and `{}` respectively. Preserve every other byte in the three configuration files.",
                    values[0], values[1], values[2], paths[0], paths[1], paths[2]
                ),
                paths.to_vec(),
                multiple,
            );
            for (index, path) in paths.iter().enumerate() {
                let baseline = format!("name = \"lane-{index}\"\n{key} = 0\nstable = true\n");
                let canonical = format!(
                    "name = \"lane-{index}\"\n{key} = {}\nstable = true\n",
                    values[index]
                );
                plan.file(path, baseline, canonical);
            }
            plan.checked(format!(
                "from pathlib import Path\nexpected = {values:?}\npaths = {paths:?}\nfor path, value in zip(paths, expected):\n    text = Path(path).read_text()\n    assert f'{key} = {{value}}' in text\n    assert 'stable = true' in text\n"
            ));
            plan
        }
        TaskParameters::AddFile {
            path,
            content_token,
            number,
        } => {
            let canonical = format!("kind=generated\ntoken={content_token}\nnumber={number}\n");
            let mut plan = FixturePlan::new(
                format!(
                    "Create `{path}` as a generated record containing the fields `kind=generated`, `token={content_token}`, and `number={number}`, each exactly once. A trailing newline is required. Field order is not significant."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.canonical.insert(path.clone(), canonical.into_bytes());
            if let Some(parent) = Path::new(path).parent().and_then(Path::to_str) {
                plan.empty_directories.insert(parent.into());
            }
            plan
        }
        TaskParameters::DeleteFile { path, number } => {
            let baseline =
                format!("obsolete generated artifact\nsequence={number}\nremove this whole file\n");
            let mut plan = FixturePlan::new(
                format!("Delete the obsolete file `{path}`. Do not change any other path."),
                vec![path.clone()],
                multiple,
            );
            plan.baseline.insert(path.clone(), baseline.into_bytes());
            plan
        }
        TaskParameters::RenameWithContent {
            old_path,
            new_path,
            old_symbol,
            symbol,
            multiplier,
        } => {
            let baseline = format!("def {old_symbol}(value):\n    return value + {multiplier}\n");
            let canonical = format!("def {symbol}(value):\n    return value * {multiplier}\n");
            let mut plan = FixturePlan::new(
                format!(
                    "Rename `{old_path}` to `{new_path}`. In the renamed file, rename `{old_symbol}` to `{symbol}` and make it multiply its input by {multiplier}. The old path and old symbol must be absent."
                ),
                vec![old_path.clone(), new_path.clone()],
                multiple,
            );
            plan.baseline
                .insert(old_path.clone(), baseline.into_bytes());
            plan.canonical
                .insert(new_path.clone(), canonical.into_bytes());
            plan.checked(format!(
                "from pathlib import Path\nimport runpy\nassert not Path({old_path:?}).exists()\nm = runpy.run_path({new_path:?})\nassert {old_symbol:?} not in m\nfor value in (-2, 0, 4):\n    assert m[{symbol:?}](value) == value * {multiplier}\n"
            ));
            plan
        }
        TaskParameters::RepeatedBlocks {
            path,
            target_label,
            old_limit,
            new_limit,
        } => {
            let block = |label: &str, mode: &str, limit: u32| {
                format!("[{label}]\nmode=legacy\nlimit={limit}\nfooter=stable\n")
                    .replace("mode=legacy", &format!("mode={mode}"))
            };
            let baseline = format!(
                "{}\n{}\n{}",
                block("alpha", "legacy", *old_limit),
                block(target_label, "legacy", *old_limit),
                block("omega", "legacy", *old_limit)
            );
            let canonical = format!(
                "{}\n{}\n{}",
                block("alpha", "legacy", *old_limit),
                block(target_label, "modern", *new_limit),
                block("omega", "legacy", *old_limit)
            );
            let mut plan = FixturePlan::new(
                format!(
                    "In `{path}`, update only the `[{target_label}]` block: set `mode=modern` and `limit={new_limit}`. The structurally identical `alpha` and `omega` blocks must retain `mode=legacy` and `limit={old_limit}`."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan.checked(block_check_script(
                path,
                target_label,
                *old_limit,
                *new_limit,
            ));
            plan
        }
        TaskParameters::RepeatedMethods {
            path,
            target_type,
            method,
            suffix,
        } => {
            let baseline = format!(
                "class Peer:\n    def {method}(self, value):\n        return f\"peer:{{value}}\"\n\nclass {target_type}:\n    def {method}(self, value):\n        return f\"target:{{value}}\"\n"
            );
            let canonical = format!(
                "class Peer:\n    def {method}(self, value):\n        return f\"peer:{{value}}\"\n\nclass {target_type}:\n    def {method}(self, value):\n        return f\"target:{{value}}:{suffix}\"\n"
            );
            let mut plan = FixturePlan::new(
                format!(
                    "Change only `{target_type}.{method}` in `{path}` to append `:{suffix}` to its existing result. `Peer.{method}` must continue returning `peer:<value>` unchanged."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan.checked(format!(
                "import runpy\nm = runpy.run_path({path:?})\nassert m['Peer']().{method}('x') == 'peer:x'\nassert m[{target_type:?}]().{method}('x') == 'target:x:{suffix}'\n"
            ));
            plan
        }
        TaskParameters::EndOfFile { path, value } => {
            let baseline = "header\nbody-one\nbody-two\nend-of-data\n".to_string();
            let canonical = format!("{baseline}footer={value}\n");
            let mut plan = FixturePlan::new(
                format!(
                    "Append `footer={value}` as the new final line of `{path}`. Keep all existing bytes, leave exactly one LF after the footer, and add no blank line."
                ),
                vec![path.clone()],
                multiple,
            );
            plan.file(path, baseline, canonical);
            plan
        }
        TaskParameters::UncommonText {
            path,
            lane,
            token,
            marker_width,
            number,
        } => {
            let (baseline, canonical, prompt) = match lane {
                UncommonTextLane::ConflictMarkers => {
                    let left = "<".repeat(usize::from(*marker_width));
                    let middle = "=".repeat(usize::from(*marker_width));
                    let right = ">".repeat(usize::from(*marker_width));
                    let baseline = format!(
                        "before=stable\n{left} ours_{token}\nvalue=old_{token}\n{middle}\nvalue=other_{token}\n{right} theirs_{token}\nafter={number}\n"
                    );
                    let canonical = baseline.replace(
                        &format!("value=old_{token}"),
                        &format!("value=updated_{token}"),
                    );
                    let prompt = format!(
                        "In `{path}`, change the `ours_{token}` side's value from `old_{token}` to `updated_{token}`. Preserve all conflict markers, including their exact width of {marker_width} characters, the other side, and all line endings."
                    );
                    (baseline.into_bytes(), canonical.into_bytes(), prompt)
                }
                UncommonTextLane::Crlf => {
                    let baseline = format!(
                        "header={number}\r\nstatus=pending_{token}\r\nowner=team_{token}\r\ntail=stable\r\n"
                    );
                    let canonical = baseline.replace(
                        &format!("status=pending_{token}"),
                        &format!("status=ready_{token}"),
                    );
                    let prompt = format!(
                        "In `{path}`, change `status=pending_{token}` to `status=ready_{token}`. Preserve every other byte and retain CRLF line endings on every line."
                    );
                    (baseline.into_bytes(), canonical.into_bytes(), prompt)
                }
            };
            let mut plan = FixturePlan::new(prompt, vec![path.clone()], multiple);
            plan.file(path, baseline, canonical);
            plan
        }
    };
    if archetype.visible_check && plan.visible_check.is_none() {
        return Err(FixtureError(format!(
            "{} did not generate its visible check",
            archetype.id
        )));
    }
    Ok(plan)
}

fn token(instance: &TaskInstance) -> Result<&str, FixtureError> {
    instance
        .task_seed
        .get(..12)
        .ok_or_else(|| FixtureError("task seed is too short".into()))
}

fn block_check_script(path: &str, target: &str, old: u32, new: u32) -> String {
    format!(
        "from pathlib import Path\nblocks = {{}}\ncurrent = None\nfor raw in Path({path:?}).read_text().splitlines():\n    line = raw.strip()\n    if line.startswith('[') and line.endswith(']'):\n        current = line[1:-1]\n        blocks[current] = {{}}\n    elif current and '=' in line:\n        key, value = line.split('=', 1)\n        blocks[current][key] = value\nassert blocks[{target:?}]['mode'] == 'modern'\nassert blocks[{target:?}]['limit'] == {new_string:?}\nfor name in ('alpha', 'omega'):\n    assert blocks[name]['mode'] == 'legacy'\n    assert blocks[name]['limit'] == {old_string:?}\n",
        new_string = new.to_string(),
        old_string = old.to_string(),
    )
}

fn check_contract(
    instance: &TaskInstance,
    plan: &FixturePlan,
    root: &Path,
    reasons: &mut Vec<String>,
) -> Result<(), FixtureError> {
    match &instance.parameters {
        TaskParameters::UniqueReplacement { path, .. }
        | TaskParameters::Removal { path, .. }
        | TaskParameters::EndOfFile { path, .. }
        | TaskParameters::UncommonText { path, .. } => {
            require_exact(path, plan, root, reasons)?;
        }
        TaskParameters::MultilineEdit { path, symbol, .. } => {
            let text = read_utf8(root, path, reasons)?;
            require_contains(&text, &format!("def {symbol}("), path, reasons);
        }
        TaskParameters::Insertion {
            path,
            anchor,
            value,
        } => {
            let text = read_utf8(root, path, reasons)?;
            let lines = text
                .lines()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            let expected = [
                "queue-start",
                anchor.as_str(),
                value.as_str(),
                "keep-after-anchor",
                "queue-end",
            ];
            if lines != expected {
                reasons.push(format!(
                    "{path} does not contain the requested insertion in order"
                ));
            }
        }
        TaskParameters::IndentationSensitive {
            path,
            section,
            timeout,
        } => {
            let text = read_utf8(root, path, reasons)?;
            let lines = text.lines().collect::<Vec<_>>();
            let section_line = format!("  {section}:");
            let start = lines.iter().position(|line| *line == section_line);
            let in_section = start.is_some_and(|start| {
                lines[start + 1..]
                    .iter()
                    .take_while(|line| line.starts_with("    ") || line.is_empty())
                    .any(|line| *line == format!("    timeout: {timeout}"))
            });
            if !in_section
                || !lines.contains(&"  neighbor:")
                || !lines.contains(&"    enabled: false")
            {
                reasons.push(format!(
                    "{path} does not preserve the required YAML nesting"
                ));
            }
        }
        TaskParameters::NearbyChanges {
            path,
            first,
            second,
            ..
        } => {
            let text = read_utf8(root, path, reasons)?;
            for symbol in [first, second] {
                require_contains(&text, &format!("def {symbol}("), path, reasons);
            }
            require_contains(
                &text,
                "def stable_helper(value):\n    return value",
                path,
                reasons,
            );
        }
        TaskParameters::TwoRelatedSourceFiles {
            model_path,
            view_path,
            symbol,
            ..
        } => {
            require_changed(model_path, plan, root, reasons)?;
            require_changed(view_path, plan, root, reasons)?;
            let model = read_utf8(root, model_path, reasons)?;
            let view = read_utf8(root, view_path, reasons)?;
            require_contains(&model, &format!("class {symbol}"), model_path, reasons);
            require_contains(&model, "limit", model_path, reasons);
            require_contains(&view, "record.limit", view_path, reasons);
        }
        TaskParameters::SourcePlusTest {
            source_path,
            test_path,
            symbol,
            boundary,
        } => {
            require_changed(source_path, plan, root, reasons)?;
            require_changed(test_path, plan, root, reasons)?;
            let source = read_utf8(root, source_path, reasons)?;
            let test = read_utf8(root, test_path, reasons)?;
            require_contains(&source, &format!("def {symbol}("), source_path, reasons);
            let test_code = test
                .lines()
                .map(|line| line.split_once('#').map_or(line, |(code, _)| code))
                .collect::<Vec<_>>()
                .join("\n");
            let mentions_error = test_code.contains("ValueError")
                && (test_code.contains("except") || test_code.contains("raises"));
            if !test_code.contains(symbol)
                || !test_code.contains(&format!("({})", boundary - 1))
                || !mentions_error
            {
                reasons.push(format!(
                    "{test_path} does not structurally cover the generated boundary and error"
                ));
            }
        }
        TaskParameters::ThreeFileConfiguration { paths, .. } => {
            for path in paths {
                require_exact(path, plan, root, reasons)?;
            }
        }
        TaskParameters::AddFile {
            path,
            content_token,
            number,
        } => {
            let text = read_utf8(root, path, reasons)?;
            let fields = parse_fields(&text);
            let expected = BTreeMap::from([
                ("kind", "generated".to_string()),
                ("number", number.to_string()),
                ("token", content_token.clone()),
            ]);
            if fields != expected || !text.ends_with('\n') {
                reasons.push(format!("{path} is not the requested generated record"));
            }
        }
        TaskParameters::DeleteFile { path, .. } => {
            if safe_join(root, path)?.exists() {
                reasons.push(format!("{path} was not deleted"));
            }
        }
        TaskParameters::RenameWithContent {
            old_path,
            new_path,
            old_symbol,
            symbol,
            ..
        } => {
            if safe_join(root, old_path)?.exists() {
                reasons.push(format!("old path {old_path} still exists"));
            }
            let text = read_utf8(root, new_path, reasons)?;
            require_contains(&text, &format!("def {symbol}("), new_path, reasons);
            if text.contains(old_symbol) {
                reasons.push(format!(
                    "old symbol {old_symbol} still exists in {new_path}"
                ));
            }
        }
        TaskParameters::RepeatedBlocks {
            path,
            target_label,
            old_limit,
            new_limit,
        } => {
            let text = read_utf8(root, path, reasons)?;
            let blocks = parse_blocks(&text);
            let target_ok = blocks.get(target_label).is_some_and(|values| {
                values.get("mode").is_some_and(|value| value == "modern")
                    && values
                        .get("limit")
                        .is_some_and(|value| value == &new_limit.to_string())
                    && values.get("footer").is_some_and(|value| value == "stable")
            });
            let peers_ok = ["alpha", "omega"].iter().all(|label| {
                blocks.get(*label).is_some_and(|values| {
                    values.get("mode").is_some_and(|value| value == "legacy")
                        && values
                            .get("limit")
                            .is_some_and(|value| value == &old_limit.to_string())
                        && values.get("footer").is_some_and(|value| value == "stable")
                })
            });
            if !target_ok || !peers_ok || blocks.len() != 3 {
                reasons.push(format!(
                    "{path} does not contain the requested block-only change"
                ));
            }
        }
        TaskParameters::RepeatedMethods {
            path,
            target_type,
            method,
            ..
        } => {
            let text = read_utf8(root, path, reasons)?;
            require_contains(&text, "class Peer:", path, reasons);
            require_contains(&text, &format!("class {target_type}:"), path, reasons);
            if text.matches(&format!("def {method}(")).count() != 2 {
                reasons.push(format!("{path} no longer has both repeated methods"));
            }
        }
    }
    Ok(())
}

fn require_exact(
    path: &str,
    plan: &FixturePlan,
    root: &Path,
    reasons: &mut Vec<String>,
) -> Result<(), FixtureError> {
    let actual = fs::read(safe_join(root, path)?).ok();
    if actual.as_ref() != plan.canonical.get(path) {
        reasons.push(format!("{path} does not match the required bytes"));
    }
    Ok(())
}

fn require_changed(
    path: &str,
    plan: &FixturePlan,
    root: &Path,
    reasons: &mut Vec<String>,
) -> Result<(), FixtureError> {
    let actual = fs::read(safe_join(root, path)?).ok();
    if actual.as_ref() == plan.baseline.get(path) {
        reasons.push(format!("{path} remains unchanged"));
    }
    Ok(())
}

fn read_utf8(root: &Path, path: &str, reasons: &mut Vec<String>) -> Result<String, FixtureError> {
    match fs::read(safe_join(root, path)?) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Ok(text),
            Err(_) => {
                reasons.push(format!("{path} is not UTF-8"));
                Ok(String::new())
            }
        },
        Err(_) => {
            reasons.push(format!("{path} is missing"));
            Ok(String::new())
        }
    }
}

fn require_contains(text: &str, needle: &str, path: &str, reasons: &mut Vec<String>) {
    if !text.contains(needle) {
        reasons.push(format!("{path} is missing required structure: {needle}"));
    }
}

fn parse_fields(text: &str) -> BTreeMap<&str, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim().to_string()))
        .collect()
}

fn parse_blocks(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut blocks = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut current = None;
    for line in text.lines().map(str::trim) {
        if let Some(label) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            blocks.entry(label.into()).or_default();
            current = Some(label.to_string());
        } else if let (Some(label), Some((key, value))) = (&current, line.split_once('=')) {
            blocks
                .entry(label.clone())
                .or_default()
                .insert(key.trim().into(), value.trim().into());
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::freeze_universe;
    use crate::suite::committed_manifest;

    fn instances() -> Vec<TaskInstance> {
        let manifest = committed_manifest().unwrap();
        let first = freeze_universe(&manifest, "fixture-seed-one", 5).unwrap();
        let second = freeze_universe(&manifest, "fixture-seed-two", 5).unwrap();
        manifest
            .archetypes
            .iter()
            .flat_map(|archetype| {
                [(&first, 0), (&second, 1)].map(|(universe, index)| {
                    universe
                        .instances
                        .iter()
                        .find(|instance| {
                            instance.archetype_id == archetype.id
                                && instance.universe_index == index
                        })
                        .unwrap()
                        .clone()
                })
            })
            .collect()
    }

    fn check_result(fixture: &GeneratedFixture) -> Option<CommandResult> {
        fixture
            .visible_check
            .as_ref()
            .map(|_| CommandResult::success())
    }

    fn hidden_result(instance: &TaskInstance) -> Option<CommandResult> {
        hidden_behavior_script(instance)
            .unwrap()
            .map(|_| CommandResult::success())
    }

    fn run_hidden(instance: &TaskInstance, root: &Path) -> Option<CommandResult> {
        let script = hidden_behavior_script(instance).unwrap()?;
        let output = std::process::Command::new("python3")
            .args(["-I", "-B", "-c", &script])
            .current_dir(root)
            .output()
            .unwrap();
        Some(CommandResult {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    #[test]
    fn every_archetype_is_deterministic_and_enforces_verification() {
        for instance in instances() {
            let authoritative = tempfile::tempdir().unwrap();
            let duplicate = tempfile::tempdir().unwrap();
            let fixture = materialize(&instance, authoritative.path()).unwrap();
            let duplicate_fixture = materialize(&instance, duplicate.path()).unwrap();
            assert_eq!(fixture, duplicate_fixture, "{}", instance.task_id);
            assert!(!fixture.prompt.to_ascii_lowercase().contains("apply_patch"));
            assert!(!authoritative.path().join("AGENTS.md").exists());

            let result = check_result(&fixture);
            let hidden = hidden_result(&instance);
            let untouched = verify_candidate(
                &instance,
                &fixture,
                authoritative.path(),
                duplicate.path(),
                result.as_ref(),
                hidden.as_ref(),
            )
            .unwrap();
            assert!(!untouched.passed, "{}", instance.task_id);

            apply_canonical_change(&instance, duplicate.path()).unwrap();
            let before = fixture.baseline_revision.clone();
            let valid = verify_candidate(
                &instance,
                &fixture,
                authoritative.path(),
                duplicate.path(),
                result.as_ref(),
                hidden.as_ref(),
            )
            .unwrap();
            assert!(valid.passed, "{}: {:?}", instance.task_id, valid.reasons);
            let manifest = committed_manifest().unwrap();
            let ignores = IgnorePrefixes::from_manifest(&manifest).unwrap();
            assert_eq!(
                capture(authoritative.path(), &ignores).unwrap().root_hash,
                before,
                "verifier mutated {}",
                instance.task_id
            );

            fs::write(duplicate.path().join("unrelated.txt"), b"unrelated\n").unwrap();
            let unrelated = verify_candidate(
                &instance,
                &fixture,
                authoritative.path(),
                duplicate.path(),
                result.as_ref(),
                hidden.as_ref(),
            )
            .unwrap();
            assert!(!unrelated.passed, "{}", instance.task_id);
            assert_eq!(
                unrelated.changed_path_allowlist.disallowed_paths,
                ["unrelated.txt"]
            );
        }
    }

    #[test]
    fn all_declared_multiple_diff_tasks_accept_an_alternative() {
        let manifest = committed_manifest().unwrap();
        let universe = freeze_universe(&manifest, "alternative-seed", 5).unwrap();
        for archetype in manifest
            .archetypes
            .iter()
            .filter(|archetype| archetype.multiple_valid_diffs)
        {
            let instance = universe
                .instances
                .iter()
                .find(|instance| {
                    instance.archetype_id == archetype.id && instance.universe_index == 0
                })
                .unwrap();
            let authoritative = tempfile::tempdir().unwrap();
            let candidate = tempfile::tempdir().unwrap();
            let fixture = materialize(instance, authoritative.path()).unwrap();
            materialize(instance, candidate.path()).unwrap();
            apply_canonical_change(instance, candidate.path()).unwrap();
            make_alternative(instance, candidate.path()).unwrap();
            let result = check_result(&fixture);
            let hidden = hidden_result(instance);
            let report = verify_candidate(
                instance,
                &fixture,
                authoritative.path(),
                candidate.path(),
                result.as_ref(),
                hidden.as_ref(),
            )
            .unwrap();
            assert!(report.passed, "{}: {:?}", archetype.id, report.reasons);
        }
    }

    #[test]
    fn hidden_behavior_rejects_three_point_special_casing() {
        let instance = instances()
            .into_iter()
            .find(|instance| matches!(instance.parameters, TaskParameters::MultilineEdit { .. }))
            .unwrap();
        let authoritative = tempfile::tempdir().unwrap();
        let candidate = tempfile::tempdir().unwrap();
        let fixture = materialize(&instance, authoritative.path()).unwrap();
        materialize(&instance, candidate.path()).unwrap();
        let TaskParameters::MultilineEdit {
            path,
            symbol,
            boundary,
            increment,
        } = &instance.parameters
        else {
            unreachable!()
        };
        write_file(
            candidate.path(),
            path,
            format!(
                "def {symbol}(value):\n    if value in ({low}, {boundary}, {high}):\n        return (max(value, {boundary}) + {increment}) * 2\n    return 0\n",
                low = boundary - 2,
                high = boundary + 3,
            )
            .as_bytes(),
        )
        .unwrap();
        let visible = CommandResult::success();
        let hidden = run_hidden(&instance, candidate.path()).unwrap();
        assert_ne!(hidden.exit_code, Some(0));
        let report = verify_candidate(
            &instance,
            &fixture,
            authoritative.path(),
            candidate.path(),
            Some(&visible),
            Some(&hidden),
        )
        .unwrap();
        assert!(!report.passed);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("hidden"))
        );
    }

    #[test]
    fn authoritative_verification_uses_pre_verifier_bytes() {
        let instance = instances()
            .into_iter()
            .find(|instance| {
                matches!(
                    instance.parameters,
                    TaskParameters::UniqueReplacement { .. }
                )
            })
            .unwrap();
        let authoritative = tempfile::tempdir().unwrap();
        let candidate = tempfile::tempdir().unwrap();
        let before = tempfile::tempdir().unwrap();
        let fixture = materialize(&instance, authoritative.path()).unwrap();
        materialize(&instance, candidate.path()).unwrap();
        materialize(&instance, before.path()).unwrap();

        apply_canonical_change(&instance, candidate.path()).unwrap();
        let report = verify_candidate(
            &instance,
            &fixture,
            authoritative.path(),
            before.path(),
            None,
            None,
        )
        .unwrap();
        assert!(!report.passed);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("baseline"))
        );
    }

    fn make_alternative(instance: &TaskInstance, root: &Path) -> Result<(), FixtureError> {
        match &instance.parameters {
            TaskParameters::AddFile {
                path,
                content_token,
                number,
            } => write_file(
                root,
                path,
                format!("number={number}\nkind=generated\ntoken={content_token}\n").as_bytes(),
            ),
            TaskParameters::Insertion {
                path,
                anchor,
                value,
            } => write_file(
                root,
                path,
                format!(
                    "queue-start\n{anchor}\n\n{value}\nkeep-after-anchor\nqueue-end\n"
                )
                .as_bytes(),
            ),
            TaskParameters::MultilineEdit {
                path,
                symbol,
                boundary,
                increment,
            } => write_file(
                root,
                path,
                format!(
                    "def {symbol}(value):\n    if value < {boundary}:\n        value = {boundary}\n    return (value + {increment}) * 2\n"
                )
                .as_bytes(),
            ),
            TaskParameters::IndentationSensitive {
                path,
                section,
                timeout,
            } => write_file(
                root,
                path,
                format!(
                    "services:\n  {section}:\n    timeout: {timeout}\n    enabled: true\n    retries: 2\n  neighbor:\n    enabled: false\n"
                )
                .as_bytes(),
            ),
            TaskParameters::NearbyChanges {
                path,
                first,
                second,
                amount,
            } => write_file(
                root,
                path,
                format!(
                    "def {first}(value):\n    return {amount} * value\n\ndef stable_helper(value):\n    return value\n\ndef {second}(value):\n    return sum((value, {amount}))\n"
                )
                .as_bytes(),
            ),
            TaskParameters::TwoRelatedSourceFiles {
                model_path,
                view_path,
                symbol,
                default_limit,
            } => {
                write_file(
                    root,
                    model_path,
                    format!(
                        "class {symbol}:\n    def __init__(self, name, limit=None):\n        self.limit = {default_limit} if limit is None else limit\n        self.name = name\n"
                    )
                    .as_bytes(),
                )?;
                write_file(
                    root,
                    view_path,
                    b"def format_record(record):\n    return 'name={} limit={}'.format(record.name, record.limit)\n",
                )
            }
            TaskParameters::SourcePlusTest {
                source_path,
                test_path,
                symbol,
                boundary,
            } => {
                write_file(
                    root,
                    source_path,
                    format!(
                        "def {symbol}(value):\n    if not value >= {boundary}:\n        raise ValueError('outside domain')\n    return 2 * value\n"
                    )
                    .as_bytes(),
                )?;
                write_file(
                    root,
                    test_path,
                    format!(
                        "import runpy\nfunction = runpy.run_path({source_path:?})[{symbol:?}]\nassert function({boundary}) == {result}\ntry:\n    function({below})\nexcept ValueError:\n    pass\nelse:\n    raise AssertionError('missing ValueError')\n",
                        result = boundary * 2,
                        below = boundary - 1,
                    )
                    .as_bytes(),
                )
            }
            TaskParameters::RepeatedBlocks {
                path,
                target_label,
                old_limit,
                new_limit,
            } => write_file(
                root,
                path,
                format!(
                    "[alpha]\nfooter=stable\nlimit={old_limit}\nmode=legacy\n\n[{target_label}]\nlimit={new_limit}\nfooter=stable\nmode=modern\n\n[omega]\nmode=legacy\nfooter=stable\nlimit={old_limit}\n"
                )
                .as_bytes(),
            ),
            TaskParameters::RepeatedMethods {
                path,
                target_type,
                method,
                suffix,
            } => write_file(
                root,
                path,
                format!(
                    "class Peer:\n    def {method}(self, value):\n        return f\"peer:{{value}}\"\n\nclass {target_type}:\n    def {method}(self, value):\n        return 'target:' + str(value) + ':' + str({suffix})\n"
                )
                .as_bytes(),
            ),
            _ => Err(FixtureError(format!(
                "{} is unexpectedly marked multiple_valid_diffs",
                instance.archetype_id
            ))),
        }
    }

    #[test]
    fn seeds_change_content_without_changing_fixture_structure() {
        let all = instances();
        for pair in all.chunks_exact(2) {
            let first = &pair[0];
            let second = &pair[1];
            assert_eq!(first.archetype_id, second.archetype_id);
            assert_ne!(first.parameters, second.parameters);
            let (_, first_archetype) = validate_instance(first).unwrap();
            let (_, second_archetype) = validate_instance(second).unwrap();
            let first_plan = build_plan(first, &first_archetype).unwrap();
            let second_plan = build_plan(second, &second_archetype).unwrap();
            assert!(
                first_plan.baseline != second_plan.baseline
                    || first_plan.canonical != second_plan.canonical
            );
            assert_eq!(first_plan.baseline.len(), second_plan.baseline.len());
            assert_eq!(
                first_plan.allowed_paths.len(),
                second_plan.allowed_paths.len()
            );
            assert_eq!(
                normalized_paths(first, first_plan.baseline.keys()),
                normalized_paths(second, second_plan.baseline.keys())
            );
        }
    }

    fn normalized_paths<'a>(
        instance: &TaskInstance,
        paths: impl Iterator<Item = &'a String>,
    ) -> Vec<String> {
        let token = token(instance).unwrap();
        paths.map(|path| path.replace(token, "{id}")).collect()
    }

    #[test]
    fn uncommon_text_preserves_crlf_and_marker_widths() {
        let uncommon = instances()
            .into_iter()
            .filter(|instance| instance.archetype_id == "uncommon-text")
            .collect::<Vec<_>>();
        let mut saw_conflict = false;
        let mut saw_crlf = false;
        for instance in uncommon {
            let root = tempfile::tempdir().unwrap();
            materialize(&instance, root.path()).unwrap();
            let path = match &instance.parameters {
                TaskParameters::UncommonText { path, .. } => path,
                _ => unreachable!(),
            };
            let baseline = fs::read(root.path().join(path)).unwrap();
            apply_canonical_change(&instance, root.path()).unwrap();
            let canonical = fs::read(root.path().join(path)).unwrap();
            match &instance.parameters {
                TaskParameters::UncommonText {
                    lane: UncommonTextLane::Crlf,
                    ..
                } => {
                    saw_crlf = true;
                    assert!(canonical.windows(2).any(|pair| pair == b"\r\n"));
                    assert!(
                        canonical
                            .iter()
                            .enumerate()
                            .all(|(index, byte)| *byte != b'\n'
                                || canonical.get(index.wrapping_sub(1)) == Some(&b'\r'))
                    );
                }
                TaskParameters::UncommonText {
                    lane: UncommonTextLane::ConflictMarkers,
                    marker_width,
                    ..
                } => {
                    saw_conflict = true;
                    for marker in [b'<', b'=', b'>'] {
                        assert_eq!(marker_length(&baseline, marker), usize::from(*marker_width));
                        assert_eq!(
                            marker_length(&canonical, marker),
                            usize::from(*marker_width)
                        );
                    }
                }
                _ => unreachable!(),
            }
        }
        assert!(saw_conflict && saw_crlf);
    }

    fn marker_length(bytes: &[u8], marker: u8) -> usize {
        bytes
            .split(|byte| *byte == b'\n')
            .find(|line| line.first() == Some(&marker))
            .unwrap()
            .iter()
            .take_while(|byte| **byte == marker)
            .count()
    }

    #[test]
    fn generated_paths_are_confined_to_the_root() {
        for instance in instances() {
            let root = tempfile::tempdir().unwrap();
            let fixture = materialize(&instance, root.path()).unwrap();
            for path in fixture.allowed_changed_paths {
                validate_relative(&path).unwrap();
                assert!(root.path().join(path).starts_with(root.path()));
            }
        }

        let manifest = committed_manifest().unwrap();
        let universe = freeze_universe(&manifest, "escape-seed", 5).unwrap();
        let mut malicious = universe.instances[0].clone();
        match &mut malicious.parameters {
            TaskParameters::UniqueReplacement { path, .. } => *path = "../escape".into(),
            _ => unreachable!(),
        }
        let root = tempfile::tempdir().unwrap();
        assert!(materialize(&malicious, root.path()).is_err());
        assert!(!root.path().parent().unwrap().join("escape").exists());
    }
}
