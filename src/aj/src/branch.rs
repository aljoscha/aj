//! Client-owned branch choices, based on the selected message's recorded settings.
//! The host resolves and validates their runtime effect when the branch is submitted.

use aj_agent::events::AgentId;
use aj_app::host::{SettingsAxis, SettingsChange};
use aj_app::settings::PersistAction;
use aj_wire::{BranchChanges, BranchSettings};

#[derive(Clone, Debug)]
pub(crate) struct BranchDraft {
    pub(crate) message: String,
    pub(crate) inherited: Option<BranchSettings>,
    pub(crate) changes: BranchChanges,
}

impl BranchDraft {
    pub(crate) fn new(message: String, inherited: Option<BranchSettings>) -> Self {
        Self {
            message,
            inherited,
            changes: BranchChanges::default(),
        }
    }

    /// Record intent even when the choice equals the inherited value. Absence
    /// means inherit, not that the user happened to pick the same value.
    pub(crate) fn set(&mut self, axis: SettingsAxis) {
        let change = crate::control::settings_request(SettingsChange {
            agent: AgentId::Main,
            persist: PersistAction::None,
            axis,
        })
        .change;
        let settings = &mut self.changes.settings;
        if change.oracle_model.is_some() {
            settings.oracle_model = change.oracle_model;
        }
        if change.oracle_thinking.is_some() {
            settings.oracle_thinking = change.oracle_thinking;
        }
        if change.oracle_speed.is_some() {
            settings.oracle_speed = change.oracle_speed;
        }
        if change.oracle_verbosity.is_some() {
            settings.oracle_verbosity = change.oracle_verbosity;
        }
        if change.model.is_some() {
            settings.model = change.model;
        }
        if change.thinking.is_some() {
            settings.thinking = change.thinking;
        }
        if change.speed.is_some() {
            settings.speed = change.speed;
        }
        if change.verbosity.is_some() {
            settings.verbosity = change.verbosity;
        }
    }

    /// Recorded choices plus explicit edits. Unrecorded axes stay unknown rather
    /// than borrowing values from the unrelated live branch.
    pub(crate) fn settings(&self) -> BranchSettings {
        let mut state = self.inherited.clone().unwrap_or_default();
        let changes = &self.changes.settings;
        if let Some(model) = &changes.oracle_model {
            state.oracle_model = Some(aj_wire::RecordedModel {
                api: model.api.clone(),
                name: model.name.clone(),
            });
        }
        if changes.oracle_thinking.is_some() {
            state.oracle_thinking.clone_from(&changes.oracle_thinking);
        }
        if changes.oracle_speed.is_some() {
            state.oracle_speed.clone_from(&changes.oracle_speed);
        }
        if changes.oracle_verbosity.is_some() {
            state.oracle_verbosity.clone_from(&changes.oracle_verbosity);
        }
        if let Some(model) = &changes.model {
            state.model = Some(aj_wire::RecordedModel {
                api: model.api.clone(),
                name: model.name.clone(),
            });
        }
        if changes.thinking.is_some() {
            state.thinking.clone_from(&changes.thinking);
        }
        if changes.speed.is_some() {
            state.speed.clone_from(&changes.speed);
        }
        if changes.verbosity.is_some() {
            state.verbosity.clone_from(&changes.verbosity);
        }
        for (provider, account) in &self.changes.accounts {
            match account {
                Some(account) => {
                    state.accounts.insert(provider.clone(), account.clone());
                }
                None => {
                    state.accounts.remove(provider);
                }
            }
        }
        state
    }
}
