//! Foreground acceptance: use the same controls that a workbench user uses.
use crate::{feature_test_support as support, local_workbench};
use beacon_kube::{ClusterId, ClusterSession, test_support::Fixture, workbench::State};
use gpui_kit::{test::TestWindowExt as _, *};
use serde_json::json;
use std::sync::Arc;

fn fixture(cx: &TestAppContext) -> (Fixture, Arc<ClusterSession>) {
    let runtime = cx.read(|cx| crate::Bridge::global(cx).handle());
    runtime.block_on(async {
        let fixture = Fixture::start(vec![json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"existing","namespace":"team","uid":"e","resourceVersion":"1"},"data":{"value":"old"}})]).await;
        let kinds = [("ConfigMap","configmaps"),("Namespace","namespaces")].into_iter().map(|(k,p)| {
            let mut kind = support::kind("", k, p); kind.verbs.push("create".into()); kind
        }).collect();
        let session = Arc::new(ClusterSession::for_testing(ClusterId::new("frozen-context"), fixture.url.clone(), kinds));
        (fixture, session)
    })
}
fn writes(f: &Fixture) -> usize {
    f.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(r, _)| {
            (r.starts_with("POST") || r.starts_with("PATCH"))
                && !r.contains("reviews")
                && !r.contains("dryRun=All")
        })
        .count()
}
#[::core::prelude::v1::test]
fn workbench_gui_directory_review_dryrun_cancel_then_confirm_retains_individual_results() {
    let cx = &mut support::context();
    let dir = tempfile::tempdir().unwrap();
    support::workspace(cx, dir.path());
    let source = dir.path().join("input");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("a.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: existing}\ndata: {value: reviewed}\n---\napiVersion: v1\nkind: Namespace\nmetadata: {name: created-ns}\n").unwrap();
    std::fs::write(
        source.join("b.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: refused}\ndata: {value: reviewed}\n",
    )
    .unwrap();
    let (f, session) = fixture(cx);
    let (window, view) =
        cx.update(|cx| local_workbench::open_window(session, "team".into(), cx).unwrap());
    let picked = source.clone();
    cx.update_window(window, |_, window, cx| {
        window.resize(size(px(1100.), px(900.)));
        window.render_frame(cx);
        window.click("workbench-yaml-directory", cx);
    })
    .unwrap();
    cx.simulate_path_prompt_response(move |options| {
        assert!(options.directories);
        Some(vec![picked])
    });
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-prepare", cx);
    })
    .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |v, _| !v.busy && v.bundle.is_some())
    });
    view.read_with(cx, |v, _| {
        assert_eq!(v.bundle.as_ref().unwrap().items().len(), 3)
    });
    assert_eq!(writes(&f), 0);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("workbench-preview", 0usize), cx);
    })
    .unwrap();
    let value_line = view.read_with(cx, |view, _| {
        crate::yaml_review::canonical_yaml(
            view.bundle.as_ref().unwrap().items()[0].desired().clone(),
        )
        .unwrap()
        .lines()
        .position(|line| line.contains("reviewed"))
        .unwrap()
    });
    support::render_until(cx, window, |w| {
        w.try_find("yaml-review-diff")
            .is_some_and(|view| view.visible())
    });
    cx.update_window(window, |_, window, cx| {
        assert!(
            window
                .within(("old", value_line))
                .find("source")
                .label()
                .unwrap()
                .contains("old")
        );
        assert!(
            window
                .within(("new", value_line))
                .find("source")
                .label()
                .unwrap()
                .contains("reviewed")
        );
        window.click("cancel-yaml-review", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("workbench-preview", 1usize), cx);
    })
    .unwrap();
    support::render_until(cx, window, |w| {
        w.try_find("yaml-review-diff")
            .is_some_and(|view| view.visible())
    });
    cx.update_window(window, |_, window, cx| {
        assert!(
            window
                .within(("new", 0usize))
                .find("source")
                .label()
                .unwrap()
                .contains("apiVersion")
        );
        window.click("cancel-yaml-review", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-dry-run-all", cx);
    })
    .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |v, _| {
            !v.busy
                && v.bundle
                    .as_ref()
                    .unwrap()
                    .items()
                    .iter()
                    .all(|i| matches!(i.state(), State::Validated))
        })
    });
    assert_eq!(writes(&f), 0);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-review", cx);
    })
    .unwrap();
    support::render_until(cx, window, |w| {
        w.try_find("confirm-yaml-review")
            .is_some_and(|view| view.visible())
    });
    cx.update_window(window, |_, window, cx| {
        window.click("cancel-yaml-review", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(writes(&f), 0);
    std::fs::write(source.join("a.yaml"), "invalid: [changed after prepare").unwrap();
    f.deny("post/configmaps/refused");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-review", cx);
    })
    .unwrap();
    support::render_until(cx, window, |w| {
        w.try_find("confirm-yaml-review")
            .is_some_and(|view| view.visible())
    });
    cx.update_window(window, |_, window, cx| {
        window.click("confirm-yaml-review", cx);
    })
    .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |v, _| !v.busy && v.report.is_some())
    });
    view.read_with(cx, |v, _| {
        let report = v.report.as_ref().unwrap();
        assert_eq!(report.context(), "frozen-context");
        assert_eq!(report.namespace(), "team");
        assert!(matches!(report.items()[0].state(), State::Succeeded));
        assert!(matches!(report.items()[1].state(), State::Succeeded));
        assert!(matches!(report.items()[2].state(),State::Failed(e) if e.contains("403")));
    });
    assert_eq!(writes(&f), 3);
    assert_eq!(
        f.object("ConfigMap", "existing")["data"]["value"],
        "reviewed"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(("workbench-result", 2usize)).visible());
    })
    .unwrap();
}
fn apply_rendered(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<local_workbench::Workbench>,
) {
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click(("workbench-dry-run", 0usize), cx);
    })
    .unwrap();
    support::settle(cx, |cx| view.read_with(cx, |v, _| !v.busy));
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-review", cx);
    })
    .unwrap();
    support::render_until(cx, window, |w| {
        w.try_find("confirm-yaml-review")
            .is_some_and(|view| view.visible())
    });
    cx.update_window(window, |_, w, cx| w.click("confirm-yaml-review", cx))
        .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |v, _| !v.busy && v.report.is_some())
    });
    view.read_with(cx, |v, _| {
        assert!(matches!(
            v.report.as_ref().unwrap().items()[0].state(),
            State::Succeeded
        ))
    });
}
#[::core::prelude::v1::test]
fn workbench_gui_file_errors_and_real_renderer_selection() {
    let cx = &mut support::context();
    let dir = tempfile::tempdir().unwrap();
    support::workspace(cx, dir.path());
    let (f, session) = fixture(cx);
    let (window, view) =
        cx.update(|cx| local_workbench::open_window(session, "team".into(), cx).unwrap());
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "apiVersion: v1\nkind: [broken\n").unwrap();
    cx.update_window(window, |_, w, cx| {
        w.resize(size(px(1100.), px(900.)));
        w.render_frame(cx);
        w.click("workbench-yaml-file", cx);
    })
    .unwrap();
    cx.simulate_path_prompt_response(move |options| {
        assert!(options.files);
        Some(vec![bad])
    });
    cx.run_until_parked();
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-prepare", cx);
    })
    .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |v, _| !v.busy && v.error.is_some())
    });
    view.read_with(cx, |v, _| {
        let e = v.error.as_ref().unwrap();
        assert!(
            e.contains("bad.yaml")
                && e.contains("document 1")
                && e.contains("line")
                && e.contains("column"),
            "{e}"
        );
    });
    let chart = dir.path().join("chart");
    std::fs::create_dir_all(chart.join("templates")).unwrap();
    std::fs::write(
        chart.join("Chart.yaml"),
        "apiVersion: v2\nname: gui\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(chart.join("templates/cm.yaml"),"apiVersion: v1\nkind: ConfigMap\nmetadata: {name: gui-helm}\ndata:\n  value: {{ .Values.message | quote }}\n").unwrap();
    let values = dir.path().join("selected.yaml");
    std::fs::write(&values, "message: gui-selected\n").unwrap();
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-helm-chart", cx);
    })
    .unwrap();
    cx.simulate_path_prompt_response(move |_| Some(vec![chart]));
    cx.run_until_parked();
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-helm-values", cx);
    })
    .unwrap();
    let selected_values = values.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![selected_values]));
    cx.run_until_parked();
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-prepare", cx);
    })
    .unwrap();
    support::settle(cx, |cx| view.read_with(cx, |v, _| !v.busy));
    view.read_with(cx, |v, _| {
        assert!(v.error.is_none(), "{:?}", v.error);
        assert_eq!(
            v.bundle.as_ref().unwrap().items()[0].desired()["data"]["value"],
            "gui-selected"
        );
    });
    assert_eq!(writes(&f), 0);
    std::fs::write(&values, "message: changed after prepare\n").unwrap();
    apply_rendered(cx, window, &view);
    assert_eq!(
        f.object("ConfigMap", "gui-helm")["data"]["value"],
        "gui-selected"
    );
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-reset", cx);
    })
    .unwrap();
    let overlay = dir.path().join("overlay");
    std::fs::create_dir(&overlay).unwrap();
    std::fs::write(
        overlay.join("cm.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: gui-overlay}\ndata: {value: base}\n",
    )
    .unwrap();
    std::fs::write(
        overlay.join("kustomization.yaml"),
        "resources: [cm.yaml]\nnamePrefix: selected-\n",
    )
    .unwrap();
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-kustomize", cx);
    })
    .unwrap();
    let selected_overlay = overlay.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![selected_overlay]));
    cx.run_until_parked();
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("workbench-prepare", cx);
    })
    .unwrap();
    support::settle(cx, |cx| view.read_with(cx, |v, _| !v.busy));
    view.read_with(cx, |v, _| {
        assert!(v.error.is_none(), "{:?}", v.error);
        assert_eq!(
            v.bundle.as_ref().unwrap().items()[0].desired()["metadata"]["name"],
            "selected-gui-overlay"
        );
    });
    assert_eq!(writes(&f), 1);
    std::fs::write(
        overlay.join("kustomization.yaml"),
        "invalid: [changed after prepare\n",
    )
    .unwrap();
    apply_rendered(cx, window, &view);
    assert_eq!(writes(&f), 2);
    assert_eq!(
        f.object("ConfigMap", "selected-gui-overlay")["data"]["value"],
        "base"
    );
}
