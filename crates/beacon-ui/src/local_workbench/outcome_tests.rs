//! GUI regression for mixed unknown, refused and successful confirmed writes.
use super::{open_window, state_label};
use crate::{Bridge, feature_test_support as support};
use beacon_kube::{ClusterId, ClusterSession, test_support::Fixture, workbench::State};
use gpui_kit::{test::TestWindowExt as _, *};
use serde_json::json;
use std::sync::Arc;

#[::core::prelude::v1::test]
fn damaged_accepted_responses_show_unknown_outcomes_and_never_replay() {
    let cx = &mut support::context();
    let directory = tempfile::tempdir().unwrap();
    support::workspace(cx, directory.path());
    let path = directory.path().join("resources.yaml");
    std::fs::write(&path, ["committed-new", "committed-edit", "refused", "succeeded"].map(|name| {
        format!("apiVersion: v1\nkind: ConfigMap\nmetadata: {{name: {name}}}\ndata: {{value: reviewed}}\n")
    }).join("---\n")).unwrap();
    let runtime = cx.read(|cx| Bridge::global(cx).handle());
    let (fixture, session) = runtime.block_on(async {
        let fixture = Fixture::start(vec![json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"committed-edit","namespace":"team","uid":"edit-uid","resourceVersion":"1"},"data":{"value":"old"}})]).await;
        let mut kind = support::kind("", "ConfigMap", "configmaps");
        kind.verbs.push("create".into());
        let session = Arc::new(ClusterSession::for_testing(ClusterId::new("pinned-outcome-context"), fixture.url.clone(), vec![kind]));
        (fixture, session)
    });
    let (window, view) = cx.update(|cx| open_window(session.clone(), "team".into(), cx).unwrap());
    cx.update_window(window, |_, window, cx| {
        window.resize(size(px(1300.), px(1100.)));
        window.render_frame(cx);
        window.click("workbench-yaml-file", cx);
    })
    .unwrap();
    let selected = path.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![selected]));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-prepare", cx);
    })
    .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |view, _| !view.busy && view.bundle.is_some())
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-dry-run-all", cx);
    })
    .unwrap();
    support::settle(cx, |cx| view.read_with(cx, |view, _| !view.busy));
    view.read_with(cx, |view, _| {
        assert!(
            view.bundle
                .as_ref()
                .unwrap()
                .items()
                .iter()
                .all(|item| item.state() == &State::Validated)
        )
    });
    let writes = || {
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
            .count()
    };
    assert_eq!(writes(), 0);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("workbench-review", cx);
    })
    .unwrap();
    support::render_until(cx, window, |window| {
        window
            .try_find("confirm-yaml-review")
            .is_some_and(|button| button.visible())
    });
    assert_eq!(writes(), 0, "opening confirmation must not write");
    fixture.damage_write_response("POST", "configmaps", "committed-new");
    fixture.damage_write_response("PATCH", "configmaps", "committed-edit");
    fixture.deny("post/configmaps/refused");
    std::fs::write(&path, "invalid: [changed after review\n").unwrap();
    cx.update_window(window, |_, window, cx| {
        window.click("confirm-yaml-review", cx)
    })
    .unwrap();
    support::settle(cx, |cx| {
        view.read_with(cx, |view, _| !view.busy && view.report.is_some())
    });
    // Public GETs prove that the server persisted both writes despite bad responses.
    runtime.block_on(async {
        for name in ["committed-new", "committed-edit"] {
            let saved = session
                .clone()
                .get_object(
                    session.discovery().kinds()[0].resource.clone(),
                    Some("team".into()),
                    name.into(),
                )
                .await
                .unwrap();
            assert_eq!(saved.data["data"]["value"], "reviewed");
        }
    });
    assert_eq!(writes(), 4, "one write attempt per resource");
    view.read_with(cx, |view, _| {
        assert!(view.bundle.is_none());
        let report = view.report.as_ref().unwrap();
        assert_eq!(report.context(), "pinned-outcome-context");
        assert_eq!(report.namespace(), "team");
        assert!(
            matches!(report.items()[2].state(), State::Failed(message) if message.contains("403"))
        );
        assert!(matches!(report.items()[3].state(), State::Succeeded));
        for index in [0, 1] {
            let label = state_label(report.items()[index].state());
            assert!(
                label.starts_with("Outcome unknown:"),
                "persisted write is not a definitive failure: {label}"
            );
        }
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for (index, name) in [(0usize, "committed-new"), (1usize, "committed-edit")] {
            let row = window.find(("workbench-result", index));
            assert!(row.visible());
            let label = row.label().expect("resource outcome must be accessible");
            assert!(label.starts_with("Outcome unknown:"), "{label}");
            for expected in [
                "pinned-outcome-context",
                name,
                "team",
                "deserializing response",
                "Inspect",
                "reconcile",
                "retry",
            ] {
                assert!(label.contains(expected), "missing {expected:?}: {label}");
            }
        }
        let refused = window.find(("workbench-result", 2usize));
        assert!(refused.label().unwrap().starts_with("Failed:"));
        assert!(refused.label().unwrap().contains("403"));
        assert_eq!(
            window.find(("workbench-result", 3usize)).label(),
            Some("Success")
        );
        // Exercise the controls directly: GPUI does not expose a native disabled
        // flag for these buttons, so the write count verifies that they cannot replay.
        window.click("workbench-review", cx);
        window.click("workbench-dry-run-all", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        writes(),
        4,
        "unknown and successful writes must not be replayed"
    );
}
