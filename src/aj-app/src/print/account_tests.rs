//! Print account choices observed through inference and resumed session settings.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_conf::Config;
use aj_models::auth::{AuthCredential, AuthStorage};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::scripted::script_from_message;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantError, AssistantMessage, Context, ErrorCategory, Message, SimpleStreamOptions,
    StopReason, StreamOptions,
};
use aj_session::{ConversationLog, ConversationPersistence, ThreadFilter};
use tempfile::TempDir;

use crate::cli::args::Args;
use crate::test_support::finalized_text_message;

const PROVIDER: &str = "anthropic";
const MODEL: &str = "claude-opus-5";

#[derive(Default)]
struct CredentialProvider {
    calls: AtomicUsize,
}

impl Provider for CredentialProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        _context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let options = options.clone();
        let model = model.clone();
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        tokio::spawn(async move {
            // Only the fixture's synthetic credentials reach this provider.
            // Echo the actual resolver result rather than the requested choice.
            let mut message = match options.resolve_api_key().await {
                Ok(resolved) => {
                    let mut message = finalized_text_message(&resolved.key);
                    message.account = resolved.account;
                    message
                }
                Err(error) => {
                    let mut message = AssistantMessage::empty();
                    message.stop_reason = StopReason::Error;
                    message.error = Some(AssistantError::new(ErrorCategory::Auth, error));
                    message
                }
            };
            message.api = model.api;
            message.provider = model.provider;
            message.model = model.id;
            for step in script_from_message(message, 0, Duration::ZERO).steps {
                producer.push(step.event);
            }
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, &options.base)
    }
}

struct Store {
    dir: TempDir,
    persistence: ConversationPersistence,
    provider: Arc<CredentialProvider>,
}

impl Store {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Self {
            persistence: ConversationPersistence::new(dir.path().join("sessions")),
            dir,
            provider: Arc::new(CredentialProvider::default()),
        };
        let auth = store.auth();
        for (label, token) in [
            ("personal", "synthetic-personal-token"),
            ("work", "synthetic-work-token"),
            ("default", "synthetic-literally-default-token"),
            ("", "synthetic-unnamed-token"),
        ] {
            auth.insert_account(
                PROVIDER,
                label,
                AuthCredential::ApiKey { key: token.into() },
            )
            .await
            .unwrap();
        }
        auth.set_default_account(PROVIDER, "personal")
            .await
            .unwrap();
        store
    }

    fn auth(&self) -> AuthStorage {
        AuthStorage::with_providers(self.dir.path().join("auth.json"), HashMap::new())
    }

    async fn run(&self, cli: &[&str]) -> anyhow::Result<(String, AssistantMessage)> {
        let mut args = Args::try_parse_from(
            ["aj", "--print", "--thinking", "off", "--speed", "standard"]
                .into_iter()
                .chain(cli.iter().copied()),
        )?;
        // Exercise Config model selection without inheriting clap's MODEL_*
        // bindings or mutating the process environment shared by other tests.
        args.model_api = None;
        args.model_name = None;
        args.model_url = None;
        assert!(args.scripted.is_none());
        assert!(args.api_key.is_none());
        let config = Config {
            model_api: Some(PROVIDER.into()),
            model_name: Some(MODEL.into()),
            spill_dir: Some(self.dir.path().join("spill").to_string_lossy().into_owned()),
            ..Config::default()
        };
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        // The same catalog model and speed on every invocation keep resume
        // restoration real without replacing the injected, network-free provider.
        let agent = tokio::time::timeout(
            Duration::from_secs(20),
            super::run_inner(
                args,
                config,
                self.auth(),
                self.persistence.clone(),
                self.dir.path().to_path_buf(),
                Arc::clone(&sink),
                Some(Arc::<CredentialProvider>::clone(&self.provider)),
            ),
        )
        .await
        .expect("print account run timed out")?;
        let message = agent
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message.as_stored_wire() {
                Some(Message::Assistant(message)) => Some(message.clone()),
                _ => None,
            })
            .expect("print produced an assistant reply");
        let output = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        Ok((output, message))
    }

    fn log(&self) -> ConversationLog {
        let id = self.persistence.get_latest_session_id().unwrap().unwrap();
        ConversationLog::resume(&self.persistence, &id).unwrap()
    }

    fn assert_pin(&self, pin: Option<&str>) {
        let log = self.log();
        let transcript = log.linearize(log.head().unwrap(), ThreadFilter::USER);
        let expected = pin
            .map(|name| (PROVIDER.to_string(), name.to_string()))
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(transcript.settings().accounts, expected);
        assert_eq!(
            transcript.settings().model,
            Some((PROVIDER.into(), MODEL.into()))
        );
    }

    async fn assert_run(&self, cli: &[&str], pin: Option<&str>, label: &str, token: &str) {
        let before = self.provider.calls.load(Ordering::SeqCst);
        let (output, message) = self.run(cli).await.unwrap();
        assert_eq!(self.provider.calls.load(Ordering::SeqCst), before + 1);
        assert_eq!(output, format!("{token}\n"));
        assert_eq!(message.account.as_deref(), Some(label));
        assert_eq!(message.provider, PROVIDER);
        assert_eq!(message.model, MODEL);
        assert!(message.error.is_none(), "{message:?}");
        self.assert_pin(pin);
    }
}

#[tokio::test]
async fn explicit_choices_override_resume_while_omission_restores_the_saved_pin() {
    let store = Store::new().await;
    store
        .assert_run(
            &["--account", "default", "create"],
            Some("default"),
            "default",
            "synthetic-literally-default-token",
        )
        .await;
    let id = store.log().session_id().to_string();

    for (flags, pin, label, token) in [
        (
            vec!["--account", "work"],
            Some("work"),
            "work",
            "synthetic-work-token",
        ),
        (vec![], Some("work"), "work", "synthetic-work-token"),
        (
            vec!["--account", ""],
            Some(""),
            "",
            "synthetic-unnamed-token",
        ),
        (vec![], Some(""), "", "synthetic-unnamed-token"),
        (
            vec!["--default-account"],
            None,
            "personal",
            "synthetic-personal-token",
        ),
    ] {
        let mut cli = vec!["continue", &id, "resume"];
        cli.extend(flags);
        store.assert_run(&cli, pin, label, token).await;
        assert_eq!(store.persistence.list_sessions().unwrap().len(), 1);
    }

    store
        .auth()
        .set_default_account(PROVIDER, "work")
        .await
        .unwrap();
    store
        .assert_run(
            &["continue", &id, "follow the changed default"],
            None,
            "work",
            "synthetic-work-token",
        )
        .await;
}

#[tokio::test]
async fn default_account_creation_stays_dynamic_and_missing_names_refuse_inference() {
    let store = Store::new().await;
    store
        .assert_run(
            &["--default-account", "create"],
            None,
            "personal",
            "synthetic-personal-token",
        )
        .await;
    let id = store.log().session_id().to_string();
    store
        .auth()
        .set_default_account(PROVIDER, "work")
        .await
        .unwrap();
    store
        .assert_run(
            &["continue", &id, "resume"],
            None,
            "work",
            "synthetic-work-token",
        )
        .await;

    let before = store.log().entries_in_order().len();
    let calls = store.provider.calls.load(Ordering::SeqCst);
    let error = store
        .run(&["continue", &id, "refuse", "--account", "missing"])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("missing"), "{error:#}");
    assert!(error.to_string().contains(PROVIDER), "{error:#}");
    assert_eq!(store.provider.calls.load(Ordering::SeqCst), calls);
    assert_eq!(store.log().entries_in_order().len(), before);
    store.assert_pin(None);

    let fresh = Store::new().await;
    let error = fresh
        .run(&["--account", "missing", "create"])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("missing"), "{error:#}");
    assert!(error.to_string().contains(PROVIDER), "{error:#}");
    assert_eq!(fresh.provider.calls.load(Ordering::SeqCst), 0);
}
