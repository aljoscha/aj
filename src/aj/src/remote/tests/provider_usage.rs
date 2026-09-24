//! Usage parity at the real adapters, with credential reads and spending observed.

use super::*;
use aj_models::auth::AuthCredential;
use aj_models::oauth::{OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider};
use aj_models::usage::{
    ProviderUsage, RateLimitResetCredits, RateLimitResetSource, RateLimitResetTarget, ResetOutcome,
    UsageError, UsageReport, UsageSource,
};

pub(crate) struct FakeUsage {
    pub(crate) name: String,
    pub(crate) reads: AtomicUsize,
    pub(crate) refreshes: AtomicUsize,
    pub(crate) attempts: StdMutex<Vec<(RateLimitResetTarget, String)>>,
    spent: StdMutex<BTreeMap<String, RateLimitResetTarget>>,
}

impl FakeUsage {
    pub(crate) fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            reads: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
            attempts: StdMutex::new(Vec::new()),
            spent: StdMutex::new(BTreeMap::new()),
        })
    }

    pub(crate) async fn host(self: &Arc<Self>, dir: &TempDir) -> SessionHost {
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        auth.register_oauth_provider(Arc::<Self>::clone(self)).await;
        for account in ["personal", "work"] {
            auth.insert_account(
                "openai-codex",
                account,
                AuthCredential::OAuth(OAuthCredentials::new(
                    format!("{}-{account}-refresh-secret", self.name),
                    format!("{}-{account}-secret", self.name),
                    0,
                )),
            )
            .await
            .unwrap();
        }
        let mut setup = host_setup(
            dir,
            snapshot(scripted(vec![], 0, Duration::ZERO)),
            HostHandles::new(dir),
            Some(&self.name),
        );
        setup.auth = auth;
        SessionHost::with_usage_sources(
            setup,
            aj_app::usage::UsageSources {
                usage: vec![Arc::<Self>::clone(self)],
                resets: vec![Arc::<Self>::clone(self)],
            },
        )
        .unwrap()
    }

    fn target(&self, account: &str) -> RateLimitResetTarget {
        RateLimitResetTarget::new(
            "openai-codex",
            Some(account.into()),
            format!("{}-{account}-identity", self.name),
        )
    }

    pub(crate) fn spent(&self) -> Vec<RateLimitResetTarget> {
        self.spent.lock().unwrap().values().cloned().collect()
    }

    async fn credential(&self, auth: &AuthStorage, account: &str) {
        let key = auth
            .get_api_key("openai-codex", Some(account))
            .await
            .unwrap()
            .unwrap()
            .key;
        assert_eq!(
            key,
            format!("{}-{account}-secret-refreshed", self.name),
            "wrong host/account credential or missing refresh"
        );
    }
}

#[async_trait]
impl OAuthProvider for FakeUsage {
    fn id(&self) -> &str {
        "openai-codex"
    }
    fn name(&self) -> &str {
        &self.name
    }
    async fn login(&self, _: &dyn OAuthCallbacks) -> Result<OAuthCredentials, OAuthError> {
        panic!("usage must not start login")
    }
    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> Result<OAuthCredentials, OAuthError> {
        assert!(
            credentials.refresh.starts_with(&self.name),
            "refresh used another host's credential"
        );
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(OAuthCredentials::new(
            credentials.refresh.clone(),
            format!("{}-refreshed", credentials.access),
            i64::MAX,
        ))
    }
}

#[async_trait]
impl UsageSource for FakeUsage {
    fn provider_id(&self) -> &str {
        "openai-codex"
    }

    async fn fetch(
        &self,
        auth: &AuthStorage,
        account: Option<&str>,
    ) -> Result<UsageReport, UsageError> {
        let account = account.expect("stored accounts must be enumerated");
        self.credential(auth, account).await;
        self.reads.fetch_add(1, Ordering::SeqCst);
        let spent = self
            .spent()
            .iter()
            .filter(|target| target.account() == Some(account))
            .count();
        Ok(UsageReport::Usage(ProviderUsage {
            windows: vec![],
            details: vec![aj_models::usage::UsageDetail {
                label: "Usage credits".into(),
                value: format!("{account} credits"),
            }],
            notes: vec![format!("{} {account} report", self.name)],
            reset_credits: Some(RateLimitResetCredits::new(
                2 - u32::try_from(spent).unwrap(),
                self.target(account),
            )),
        }))
    }
}

#[async_trait]
impl RateLimitResetSource for FakeUsage {
    fn provider_id(&self) -> &str {
        "openai-codex"
    }

    async fn consume_reset_credit(
        &self,
        auth: &AuthStorage,
        target: &RateLimitResetTarget,
        key: &str,
    ) -> Result<ResetOutcome, UsageError> {
        self.attempts
            .lock()
            .unwrap()
            .push((target.clone(), key.into()));
        let account = target.account().expect("a pinned account");
        self.credential(auth, account).await;
        if *target != self.target(account) {
            return Err(UsageError::StaleResetTarget);
        }
        let mut spent = self.spent.lock().unwrap();
        if let Some(original) = spent.get(key) {
            assert_eq!(original, target, "retry changed account");
            return Ok(ResetOutcome::AlreadyRedeemed);
        }
        spent.insert(key.into(), target.clone());
        Err(UsageError::Fetch("response lost after consumption".into()))
    }
}

#[tokio::test]
async fn provider_usage_local_and_remote_adapters_preserve_facts_and_reset_failures() {
    let dir = TempDir::new().unwrap();
    let source = FakeUsage::new("adapter-host");
    let host = source.host(&dir).await;
    let session = host.create().await.unwrap();
    let server = RemoteServer::bind(host.clone(), addr("127.0.0.1:0"), IdentityGate::local())
        .await
        .unwrap();
    let local = Control::local(host.clone());
    let remote = Control::remote(RemoteClient::new(&server.url()).unwrap());
    let report = local.provider_usage(&session).await.unwrap();
    assert_eq!(report, remote.provider_usage(&session).await.unwrap());
    assert_eq!(report.reset_providers, vec!["openai-codex"]);
    let facts = serde_json::to_string(&report).unwrap();
    assert!(!facts.contains("secret"));
    assert!(
        host.hello()
            .capabilities
            .contains(&aj_wire::PROVIDER_USAGE_CAPABILITY.into())
    );
    assert!(
        host.hello()
            .capabilities
            .contains(&aj_wire::PROVIDER_USAGE_RESET_CAPABILITY.into())
    );
    let malformed = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session}/usage/reset",
            server.url()
        ))
        .json(&serde_json::json!({
            "target": {"provider_id": "openai-codex", "account": "work",
                "upstream_account_id": "identity", "unexpected": true},
            "idempotency_key": "malformed"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert!(source.attempts.lock().unwrap().is_empty());
    for (index, control) in [local, remote].iter().enumerate() {
        let reads = source.reads.load(Ordering::SeqCst);
        assert!(control.provider_usage("missing").await.is_err());
        assert_eq!(source.reads.load(Ordering::SeqCst), reads);
        let request = aj_wire::UsageResetRequest {
            target: source.target("work"),
            idempotency_key: format!("attempt-{index}"),
        };
        assert!(
            control
                .reset_provider_usage("missing", &request)
                .await
                .is_err()
        );
        let first = control
            .reset_provider_usage(&session, &request)
            .await
            .unwrap();
        assert_eq!(
            first,
            Err(aj_wire::UsageResetFailure::Error(
                "response lost after consumption".into()
            ))
        );
        assert_eq!(
            control
                .reset_provider_usage(&session, &request)
                .await
                .unwrap(),
            Ok(ResetOutcome::AlreadyRedeemed)
        );
        let stale = aj_wire::UsageResetRequest {
            target: RateLimitResetTarget::new(
                "openai-codex",
                Some("work".into()),
                "replaced-identity".into(),
            ),
            idempotency_key: "stale".into(),
        };
        assert_eq!(
            control
                .reset_provider_usage(&session, &stale)
                .await
                .unwrap(),
            Err(aj_wire::UsageResetFailure::StaleTarget)
        );
    }
    assert_eq!(source.spent().len(), 2);
    assert!(
        source
            .spent()
            .iter()
            .all(|target| target.account() == Some("work"))
    );
    let unsupported =
        Control::remote(RemoteClient::new(&format!("{}/nowhere", server.url())).unwrap());
    assert!(
        unsupported
            .provider_usage(&session)
            .await
            .unwrap_err()
            .unknown_endpoint()
    );
    assert!(
        unsupported
            .reset_provider_usage(
                &session,
                &aj_wire::UsageResetRequest {
                    target: source.target("work"),
                    idempotency_key: "unsupported".into()
                }
            )
            .await
            .unwrap_err()
            .unknown_endpoint()
    );
    host.shutdown().await;
    server.shutdown().await;
}
