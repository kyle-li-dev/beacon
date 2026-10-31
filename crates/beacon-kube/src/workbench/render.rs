//! Fixed offline renderer modes over a bounded, symlink-free input snapshot.
use super::{Document, Source};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt as _;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 1000;
fn read(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_BYTES as u64
    {
        return Err(format!(
            "{}: choose a regular local file of at most 16 MiB; symlinks are refused.",
            path.display()
        ));
    }
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}
fn files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_owned()];
    let mut files = vec![];
    let mut count = 0;
    let mut bytes = 0;
    while let Some(path) = pending.pop() {
        count += 1;
        if count > MAX_FILES {
            return Err(
                "Local input exceeds 1000 files or directories. Choose a smaller directory.".into(),
            );
        }
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{}: symlinks are refused in local inputs.",
                path.display()
            ));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|e| format!("{}: {e}", path.display()))? {
                pending.push(entry.map_err(|e| e.to_string())?.path());
            }
        } else if metadata.is_file() {
            bytes += metadata.len();
            if bytes > MAX_BYTES as u64 {
                return Err("Local input exceeds the 16 MiB snapshot limit.".into());
            }
            files.push(path);
        } else {
            return Err(format!(
                "{}: only regular files and directories are supported.",
                path.display()
            ));
        }
    }
    files.sort();
    Ok(files)
}
fn parse(text: &str, path: &Path) -> Result<Vec<Document>, String> {
    // Let the YAML parser interpret the complete stream, including quoted and
    // block scalars. Marker lines are used only to label physical documents.
    let mut starts = vec![1];
    let mut content = false;
    for (index, line) in text.lines().enumerate() {
        let marker = line
            .strip_prefix("---")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        if marker {
            if content {
                starts.push(index + 1);
            }
            content = true;
        } else if !line.trim().is_empty() && !line.trim_start().starts_with(['#', '%']) {
            content = true;
        }
    }
    let document_at = |line: usize| starts.partition_point(|start| *start <= line).max(1);
    let values: Vec<serde_saphyr::Spanned<Value>> =
        serde_saphyr::from_multiple(text).map_err(|error| {
            let location = error.location();
            let line = location.as_ref().map_or(1, |loc| loc.line() as usize);
            let column = location.as_ref().map_or(1, |loc| loc.column());
            format!(
                "{} · document {} · line {line}, column {column}: {error}",
                path.display(),
                document_at(line)
            )
        })?;
    values
        .into_iter()
        .map(|spanned| {
            let line = spanned.referenced.line() as usize;
            let column = spanned.referenced.column();
            let location = format!(
                "{} · document {} · line {line}, column {column}",
                path.display(),
                document_at(line)
            );
            if !spanned.value.is_object() {
                return Err(format!(
                    "{location}: expected one Kubernetes resource object."
                ));
            }
            Ok(Document {
                location,
                value: spanned.value,
            })
        })
        .collect()
}
fn snapshot(root: &Path, destination: &Path) -> Result<(), String> {
    if !fs::symlink_metadata(root)
        .map_err(|e| format!("{}: {e}", root.display()))?
        .is_dir()
    {
        return Err(format!("{}: choose a local directory.", root.display()));
    }
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    for source in files(root)? {
        let path = destination.join(source.strip_prefix(root).map_err(|e| e.to_string())?);
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        // Read once, then render only the private copy. Never validate one tree and execute another.
        fs::write(
            &path,
            fs::read(&source).map_err(|e| format!("{}: {e}", source.display()))?,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn local_path(base: &Path, reference: &str, root: &Path) -> Result<(), String> {
    // Reject URL syntax before filesystem lookup. A directory literally named
    // "https:" must not disguise an HTTP reference accepted by Kustomize.
    let host_like = reference.split_once('/').is_some_and(|(first, _)| {
        first != "." && first != ".." && (first.contains('.') || first.contains('@'))
    });
    if reference.contains([':', '?', '#']) || host_like {
        return Err(format!(
            "{}: remote reference {reference:?} is disabled. Use an explicit ./ path for a local directory containing a dot.",
            base.display()
        ));
    }
    let path = base.join(reference);
    let resolved = path.canonicalize().map_err(|e| {
        format!(
            "{}: local input {reference:?} is unavailable: {e}",
            base.display()
        )
    })?;
    if Path::new(reference).is_absolute() || !resolved.starts_with(root) {
        return Err(format!(
            "{}: {reference:?} must stay inside the selected local directory. Remote inputs and external bases are refused.",
            base.display()
        ));
    }
    Ok(())
}
fn validate_kustomize(root: &Path) -> Result<(), String> {
    for path in files(root)? {
        if path
            .extension()
            .is_some_and(|extension| matches!(extension.to_str(), Some("yaml" | "yml" | "json")))
            && read(&path)?.contains("config.kubernetes.io/function")
        {
            return Err(format!(
                "{}: Kustomize exec/container functions are disabled in local-only rendering.",
                path.display()
            ));
        }
        if !matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("kustomization.yaml" | "kustomization.yml" | "Kustomization")
        ) {
            continue;
        }
        let text = read(&path)?;
        let config: Value =
            serde_saphyr::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let fields = config
            .as_object()
            .ok_or_else(|| format!("{}: expected a kustomization mapping", path.display()))?;
        let base = path.parent().unwrap();
        for (key, value) in fields {
            if !matches!(
                key.as_str(),
                "apiVersion"
                    | "kind"
                    | "resources"
                    | "bases"
                    | "components"
                    | "namespace"
                    | "namePrefix"
                    | "nameSuffix"
                    | "labels"
                    | "commonLabels"
                    | "commonAnnotations"
                    | "images"
                    | "replicas"
                    | "patches"
                    | "patchesStrategicMerge"
                    | "patchesJson6902"
                    | "configMapGenerator"
                    | "secretGenerator"
                    | "generatorOptions"
                    | "replacements"
                    | "sortOptions"
                    | "buildMetadata"
                    | "crds"
                    | "configurations"
            ) {
                return Err(format!(
                    "{}: {key} is unsupported in local-only rendering. Helm downloads, plugins, functions and custom OpenAPI sources are disabled.",
                    path.display()
                ));
            }
            if matches!(
                key.as_str(),
                "resources" | "bases" | "components" | "crds" | "configurations"
            ) {
                let paths = value.as_array().ok_or_else(|| {
                    format!("{}: {key} must be a list of local paths", path.display())
                })?;
                for reference in paths {
                    local_path(
                        base,
                        reference
                            .as_str()
                            .ok_or("Local resource path must be a string")?,
                        root,
                    )?;
                }
            }
            if matches!(key.as_str(), "patchesStrategicMerge") {
                for patch in value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if !patch.contains('\n') {
                        local_path(base, patch, root)?;
                    }
                }
            }
        }
        fn references(value: &Value, base: &Path, root: &Path) -> Result<(), String> {
            match value {
                Value::Object(fields) => {
                    for (key, value) in fields {
                        if key == "path" || key == "env" {
                            local_path(
                                base,
                                value.as_str().ok_or("Local path must be a string")?,
                                root,
                            )?;
                        } else if key == "files" || key == "envs" {
                            for value in value.as_array().into_iter().flatten() {
                                let value =
                                    value.as_str().ok_or("Local file path must be a string")?;
                                local_path(
                                    base,
                                    value.split_once('=').map_or(value, |(_, p)| p),
                                    root,
                                )?;
                            }
                        } else {
                            references(value, base, root)?;
                        }
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        references(value, base, root)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        // Only path-bearing operations; JSON Patch's /data/key is not a file path.
        for key in [
            "patches",
            "patchesJson6902",
            "configMapGenerator",
            "secretGenerator",
            "replacements",
        ] {
            if let Some(value) = config.get(key) {
                references(value, base, root)?;
            }
        }
    }
    Ok(())
}
fn validate_helm(root: &Path) -> Result<(), String> {
    for path in files(root)? {
        if path.extension().is_some_and(|e| e == "tgz" || e == "gz") {
            return Err(format!(
                "{}: unpack local chart dependencies before rendering; archives are refused.",
                path.display()
            ));
        }
        if path.file_name().is_some_and(|name| name == "Chart.yaml") {
            let chart: Value = serde_saphyr::from_str(&read(&path)?)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if chart
                .get("dependencies")
                .and_then(Value::as_array)
                .is_some_and(|d| !d.is_empty())
            {
                return Err(format!(
                    "{}: dependency resolution is disabled. Use a self-contained local chart.",
                    path.display()
                ));
            }
        }
        if path
            .file_name()
            .is_some_and(|name| name == "values.schema.json")
        {
            // Helm's schema resolver can fetch $ref URLs even in template mode.
            // Disable references altogether rather than granting it file/network capabilities.
            fn has_ref(value: &Value) -> bool {
                match value {
                    Value::Object(fields) => {
                        fields.contains_key("$ref")
                            || fields.contains_key("$schema")
                            || fields.values().any(has_ref)
                    }
                    Value::Array(values) => values.iter().any(has_ref),
                    _ => false,
                }
            }
            let schema: Value = serde_json::from_str(&read(&path)?)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if has_ref(&schema) {
                return Err(format!(
                    "{}: schema $ref and $schema resolution is disabled in local-only rendering. Inline the schema and omit its external dialect URI.",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}
fn program(name: &str) -> Result<PathBuf, String> {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    };
    for directory in std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
    {
        let path = directory.join(&executable);
        if path.is_file() {
            return path.canonicalize().map_err(|e| e.to_string());
        }
    }
    Err(format!(
        "Install {name} locally and make it available on Beacon's PATH. No tool downloads are performed."
    ))
}
async fn output(
    stream: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    stream
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(std::io::Error::other(
            "Renderer output exceeds the supported size.",
        ));
    }
    Ok(bytes)
}
async fn run(program: PathBuf, args: Vec<String>, private: &Path) -> Result<String, String> {
    let empty = private.join("empty-kubeconfig");
    fs::write(
        &empty,
        "apiVersion: v1\nkind: Config\nclusters: []\ncontexts: []\nusers: []\n",
    )
    .map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(private)
        .env_clear()
        .env("HOME", private)
        .env("USERPROFILE", private)
        .env("TMPDIR", private)
        .env("TMP", private)
        .env("TEMP", private)
        .env("KUBECONFIG", empty)
        .env("PATH", private)
        .env("HELM_CONFIG_HOME", private)
        .env("HELM_CACHE_HOME", private)
        .env("HELM_DATA_HOME", private)
        .env("HELM_NO_PLUGINS", "1")
        // Defense in depth for the tools' HTTP transports. The validated input
        // graph and fixed client modes exclude network-backed rendering features.
        // Never inherit a proxy URL or credentials from the user's environment.
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|e| format!("Local renderer could not start: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let (out, err) = tokio::try_join!(output(stdout, MAX_BYTES), output(stderr, 256 * 1024))?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    })
    .await;
    match result {
        Ok(Ok((status, out, _))) if status.success() => {
            String::from_utf8(out).map_err(|e| e.to_string())
        }
        Ok(Ok((status, _, err))) => Err(format!(
            "Local renderer failed ({status}): {}",
            String::from_utf8_lossy(&err).trim()
        )),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(format!("Local renderer failed: {error}"))
        }
        Err(_) => {
            let _ = child.kill().await;
            Err("Local renderer timed out after 60 seconds. No resources were written.".into())
        }
    }
}
enum Input {
    Documents(Vec<Document>),
    Renderer {
        private: tempfile::TempDir,
        program: PathBuf,
        args: Vec<String>,
        source: PathBuf,
    },
}
fn prepare_input(source: Source, namespace: String) -> Result<Input, String> {
    match source {
        Source::Yaml(path) => {
            let paths = if path.is_dir() {
                files(&path)?
                    .into_iter()
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|ext| ext == "yaml" || ext == "yml")
                    })
                    .collect()
            } else {
                vec![path]
            };
            let mut documents = vec![];
            for path in paths {
                documents.extend(parse(&read(&path)?, &path)?);
            }
            Ok(Input::Documents(documents))
        }
        Source::Helm {
            chart,
            values,
            release,
        } => {
            if release.is_empty()
                || release.starts_with('-')
                || !release
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Err(
                    "Choose a Helm release name using lowercase letters, numbers and hyphens."
                        .into(),
                );
            }
            let private = tempfile::tempdir().map_err(|error| error.to_string())?;
            let copied = private.path().join("chart");
            snapshot(&chart, &copied)?;
            validate_helm(&copied).map_err(|error| {
                error.replace(&*copied.to_string_lossy(), &chart.display().to_string())
            })?;
            let mut args = vec![
                "template".into(),
                release,
                copied.display().to_string(),
                "--namespace".into(),
                namespace,
                "--include-crds".into(),
                "--dry-run=client".into(),
            ];
            for (index, path) in values.iter().enumerate() {
                let target = private.path().join(format!("values-{index}.yaml"));
                fs::write(&target, read(path)?).map_err(|error| error.to_string())?;
                args.extend(["--values".into(), target.display().to_string()]);
            }
            Ok(Input::Renderer {
                private,
                program: program("helm")?,
                args,
                source: chart,
            })
        }
        Source::Kustomize(source) => {
            let private = tempfile::tempdir().map_err(|error| error.to_string())?;
            let copied = private.path().join("overlay");
            snapshot(&source, &copied)?;
            let copied = copied.canonicalize().map_err(|error| error.to_string())?;
            validate_kustomize(&copied).map_err(|error| {
                error.replace(&*copied.to_string_lossy(), &source.display().to_string())
            })?;
            let declared = std::env::var("TRESTLE_KUSTOMIZE_ARGV")
                .ok()
                .map(|text| {
                    serde_json::from_str::<Vec<String>>(&text)
                        .map_err(|error| format!("Invalid local Kustomize command: {error}"))
                })
                .transpose()?;
            let (executable, mut args) = match declared.as_deref() {
                Some([binary, subcommand]) if Path::new(binary).file_name().is_some_and(|name| name == "kubectl" || name == "kubectl.exe") && subcommand == "kustomize" =>
                    (PathBuf::from(binary), vec!["kustomize".into()]),
                Some([binary]) if Path::new(binary).file_name().is_some_and(|name| name == "kustomize" || name == "kustomize.exe") =>
                    (PathBuf::from(binary), vec![]),
                Some(_) => return Err("Local Kustomize command must be kubectl kustomize or standalone kustomize, without extra flags.".into()),
                None => (program("kubectl")?, vec!["kustomize".into()]),
            };
            let executable = if executable.is_absolute() {
                executable
            } else {
                program(
                    executable
                        .to_str()
                        .ok_or("Renderer executable must be a local path")?,
                )?
            };
            args.extend([
                copied.display().to_string(),
                "--load-restrictor=LoadRestrictionsRootOnly".into(),
            ]);
            Ok(Input::Renderer {
                private,
                program: executable,
                args,
                source,
            })
        }
    }
}
pub(super) async fn load(source: &Source, namespace: &str) -> Result<Vec<Document>, String> {
    let source = source.clone();
    let namespace = namespace.to_owned();
    // Filesystem traversal, parsing and snapshot creation stay off the GPUI thread.
    let prepared = tokio::task::spawn_blocking(move || prepare_input(source, namespace))
        .await
        .map_err(|error| error.to_string())??;
    match prepared {
        Input::Documents(documents) => Ok(documents),
        Input::Renderer {
            private,
            program,
            args,
            source,
        } => {
            let text = run(program, args, private.path())
                .await
                .map_err(|error| format!("{}: {error}", source.display()))?;
            parse(&text, &source)
        }
    }
}
