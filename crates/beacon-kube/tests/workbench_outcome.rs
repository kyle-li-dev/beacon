//! Public API regression for a write that commits before its response is damaged.
#![cfg(feature = "test-support")]
use beacon_kube::{
    ApiResource, ClusterId, ClusterSession, GroupVersionKind, Kind,
    test_support::Fixture,
    workbench::{self, Source, State},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn kind() -> Kind {
    Kind {
        resource: ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "ConfigMap"),
            "configmaps",
        ),
        namespaced: true,
        verbs: ["get", "create", "patch"].map(str::to_owned).to_vec(),
    }
}
fn existing(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"team","uid":format!("uid-{name}"),"resourceVersion":"1"},"data":{"value":"old"}})
}
fn writes(fixture: &Fixture) -> Vec<String> {
    fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(request, _)| {
            (request.starts_with("POST") || request.starts_with("PATCH"))
                && !request.contains("reviews")
                && !request.contains("dryRun=All")
        })
        .map(|(request, _)| request.clone())
        .collect()
}
async fn damaged_response(patch: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("resources.yaml");
    let yaml = ["committed", "refused", "succeeded"].map(|name| {
        format!("apiVersion: v1\nkind: ConfigMap\nmetadata: {{name: {name}}}\ndata: {{value: reviewed}}\n")
    }).join("---\n");
    std::fs::write(&path, yaml).unwrap();
    let fixture = Fixture::start(if patch {
        vec![existing("committed"), existing("refused")]
    } else {
        vec![]
    })
    .await;
    let session = Arc::new(ClusterSession::for_testing(
        ClusterId::new("pinned-outcome-context"),
        fixture.url.clone(),
        vec![kind()],
    ));
    let mut bundle = workbench::prepare(session.clone(), Source::Yaml(path.clone()), "team".into())
        .await
        .unwrap();
    bundle.dry_run_all().await;
    assert!(
        bundle
            .items()
            .iter()
            .all(|item| item.state() == &State::Validated)
    );
    assert!(writes(&fixture).is_empty());
    let method = if patch { "PATCH" } else { "POST" };
    fixture.damage_write_response(method, "configmaps", "committed");
    fixture.deny(&format!("{}/configmaps/refused", method.to_lowercase()));
    std::fs::write(&path, "invalid: [changed after prepare\n").unwrap();
    let report = bundle.confirm().unwrap().execute().await;
    // Confirm persistence independently before asserting how the result is shown.
    let saved = session
        .clone()
        .get_object(kind().resource, Some("team".into()), "committed".into())
        .await
        .unwrap();
    assert_eq!(saved.data["data"]["value"], "reviewed");
    assert!(saved.metadata.uid.is_some());
    assert_eq!(saved.metadata.namespace.as_deref(), Some("team"));
    assert_eq!(
        saved.metadata.resource_version.as_deref(),
        Some(if patch { "2" } else { "1" })
    );
    assert_eq!(report.context(), "pinned-outcome-context");
    assert_eq!(report.namespace(), "team");
    assert_eq!(report.items()[0].identity(), "ConfigMap team/committed");
    assert_eq!(report.items()[0].desired()["data"]["value"], "reviewed");
    assert!(matches!(report.items()[1].state(), State::Failed(message) if message.contains("403")));
    assert!(matches!(report.items()[2].state(), State::Succeeded));
    assert_eq!(
        fixture.object("ConfigMap", "succeeded")["data"]["value"],
        "reviewed"
    );
    assert_eq!(
        writes(&fixture).len(),
        3,
        "one attempt per item, no automatic replay"
    );
    let diagnostic = format!("{:?}", report.items()[0].state());
    // This also compiles against the reviewed public API, yielding a runtime RED
    // when its ordinary Failed state incorrectly describes the persisted write.
    assert!(
        diagnostic.starts_with("Unknown("),
        "persisted {method} must retain an unknown outcome: {diagnostic}"
    );
    for expected in [
        "pinned-outcome-context",
        &fixture.url,
        "ConfigMap team/committed",
        "deserializing response",
        "Inspect",
        "reconcile",
        "before",
        "retry",
    ] {
        assert!(
            diagnostic.contains(expected),
            "missing {expected:?}: {diagnostic}"
        );
    }
}
#[tokio::test]
async fn damaged_accepted_create_response_preserves_unknown_outcome() {
    damaged_response(false).await;
}
#[tokio::test]
async fn damaged_accepted_patch_response_preserves_unknown_outcome() {
    damaged_response(true).await;
}
