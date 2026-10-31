//! Local files, Helm values and Kustomize overlays share one immutable review flow.
use crate::{bridge::Bridge, scroll::OverflowScrollbars as _, yaml_review};
use beacon_kube::{
    ClusterSession,
    workbench::{self, Bundle, Report, Source, State},
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;

pub(crate) struct Workbench {
    session: Arc<ClusterSession>,
    source: Option<Source>,
    namespace: Entity<InputState>,
    release: Entity<InputState>,
    pub(super) bundle: Option<Bundle>,
    pub(super) report: Option<Report>,
    pub(super) busy: bool,
    reviewing: bool,
    pub(super) error: Option<String>,
    _operation: Option<Task<()>>,
    _review: Option<Entity<yaml_review::ReviewView>>,
}
pub(crate) fn open(session: Arc<ClusterSession>, namespace: String, cx: &mut App) {
    cx.defer(move |cx| {
        if let Err(error) = open_window(session, namespace, cx) {
            tracing::error!(%error,"could not open local workbench");
        }
    });
}
pub(crate) fn open_window(
    session: Arc<ClusterSession>,
    namespace: String,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<Workbench>)> {
    gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1050.), px(800.)),
                cx,
            ))),
            window_min_size: Some(size(px(700.), px(550.))),
            ..TitleBar::window_options()
        },
        cx,
        move |window, cx| {
            #[cfg(all(test, feature = "ui-tests"))]
            window.set_view_retention(false);
            let view = cx.new(|cx| Workbench {
                session,
                source: None,
                namespace: cx.new(|cx| InputState::new(window, cx).default_value(namespace)),
                release: cx.new(|cx| InputState::new(window, cx).default_value("local-preview")),
                bundle: None,
                report: None,
                busy: false,
                reviewing: false,
                error: None,
                _operation: None,
                _review: None,
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| {
                    if view.busy {
                        view.error = Some("Wait for the running operation to finish so individual outcomes remain visible.".into());
                        cx.notify();
                        false
                    } else { true }
                }).unwrap_or(true)
            });
            crate::i18n::window_title("Local configuration workbench", window, cx);
            window.activate_window();
            view
        },
    )
}
#[derive(Clone, Copy)]
enum Choice {
    File,
    Directory,
    Chart,
    Values,
    Overlay,
}
impl Workbench {
    fn locked(&self) -> bool {
        self.busy || self.reviewing || self.bundle.is_some() || self.report.is_some()
    }
    fn choose(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        let directory = matches!(choice, Choice::Directory | Choice::Chart | Choice::Overlay);
        let choosing = cx.prompt_for_paths(PathPromptOptions {
            files: !directory,
            directories: directory,
            multiple: matches!(choice, Choice::Values),
            prompt: Some("Choose local workbench input".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = choosing.await
                && let Some(first) = paths.first()
            {
                let first = first.clone();
                let _ = this.update(cx, |view, cx| {
                    if view.locked() {
                        return;
                    }
                    view.error = None;
                    view.source = Some(match choice {
                        Choice::File | Choice::Directory => Source::Yaml(first),
                        Choice::Chart => Source::Helm {
                            chart: first,
                            values: vec![],
                            release: "local-preview".into(),
                        },
                        Choice::Values => {
                            if let Some(Source::Helm { values, .. }) = &mut view.source {
                                *values = paths;
                            }
                            cx.notify();
                            return;
                        }
                        Choice::Overlay => Source::Kustomize(first),
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }
    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        let Some(mut source) = self.source.clone() else {
            self.error =
                Some("Choose a local YAML file, directory, chart or overlay first.".into());
            cx.notify();
            return;
        };
        if let Source::Helm { release, .. } = &mut source {
            *release = self.release.read(cx).value().trim().to_owned();
        }
        let namespace = self.namespace.read(cx).value().trim().to_owned();
        let session = self.session.clone();
        self.busy = true;
        self.error = None;
        cx.notify();
        let preparing = Bridge::global(cx)
            .run(async move { workbench::prepare(session, source, namespace).await });
        self._operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = preparing.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                match result {
                    Ok(Ok(bundle)) => view.bundle = Some(bundle),
                    Ok(Err(error)) => view.error = Some(error),
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }
    fn dry_run(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.reviewing {
            return;
        }
        let Some(mut bundle) = self.bundle.take() else {
            return;
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let validating = Bridge::global(cx).run(async move {
            if let Some(index) = index {
                bundle.dry_run(index).await;
            } else {
                bundle.dry_run_all().await;
            }
            bundle
        });
        self._operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = validating.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                match result {
                    Ok(bundle) => view.bundle = Some(bundle),
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }
    fn preview(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.reviewing {
            return;
        }
        let items = self
            .bundle
            .as_ref()
            .map(Bundle::items)
            .or_else(|| self.report.as_ref().map(Report::items));
        let Some(item) = items.and_then(|items| items.get(index)) else {
            return;
        };
        let before = item.before().cloned();
        let desired = item.desired().clone();
        let create = before.is_none();
        let context = format!(
            "{} · {} · {}",
            self.session.id().display_name(),
            item.identity(),
            item.location()
        );
        self.busy = true;
        let preparing = Bridge::global(cx).run(async move {
            match before {
                Some(before) => yaml_review::Preview::apply(before, desired),
                None => yaml_review::Preview::create(desired),
            }
        });
        self._operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = preparing.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = false;
                match result {
                    Ok(Ok(preview)) => {
                        let review = yaml_review::open(preview, context, create, false, window, cx);
                        review.update(cx, |view, _| view.read_only());
                        view._review = Some(review);
                    }
                    Ok(Err(error)) => view.error = Some(error),
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.reviewing {
            return;
        }
        let Some(bundle) = self.bundle.as_ref().filter(|bundle| bundle.can_confirm()) else {
            return;
        };
        let resources = bundle
            .items()
            .iter()
            .filter(|item| item.state() == &State::Validated)
            .map(|item| {
                (
                    item.identity().to_owned(),
                    item.location().to_owned(),
                    item.before().cloned(),
                    item.desired().clone(),
                )
            })
            .collect::<Vec<_>>();
        let count = resources.len();
        let context = format!(
            "{} · {} · default namespace {} · {} refused or unchanged · {count} validated resources",
            bundle.context(),
            bundle.server(),
            bundle.namespace(),
            bundle.items().len() - count,
        );
        self.busy = true;
        self.reviewing = true;
        let preparing = Bridge::global(cx).run(async move {
            let mut before = String::new();
            let mut after = String::new();
            for (identity, location, current, desired) in resources {
                let header = format!("# {identity}\n# {location}\n");
                before.push_str(&header);
                if let Some(current) = current {
                    before.push_str(&yaml_review::canonical_yaml(current)?);
                }
                before.push_str("\n---\n");
                after.push_str(&header);
                after.push_str(&yaml_review::canonical_yaml(desired)?);
                after.push_str("\n---\n");
            }
            yaml_review::Preview::text(&before, after)
        });
        self._operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = preparing.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = false;
                match result {
                    Ok(Ok(preview)) => {
                        let review = yaml_review::open(preview, context, false, false, window, cx);
                        review.update(cx, |view, _| {
                            view.cluster_sources();
                            view.confirmation_label(&format!("Confirm & apply {count} resources"));
                        });
                        cx.subscribe_in(
                            &review,
                            window,
                            |view, _, event: &yaml_review::ReviewEvent, window, cx| {
                                view.reviewing = false;
                                if matches!(event, yaml_review::ReviewEvent::Confirmed(_)) {
                                    view.execute(window, cx);
                                }
                                cx.notify();
                            },
                        )
                        .detach();
                        view._review = Some(review);
                    }
                    Ok(Err(error)) => {
                        view.reviewing = false;
                        view.error = Some(error);
                    }
                    Err(error) => {
                        view.reviewing = false;
                        view.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn execute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(bundle) = self.bundle.take() else {
            return;
        };
        let confirmed = match bundle.confirm() {
            Ok(plan) => plan,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.busy = true;
        cx.notify();
        let applying = Bridge::global(cx).run(async move { confirmed.execute().await });
        self._operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = applying.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                match result {
                    Ok(report) => view.report = Some(report),
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }
    fn source_label(&self) -> String {
        match &self.source {
            None => "Choose local input to prepare a resource review.".into(),
            Some(Source::Yaml(path)) => path.display().to_string(),
            Some(Source::Kustomize(path)) => format!("Kustomize overlay: {}", path.display()),
            Some(Source::Helm { chart, values, .. }) => format!(
                "Helm chart: {} · values (in order): {}",
                chart.display(),
                values
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}
fn state_label(state: &State) -> String {
    match state {
        State::Prepared => "Ready for server dry-run".into(),
        State::Validated => "Server dry-run passed · no write performed".into(),
        State::Unchanged => "Unchanged · skipped".into(),
        State::Succeeded => "Success".into(),
        State::Failed(message) => format!("Failed: {message}"),
    }
}
impl Render for Workbench {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locked = self.locked();
        let items = self
            .bundle
            .as_ref()
            .map(Bundle::items)
            .or_else(|| self.report.as_ref().map(Report::items));
        let rows = items
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, item)| {
                v_flex()
                    .gap_1()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(item.identity().to_owned()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(item.location().to_owned()),
                    )
                    .child(
                        div()
                            .id(("workbench-result", index))
                            .test_support()
                            .text_sm()
                            .child(state_label(item.state())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(("workbench-preview", index))
                                    .small()
                                    .label("Preview / diff")
                                    .disabled(self.busy || self.reviewing)
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.preview(index, window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("workbench-dry-run", index))
                                    .small()
                                    .label("Server dry-run")
                                    .disabled(
                                        self.busy
                                            || self.reviewing
                                            || self.report.is_some()
                                            || !matches!(
                                                item.state(),
                                                State::Prepared | State::Validated
                                            ),
                                    )
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.dry_run(Some(index), window, cx)
                                    })),
                            ),
                    )
            })
            .collect::<Vec<_>>();
        let source_controls = h_flex().gap_2().flex_wrap().children(
            [
                ("workbench-yaml-file", "YAML file", Choice::File),
                (
                    "workbench-yaml-directory",
                    "YAML directory",
                    Choice::Directory,
                ),
                ("workbench-helm-chart", "Helm chart", Choice::Chart),
                ("workbench-helm-values", "Select values", Choice::Values),
                ("workbench-kustomize", "Kustomize overlay", Choice::Overlay),
            ]
            .into_iter()
            .map(|(id, label, choice)| {
                Button::new(id)
                    .small()
                    .label(label)
                    .disabled(
                        locked
                            || (matches!(choice, Choice::Values)
                                && !matches!(self.source, Some(Source::Helm { .. }))),
                    )
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.choose(choice, window, cx)),
                    )
            }),
        );
        let scope_controls = h_flex()
            .gap_3()
            .items_center()
            .child("Default namespace")
            .child(
                Input::new(&self.namespace)
                    .id("workbench-namespace")
                    .disabled(locked)
                    .w(px(180.)),
            )
            .when(matches!(self.source, Some(Source::Helm { .. })), |row| {
                row.child("Release name")
                    .child(Input::new(&self.release).disabled(locked).w(px(180.)))
            });
        let footer = h_flex()
            .p_4()
            .gap_2()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("workbench-reset")
                    .small()
                    .ghost()
                    .label(if self.bundle.is_some() {
                        "Cancel / choose another source"
                    } else {
                        "Choose another source"
                    })
                    .disabled(self.busy || self.reviewing)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.bundle = None;
                        view.report = None;
                        view.error = None;
                        view._review = None;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("workbench-prepare")
                    .small()
                    .label("Prepare review")
                    .disabled(locked || self.source.is_none())
                    .on_click(cx.listener(|view, _, window, cx| view.prepare(window, cx))),
            )
            .child(
                Button::new("workbench-dry-run-all")
                    .small()
                    .label("Dry-run all")
                    .disabled(self.busy || self.reviewing || self.bundle.is_none())
                    .on_click(cx.listener(|view, _, window, cx| view.dry_run(None, window, cx))),
            )
            .child(
                Button::new("workbench-review")
                    .small()
                    .primary()
                    .label(if self.busy {
                        "Working…"
                    } else {
                        "Review & apply…"
                    })
                    .disabled(
                        self.busy
                            || self.reviewing
                            || !self.bundle.as_ref().is_some_and(Bundle::can_confirm),
                    )
                    .on_click(cx.listener(|view, _, window, cx| view.review(window, cx))),
            );
        v_flex().size_full().bg(cx.theme().background).text_color(cx.theme().foreground)
            .child(TitleBar::new().child("Local configuration workbench"))
            .child(
                v_flex().p_4().gap_3().flex_shrink_0()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                        "Target: {} · {}", self.session.id().display_name(), self.session.server(),
                    )))
                    .child(source_controls)
                    .child(div().text_sm().child(self.source_label()))
                    .child(scope_controls)
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(
                        if self.bundle.is_some() || self.report.is_some() {
                            "Prepared resources and target are frozen. Choose another source to prepare a new review."
                        } else {
                            "Local inputs only. Keep chart dependencies self-contained and overlay bases inside the chosen directory."
                        },
                    )),
            )
            .child(div().id("workbench-resources").flex_1().min_h_0().overflow_y_scrollbar().children(rows))
            .children(self.error.as_ref().map(|error| {
                div().id("workbench-error").test_support().max_h(px(150.)).overflow_y_scrollbar()
                    .p_3().text_color(cx.theme().danger).child(error.clone())
            }))
            .child(footer)
    }
}
