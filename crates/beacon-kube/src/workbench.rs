//! Local configuration preparation and one-shot, explicitly confirmed execution.
//! The bundle owns its session, scope and payloads. Execution never reopens sources.
mod render;

use crate::{ClusterSession, Kind, edit, ops};
use kube::{
    Api,
    api::{DynamicObject, Patch, PatchParams},
};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub enum Source {
    Yaml(PathBuf),
    Helm {
        chart: PathBuf,
        values: Vec<PathBuf>,
        release: String,
    },
    Kustomize(PathBuf),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Prepared,
    Validated,
    Unchanged,
    Succeeded,
    /// The write may have committed. Reconcile the target before another review.
    Unknown(String),
    Failed(String),
}
struct Document {
    location: String,
    value: Value,
}
pub struct Item {
    location: String,
    identity: String,
    desired: Value,
    before: Option<Value>,
    kind: Option<Kind>,
    patch: Option<edit::Plan>,
    state: State,
}
impl Item {
    pub fn location(&self) -> &str {
        &self.location
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn desired(&self) -> &Value {
        &self.desired
    }
    pub fn before(&self) -> Option<&Value> {
        self.before.as_ref()
    }
    pub fn state(&self) -> &State {
        &self.state
    }
}
pub struct Bundle {
    session: Arc<ClusterSession>,
    namespace: String,
    items: Vec<Item>,
}
/// Created only by consuming a bundle whose eligible items passed server dry-run.
/// Neither this token nor its report can be executed twice.
pub struct Confirmed(Bundle);
pub struct Report(Bundle);
impl Report {
    pub fn items(&self) -> &[Item] {
        self.0.items()
    }
    pub fn context(&self) -> &str {
        self.0.context()
    }
    pub fn namespace(&self) -> &str {
        self.0.namespace()
    }
}
impl Bundle {
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    pub fn context(&self) -> &str {
        self.session.id().display_name()
    }
    pub fn server(&self) -> &str {
        self.session.server()
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn can_confirm(&self) -> bool {
        self.items.iter().any(|i| i.state == State::Validated)
            && !self.items.iter().any(|i| i.state == State::Prepared)
    }
    pub fn confirm(self) -> Result<Confirmed, String> {
        if !self.can_confirm() {
            return Err("Dry-run every prepared resource before confirming. At least one resource must pass server validation.".into());
        }
        Ok(Confirmed(self))
    }
    pub async fn dry_run_all(&mut self) {
        for index in 0..self.items.len() {
            self.dry_run(index).await;
        }
    }
    pub async fn dry_run(&mut self, index: usize) {
        let Some(item) = self.items.get_mut(index) else {
            return;
        };
        if item.state != State::Prepared && item.state != State::Validated {
            return;
        }
        item.state = match submit(&self.session, item, true).await {
            Ok(()) => State::Validated,
            Err(state) => state,
        };
    }
}
impl Confirmed {
    pub async fn execute(mut self) -> Report {
        for item in &mut self.0.items {
            if item.state != State::Validated {
                continue;
            }
            item.state = match submit(&self.0.session, item, false).await {
                Ok(()) => State::Succeeded,
                Err(state) => state,
            };
        }
        Report(self.0)
    }
}
async fn submit(session: &Arc<ClusterSession>, item: &Item, dry_run: bool) -> Result<(), State> {
    let operation = async {
        let kind = item
            .kind
            .as_ref()
            .ok_or_else(|| crate::Error::Manifest("Resource discovery is unavailable.".into()))?;
        if let Some(plan) = &item.patch {
            let namespace = item
                .desired
                .pointer("/metadata/namespace")
                .and_then(Value::as_str);
            let name = item.desired["metadata"]["name"]
                .as_str()
                .unwrap_or_default();
            let rules = session.clone().rules(namespace.map(str::to_owned)).await;
            if !kind.supports("patch")
                || !rules.allows_named("patch", &kind.resource.group, &kind.resource.plural, name)
            {
                return Err(crate::Error::Manifest(format!(
                    "Forbidden: cannot patch {} {} (permission precheck).",
                    kind.display_name(),
                    item.identity
                )));
            }
            let api: Api<DynamicObject> = match namespace {
                Some(namespace) => {
                    Api::namespaced_with(session.write_client().clone(), namespace, &kind.resource)
                }
                None => Api::all_with(session.write_client().clone(), &kind.resource),
            };
            let params = PatchParams {
                dry_run,
                field_manager: Some(ops::FIELD_MANAGER.into()),
                ..Default::default()
            };
            api.patch(name, &params, &Patch::<Value>::Json(plan.patch()))
                .await?;
        } else {
            session
                .clone()
                .create(kind.clone(), item.desired.clone(), dry_run)
                .await?;
        }
        Ok::<_, crate::Error>(())
    };
    match tokio::time::timeout(std::time::Duration::from_secs(30), operation).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            // A structured client refusal or local preflight error is definitive.
            // Transport/decoding errors and server timeouts may follow a commit.
            let uncertain = !dry_run
                && match &error {
                    crate::Error::Api(kube::Error::Api(status)) => {
                        status.code == 408 || !(400..500).contains(&status.code)
                    }
                    crate::Error::Api(
                        kube::Error::BuildRequest(_)
                        | kube::Error::HttpError(_)
                        | kube::Error::Auth(_),
                    ) => false,
                    crate::Error::Api(_) => true,
                    _ => false,
                };
            let message = error.user_message();
            Err(if uncertain {
                unknown_outcome(session, item, &message)
            } else {
                State::Failed(message)
            })
        }
        Err(_) if dry_run => Err(State::Failed(
            "Server dry-run timed out. No resource writes were requested.".into(),
        )),
        Err(_) => Err(unknown_outcome(session, item, "Request timed out.")),
    }
}
fn unknown_outcome(session: &ClusterSession, item: &Item, error: &str) -> State {
    State::Unknown(format!(
        "{} · {} · {}: the resource may have been saved. Inspect this exact target and reconcile its current state before preparing another operation. Do not blindly retry.\n{error}",
        session.id().display_name(),
        session.server(),
        item.identity,
    ))
}
pub async fn prepare(
    session: Arc<ClusterSession>,
    source: Source,
    namespace: String,
) -> Result<Bundle, String> {
    if namespace.trim().is_empty() {
        return Err("Choose an explicit default namespace.".into());
    }
    let documents = render::load(&source, &namespace).await?;
    if documents.is_empty() {
        return Err("No resource documents found in the selected local input.".into());
    }
    let mut items = Vec::new();
    let mut identities = HashMap::new();
    for document in documents {
        let mut item = Item {
            location: document.location,
            identity: String::new(),
            desired: document.value,
            before: None,
            kind: None,
            patch: None,
            state: State::Prepared,
        };
        let api_version = item.desired["apiVersion"]
            .as_str()
            .ok_or_else(|| format!("{}: apiVersion is required.", item.location))?;
        let kind_name = item.desired["kind"]
            .as_str()
            .ok_or_else(|| format!("{}: kind is required.", item.location))?;
        let kind = session
            .discovery()
            .kinds()
            .iter()
            .find(|kind| {
                kind.resource.api_version == api_version && kind.resource.kind == kind_name
            })
            .cloned();
        let name = item
            .desired
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "{}: metadata.name is required for an immutable resource identity.",
                    item.location
                )
            })?
            .to_string();
        let Some(kind) = kind else {
            item.identity = format!("{api_version} {kind_name} {name}");
            item.state = State::Failed(format!(
                "{}: discovery does not resolve {api_version} {kind_name}; scope and endpoint are unknown. Refresh discovery before preparing again.",
                item.location
            ));
            items.push(item);
            continue;
        };
        if kind.namespaced && item.desired.pointer("/metadata/namespace").is_none() {
            item.desired["metadata"]["namespace"] = Value::String(namespace.clone());
        }
        ops::prepare_create(&kind, item.desired.clone())
            .map_err(|error| format!("{}: {}", item.location, error.user_message()))?;
        let object_namespace = item
            .desired
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            .map(str::to_owned);
        item.identity = format!(
            "{} {}/{}",
            kind.display_name(),
            object_namespace.as_deref().unwrap_or("<cluster>"),
            name
        );
        // Versions of the same group/plural address the same Kubernetes identity.
        let key = (
            kind.resource.group.clone(),
            kind.resource.plural.clone(),
            object_namespace.clone(),
            name.clone(),
        );
        if let Some(first) = identities.insert(key, item.location.clone()) {
            return Err(format!(
                "{}: duplicate resolved identity {}; first defined at {first}.",
                item.location, item.identity
            ));
        }
        match session
            .clone()
            .get_object(kind.resource.clone(), object_namespace, name)
            .await
        {
            Ok(current) => {
                let current = serde_json::to_value(current).map_err(|error| error.to_string())?;
                let mut draft = item.desired.clone();
                draft["metadata"]["uid"] = current["metadata"]["uid"].clone();
                draft["metadata"]["resourceVersion"] =
                    current["metadata"]["resourceVersion"].clone();
                match edit::Plan::review(&current, draft, current.clone(), false) {
                    Ok(plan) => {
                        if plan.before == plan.after {
                            item.state = State::Unchanged;
                        }
                        item.desired = plan.after.clone();
                        item.before = Some(plan.before.clone());
                        item.patch = Some(plan);
                    }
                    Err(error) => item.state = State::Failed(error.user_message()),
                }
            }
            Err(crate::Error::Api(kube::Error::Api(status))) if status.code == 404 => {}
            Err(error) => {
                item.state = State::Failed(format!(
                    "Could not read current resource: {}",
                    error.user_message()
                ))
            }
        }
        item.kind = Some(kind);
        items.push(item);
    }
    Ok(Bundle {
        session,
        namespace,
        items,
    })
}
