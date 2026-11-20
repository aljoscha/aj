//! Detached branch preparation and explicit branch entries.
use super::*;
use aj_session::ThreadFilter;

/// Build against private account choices. Cloning the runtime alone would
/// leave its lazy credential resolvers attached to the live branch.
pub(super) async fn prepare(
    mut run: RunConfigSnapshot,
    historical: &aj_session::SessionSettings,
    shared: &HostShared,
    mut env: BTreeMap<String, String>,
    changes: &aj_wire::BranchChanges,
) -> Result<(RunConfigSnapshot, BTreeMap<String, String>), HostError> {
    let auth = &shared.auth;
    run.accounts = crate::model::SessionAccounts::default();
    run.accounts.replace(historical.accounts.clone());
    run.bind_accounts(auth);
    if let Some(restore) = &shared.restore {
        let detached = Arc::new(StdMutex::new(run));
        crate::session_setup::restore_session_settings(&detached, historical, restore);
        run = detached.lock().expect("run config mutex poisoned").clone();
    }
    if changes.settings.account.is_some() {
        return Err(HostError::Invalid(
            "branch settings.account is not supported. Use accounts instead.".into(),
        ));
    }
    if changes.settings.thinking_display.is_some() {
        return Err(HostError::Invalid(
            "thinking_display is a live-only setting".into(),
        ));
    }
    if changes.settings != SessionSettings::default() {
        run = apply_settings(run, Some(&changes.settings), &shared.catalog, auth, true)?;
    }
    for (provider, account) in &changes.accounts {
        crate::model::validate_account_selection(auth, provider, account.as_deref())
            .await
            .map_err(HostError::Invalid)?;
        run.accounts.set(provider, account.clone());
    }
    for (key, value) in &changes.env {
        aj_session::validate_session_env(&BTreeMap::from([(
            key.clone(),
            value.clone().unwrap_or_default(),
        )]))
        .map_err(|err| HostError::Invalid(err.to_string()))?;
        match value {
            Some(value) => {
                env.insert(key.clone(), value.clone());
            }
            None => {
                env.remove(key);
            }
        }
    }
    aj_session::validate_session_env(&env).map_err(|err| HostError::Invalid(err.to_string()))?;
    run.bind_accounts(auth);
    Ok((run, env))
}

/// Append the entries of a branch's explicit `changes`. Call inside a
/// journal write.
pub(super) fn append_changes(
    log: &mut aj_session::SessionLog,
    changes: &aj_wire::BranchChanges,
    run: &RunConfigSnapshot,
    env: &BTreeMap<String, String>,
) -> Result<(), aj_session::SessionError> {
    let settings = run.settings();
    if let Some(model) = &changes.settings.oracle_model {
        log.append_oracle_model_change(&model.api, &model.name)?;
    }
    if let Some(effort) = &changes.settings.oracle_thinking {
        log.append_oracle_thinking_change(effort)?;
    }
    if let Some(verbosity) = &changes.settings.oracle_verbosity {
        log.append_oracle_verbosity_change(verbosity)?;
    }
    if changes.settings.model.is_some() {
        log.append_model_change(
            ThreadFilter::MAIN,
            &settings.provider,
            &settings.model_id,
            settings.context_window,
        )?;
    }
    if changes.settings.thinking.is_some() {
        log.append_thinking_change(ThreadFilter::MAIN, &settings.thinking)?;
    }
    if changes.settings.speed.is_some() {
        log.append_speed_change(ThreadFilter::MAIN, &settings.speed)?;
    }
    if changes.settings.verbosity.is_some() {
        log.append_verbosity_change(ThreadFilter::MAIN, &settings.verbosity)?;
    }
    for (provider, account) in &changes.accounts {
        log.append_account_change(provider, account.as_deref())?;
    }
    if !changes.env.is_empty() {
        log.append_env_change(env.clone())?;
    }
    Ok(())
}
