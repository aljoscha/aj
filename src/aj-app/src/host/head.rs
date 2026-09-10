//! Detached branch preparation and explicit branch records.
use super::*;
use aj_session::ThreadFilter;

/// Build against private account choices. Cloning the runtime alone would
/// leave its lazy credential resolvers attached to the live branch.
pub(super) async fn prepare(
    mut run: RunConfigSnapshot,
    config: &Config,
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
        crate::session_setup::restore_session_settings(config, &detached, historical, restore);
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

pub(super) fn append_changes(
    log: &mut aj_session::ConversationLog,
    changes: &aj_wire::BranchChanges,
    run: &RunConfigSnapshot,
    env: &BTreeMap<String, String>,
) -> Result<(), HostError> {
    let settings = run.settings();
    let persist = |err| HostError::Internal(Box::new(err));
    if changes.settings.model.is_some() {
        log.append_model_change(ThreadFilter::USER, &settings.provider, &settings.model_id)
            .map_err(persist)?;
    }
    if changes.settings.thinking.is_some() {
        log.append_thinking_change(ThreadFilter::USER, &settings.thinking)
            .map_err(persist)?;
    }
    if changes.settings.speed.is_some() {
        log.append_speed_change(ThreadFilter::USER, &settings.speed)
            .map_err(persist)?;
    }
    if changes.settings.verbosity.is_some() {
        log.append_verbosity_change(ThreadFilter::USER, &settings.verbosity)
            .map_err(persist)?;
    }
    for (provider, account) in &changes.accounts {
        log.append_account_change(provider, account.as_deref())
            .map_err(persist)?;
    }
    if !changes.env.is_empty() {
        log.append_env_change(env.clone()).map_err(persist)?;
    }
    Ok(())
}
