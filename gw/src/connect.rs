//! Install, connect and uninstall agents (pi, opencode, Claude Code, codex).
//!
//! `connect` writes only a base URL and a key, keeps a timestamped backup next
//! to each file, and records the replaced values in `<state_dir>/connect.json`
//! so `disconnect` can put back exactly those values and nothing else.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use toml_edit::{DocumentMut, Item, Table};

/// Agents `gw connect` understands.
pub const AGENTS: [&str; 4] = ["pi", "opencode", "claude", "codex"];

/// `$HOME`, where the agent configs live.
pub fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

/// Base URL clients should use for a listen address.
pub fn default_url(listen: std::net::SocketAddr) -> String {
    format!("http://{listen}/v1")
}

/// What `gw connect` writes.
#[derive(Clone)]
pub struct Connect {
    /// Base URL clients should use, e.g. `http://127.0.0.1:10204/v1`.
    pub url: String,
    /// Gateway key. `None` is only useful for codex, which reads it from the environment.
    pub key: Option<String>,
    pub dry_run: bool,
    pub state_dir: PathBuf,
    /// Model ids to list in agents that need an explicit list (pi, opencode).
    pub models: Vec<String>,
}

/// Agents `gw connect` has configured, per the manifest.
pub fn connected(state_dir: &Path) -> anyhow::Result<Vec<String>> {
    Ok(Manifest::load(state_dir)?.agents.into_keys().collect())
}

/// Agents whose config directory exists under `home`.
pub fn detect(home: &Path) -> Vec<&'static str> {
    let dirs = [
        ("pi", ".pi/agent"),
        ("opencode", ".config/opencode"),
        ("claude", ".claude"),
        ("codex", ".codex"),
    ];
    dirs.into_iter()
        .filter(|(_, dir)| home.join(dir).is_dir())
        .map(|(agent, _)| agent)
        .collect()
}

/// Connect one agent and report every change.
pub fn connect(agent: &str, opts: &Connect, home: &Path) -> anyhow::Result<Vec<String>> {
    let agent = Agent::parse(agent)?;
    let sets = agent.files(opts, home)?;
    let mut lines = apply(agent.name(), &sets, &opts.state_dir, home, opts.dry_run)?;
    if agent == Agent::Codex {
        // The key itself never goes to stdout: it would end up in shell history.
        lines.push("codex: export GW_API_KEY=$(cat <key-file>)".into());
    }
    Ok(lines)
}

/// Restore the values `connect` replaced for one agent.
pub fn disconnect(agent: &str, home: &Path, state_dir: &Path) -> anyhow::Result<Vec<String>> {
    let agent = Agent::parse(agent)?;
    let name = agent.name();
    let mut manifest = Manifest::load(state_dir)?;
    let files = manifest.agents.get(name).cloned().unwrap_or_default();
    if files.is_empty() {
        return Ok(vec![format!("{name}: not connected")]);
    }
    let mut lines = Vec::new();
    let mut left = BTreeMap::new();
    for (file, records) in &files {
        let file = PathBuf::from(file);
        if !file.exists() {
            lines.push(format!(
                "{name}: {}: missing, nothing to restore",
                rel(&file, home)
            ));
            continue;
        }
        let kind = kind_of(&file);
        let mut doc = read_doc(&file, kind)?;
        if doc.comments_lost() {
            lines.push(format!(
                "{name}: {}: has comments, left alone; restore by hand:",
                rel(&file, home)
            ));
            for record in records {
                lines.push(format!(
                    "  {} = {}",
                    record.key,
                    show(&record.key, record.old.as_ref())
                ));
            }
            left.insert(file.to_string_lossy().into_owned(), records.clone());
            continue;
        }
        let mut changed = Vec::new();
        let mut drifted = Vec::new();
        for record in records {
            // Only touch values that are still ours; a re-pointed client stays re-pointed.
            if doc.get(&record.key)?.as_ref() != Some(&record.set) {
                drifted.push(record.clone());
                continue;
            }
            match &record.old {
                Some(old) => {
                    if doc.get(&record.key)?.as_ref() != Some(old) {
                        doc.set(&record.key, old.clone())?;
                        changed.push(record.key.clone());
                    }
                }
                None => {
                    doc.remove(&record.key);
                    // Containers connect created go away with their last key.
                    for path in record.created.iter().rev() {
                        doc.prune(path);
                    }
                    changed.push(record.key.clone());
                }
            }
        }
        for record in &drifted {
            lines.push(format!(
                "{name}: {}: {}: left alone (drifted)",
                rel(&file, home),
                record.key
            ));
        }
        if !drifted.is_empty() {
            left.insert(file.to_string_lossy().into_owned(), drifted);
        }
        if changed.is_empty() {
            lines.push(format!("{name}: {}: unchanged", rel(&file, home)));
            continue;
        }
        let backup = backup(&file)?;
        write_atomic(&file, doc.render()?.as_bytes())?;
        lines.push(format!(
            "{name}: {}: restored {}",
            rel(&file, home),
            changed.join(", ")
        ));
        lines.push(format!("{name}: backup: {}", rel(&backup, home)));
    }
    if left.is_empty() {
        manifest.agents.remove(name);
    } else {
        manifest.agents.insert(name.to_string(), left);
    }
    manifest.save(state_dir)?;
    Ok(lines)
}

/// Report which agents are connected and whether their files still match.
pub fn status(home: &Path, state_dir: &Path) -> anyhow::Result<Vec<String>> {
    let manifest = Manifest::load(state_dir)?;
    if manifest.agents.is_empty() {
        return Ok(vec!["no agents connected".into()]);
    }
    let mut lines = Vec::new();
    for (agent, files) in &manifest.agents {
        let mut drifted = Vec::new();
        let mut total = 0;
        for (file, records) in files {
            let file = PathBuf::from(file);
            let kind = kind_of(&file);
            for record in records {
                total += 1;
                let current = if file.exists() {
                    read_doc(&file, kind)?.get(&record.key)?
                } else {
                    None
                };
                if current.as_ref() != Some(&record.set) {
                    drifted.push(format!(
                        "  {}: {} = {} [drifted]",
                        rel(&file, home),
                        record.key,
                        show(&record.key, current.as_ref())
                    ));
                }
            }
        }
        if drifted.is_empty() {
            lines.push(format!("{agent}: connected, {total} settings in sync"));
        } else {
            lines.push(format!(
                "{agent}: connected, {} of {total} settings drifted",
                drifted.len()
            ));
            lines.extend(drifted);
        }
        if agent == "pi" {
            if let Some(line) = client_cache_tools(home) {
                lines.push(format!("{agent}: {line}"));
            }
        }
    }
    Ok(lines)
}

/// Client-side cache rewrites stay installed; gw does not touch them.
fn client_cache_tools(home: &Path) -> Option<String> {
    let settings = fs::read_to_string(home.join(".pi/agent/settings.json")).unwrap_or_default();
    let mut tools = Vec::new();
    if settings.contains("pi-cache-optimizer") {
        tools.push("pi-cache-optimizer (~/.pi/agent/settings.json packages)");
    }
    if home
        .join(".pi/agent/extensions/opencodex-anthropic.ts")
        .exists()
    {
        tools.push("opencodex-anthropic.ts (~/.pi/agent/extensions)");
    }
    (!tools.is_empty())
        .then(|| format!("client-side cache tools still active: {}", tools.join(", ")))
}

#[derive(Clone, Copy, PartialEq)]
enum Agent {
    Pi,
    Opencode,
    Claude,
    Codex,
}

impl Agent {
    fn parse(agent: &str) -> anyhow::Result<Self> {
        match agent {
            "pi" => Ok(Agent::Pi),
            "opencode" => Ok(Agent::Opencode),
            "claude" => Ok(Agent::Claude),
            "codex" => Ok(Agent::Codex),
            other => Err(anyhow::anyhow!(
                "unknown agent {other:?}, expected one of {}",
                AGENTS.join(", ")
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Agent::Pi => "pi",
            Agent::Opencode => "opencode",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    fn files(self, opts: &Connect, home: &Path) -> anyhow::Result<Vec<WriteSet>> {
        let pi_models = home.join(".pi/agent/models.json");
        match self {
            Agent::Pi => Ok(vec![
                WriteSet {
                    file: pi_models,
                    kind: Kind::Json,
                    changes: vec![
                        (
                            "providers.gateway.baseUrl".into(),
                            Scalar::Str(opts.url.clone()),
                        ),
                        ("providers.gateway.apiKey".into(), Scalar::Str(key(opts))),
                        (
                            "providers.gateway.api".into(),
                            Scalar::Str("openai-completions".into()),
                        ),
                    ]
                    .into_iter()
                    .chain(model_list(
                        "providers.gateway.models",
                        &opts.models,
                        |id| serde_json::json!({ "id": id }),
                    ))
                    .collect(),
                },
                WriteSet {
                    file: home.join(".pi/agent/settings.json"),
                    kind: Kind::Json,
                    // Gateway-side warming replaces pi's own, which would double bill.
                    changes: vec![("cacheWarming".into(), Scalar::Str("off".into()))],
                },
            ]),
            Agent::Opencode => {
                let dir = home.join(".config/opencode");
                let file = ["opencode.json", "opencode.jsonc"]
                    .iter()
                    .map(|name| dir.join(name))
                    .find(|path| path.exists())
                    .unwrap_or_else(|| dir.join("opencode.json"));
                Ok(vec![WriteSet {
                    file,
                    kind: Kind::Json,
                    changes: vec![
                        (
                            "provider.gw.npm".into(),
                            Scalar::Str("@ai-sdk/openai-compatible".into()),
                        ),
                        (
                            "provider.gw.options.baseURL".into(),
                            Scalar::Str(opts.url.clone()),
                        ),
                        ("provider.gw.options.apiKey".into(), Scalar::Str(key(opts))),
                    ]
                    .into_iter()
                    .chain(model_list(
                        "provider.gw.models",
                        &opts.models,
                        |id| serde_json::json!({ "name": id }),
                    ))
                    .collect(),
                }])
            }
            Agent::Claude => Ok(vec![WriteSet {
                file: home.join(".claude/settings.json"),
                kind: Kind::Json,
                changes: vec![
                    (
                        "env.ANTHROPIC_BASE_URL".into(),
                        Scalar::Str(anthropic_base(&opts.url)),
                    ),
                    ("env.ANTHROPIC_AUTH_TOKEN".into(), Scalar::Str(key(opts))),
                ],
            }]),
            Agent::Codex => Ok(vec![WriteSet {
                file: home.join(".codex/config.toml"),
                kind: Kind::Toml,
                // The key stays in the environment so it never lands in the config file.
                changes: vec![
                    ("model_provider".into(), Scalar::Str("gw".into())),
                    ("model_providers.gw.name".into(), Scalar::Str("gw".into())),
                    (
                        "model_providers.gw.base_url".into(),
                        Scalar::Str(opts.url.clone()),
                    ),
                    (
                        "model_providers.gw.env_key".into(),
                        Scalar::Str("GW_API_KEY".into()),
                    ),
                    (
                        "model_providers.gw.wire_api".into(),
                        Scalar::Str("responses".into()),
                    ),
                ],
            }]),
        }
    }
}

/// The `models` change for an agent; none when the gateway lists no models.
fn model_list(
    path: &str,
    ids: &[String],
    entry: fn(&str) -> serde_json::Value,
) -> Option<(String, Scalar)> {
    if ids.is_empty() {
        return None;
    }
    let value = if path.starts_with("providers.") {
        // pi takes an array of entries.
        serde_json::Value::Array(ids.iter().map(|id| entry(id)).collect())
    } else {
        // opencode takes an object keyed by model id.
        serde_json::Value::Object(ids.iter().map(|id| (id.clone(), entry(id))).collect())
    };
    Some((path.into(), Scalar::Json(value)))
}

/// A gateway with no `api_key_file` is loopback-only and checks no key, but
/// agents still insist on a non-empty one.
fn key(opts: &Connect) -> String {
    opts.key.clone().unwrap_or_else(|| "gw".into())
}

fn kind_of(file: &Path) -> Kind {
    if file.extension().and_then(|e| e.to_str()) == Some("toml") {
        Kind::Toml
    } else {
        Kind::Json
    }
}

/// Anthropic clients want the root URL, so drop a single `/v1`.
fn anthropic_base(url: &str) -> String {
    let url = url.strip_suffix('/').unwrap_or(url);
    url.strip_suffix("/v1").unwrap_or(url).to_string()
}

struct WriteSet {
    file: PathBuf,
    kind: Kind,
    changes: Vec<(String, Scalar)>,
}

fn apply(
    agent: &str,
    sets: &[WriteSet],
    state_dir: &Path,
    home: &Path,
    dry_run: bool,
) -> anyhow::Result<Vec<String>> {
    let mut manifest = Manifest::load(state_dir)?;
    let mut lines = Vec::new();
    for set in sets {
        let existed = set.file.exists();
        let mut doc = if existed {
            read_doc(&set.file, set.kind)?
        } else {
            Doc::empty(set.kind)
        };
        let records = manifest.records(agent, &set.file);
        let mut next = Vec::new();
        let mut pending = Vec::new();
        for (path, value) in &set.changes {
            let current = doc.get(path)?;
            let (old, created) = match records.iter().find(|r| &r.key == path) {
                // Keep what the first connect replaced, not what we last wrote.
                Some(previous) => (previous.old.clone(), previous.created.clone()),
                None => (current.clone(), doc.missing_ancestors(path)),
            };
            next.push(Saved {
                key: path.clone(),
                old,
                set: value.clone(),
                created,
            });
            if current.as_ref() != Some(value) {
                pending.push((path.clone(), value.clone(), current));
            }
        }
        if pending.is_empty() {
            lines.push(format!("{agent}: {}: unchanged", rel(&set.file, home)));
            if !dry_run {
                manifest.set_records(agent, &set.file, next);
                manifest.save(state_dir)?;
            }
            continue;
        }
        if doc.comments_lost() {
            // Rewriting would drop the user's comments, so show the edit instead.
            lines.push(format!(
                "{agent}: {}: has comments, not written; add by hand:",
                rel(&set.file, home)
            ));
            for (path, value, _) in &pending {
                lines.push(format!("  {path} = {}", show(path, Some(value))));
            }
            if pending.iter().any(|(path, ..)| is_secret(path)) {
                lines.push("  (keys are not printed; read them from the key file)".into());
            }
            continue;
        }
        for (path, value, current) in &pending {
            lines.push(format!(
                "{agent}: {}{}: {path}: {} -> {}",
                rel(&set.file, home),
                if dry_run { " (dry run)" } else { "" },
                show(path, current.as_ref()),
                show(path, Some(value))
            ));
        }
        if dry_run {
            continue;
        }
        for (path, value, _) in &pending {
            doc.set(path, value.clone())?;
        }
        let backup = existed.then(|| backup(&set.file)).transpose()?;
        write_atomic(&set.file, doc.render()?.as_bytes())?;
        if let Some(backup) = backup {
            lines.push(format!("{agent}: backup: {}", rel(&backup, home)));
        }
        // Record this file now so a later failure cannot lose what was written.
        manifest.set_records(agent, &set.file, next);
        manifest.save(state_dir)?;
    }
    Ok(lines)
}

fn rel(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Bool(bool),
    Int(i64),
    Str(String),
    /// An array or object, e.g. a model list.
    Json(serde_json::Value),
}

impl Scalar {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Scalar::Bool(v) => (*v).into(),
            Scalar::Int(v) => (*v).into(),
            Scalar::Str(v) => v.as_str().into(),
            Scalar::Json(v) => v.clone(),
        }
    }

    fn to_toml(&self) -> toml_edit::Value {
        match self {
            Scalar::Bool(v) => (*v).into(),
            Scalar::Int(v) => (*v).into(),
            Scalar::Str(v) => v.as_str().into(),
            Scalar::Json(v) => v.to_string().into(),
        }
    }
}

fn is_secret(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("apikey") || lower.ends_with("auth_token")
}

/// Print form; values whose key looks secret are masked.
fn show(path: &str, value: Option<&Scalar>) -> String {
    match value {
        None => "(unset)".into(),
        Some(_) if is_secret(path) => "***".into(),
        Some(Scalar::Bool(v)) => v.to_string(),
        Some(Scalar::Int(v)) => v.to_string(),
        Some(Scalar::Str(v)) => v.clone(),
        Some(Scalar::Json(v)) => format!(
            "{} entries",
            v.as_array()
                .map_or_else(|| v.as_object().map_or(0, |o| o.len()), Vec::len)
        ),
    }
}

fn scalar_from_json(value: &serde_json::Value) -> anyhow::Result<Option<Scalar>> {
    Ok(match value {
        serde_json::Value::Bool(v) => Some(Scalar::Bool(*v)),
        serde_json::Value::Number(v) => v.as_i64().map(Scalar::Int),
        serde_json::Value::String(v) => Some(Scalar::Str(v.clone())),
        serde_json::Value::Null => None,
        other => Some(Scalar::Json(other.clone())),
    })
}

fn scalar_from_toml(item: &Item) -> anyhow::Result<Option<Scalar>> {
    if item.is_none() {
        return Ok(None);
    }
    if let Some(v) = item.as_str() {
        return Ok(Some(Scalar::Str(v.into())));
    }
    if let Some(v) = item.as_bool() {
        return Ok(Some(Scalar::Bool(v)));
    }
    if let Some(v) = item.as_integer() {
        return Ok(Some(Scalar::Int(v)));
    }
    anyhow::bail!("refusing to touch a non-scalar value")
}

#[derive(Clone, Copy)]
enum Kind {
    Json,
    Toml,
}

/// A config file held as a nested table, addressed by dotted keys.
enum Doc {
    Json(serde_json::Value),
    /// JSON that carried comments and must not be written back.
    Jsonc(serde_json::Value),
    /// Kept as text so comments and formatting survive the edit.
    Toml(DocumentMut),
}

impl Doc {
    fn empty(kind: Kind) -> Self {
        match kind {
            Kind::Json => Doc::Json(serde_json::Value::Object(Default::default())),
            Kind::Toml => Doc::Toml(DocumentMut::new()),
        }
    }

    fn parse(kind: Kind, text: &str) -> anyhow::Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::empty(kind));
        }
        Ok(match kind {
            Kind::Json => {
                let (text, comments) = strip_comments(text);
                let value = serde_json::from_str(&text)?;
                if comments {
                    Doc::Jsonc(value)
                } else {
                    Doc::Json(value)
                }
            }
            Kind::Toml => Doc::Toml(text.parse::<DocumentMut>()?),
        })
    }

    fn comments_lost(&self) -> bool {
        matches!(self, Doc::Jsonc(_))
    }

    fn render(&self) -> anyhow::Result<String> {
        Ok(match self {
            Doc::Json(value) => format!("{}\n", serde_json::to_string_pretty(value)?),
            Doc::Jsonc(_) => anyhow::bail!("file has comments and cannot be rewritten"),
            Doc::Toml(doc) => doc.to_string(),
        })
    }

    fn get(&self, path: &str) -> anyhow::Result<Option<Scalar>> {
        let keys: Vec<&str> = path.split('.').collect();
        Ok(match self {
            Doc::Json(root) | Doc::Jsonc(root) => json_walk(root, &keys)
                .map(scalar_from_json)
                .transpose()?
                .flatten(),
            Doc::Toml(root) => toml_walk(root.as_item(), &keys)
                .map(scalar_from_toml)
                .transpose()?
                .flatten(),
        })
    }

    /// Dotted prefixes of `path` that do not exist yet, shallow to deep.
    fn missing_ancestors(&self, path: &str) -> Vec<String> {
        let keys: Vec<&str> = path.split('.').collect();
        (1..keys.len())
            .filter(|&depth| !self.is_table(&keys[..depth]))
            .map(|depth| keys[..depth].join("."))
            .collect()
    }

    fn is_table(&self, keys: &[&str]) -> bool {
        match self {
            Doc::Json(root) | Doc::Jsonc(root) => {
                json_walk(root, keys).is_some_and(|v| v.is_object())
            }
            Doc::Toml(root) => toml_walk(root.as_item(), keys).is_some_and(Item::is_table),
        }
    }

    fn set(&mut self, path: &str, value: Scalar) -> anyhow::Result<()> {
        let keys: Vec<&str> = path.split('.').collect();
        let (last, parents) = keys.split_last().expect("path is never empty");
        match self {
            Doc::Json(root) | Doc::Jsonc(root) => {
                let mut current = root;
                for key in parents {
                    current = json_child(current, key)?;
                }
                let Some(map) = current.as_object_mut() else {
                    anyhow::bail!("{path}: parent is not an object");
                };
                map.insert((*last).to_string(), value.to_json());
            }
            Doc::Toml(root) => {
                let mut table = root.as_table_mut();
                for key in parents {
                    let item = table
                        .entry(key)
                        .or_insert_with(|| Item::Table(Table::new()));
                    let Some(next) = item.as_table_mut() else {
                        anyhow::bail!("{path}: parent is not a table");
                    };
                    table = next;
                }
                table.insert(last, Item::Value(value.to_toml()));
            }
        }
        Ok(())
    }

    fn remove(&mut self, path: &str) {
        let keys: Vec<&str> = path.split('.').collect();
        let (last, parents) = keys.split_last().expect("path is never empty");
        match self {
            Doc::Json(root) | Doc::Jsonc(root) => {
                if let Some(serde_json::Value::Object(map)) = json_walk_mut(root, parents) {
                    map.remove(*last);
                }
            }
            Doc::Toml(root) => {
                if let Some(table) =
                    toml_walk_mut(root.as_item_mut(), parents).and_then(Item::as_table_mut)
                {
                    table.remove(last);
                }
            }
        }
    }

    /// Drop the table at `path` when it is empty.
    fn prune(&mut self, path: &str) {
        let keys: Vec<&str> = path.split('.').collect();
        let (last, parents) = keys.split_last().expect("path is never empty");
        match self {
            Doc::Json(root) | Doc::Jsonc(root) => {
                if let Some(serde_json::Value::Object(map)) = json_walk_mut(root, parents) {
                    let empty = map
                        .get(*last)
                        .is_some_and(|v| v.as_object().is_some_and(|inner| inner.is_empty()));
                    if empty {
                        map.remove(*last);
                    }
                }
            }
            Doc::Toml(root) => {
                if let Some(table) =
                    toml_walk_mut(root.as_item_mut(), parents).and_then(Item::as_table_mut)
                {
                    let empty = table
                        .get(last)
                        .and_then(Item::as_table)
                        .is_some_and(Table::is_empty);
                    if empty {
                        table.remove(last);
                    }
                }
            }
        }
    }
}

/// Strip `//` and `/* */` comments outside strings; reports whether any were found.
fn strip_comments(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut found = false;
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                found = true;
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push(c);
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                found = true;
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            _ => out.push(c),
        }
    }
    (out, found)
}

fn json_walk<'a>(mut value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    for key in keys {
        value = value.get(*key)?;
    }
    Some(value)
}

fn toml_walk<'a>(mut item: &'a Item, keys: &[&str]) -> Option<&'a Item> {
    for key in keys {
        item = item.get(*key)?;
    }
    Some(item)
}

fn json_walk_mut<'a>(
    mut value: &'a mut serde_json::Value,
    keys: &[&str],
) -> Option<&'a mut serde_json::Value> {
    for key in keys {
        value = value.get_mut(*key)?;
    }
    Some(value)
}

fn toml_walk_mut<'a>(mut item: &'a mut Item, keys: &[&str]) -> Option<&'a mut Item> {
    for key in keys {
        item = item.get_mut(*key)?;
    }
    Some(item)
}

fn json_child<'a>(
    value: &'a mut serde_json::Value,
    key: &str,
) -> anyhow::Result<&'a mut serde_json::Value> {
    let Some(map) = value.as_object_mut() else {
        anyhow::bail!("refusing to replace a non-object value with table {key:?}");
    };
    Ok(map
        .entry(key.to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default())))
}

fn read_doc(path: &Path, kind: Kind) -> anyhow::Result<Doc> {
    let text = fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Doc::parse(kind, &text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Copy `file` to `<file>.gw-bak-<nanos>` before it is rewritten; never overwrites.
fn backup(file: &Path) -> anyhow::Result<PathBuf> {
    let stamp = nanos();
    let name = file_name(file);
    for attempt in 0..100 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let backup = file.with_file_name(format!("{name}.gw-bak-{stamp}{suffix}"));
        let mut dst = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(dst) => dst,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        };
        io::copy(&mut fs::File::open(file)?, &mut dst)?;
        dst.set_permissions(fs::metadata(file)?.permissions())?;
        return Ok(backup);
    }
    anyhow::bail!("{}: cannot create a backup", file.display())
}

/// Write through a private tmp file so configs holding keys are never group/world readable.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!("{}.gw-tmp-{}", file_name(path), nanos()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    key: String,
    /// Value before the first connect; `None` means the key did not exist.
    #[serde(default)]
    old: Option<Scalar>,
    /// Value the last connect wrote.
    set: Scalar,
    /// Containers connect had to create, shallow to deep.
    #[serde(default)]
    created: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    agents: BTreeMap<String, BTreeMap<String, Vec<Saved>>>,
}

impl Manifest {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("connect.json")
    }

    fn load(state_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(state_dir);
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        let path = Self::path(state_dir);
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        // The manifest holds the old values it replaced, so keep it private.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_dir)?;
        write_atomic(&path, text.as_bytes())
    }

    fn records(&self, agent: &str, file: &Path) -> Vec<Saved> {
        let file = file.to_string_lossy().into_owned();
        self.agents
            .get(agent)
            .and_then(|files| files.get(&file))
            .cloned()
            .unwrap_or_default()
    }

    fn set_records(&mut self, agent: &str, file: &Path, records: Vec<Saved>) {
        self.agents
            .entry(agent.to_string())
            .or_default()
            .insert(file.to_string_lossy().into_owned(), records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "gw-connect-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("home")).unwrap();
            fs::create_dir_all(root.join("state")).unwrap();
            Sandbox { root }
        }

        fn home(&self) -> PathBuf {
            self.root.join("home")
        }

        fn state(&self) -> PathBuf {
            self.root.join("state")
        }

        fn write(&self, rel: &str, body: &str) {
            let path = self.home().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }

        fn read_json(&self, rel: &str) -> serde_json::Value {
            serde_json::from_str(&fs::read_to_string(self.home().join(rel)).unwrap()).unwrap()
        }

        fn read_toml(&self, rel: &str) -> toml::Value {
            toml::from_str(&fs::read_to_string(self.home().join(rel)).unwrap()).unwrap()
        }

        fn mode(&self, rel: &str) -> u32 {
            fs::metadata(self.home().join(rel))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        }

        fn backups(&self, rel: &str) -> usize {
            let dir = self.home().join(rel).parent().unwrap().to_path_buf();
            let stem = Path::new(rel)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{stem}.gw-bak-"))
                })
                .count()
        }

        fn opts(&self, url: &str, key: &str) -> Connect {
            Connect {
                url: url.into(),
                key: Some(key.into()),
                dry_run: false,
                state_dir: self.state(),
                models: Vec::new(),
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const URL: &str = "http://127.0.0.1:10204/v1";
    const KEY: &str = "sk-fake";

    fn pi_sandbox() -> Sandbox {
        let sandbox = Sandbox::new();
        sandbox.write(
            ".pi/agent/models.json",
            r#"{"providers":{"gateway":{"baseUrl":"https://old.example/v1","api":"openai-completions","apiKey":"sk-old","models":{"m":{"id":"m"}}}}}"#,
        );
        sandbox.write(
            ".pi/agent/settings.json",
            r#"{"theme":"dark","cacheWarming":"on"}"#,
        );
        sandbox
    }

    #[test]
    fn pi_connect_is_idempotent_and_keeps_models() {
        let sandbox = pi_sandbox();
        let opts = sandbox.opts(URL, KEY);
        let lines = connect("pi", &opts, &sandbox.home()).unwrap();
        assert!(lines.iter().any(|l| l.contains("baseUrl")), "{lines:?}");
        assert!(
            lines.iter().all(|l| !l.contains(KEY)),
            "key leaked: {lines:?}"
        );

        let models = sandbox.read_json(".pi/agent/models.json");
        let gateway = &models["providers"]["gateway"];
        assert_eq!(gateway["baseUrl"], URL);
        assert_eq!(gateway["apiKey"], KEY);
        assert_eq!(gateway["api"], "openai-completions");
        assert!(gateway["models"]["m"].is_object());
        assert_eq!(
            sandbox.read_json(".pi/agent/settings.json")["cacheWarming"],
            "off"
        );
        assert_eq!(sandbox.backups(".pi/agent/models.json"), 1);

        let again = connect("pi", &opts, &sandbox.home()).unwrap();
        assert!(again.iter().all(|l| l.ends_with("unchanged")), "{again:?}");
        assert_eq!(sandbox.backups(".pi/agent/models.json"), 1);
        assert_eq!(sandbox.backups(".pi/agent/settings.json"), 1);
    }

    #[test]
    fn pi_disconnect_restores_only_the_recorded_values() {
        let sandbox = pi_sandbox();
        let opts = sandbox.opts(URL, KEY);
        connect("pi", &opts, &sandbox.home()).unwrap();

        // A later user edit must survive the restore.
        let mut models = sandbox.read_json(".pi/agent/models.json");
        models["providers"]["gateway"]["models"]["extra"] = serde_json::json!({"id": "extra"});
        sandbox.write(
            ".pi/agent/models.json",
            &serde_json::to_string(&models).unwrap(),
        );

        let lines = disconnect("pi", &sandbox.home(), &sandbox.state()).unwrap();
        assert!(lines.iter().any(|l| l.contains("restored")), "{lines:?}");

        let models = sandbox.read_json(".pi/agent/models.json");
        assert_eq!(
            models["providers"]["gateway"]["baseUrl"],
            "https://old.example/v1"
        );
        assert_eq!(models["providers"]["gateway"]["apiKey"], "sk-old");
        assert!(models["providers"]["gateway"]["models"]["extra"].is_object());
        assert_eq!(
            sandbox.read_json(".pi/agent/settings.json")["cacheWarming"],
            "on"
        );

        let status = status(&sandbox.home(), &sandbox.state()).unwrap();
        assert_eq!(status, vec!["no agents connected".to_string()]);
    }

    #[test]
    fn reconnect_keeps_the_first_original() {
        let sandbox = pi_sandbox();
        connect("pi", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap();
        connect(
            "pi",
            &sandbox.opts("http://127.0.0.1:9999/v1", "sk-new"),
            &sandbox.home(),
        )
        .unwrap();
        disconnect("pi", &sandbox.home(), &sandbox.state()).unwrap();
        let models = sandbox.read_json(".pi/agent/models.json");
        assert_eq!(
            models["providers"]["gateway"]["baseUrl"],
            "https://old.example/v1"
        );
        assert_eq!(models["providers"]["gateway"]["apiKey"], "sk-old");
    }

    #[test]
    fn disconnect_leaves_drifted_values_alone() {
        let sandbox = pi_sandbox();
        connect("pi", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap();

        let mut models = sandbox.read_json(".pi/agent/models.json");
        models["providers"]["gateway"]["apiKey"] = serde_json::json!("sk-user-rotated");
        sandbox.write(
            ".pi/agent/models.json",
            &serde_json::to_string(&models).unwrap(),
        );

        let lines = disconnect("pi", &sandbox.home(), &sandbox.state()).unwrap();
        assert!(
            lines.iter().any(|l| l.contains("left alone (drifted)")),
            "{lines:?}"
        );
        let models = sandbox.read_json(".pi/agent/models.json");
        assert_eq!(models["providers"]["gateway"]["apiKey"], "sk-user-rotated");
        assert_eq!(
            models["providers"]["gateway"]["baseUrl"],
            "https://old.example/v1"
        );
        // The drifted record stays recorded so a later disconnect can still reach it.
        let status = status(&sandbox.home(), &sandbox.state()).unwrap();
        assert!(status.iter().any(|l| l.contains("drifted")), "{status:?}");
    }

    #[test]
    fn pi_dry_run_writes_nothing() {
        let sandbox = pi_sandbox();
        let before = fs::read_to_string(sandbox.home().join(".pi/agent/models.json")).unwrap();
        let opts = Connect {
            dry_run: true,
            ..sandbox.opts(URL, KEY)
        };
        let lines = connect("pi", &opts, &sandbox.home()).unwrap();
        assert!(lines.iter().any(|l| l.contains("dry run")), "{lines:?}");
        assert_eq!(
            fs::read_to_string(sandbox.home().join(".pi/agent/models.json")).unwrap(),
            before
        );
        assert_eq!(sandbox.backups(".pi/agent/models.json"), 0);
        assert!(!Manifest::path(&sandbox.state()).exists());
    }

    #[test]
    fn disconnect_without_connect_does_nothing() {
        let sandbox = Sandbox::new();
        let lines = disconnect("pi", &sandbox.home(), &sandbox.state()).unwrap();
        assert_eq!(lines, vec!["pi: not connected".to_string()]);
    }

    #[test]
    fn state_dir_and_manifest_are_private() {
        let sandbox = pi_sandbox();
        // Start with no state dir so connect has to create it.
        fs::remove_dir_all(sandbox.state()).unwrap();
        connect("pi", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap();

        let dir = fs::metadata(sandbox.state()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir, 0o700);
        let manifest = fs::metadata(Manifest::path(&sandbox.state()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(manifest, 0o600);
        assert_eq!(sandbox.mode(".pi/agent/models.json"), 0o600);
        assert_eq!(sandbox.mode(".pi/agent/settings.json"), 0o600);
    }

    #[test]
    fn manifest_is_saved_per_file_so_earlier_writes_stay_recorded() {
        let sandbox = pi_sandbox();
        // A directory where settings.json belongs makes the second write fail.
        fs::remove_file(sandbox.home().join(".pi/agent/settings.json")).unwrap();
        fs::create_dir(sandbox.home().join(".pi/agent/settings.json")).unwrap();

        connect("pi", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap_err();

        let text = fs::read_to_string(Manifest::path(&sandbox.state())).unwrap();
        assert!(text.contains("models.json"), "{text}");
    }

    #[test]
    fn backups_use_nanoseconds_and_never_collide() {
        let sandbox = pi_sandbox();
        let file = sandbox.home().join(".pi/agent/models.json");
        let first = backup(&file).unwrap();
        let second = backup(&file).unwrap();
        assert_ne!(first, second);
        assert!(first.to_string_lossy().contains(".gw-bak-"));
        assert!(second.to_string_lossy().contains(".gw-bak-"));
    }

    #[test]
    fn anthropic_base_trims_one_v1() {
        assert_eq!(
            anthropic_base("http://127.0.0.1:10204/v1"),
            "http://127.0.0.1:10204"
        );
        assert_eq!(
            anthropic_base("http://127.0.0.1:10204/v1/"),
            "http://127.0.0.1:10204"
        );
        assert_eq!(anthropic_base("http://h/v1/v1"), "http://h/v1");
        assert_eq!(anthropic_base("http://h/v1beta"), "http://h/v1beta");
    }

    #[test]
    fn status_reports_client_side_cache_tools() {
        let sandbox = pi_sandbox();
        sandbox.write(
            ".pi/agent/settings.json",
            r#"{"theme":"dark","cacheWarming":"on","packages":["npm:pi-cache-optimizer"]}"#,
        );
        sandbox.write(".pi/agent/extensions/opencodex-anthropic.ts", "// ext\n");
        connect("pi", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap();

        let lines = status(&sandbox.home(), &sandbox.state()).unwrap();
        assert!(
            lines.contains(
                &"pi: client-side cache tools still active: pi-cache-optimizer (~/.pi/agent/settings.json packages), opencodex-anthropic.ts (~/.pi/agent/extensions)".to_string()
            ),
            "{lines:?}"
        );
        // Reported only: the package and the extension stay in place.
        assert_eq!(
            sandbox.read_json(".pi/agent/settings.json")["packages"][0],
            "npm:pi-cache-optimizer"
        );
        assert!(sandbox
            .home()
            .join(".pi/agent/extensions/opencodex-anthropic.ts")
            .exists());
    }

    fn opencode_sandbox() -> Sandbox {
        let sandbox = Sandbox::new();
        sandbox.write(
            ".config/opencode/opencode.json",
            r#"{"$schema":"x","provider":{"headroom":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"http://127.0.0.1:8787/v1"}}}}"#,
        );
        sandbox
    }

    #[test]
    fn opencode_connect_update_disconnect() {
        let sandbox = opencode_sandbox();
        let opts = sandbox.opts(URL, KEY);
        connect("opencode", &opts, &sandbox.home()).unwrap();

        let config = sandbox.read_json(".config/opencode/opencode.json");
        let gw = &config["provider"]["gw"];
        assert_eq!(gw["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(gw["options"]["baseURL"], URL);
        assert_eq!(gw["options"]["apiKey"], KEY);
        assert!(config["provider"]["headroom"].is_object());

        let again = connect("opencode", &opts, &sandbox.home()).unwrap();
        assert!(again.iter().all(|l| l.ends_with("unchanged")), "{again:?}");
        assert_eq!(sandbox.backups(".config/opencode/opencode.json"), 1);

        disconnect("opencode", &sandbox.home(), &sandbox.state()).unwrap();
        let config = sandbox.read_json(".config/opencode/opencode.json");
        assert!(config["provider"].get("gw").is_none(), "{}", config);
        assert!(config["provider"]["headroom"].is_object());
    }

    #[test]
    fn opencode_jsonc_with_comments_is_not_rewritten() {
        let sandbox = Sandbox::new();
        let body = "// keep me\n{\n  /* block */\n  \"provider\": {}\n}\n";
        sandbox.write(".config/opencode/opencode.jsonc", body);

        let lines = connect("opencode", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("has comments, not written")),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.contains(KEY)),
            "key leaked: {lines:?}"
        );
        assert_eq!(
            fs::read_to_string(sandbox.home().join(".config/opencode/opencode.jsonc")).unwrap(),
            body
        );
        assert!(!Manifest::path(&sandbox.state()).exists());
    }

    #[test]
    fn opencode_dry_run_writes_nothing() {
        let sandbox = opencode_sandbox();
        let before =
            fs::read_to_string(sandbox.home().join(".config/opencode/opencode.json")).unwrap();
        let opts = Connect {
            dry_run: true,
            ..sandbox.opts(URL, KEY)
        };
        connect("opencode", &opts, &sandbox.home()).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.home().join(".config/opencode/opencode.json")).unwrap(),
            before
        );
        assert_eq!(sandbox.backups(".config/opencode/opencode.json"), 0);
        assert!(!Manifest::path(&sandbox.state()).exists());
    }

    #[test]
    fn claude_connect_drops_v1_suffix() {
        let sandbox = Sandbox::new();
        sandbox.write(".claude/settings.json", r#"{"model":"opus"}"#);
        let opts = sandbox.opts(URL, KEY);
        connect("claude", &opts, &sandbox.home()).unwrap();

        let settings = sandbox.read_json(".claude/settings.json");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:10204"
        );
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], KEY);
        assert_eq!(settings["model"], "opus");

        let again = connect("claude", &opts, &sandbox.home()).unwrap();
        assert!(again.iter().all(|l| l.ends_with("unchanged")), "{again:?}");

        disconnect("claude", &sandbox.home(), &sandbox.state()).unwrap();
        let settings = sandbox.read_json(".claude/settings.json");
        assert!(settings.get("env").is_none(), "{settings}");
        assert_eq!(settings["model"], "opus");
    }

    #[test]
    fn claude_dry_run_writes_nothing() {
        let sandbox = Sandbox::new();
        sandbox.write(".claude/settings.json", r#"{"model":"opus"}"#);
        let before = fs::read_to_string(sandbox.home().join(".claude/settings.json")).unwrap();
        let opts = Connect {
            dry_run: true,
            ..sandbox.opts(URL, KEY)
        };
        connect("claude", &opts, &sandbox.home()).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.home().join(".claude/settings.json")).unwrap(),
            before
        );
        assert_eq!(sandbox.backups(".claude/settings.json"), 0);
    }

    fn codex_sandbox() -> Sandbox {
        let sandbox = Sandbox::new();
        sandbox.write(
            ".codex/config.toml",
            "model = \"gpt-5\"\nmodel_provider = \"headroom\"\n\n[model_providers.headroom]\nname = \"headroom\"\nbase_url = \"http://127.0.0.1:8787/v1\"\n",
        );
        sandbox
    }

    #[test]
    fn codex_connect_prints_export_and_never_writes_the_key() {
        let sandbox = codex_sandbox();
        let opts = sandbox.opts(URL, KEY);
        let lines = connect("codex", &opts, &sandbox.home()).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l == "codex: export GW_API_KEY=$(cat <key-file>)"),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.contains(KEY)),
            "key leaked: {lines:?}"
        );

        let config = sandbox.read_toml(".codex/config.toml");
        assert_eq!(config.get("model_provider").unwrap().as_str(), Some("gw"));
        assert_eq!(config.get("model").unwrap().as_str(), Some("gpt-5"));
        let gw = config
            .get("model_providers")
            .and_then(|p| p.get("gw"))
            .expect("gw provider");
        assert_eq!(gw.get("name").unwrap().as_str(), Some("gw"));
        assert_eq!(gw.get("base_url").unwrap().as_str(), Some(URL));
        assert_eq!(gw.get("env_key").unwrap().as_str(), Some("GW_API_KEY"));
        assert_eq!(gw.get("wire_api").unwrap().as_str(), Some("responses"));
        assert!(config
            .get("model_providers")
            .and_then(|p| p.get("headroom"))
            .is_some());
        let text = fs::read_to_string(sandbox.home().join(".codex/config.toml")).unwrap();
        assert!(!text.contains(KEY), "key written to config: {text}");

        let again = connect("codex", &opts, &sandbox.home()).unwrap();
        let writes: Vec<_> = again.iter().filter(|l| !l.contains("export")).collect();
        assert!(writes.iter().all(|l| l.ends_with("unchanged")), "{again:?}");

        disconnect("codex", &sandbox.home(), &sandbox.state()).unwrap();
        let config = sandbox.read_toml(".codex/config.toml");
        assert_eq!(
            config.get("model_provider").unwrap().as_str(),
            Some("headroom")
        );
        assert!(
            config
                .get("model_providers")
                .and_then(|p| p.get("gw"))
                .is_none(),
            "gw table should be removed"
        );
    }

    #[test]
    fn codex_keeps_comments_and_layout() {
        let sandbox = Sandbox::new();
        sandbox.write(
            ".codex/config.toml",
            "# top comment\nmodel = \"gpt-5\" # inline\n\n# provider note\n[model_providers.headroom]\nname = \"headroom\"\n",
        );
        connect("codex", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap();

        let text = fs::read_to_string(sandbox.home().join(".codex/config.toml")).unwrap();
        assert!(text.contains("# top comment"), "{text}");
        assert!(text.contains("# inline"), "{text}");
        assert!(text.contains("# provider note"), "{text}");
        let config = sandbox.read_toml(".codex/config.toml");
        assert_eq!(config.get("model_provider").unwrap().as_str(), Some("gw"));
    }

    #[test]
    fn codex_needs_no_key() {
        let sandbox = Sandbox::new();
        let opts = Connect {
            url: URL.into(),
            key: None,
            dry_run: false,
            state_dir: sandbox.state(),
            models: Vec::new(),
        };
        let lines = connect("codex", &opts, &sandbox.home()).unwrap();
        assert!(lines.iter().any(|l| l.contains("export GW_API_KEY")));
        let config = sandbox.read_toml(".codex/config.toml");
        assert_eq!(config.get("model_provider").unwrap().as_str(), Some("gw"),);
    }

    #[test]
    fn codex_dry_run_writes_nothing() {
        let sandbox = codex_sandbox();
        let before = fs::read_to_string(sandbox.home().join(".codex/config.toml")).unwrap();
        let opts = Connect {
            dry_run: true,
            ..sandbox.opts(URL, KEY)
        };
        connect("codex", &opts, &sandbox.home()).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.home().join(".codex/config.toml")).unwrap(),
            before
        );
        assert_eq!(sandbox.backups(".codex/config.toml"), 0);
        assert!(!Manifest::path(&sandbox.state()).exists());
    }

    #[test]
    fn connect_without_a_key_uses_a_placeholder() {
        let sandbox = Sandbox::new();
        let opts = Connect {
            url: URL.into(),
            key: None,
            dry_run: false,
            state_dir: sandbox.state(),
            models: Vec::new(),
        };
        connect("pi", &opts, &sandbox.home()).unwrap();
        let models = std::fs::read_to_string(sandbox.home().join(".pi/agent/models.json")).unwrap();
        assert!(models.contains("\"apiKey\": \"gw\""), "{models}");
    }

    #[test]
    fn unknown_agent_is_rejected() {
        let sandbox = Sandbox::new();
        let error = connect("nope", &sandbox.opts(URL, KEY), &sandbox.home()).unwrap_err();
        assert!(error.to_string().contains("unknown agent"), "{error}");
    }

    #[test]
    fn detect_finds_agent_directories() {
        let sandbox = Sandbox::new();
        fs::create_dir_all(sandbox.home().join(".pi/agent")).unwrap();
        fs::create_dir_all(sandbox.home().join(".codex")).unwrap();
        assert_eq!(detect(&sandbox.home()), ["pi", "codex"]);
    }

    #[test]
    fn pi_and_opencode_get_model_lists_and_disconnect_restores() {
        let sandbox = Sandbox::new();
        let mut opts = sandbox.opts(URL, KEY);
        opts.models = vec!["a/b".into(), "c/d".into()];
        connect("pi", &opts, &sandbox.home()).unwrap();
        connect("opencode", &opts, &sandbox.home()).unwrap();
        let pi: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(sandbox.home().join(".pi/agent/models.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(pi["providers"]["gateway"]["models"][1]["id"], "c/d");
        let oc: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(sandbox.home().join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(oc["provider"]["gw"]["models"]["a/b"]["name"], "a/b");
        disconnect("pi", &sandbox.home(), &sandbox.state()).unwrap();
        let pi = fs::read_to_string(sandbox.home().join(".pi/agent/models.json")).unwrap();
        assert!(!pi.contains("c/d"), "{pi}");
    }
}
