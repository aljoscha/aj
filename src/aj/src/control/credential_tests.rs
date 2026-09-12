use super::*;
use crate::gateway::{Gateway, GatewayServer, GatewaySetup, Tuning};
use crate::remote::tests::{HostHandles, addr, bounded, scripted, scripted_host};
use crate::remote::{IdentityGate, RemoteServer};
use aj_app::auth::{LoginCallbacks, LoginTarget};
use aj_models::{
    auth::{AuthCredential, AuthStorage, DEFAULT_ACCOUNT_LABEL},
    oauth::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider},
};
use aj_wire::{CredentialMutation as Mutation, CredentialOutcome as Outcome};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

struct Provider;
#[async_trait]
impl OAuthProvider for Provider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake OAuth"
    }
    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> Result<OAuthCredentials, OAuthError> {
        callbacks.on_auth(OAuthAuthInfo {
            url: "https://example.test/authorize",
            manual_url: Some("https://example.test/manual"),
            instructions: Some("Authorize"),
        });
        callbacks.on_progress("Waiting for authorization");
        assert_eq!(callbacks.on_prompt("Confirm").await?, "confirmed");
        assert!(callbacks.supports_manual_code_input());
        assert_eq!(callbacks.on_manual_code_input().await?, "secret-code");
        Ok(OAuthCredentials::new(
            "host-refresh-secret",
            "host-access-secret",
            i64::MAX,
        ))
    }
    async fn refresh_token(&self, _: &OAuthCredentials) -> Result<OAuthCredentials, OAuthError> {
        panic!("overview must not refresh")
    }
}

#[derive(Default)]
struct Callbacks {
    seen: Mutex<Vec<String>>,
}
#[async_trait]
impl OAuthCallbacks for Callbacks {
    fn on_auth(&self, info: OAuthAuthInfo<'_>) {
        assert_eq!(info.manual_url, Some("https://example.test/manual"));
        self.seen.lock().unwrap().push("auth".into());
    }
    fn on_progress(&self, message: &str) {
        self.seen.lock().unwrap().push(message.into());
    }
    async fn on_prompt(&self, message: &str) -> Result<String, OAuthError> {
        assert_eq!(message, "Confirm");
        self.seen.lock().unwrap().push("prompt".into());
        Ok("confirmed".into())
    }
    fn supports_manual_code_input(&self) -> bool {
        true
    }
    async fn on_manual_code_input(&self) -> Result<String, OAuthError> {
        self.seen.lock().unwrap().push("code".into());
        Ok("secret-code".into())
    }
}
#[async_trait]
impl LoginCallbacks for Callbacks {
    async fn prompt_account_label(&self, existing: &[String]) -> Result<String, OAuthError> {
        assert_eq!(existing, &[DEFAULT_ACCOUNT_LABEL]);
        self.seen.lock().unwrap().push("label".into());
        Ok("work".into())
    }
}

// Host tasks can outlive a panicking test, so their storage stays under a
// process-lifetime root.
fn task_directory() -> &'static tempfile::TempDir {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::TempDir::with_prefix("aj-task-lifetime-").unwrap());
    Box::leak(Box::new(tempfile::TempDir::new_in(root.path()).unwrap()))
}

struct Fixture {
    _host_dir: &'static tempfile::TempDir,
    _gateway_dir: &'static tempfile::TempDir,
    auth: AuthStorage,
    host: SessionHost,
    server: RemoteServer,
    gateway: Gateway,
    gateway_server: GatewayServer,
    controls: Vec<(Control, String)>,
}
impl Fixture {
    async fn new() -> Self {
        Self::build(false).await
    }

    /// Run the fake provider's flow here and store the result through
    /// `control`, which is what the login dialog does.
    async fn login(
        &self,
        control: &Control,
        session: &str,
        target: LoginTarget,
        callbacks: &Callbacks,
    ) -> Outcome {
        let mutation = aj_app::auth::login(&Provider, target, callbacks)
            .await
            .unwrap();
        control.mutate_credentials(session, mutation).await.unwrap()
    }

    async fn build(stored: bool) -> Self {
        let host_dir = task_directory();
        let gateway_dir = task_directory();
        let handles = HostHandles::new(host_dir);
        let auth = handles.auth.clone();
        auth.register_oauth_provider(Arc::new(Provider)).await;
        let host = scripted_host(host_dir, scripted(vec![], 0, Duration::ZERO), handles, None);
        let session = if stored {
            use aj_session::{
                ConversationEntryKind, ConversationLog, ConversationPersistence, ThreadKind,
            };
            let persistence = ConversationPersistence::new(host_dir.path().join("sessions"));
            let mut log = ConversationLog::create(&persistence).unwrap();
            log.append(
                None,
                ThreadKind::User,
                None,
                ConversationEntryKind::Message {
                    message: aj_agent::message::AgentMessage::wire(
                        aj_models::types::Message::User(aj_models::types::UserMessage::text(
                            "stored",
                        )),
                    ),
                },
            )
            .unwrap();
            log.session_id().to_string()
        } else {
            host.create().await.unwrap()
        };
        let server = RemoteServer::bind_with(
            host.clone(),
            addr("127.0.0.1:0"),
            IdentityGate::local(),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let gateway = Gateway::new(GatewaySetup {
            state_dir: gateway_dir.path().to_path_buf(),
            static_hosts: vec![server.url().try_into().unwrap()],
            tuning: Tuning {
                upstream_timeout: Duration::from_millis(500),
                ..Tuning::default()
            },
        })
        .unwrap();
        let gateway_server =
            GatewayServer::bind(gateway.clone(), addr("127.0.0.1:0"), IdentityGate::local())
                .await
                .unwrap();
        let remote = RemoteClient::new(&server.url())
            .unwrap()
            .with_silence(Duration::from_millis(500))
            .with_open_timeout(Duration::from_millis(500));
        let proxied = RemoteClient::new(&gateway_server.url())
            .unwrap()
            .with_silence(Duration::from_millis(500))
            .with_open_timeout(Duration::from_millis(500));
        let routed = bounded("gateway session discovery", async {
            loop {
                if let Some(row) = proxied
                    .sessions()
                    .await
                    .unwrap()
                    .sessions
                    .into_iter()
                    .next()
                {
                    break row.id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert_ne!(session, routed);
        assert!(
            remote
                .hello()
                .await
                .unwrap()
                .capabilities
                .iter()
                .any(|s| s == aj_wire::CREDENTIALS_CAPABILITY)
        );
        let controls = vec![
            (Control::local(host.clone()), session.clone()),
            (Control::remote(remote), session),
            (Control::remote(proxied), routed),
        ];
        Self {
            _host_dir: host_dir,
            _gateway_dir: gateway_dir,
            auth,
            host,
            server,
            gateway,
            gateway_server,
            controls,
        }
    }
    async fn close(self) {
        self.gateway_server.shutdown().await;
        self.gateway.shutdown().await;
        self.host.shutdown().await;
        self.server.shutdown().await;
    }
}

#[tokio::test]
async fn credentials_real_adapters_overview_mutations_and_stored_login() {
    let fixture = Fixture::new().await;
    let client_dir = tempfile::tempdir().unwrap();
    let client_auth = AuthStorage::new(client_dir.path().join("auth.json"));
    client_auth
        .insert_bare(
            "fake",
            AuthCredential::ApiKey {
                key: "client-only-secret".into(),
            },
        )
        .await
        .unwrap();
    let client_before = std::fs::read(client_auth.path()).unwrap();
    for (control, session) in &fixture.controls {
        fixture
            .auth
            .insert_bare(
                "fake",
                AuthCredential::ApiKey {
                    key: "host-api-secret".into(),
                },
            )
            .await
            .unwrap();
        fixture
            .auth
            .set_runtime_api_key("fake", "runtime-secret".into())
            .await;
        let overridden = control.credential_overview(session).await.unwrap();
        assert_eq!(
            overridden.stored.get("fake"),
            Some(&aj_wire::StoredCredentialMetadata::Bare)
        );
        assert!(
            overridden
                .statuses
                .iter()
                .any(|row| row.provider_id == "fake" && row.summary.contains("override"))
        );
        assert!(
            !serde_json::to_string(&overridden)
                .unwrap()
                .contains("runtime-secret")
        );
        fixture.auth.remove_runtime_api_key("fake").await;
        let callbacks = Callbacks::default();
        let target = LoginTarget::new_account(overridden.stored.get("fake"));
        assert_eq!(
            fixture.login(control, session, target, &callbacks).await,
            Outcome::Applied
        );
        assert_eq!(
            *callbacks.seen.lock().unwrap(),
            [
                "label",
                "auth",
                "Waiting for authorization",
                "prompt",
                "code"
            ]
        );
        let replacement = Callbacks::default();
        assert_eq!(
            fixture
                .login(
                    control,
                    session,
                    LoginTarget::ExistingAccount(Some("work".into())),
                    &replacement
                )
                .await,
            Outcome::Applied
        );
        assert!(
            !replacement
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "label")
        );
        let overview = control.credential_overview(session).await.unwrap();
        assert!(
            overview
                .oauth_providers
                .iter()
                .any(|p| p.id == "fake" && p.name == "Fake OAuth")
        );
        let rows: Vec<_> = overview
            .statuses
            .iter()
            .filter(|r| r.provider_id == "fake")
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|r| r.account_label.as_deref() == Some(DEFAULT_ACCOUNT_LABEL) && r.is_default)
        );
        assert!(
            rows.iter()
                .any(|r| r.account_label.as_deref() == Some("work")
                    && !r.is_default
                    && r.summary.contains("subscription"))
        );
        let wire = serde_json::to_string(&overview).unwrap();
        for secret in [
            "host-api-secret",
            "host-refresh-secret",
            "host-access-secret",
            "client-only-secret",
            "secret-code",
        ] {
            assert!(!wire.contains(secret));
        }
        assert!(matches!(
            fixture.auth.get_account("fake", "work").await.unwrap(),
            Some(AuthCredential::OAuth(_))
        ));
        assert_eq!(
            control
                .mutate_credentials(
                    session,
                    Mutation::Logout {
                        provider: "fake".into(),
                        account_label: DEFAULT_ACCOUNT_LABEL.into()
                    }
                )
                .await
                .unwrap(),
            Outcome::RemovingDefault {
                provider: "fake".into(),
                account_label: DEFAULT_ACCOUNT_LABEL.into()
            }
        );
        assert!(
            matches!(control.mutate_credentials(session, Mutation::LogoutAll { provider: "fake".into(), expected_accounts: vec!["work".into()] }).await.unwrap(), Outcome::Failed { code, .. } if code == "credentials_changed")
        );
        assert_eq!(
            control
                .mutate_credentials(
                    session,
                    Mutation::SetDefault {
                        provider: "fake".into(),
                        account_label: "work".into()
                    }
                )
                .await
                .unwrap(),
            Outcome::Applied
        );
        assert_eq!(
            control
                .mutate_credentials(
                    session,
                    Mutation::LogoutWithNewDefault {
                        provider: "fake".into(),
                        account_label: "work".into(),
                        new_default: DEFAULT_ACCOUNT_LABEL.into()
                    }
                )
                .await
                .unwrap(),
            Outcome::Applied
        );
        assert_eq!(
            control
                .mutate_credentials(
                    session,
                    Mutation::LogoutAll {
                        provider: "fake".into(),
                        expected_accounts: vec![DEFAULT_ACCOUNT_LABEL.into()]
                    }
                )
                .await
                .unwrap(),
            Outcome::Applied
        );
        let callbacks = Callbacks::default();
        assert_eq!(
            fixture
                .login(control, session, LoginTarget::new_account(None), &callbacks)
                .await,
            Outcome::Applied
        );
        assert!(!callbacks.seen.lock().unwrap().iter().any(|s| s == "label"));
        assert_eq!(
            fixture
                .login(
                    control,
                    session,
                    LoginTarget::ExistingAccount(None),
                    &callbacks
                )
                .await,
            Outcome::Applied
        );
        assert_eq!(
            control
                .mutate_credentials(
                    session,
                    Mutation::LogoutBare {
                        provider: "fake".into()
                    }
                )
                .await
                .unwrap(),
            Outcome::Applied
        );
    }
    assert_eq!(std::fs::read(client_auth.path()).unwrap(), client_before);
    fixture.close().await;
}

#[tokio::test]
async fn credentials_http_and_gateway_refuse_unknown_fields_and_unknown_sessions_before_writes() {
    let fixture = Fixture::new().await;
    fixture
        .auth
        .insert_bare("fake", AuthCredential::ApiKey { key: "keep".into() })
        .await
        .unwrap();
    let before = std::fs::read(fixture.auth.path()).unwrap();
    let http = reqwest::Client::new();
    for (control, session) in &fixture.controls {
        assert!(
            control
                .mutate_credentials(
                    "absent",
                    Mutation::LogoutBare {
                        provider: "fake".into()
                    }
                )
                .await
                .is_err()
        );
        let Some(base) = control.base_url() else {
            continue;
        };
        for body in [
            serde_json::json!({"action":"logout_bare", "provider":"fake", "unknown":true}),
            serde_json::json!({
                "action":"store", "provider":"fake",
                "target":{"kind":"new", "label":null, "unknown":true},
                "credentials":{"refresh":"r", "access":"a", "expires":1}
            }),
        ] {
            let response = http
                .post(format!("{base}/v1/sessions/{session}/credentials"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        }
    }
    assert_eq!(std::fs::read(fixture.auth.path()).unwrap(), before);
    fixture.close().await;
}

#[tokio::test]
async fn credentials_cold_and_locked_anchors_never_resume_or_repair_logs() {
    use aj_session::{ConversationPersistence, SessionLock};
    use std::io::Write;
    let fixture = Fixture::build(true).await;
    let session = &fixture.controls[0].1;
    let persistence = ConversationPersistence::new(fixture._host_dir.path().join("sessions"));
    let path = persistence.sessions_dir().join(format!("{session}.jsonl"));
    // A recoverable partial tail makes an accidental resume/repair observable.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    for locked in [false, true] {
        let guard = locked.then(|| {
            SessionLock::try_acquire(&persistence, session, "external")
                .unwrap()
                .unwrap()
        });
        for (control, routed) in &fixture.controls {
            let live = || async {
                fixture
                    .host
                    .sessions()
                    .await
                    .unwrap()
                    .sessions
                    .iter()
                    .find(|r| &r.id == session)
                    .unwrap()
                    .live
            };
            assert!(!live().await);
            control.credential_overview(routed).await.unwrap();
            assert_eq!(
                fixture
                    .login(
                        control,
                        routed,
                        LoginTarget::new_account(None),
                        &Callbacks::default()
                    )
                    .await,
                Outcome::Applied
            );
            assert_eq!(
                control
                    .mutate_credentials(
                        routed,
                        Mutation::LogoutBare {
                            provider: "fake".into()
                        }
                    )
                    .await
                    .unwrap(),
                Outcome::Applied
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(SessionLock::is_held(&persistence, session).unwrap(), locked);
            assert!(!live().await);
        }
        drop(guard);
    }
    fixture.close().await;
}
