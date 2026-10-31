//! Business acceptance for the local workbench. All API traffic is loopback.
#![cfg(feature = "test-support")]
use beacon_kube::{
    ApiResource, ClusterId, ClusterSession, GroupVersionKind, Kind,
    test_support::Fixture,
    workbench::{self, Source, State},
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

fn kinds() -> Vec<Kind> {
    [
        ("ConfigMap", "configmaps", true),
        ("Namespace", "namespaces", false),
    ]
    .into_iter()
    .map(|(kind, plural, namespaced)| Kind {
        resource: ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk("", "v1", kind), plural),
        namespaced,
        verbs: ["get", "create", "patch"].map(str::to_owned).to_vec(),
    })
    .collect()
}
fn object(name: &str, value: &str) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name},"data":{"value":value}})
}
fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_saphyr::to_string(value).unwrap()).unwrap();
}
fn writes(fixture: &Fixture) -> Vec<(String, Value)> {
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
        .cloned()
        .collect()
}
fn session(f: &Fixture) -> Arc<ClusterSession> {
    Arc::new(ClusterSession::for_testing(
        ClusterId::new("pinned-context"),
        f.url.clone(),
        kinds(),
    ))
}
#[tokio::test]
async fn workbench_directory_multidoc_scope_diff_dryrun_confirm_partial_and_frozen_sources() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.yaml"), &object("existing", "reviewed"));
    std::fs::write(
        dir.path().join("b.yml"),
        format!(
            "{}\n---\napiVersion: v1\nkind: Namespace\nmetadata:\n  name: created-ns\n",
            serde_saphyr::to_string(&object("new", "selected")).unwrap()
        ),
    )
    .unwrap();
    let mut existing = object("existing", "old");
    existing["metadata"] =
        json!({"name":"existing","namespace":"team","uid":"old-uid","resourceVersion":"1"});
    let f = Fixture::start(vec![existing]).await;
    let bundle = workbench::prepare(
        session(&f),
        Source::Yaml(dir.path().to_owned()),
        "team".into(),
    )
    .await
    .unwrap();
    assert_eq!(bundle.items().len(), 3);
    assert!(bundle.items()[0].before().is_some());
    assert!(bundle.items()[1].before().is_none());
    assert_eq!(bundle.items()[0].desired()["data"]["value"], "reviewed");
    assert_eq!(bundle.items()[0].desired()["metadata"]["namespace"], "team");
    assert!(
        bundle.items()[2].desired()["metadata"]
            .get("namespace")
            .is_none()
    );
    assert!(writes(&f).is_empty());
    assert!(
        bundle.confirm().is_err(),
        "confirmation requires actual server dry-run"
    );
    // The failed early confirmation returns no executable plan; prepare again.
    let mut bundle = workbench::prepare(
        session(&f),
        Source::Yaml(dir.path().to_owned()),
        "team".into(),
    )
    .await
    .unwrap();
    bundle.dry_run_all().await;
    assert!(
        bundle
            .items()
            .iter()
            .all(|i| matches!(i.state(), State::Validated))
    );
    assert!(writes(&f).is_empty());
    assert!(
        f.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(r, _)| r.contains("dryRun=All"))
            .count()
            >= 3
    );
    write(
        &dir.path().join("a.yaml"),
        &object("redirected", "changed after review"),
    );
    std::fs::remove_file(dir.path().join("b.yml")).unwrap();
    f.deny("post/configmaps/new");
    let report = bundle.confirm().unwrap().execute().await;
    assert_eq!(report.context(), "pinned-context");
    assert_eq!(report.namespace(), "team");
    assert!(matches!(report.items()[0].state(), State::Succeeded));
    assert!(matches!(report.items()[1].state(), State::Failed(message) if message.contains("403")));
    assert!(matches!(report.items()[2].state(), State::Succeeded));
    assert_eq!(
        f.object("ConfigMap", "existing")["data"]["value"],
        "reviewed"
    );
    assert_eq!(writes(&f).len(), 3, "one attempt each, no replay");
}
#[tokio::test]
async fn workbench_file_cancel_errors_duplicates_unknown_and_forbidden() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::start(vec![]).await;
    let a = dir.path().join("a.yaml");
    write(&a, &object("one", "kept"));
    let mut cancelled = workbench::prepare(session(&f), Source::Yaml(a.clone()), "team".into())
        .await
        .unwrap();
    cancelled.dry_run_all().await;
    drop(cancelled);
    assert!(writes(&f).is_empty());
    let mut duplicate = object("one", "same identity");
    duplicate["metadata"]["namespace"] = json!("team");
    write(&dir.path().join("b.yaml"), &duplicate);
    let error = workbench::prepare(
        session(&f),
        Source::Yaml(dir.path().to_owned()),
        "team".into(),
    )
    .await
    .err()
    .unwrap();
    for text in ["a.yaml", "b.yaml", "document 1", "duplicate", "team"] {
        assert!(error.contains(text), "{error}");
    }
    std::fs::remove_file(dir.path().join("b.yaml")).unwrap();
    std::fs::write(
        &a,
        format!(
            "{}---\napiVersion: v1\nkind: [broken\n",
            serde_saphyr::to_string(&object("one", "ok")).unwrap()
        ),
    )
    .unwrap();
    let error = workbench::prepare(session(&f), Source::Yaml(a.clone()), "team".into())
        .await
        .err()
        .unwrap();
    for text in ["a.yaml", "document 2", "line 9, column 7"] {
        assert!(error.contains(text), "{error}");
    }
    std::fs::write(
        &a,
        "apiVersion: absent.io/v1\nkind: Unavailable\nmetadata: {name: missing}\n",
    )
    .unwrap();
    let unknown = workbench::prepare(session(&f), Source::Yaml(a.clone()), "team".into())
        .await
        .unwrap();
    assert!(
        matches!(unknown.items()[0].state(), State::Failed(message) if message.contains("discovery") && message.contains("Unavailable"))
    );
    write(&a, &object("forbidden", "value"));
    f.deny("post/configmaps");
    let mut refused = workbench::prepare(session(&f), Source::Yaml(a), "team".into())
        .await
        .unwrap();
    refused.dry_run_all().await;
    assert!(
        matches!(refused.items()[0].state(), State::Failed(message) if message.contains("403"))
    );
    assert!(refused.confirm().is_err());
    assert!(writes(&f).is_empty());
}
#[tokio::test]
async fn workbench_real_local_helm_selected_values_and_kustomize_overlay_share_execution() {
    let dir = tempfile::tempdir().unwrap();
    let chart = dir.path().join("chart");
    std::fs::create_dir_all(chart.join("templates")).unwrap();
    std::fs::write(
        chart.join("Chart.yaml"),
        "apiVersion: v2\nname: tiny\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(chart.join("values.yaml"), "message: default\n").unwrap();
    std::fs::write(chart.join("templates/cm.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Release.Name }}\ndata:\n  value: {{ .Values.message | quote }}\n").unwrap();
    let values = dir.path().join("chosen.yaml");
    std::fs::write(&values, "message: selected\n").unwrap();
    let overlay = dir.path().join("overlay");
    std::fs::create_dir_all(overlay.join("base")).unwrap();
    write(&overlay.join("base/cm.yaml"), &object("overlay", "base"));
    std::fs::write(
        overlay.join("base/kustomization.yaml"),
        "resources: [cm.yaml]\n",
    )
    .unwrap();
    std::fs::write(overlay.join("kustomization.yaml"), "resources: [base]\npatches:\n- target: {kind: ConfigMap, name: overlay}\n  patch: |-\n    - op: replace\n      path: /data/value\n      value: overlay-selected\n").unwrap();
    let f = Fixture::start(vec![]).await;
    for (source, name, expected) in [
        (
            Source::Helm {
                chart: chart.clone(),
                values: vec![values.clone()],
                release: "chosen".into(),
            },
            "chosen",
            "selected",
        ),
        (
            Source::Kustomize(overlay.clone()),
            "overlay",
            "overlay-selected",
        ),
    ] {
        let mut bundle = workbench::prepare(session(&f), source, "render-team".into())
            .await
            .unwrap();
        assert_eq!(bundle.items().len(), 1);
        assert_eq!(bundle.items()[0].desired()["data"]["value"], expected);
        bundle.dry_run_all().await;
        if name == "chosen" {
            std::fs::write(&values, "message: changed\n").unwrap();
        } else {
            write(&overlay.join("base/cm.yaml"), &object("later", "changed"));
        }
        let report = bundle.confirm().unwrap().execute().await;
        assert!(matches!(report.items()[0].state(), State::Succeeded));
        assert_eq!(f.object("ConfigMap", name)["data"]["value"], expected);
        assert_eq!(
            f.object("ConfigMap", name)["metadata"]["namespace"],
            "render-team"
        );
    }
}
#[tokio::test]
async fn workbench_physical_documents_scope_validation_and_stale_review_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.yaml");
    std::fs::write(&path, "---\n# empty document\n---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: text, namespace: explicit}\ndata:\n  value: |-\n    first\n    ---\n    last\n---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: text}\ndata: {value: other namespace}\n").unwrap();
    let f = Fixture::start(vec![]).await;
    let mut bundle = workbench::prepare(session(&f), Source::Yaml(path.clone()), "team".into())
        .await
        .unwrap();
    assert_eq!(bundle.items().len(), 2);
    assert!(bundle.items()[0].location().contains("document 2"));
    assert!(bundle.items()[1].location().contains("document 3"));
    assert_eq!(
        bundle.items()[0].desired()["data"]["value"],
        "first\n---\nlast"
    );
    bundle.dry_run_all().await;
    drop(bundle); // Cancel after validation, including an explicit namespace.
    assert!(writes(&f).is_empty());
    std::fs::write(
        &path,
        "apiVersion: v1\nkind: Namespace\nmetadata: {name: invalid-scope, namespace: team}\n",
    )
    .unwrap();
    assert!(
        workbench::prepare(session(&f), Source::Yaml(path.clone()), "team".into())
            .await
            .is_err()
    );
    let mut current = object("stale", "before");
    current["metadata"] =
        json!({"name":"stale","namespace":"team","uid":"incarnation","resourceVersion":"1"});
    f.put(current.clone());
    write(&path, &object("stale", "reviewed"));
    let mut bundle = workbench::prepare(session(&f), Source::Yaml(path), "team".into())
        .await
        .unwrap();
    bundle.dry_run_all().await;
    current["metadata"]["resourceVersion"] = json!("2");
    current["data"]["value"] = json!("concurrent edit");
    f.put(current);
    let other = Fixture::start(vec![]).await;
    let _other_session = session(&other); // A newly selected session cannot redirect the bundle.
    let report = bundle.confirm().unwrap().execute().await;
    assert!(matches!(report.items()[0].state(), State::Failed(message) if message.contains("409")));
    assert_eq!(
        f.object("ConfigMap", "stale")["data"]["value"],
        "concurrent edit"
    );
    assert!(other.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn workbench_renderer_refuses_remote_symlinks_plugins_and_preserves_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::start(vec![]).await;
    for text in [
        "resources: [https://example.invalid/base]\n",
        "generators: [plugin.yaml]\n",
        "resources: [../outside.yaml]\n",
        "helmCharts: [{name: remote, repo: https://example.invalid}]\n",
    ] {
        std::fs::write(dir.path().join("kustomization.yaml"), text).unwrap();
        assert!(
            workbench::prepare(
                session(&f),
                Source::Kustomize(dir.path().to_owned()),
                "team".into()
            )
            .await
            .is_err()
        );
    }
    std::fs::write(
        dir.path().join("kustomization.yaml"),
        "resources: [missing.yaml]\n",
    )
    .unwrap();
    let error = workbench::prepare(
        session(&f),
        Source::Kustomize(dir.path().to_owned()),
        "team".into(),
    )
    .await
    .err()
    .unwrap();
    assert!(error.contains("missing.yaml"), "{error}");
    // A loopback URL is still a remote input. Neither renderer may request it.
    std::fs::write(
        dir.path().join("kustomization.yaml"),
        format!("resources: [{}/config.yaml]\n", f.url),
    )
    .unwrap();
    assert!(
        workbench::prepare(
            session(&f),
            Source::Kustomize(dir.path().to_owned()),
            "team".into()
        )
        .await
        .is_err()
    );
    // Even an existing path with URL-shaped components cannot authorize HTTP.
    #[cfg(unix)]
    {
        let disguised = dir.path().join(format!("{}/config.yaml", f.url));
        std::fs::create_dir_all(disguised.parent().unwrap()).unwrap();
        write(&disguised, &object("disguised", "must not request URL"));
    }
    let remote_error = workbench::prepare(
        session(&f),
        Source::Kustomize(dir.path().to_owned()),
        "team".into(),
    )
    .await
    .err()
    .unwrap();
    assert!(remote_error.contains("remote reference"), "{remote_error}");
    std::fs::write(dir.path().join("cm.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: functions\n  annotations:\n    config.kubernetes.io/function: 'exec: {path: forbidden}'\n").unwrap();
    std::fs::write(
        dir.path().join("kustomization.yaml"),
        "resources: [cm.yaml]\n",
    )
    .unwrap();
    let function_error = workbench::prepare(
        session(&f),
        Source::Kustomize(dir.path().to_owned()),
        "team".into(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        function_error.contains("functions are disabled"),
        "{function_error}"
    );
    let chart = dir.path().join("chart");
    std::fs::create_dir_all(chart.join("templates")).unwrap();
    std::fs::write(
        chart.join("Chart.yaml"),
        "apiVersion: v2\nname: boundary\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        chart.join("values.schema.json"),
        json!({"$ref":format!("{}/schema",f.url)}).to_string(),
    )
    .unwrap();
    assert!(
        workbench::prepare(
            session(&f),
            Source::Helm {
                chart: chart.clone(),
                values: vec![],
                release: "boundary".into()
            },
            "team".into()
        )
        .await
        .is_err()
    );
    std::fs::remove_file(chart.join("values.schema.json")).unwrap();
    std::fs::write(
        chart.join("templates/broken.yaml"),
        "{{ unavailable_function }}\n",
    )
    .unwrap();
    let render_error = workbench::prepare(
        session(&f),
        Source::Helm {
            chart,
            values: vec![],
            release: "boundary".into(),
        },
        "team".into(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        render_error.contains("Local renderer failed")
            && render_error.contains("templates/broken.yaml")
            && render_error.contains("unavailable_function"),
        "{render_error}"
    );
    assert!(
        f.requests.lock().unwrap().is_empty(),
        "remote renderer inputs must fail before HTTP or API traffic"
    );
    #[cfg(unix)]
    {
        let external = tempfile::tempdir().unwrap();
        write(
            &external.path().join("outside.yaml"),
            &object("outside", "refuse symlink"),
        );
        let linked = dir.path().join("linked.yaml");
        std::os::unix::fs::symlink(external.path().join("outside.yaml"), &linked).unwrap();
        assert!(
            workbench::prepare(session(&f), Source::Yaml(linked), "team".into())
                .await
                .err()
                .unwrap()
                .contains("symlink")
        );
    }
    assert!(writes(&f).is_empty());
}
