use std::sync::Arc;

use hyphae::{Gettable, Materialize};
use marshal_entities::{ImportSandboxRun, SandboxArtifact, SandboxRun};
use myko::command::{CommandContext, CommandHandler};
use myko::core::item::Eventable;
use myko::request::RequestContext;
use myko::server::{MykoServerContext, Persister};
use myko_server::{BlackholePersister, MykoServer};

fn setup() -> (MykoServer, MykoServerContext) {
    marshal_entities::link();
    let persister: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = MykoServer::builder()
        .with_default_persister(persister)
        .build();
    let context = server.ctx();
    (server, context)
}

fn import(
    context: &MykoServerContext,
    artifact: SandboxArtifact,
) -> Result<marshal_entities::SandboxRunId, myko::command::CommandError> {
    let request = RequestContext::new(
        Arc::<str>::from("sandbox-import-test"),
        None,
        vec![Arc::<str>::from("test")],
        uuid::Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    let command = CommandContext::new(
        Arc::from("ImportSandboxRun"),
        Arc::new(request),
        Arc::new(context.clone()),
    );
    ImportSandboxRun { artifact }.execute(command)
}

fn fixture() -> SandboxArtifact {
    serde_json::from_str(include_str!("fixtures/sandbox-run.json")).unwrap()
}

#[test]
fn imports_and_updates_one_shared_run_without_creating_sessions_or_messages() {
    let (_server, context) = setup();
    let artifact = fixture();
    let id = import(&context, artifact.clone()).unwrap();
    let mut updated = artifact;
    updated.label = "reviewed candidate".into();
    assert_eq!(import(&context, updated).unwrap(), id);
    let store = context
        .registry
        .get(SandboxRun::ENTITY_NAME_STATIC)
        .unwrap();
    let entries = store.entries().materialize().get();
    assert_eq!(entries.len(), 1);
    let imported = entries
        .into_iter()
        .filter_map(|(_, item)| myko::utils::downcast_item::<SandboxRun>(&item))
        .next()
        .unwrap();
    assert_eq!(imported.artifact.label, "reviewed candidate");
    assert!(
        context.registry.get("Session").is_none_or(|store| store
            .entries()
            .materialize()
            .get()
            .is_empty())
    );
    assert!(
        context.registry.get("Message").is_none_or(|store| store
            .entries()
            .materialize()
            .get()
            .is_empty())
    );
}

#[test]
fn rejects_unsupported_or_inconsistent_artifacts_before_publishing() {
    let (_server, context) = setup();
    let mut unsupported = fixture();
    unsupported.schema_version = 2;
    assert!(import(&context, unsupported).is_err());
    let mut counters = fixture();
    counters.cached_input_tokens = counters.input_tokens + 1;
    assert!(import(&context, counters).is_err());
    let mut oversized = fixture();
    oversized.instructions = "x".repeat(marshal_entities::MAX_SANDBOX_ARTIFACT_BYTES);
    assert!(import(&context, oversized).is_err());
    let mut unordered = fixture();
    unordered.timeline[0].seconds = -1.0;
    assert!(import(&context, unordered).is_err());
    assert!(
        context
            .registry
            .get(SandboxRun::ENTITY_NAME_STATIC)
            .is_none_or(|store| store.entries().materialize().get().is_empty())
    );
}

#[test]
fn quiet_but_incomplete_is_not_success() {
    let mut artifact = fixture();
    assert!(artifact.succeeded());
    artifact.tested = false;
    assert!(!artifact.succeeded());
}
