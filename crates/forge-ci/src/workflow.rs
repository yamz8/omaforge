//! Workflow files: the subset of GitHub Actions' YAML that omaforge runs.
//!
//! Supported: `name`, `run-name`, `on` (`push`, `pull_request`, `workflow_dispatch`, with
//! branch, tag and path filters), `env`, `defaults.run`, and per job `name`, `runs-on`,
//! `needs`, `if`, `env`, `container`, `strategy.matrix` (with `include` / `exclude`),
//! `fail-fast`, `timeout-minutes`, `continue-on-error`, `outputs`, `defaults.run` and
//! `steps` (`run` or `uses`, with `id`, `name`, `if`, `env`, `with`, `shell`,
//! `working-directory`, `continue-on-error`, `timeout-minutes`). Anything else is ignored.

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use yaml_rust2::{Yaml, YamlLoader};

use crate::glob;

#[derive(Debug, Clone, Default)]
pub struct Workflow {
    pub name: Option<String>,
    pub run_name: Option<String>,
    pub on: Triggers,
    pub env: Map<String, Value>,
    pub defaults: RunDefaults,
    /// In file order.
    pub jobs: Vec<(String, Job)>,
}

#[derive(Debug, Clone, Default)]
pub struct Triggers {
    pub push: Option<RefFilter>,
    pub pull_request: Option<RefFilter>,
    pub workflow_dispatch: bool,
}

impl Triggers {
    /// Whether matching needs the paths a change touches.
    pub fn has_path_filters(&self) -> bool {
        [&self.push, &self.pull_request]
            .into_iter()
            .flatten()
            .any(|f| f.paths.is_some() || f.paths_ignore.is_some())
    }
}

/// `branches`, `tags` and `paths` filters, each with its `-ignore` form. For pull requests
/// `branches` applies to the base branch and `types` to the action.
#[derive(Debug, Clone, Default)]
pub struct RefFilter {
    pub branches: Option<Vec<String>>,
    pub branches_ignore: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub tags_ignore: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub paths_ignore: Option<Vec<String>>,
    pub types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunDefaults {
    pub shell: Option<String>,
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub name: Option<String>,
    /// A label or list of labels, possibly with expressions.
    pub runs_on: Value,
    pub needs: Vec<String>,
    #[serde(rename = "if")]
    pub condition: Option<String>,
    pub env: Map<String, Value>,
    pub container: Option<Container>,
    pub matrix: Option<Matrix>,
    pub fail_fast: bool,
    pub timeout_minutes: Option<f64>,
    pub continue_on_error: bool,
    pub outputs: Map<String, Value>,
    pub defaults: RunDefaults,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Container {
    pub image: String,
    pub env: Map<String, Value>,
    pub options: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Matrix {
    /// Dimensions in file order.
    pub dimensions: Vec<(String, Vec<Value>)>,
    pub include: Vec<Map<String, Value>>,
    pub exclude: Vec<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "if")]
    pub condition: Option<String>,
    pub run: Option<String>,
    pub uses: Option<String>,
    pub with: Map<String, Value>,
    pub env: Map<String, Value>,
    pub shell: Option<String>,
    pub working_directory: Option<String>,
    /// `true`, `false` or an expression.
    pub continue_on_error: Value,
    pub timeout_minutes: Option<f64>,
}

impl Step {
    /// The name shown in logs: `name`, else `Run <first line>` or `Run <action>`.
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        if let Some(uses) = &self.uses {
            return format!("Run {uses}");
        }
        let first = self
            .run
            .as_deref()
            .unwrap_or("")
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        format!("Run {first}")
    }
}

/// What triggered a run, for matching against `on`.
pub enum Event<'a> {
    /// A push of `git_ref` (a full ref name). `changed` is `None` when unknown (a new
    /// branch), in which case path filters don't hold the run back.
    Push {
        git_ref: &'a str,
        changed: Option<&'a [String]>,
    },
    PullRequest {
        action: &'a str,
        base_branch: &'a str,
        changed: Option<&'a [String]>,
    },
}

impl Workflow {
    pub fn parse(source: &str) -> Result<Self> {
        let docs = YamlLoader::load_from_str(source).map_err(|e| anyhow!("invalid YAML: {e}"))?;
        let root = match docs.into_iter().next() {
            Some(doc) => to_json(doc)?,
            None => bail!("the workflow file is empty"),
        };
        let Value::Object(root) = root else {
            bail!("a workflow must be a mapping with `on` and `jobs`");
        };
        let on = root
            .get("on")
            .ok_or_else(|| anyhow!("the workflow has no `on` section saying when to run"))?;
        let jobs_value = root
            .get("jobs")
            .ok_or_else(|| anyhow!("the workflow has no `jobs`"))?;
        let Value::Object(jobs_map) = jobs_value else {
            bail!("`jobs` must be a mapping of job ids to jobs");
        };
        if jobs_map.is_empty() {
            bail!("the workflow has no jobs");
        }
        let mut jobs = Vec::new();
        for (id, job) in jobs_map {
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                || id.starts_with(|c: char| c.is_ascii_digit() || c == '-')
            {
                bail!("job id `{id}` must start with a letter or `_` and contain only letters, digits, `-` and `_`");
            }
            let job = parse_job(job).with_context(|| format!("in job `{id}`"))?;
            jobs.push((id.clone(), job));
        }
        for (id, job) in &jobs {
            for need in &job.needs {
                if !jobs.iter().any(|(other, _)| other == need) {
                    bail!("job `{id}` needs `{need}`, which doesn't exist");
                }
            }
        }
        check_cycles(&jobs)?;
        Ok(Workflow {
            name: optional_string(&root, "name")?,
            run_name: optional_string(&root, "run-name")?,
            on: parse_triggers(on)?,
            env: string_map(root.get("env"), "env")?,
            defaults: parse_defaults(root.get("defaults"))?,
            jobs,
        })
    }

    /// Whether `event` should start this workflow.
    pub fn triggered_by(&self, event: &Event) -> bool {
        match event {
            Event::Push { git_ref, changed } => {
                let Some(filter) = &self.on.push else {
                    return false;
                };
                let refs_ok = if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
                    if filter.branches.is_none() && filter.branches_ignore.is_none() {
                        filter.tags.is_none() && filter.tags_ignore.is_none()
                    } else {
                        passes(branch, &filter.branches, &filter.branches_ignore)
                    }
                } else if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
                    if filter.tags.is_none() && filter.tags_ignore.is_none() {
                        filter.branches.is_none() && filter.branches_ignore.is_none()
                    } else {
                        passes(tag, &filter.tags, &filter.tags_ignore)
                    }
                } else {
                    false
                };
                // As on GitHub, path filters don't apply to tags.
                refs_ok && (git_ref.starts_with("refs/tags/") || paths_pass(filter, *changed))
            }
            Event::PullRequest {
                action,
                base_branch,
                changed,
            } => {
                let Some(filter) = &self.on.pull_request else {
                    return false;
                };
                let types_ok = match &filter.types {
                    Some(types) => types.iter().any(|t| t == action),
                    None => matches!(*action, "opened" | "synchronize" | "reopened"),
                };
                types_ok
                    && passes(base_branch, &filter.branches, &filter.branches_ignore)
                    && paths_pass(filter, *changed)
            }
        }
    }
}

/// `include` / `ignore` filters on a name: with an include list the name must match it
/// (patterns starting with `!` exclude, and later patterns win); with an ignore list it
/// must not match.
fn passes(name: &str, include: &Option<Vec<String>>, ignore: &Option<Vec<String>>) -> bool {
    if let Some(patterns) = include {
        return glob::matches_list(patterns, name);
    }
    if let Some(patterns) = ignore {
        return !glob::matches_list(patterns, name);
    }
    true
}

fn paths_pass(filter: &RefFilter, changed: Option<&[String]>) -> bool {
    let Some(changed) = changed else { return true };
    if let Some(patterns) = &filter.paths {
        return changed.iter().any(|p| glob::matches_list(patterns, p));
    }
    if let Some(patterns) = &filter.paths_ignore {
        return changed.iter().any(|p| !glob::matches_list(patterns, p));
    }
    true
}

fn check_cycles(jobs: &[(String, Job)]) -> Result<()> {
    fn visit<'a>(
        id: &'a str,
        jobs: &'a [(String, Job)],
        path: &mut Vec<&'a str>,
        done: &mut Vec<&'a str>,
    ) -> Result<()> {
        if done.contains(&id) {
            return Ok(());
        }
        if path.contains(&id) {
            path.push(id);
            bail!("jobs depend on each other in a cycle: {}", path.join(" → "));
        }
        path.push(id);
        let job = &jobs
            .iter()
            .find(|(other, _)| other == id)
            .expect("checked")
            .1;
        for need in &job.needs {
            visit(need, jobs, path, done)?;
        }
        path.pop();
        done.push(id);
        Ok(())
    }
    let mut done = Vec::new();
    for (id, _) in jobs {
        visit(id, jobs, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

fn parse_triggers(on: &Value) -> Result<Triggers> {
    let mut triggers = Triggers::default();
    let mut add = |name: &str, config: Option<&Value>| -> Result<()> {
        match name {
            "push" => triggers.push = Some(parse_filter(config, name)?),
            "pull_request" => triggers.pull_request = Some(parse_filter(config, name)?),
            "workflow_dispatch" => triggers.workflow_dispatch = true,
            // Other events (schedule, release, ...) never fire here yet.
            _ => {}
        }
        Ok(())
    };
    match on {
        Value::String(name) => add(name, None)?,
        Value::Array(names) => {
            for name in names {
                match name {
                    Value::String(name) => add(name, None)?,
                    _ => bail!("`on` must list event names"),
                }
            }
        }
        Value::Object(events) => {
            for (name, config) in events {
                add(name, Some(config))?;
            }
        }
        _ => bail!("`on` must be an event name, a list or a mapping"),
    }
    Ok(triggers)
}

fn parse_filter(config: Option<&Value>, event: &str) -> Result<RefFilter> {
    let map = match config {
        None | Some(Value::Null) => return Ok(RefFilter::default()),
        Some(Value::Object(map)) => map,
        Some(_) => bail!("`on.{event}` must be a mapping"),
    };
    let list = |key: &str| -> Result<Option<Vec<String>>> {
        match map.get(key) {
            None => Ok(None),
            Some(value) => string_list(value)
                .map(Some)
                .with_context(|| format!("in `on.{event}.{key}`")),
        }
    };
    let filter = RefFilter {
        branches: list("branches")?,
        branches_ignore: list("branches-ignore")?,
        tags: list("tags")?,
        tags_ignore: list("tags-ignore")?,
        paths: list("paths")?,
        paths_ignore: list("paths-ignore")?,
        types: list("types")?,
    };
    if filter.branches.is_some() && filter.branches_ignore.is_some() {
        bail!("`on.{event}` can't have both `branches` and `branches-ignore`");
    }
    if filter.tags.is_some() && filter.tags_ignore.is_some() {
        bail!("`on.{event}` can't have both `tags` and `tags-ignore`");
    }
    if filter.paths.is_some() && filter.paths_ignore.is_some() {
        bail!("`on.{event}` can't have both `paths` and `paths-ignore`");
    }
    Ok(filter)
}

fn parse_defaults(value: Option<&Value>) -> Result<RunDefaults> {
    let Some(run) = value.and_then(|d| d.get("run")) else {
        return Ok(RunDefaults::default());
    };
    let Value::Object(run) = run else {
        bail!("`defaults.run` must be a mapping");
    };
    Ok(RunDefaults {
        shell: optional_string(run, "shell")?,
        working_directory: optional_string(run, "working-directory")?,
    })
}

fn parse_job(value: &Value) -> Result<Job> {
    let Value::Object(map) = value else {
        bail!("a job must be a mapping");
    };
    if map.contains_key("uses") {
        bail!("reusable workflows (`uses:` on a job) aren't supported yet");
    }
    let runs_on = match map.get("runs-on") {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(labels)) => Value::Array(
            labels
                .iter()
                .map(|l| scalar(l).map(Value::String))
                .collect::<Result<_>>()?,
        ),
        Some(Value::Object(group)) => match group.get("labels") {
            Some(labels) => Value::Array(
                string_list(labels)?
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
            None => bail!("`runs-on` must name runner labels"),
        },
        Some(_) => bail!("`runs-on` must be a label or a list of labels"),
        None => bail!("`runs-on` is missing"),
    };
    let needs = match map.get("needs") {
        None => Vec::new(),
        Some(value) => string_list(value).context("in `needs`")?,
    };
    let container = match map.get("container") {
        None | Some(Value::Null) => None,
        Some(Value::String(image)) => Some(Container {
            image: image.clone(),
            ..Container::default()
        }),
        Some(Value::Object(c)) => Some(Container {
            image: optional_string(c, "image")?
                .ok_or_else(|| anyhow!("`container` needs an `image`"))?,
            env: string_map(c.get("env"), "container.env")?,
            options: optional_string(c, "options")?,
        }),
        Some(_) => bail!("`container` must be an image name or a mapping"),
    };
    let (matrix, fail_fast) = match map.get("strategy") {
        None => (None, true),
        Some(Value::Object(strategy)) => {
            let fail_fast = match strategy.get("fail-fast") {
                None => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => bail!("`strategy.fail-fast` must be true or false"),
            };
            let matrix = match strategy.get("matrix") {
                None => None,
                Some(value) => Some(parse_matrix(value)?),
            };
            (matrix, fail_fast)
        }
        Some(_) => bail!("`strategy` must be a mapping"),
    };
    let steps = match map.get("steps") {
        Some(Value::Array(steps)) if !steps.is_empty() => steps
            .iter()
            .enumerate()
            .map(|(i, step)| parse_step(step).with_context(|| format!("in step {}", i + 1)))
            .collect::<Result<Vec<_>>>()?,
        Some(Value::Array(_)) | None => bail!("a job needs `steps`"),
        Some(_) => bail!("`steps` must be a list"),
    };
    let mut ids = Vec::new();
    for step in &steps {
        if let Some(id) = &step.id {
            if ids.contains(&id) {
                bail!("two steps have the id `{id}`");
            }
            ids.push(id);
        }
    }
    Ok(Job {
        name: optional_string(map, "name")?,
        runs_on,
        needs,
        condition: optional_condition(map)?,
        env: string_map(map.get("env"), "env")?,
        container,
        matrix,
        fail_fast,
        timeout_minutes: optional_number(map, "timeout-minutes")?,
        continue_on_error: matches!(map.get("continue-on-error"), Some(Value::Bool(true))),
        outputs: string_map(map.get("outputs"), "outputs")?,
        defaults: parse_defaults(map.get("defaults"))?,
        steps,
    })
}

fn parse_matrix(value: &Value) -> Result<Matrix> {
    let Value::Object(map) = value else {
        bail!(
            "`strategy.matrix` must be a mapping (matrices from expressions aren't supported yet)"
        );
    };
    let mut matrix = Matrix::default();
    for (key, value) in map {
        let objects = |v: &Value| -> Result<Vec<Map<String, Value>>> {
            match v {
                Value::Array(items) => items
                    .iter()
                    .map(|item| match item {
                        Value::Object(o) => Ok(o.clone()),
                        _ => bail!("`matrix.{key}` must be a list of mappings"),
                    })
                    .collect(),
                _ => bail!("`matrix.{key}` must be a list of mappings"),
            }
        };
        match key.as_str() {
            "include" => matrix.include = objects(value)?,
            "exclude" => matrix.exclude = objects(value)?,
            _ => match value {
                Value::Array(values) if !values.is_empty() => {
                    matrix.dimensions.push((key.clone(), values.clone()))
                }
                Value::Array(_) => bail!("`matrix.{key}` is empty"),
                _ => bail!("`matrix.{key}` must be a list of values"),
            },
        }
    }
    Ok(matrix)
}

impl Matrix {
    /// Every combination to run, following GitHub's rules: the product of the dimensions,
    /// minus `exclude` matches, then each `include` either extends the combinations it
    /// matches or adds a new one.
    pub fn expand(&self) -> Result<Vec<Map<String, Value>>> {
        let mut combos: Vec<Map<String, Value>> = vec![Map::new()];
        for (key, values) in &self.dimensions {
            combos = combos
                .into_iter()
                .flat_map(|combo| {
                    values.iter().map(move |value| {
                        let mut next = combo.clone();
                        next.insert(key.clone(), value.clone());
                        next
                    })
                })
                .collect();
        }
        if self.dimensions.is_empty() {
            combos.clear();
        }
        combos.retain(|combo| {
            !self
                .exclude
                .iter()
                .any(|ex| ex.iter().all(|(k, v)| combo.get(k) == Some(v)))
        });
        let originals = combos.len();
        for include in &self.include {
            let mut matched = false;
            for combo in combos.iter_mut().take(originals) {
                let original_keys_match = include.iter().all(|(k, v)| {
                    !self.dimensions.iter().any(|(d, _)| d == k) || combo.get(k) == Some(v)
                });
                if original_keys_match {
                    matched = true;
                    for (k, v) in include {
                        combo.insert(k.clone(), v.clone());
                    }
                }
            }
            if !matched {
                combos.push(include.clone());
            }
        }
        if combos.is_empty() {
            bail!("the matrix has no combinations left to run");
        }
        if combos.len() > 256 {
            bail!(
                "the matrix has {} combinations; at most 256 are allowed",
                combos.len()
            );
        }
        Ok(combos)
    }
}

fn parse_step(value: &Value) -> Result<Step> {
    let Value::Object(map) = value else {
        bail!("a step must be a mapping");
    };
    let step = Step {
        id: optional_string(map, "id")?,
        name: optional_string(map, "name")?,
        condition: optional_condition(map)?,
        run: optional_string(map, "run")?,
        uses: optional_string(map, "uses")?,
        with: string_map(map.get("with"), "with")?,
        env: string_map(map.get("env"), "env")?,
        shell: optional_string(map, "shell")?,
        working_directory: optional_string(map, "working-directory")?,
        continue_on_error: match map.get("continue-on-error") {
            None => Value::Bool(false),
            Some(Value::Bool(b)) => Value::Bool(*b),
            Some(Value::String(s)) => Value::String(s.clone()),
            Some(_) => bail!("`continue-on-error` must be true, false or an expression"),
        },
        timeout_minutes: optional_number(map, "timeout-minutes")?,
    };
    match (&step.run, &step.uses) {
        (Some(_), Some(_)) => bail!("a step can't have both `run` and `uses`"),
        (None, None) => bail!("a step needs `run` or `uses`"),
        _ => {}
    }
    if let Some(id) = &step.id {
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("step id `{id}` may only contain letters, digits, `-` and `_`");
        }
    }
    Ok(step)
}

fn optional_condition(map: &Map<String, Value>) -> Result<Option<String>> {
    match map.get("if") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => bail!("`if` must be an expression"),
    }
}

fn optional_string(map: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => scalar(value)
            .map(Some)
            .with_context(|| format!("in `{key}`")),
    }
}

fn optional_number(map: &Map<String, Value>, key: &str) -> Result<Option<f64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(_) => bail!("`{key}` must be a number"),
    }
}

/// A string, number or boolean as text.
fn scalar(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("expected a single value, not a list or mapping"),
    }
}

fn string_list(value: &Value) -> Result<Vec<String>> {
    match value {
        Value::Array(items) => items.iter().map(scalar).collect(),
        other => Ok(vec![scalar(other)?]),
    }
}

/// `env`-like mappings: values stay JSON (strings, numbers, booleans).
fn string_map(value: Option<&Value>, what: &str) -> Result<Map<String, Value>> {
    match value {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => {
            for (key, value) in map {
                if matches!(value, Value::Array(_) | Value::Object(_)) {
                    bail!("`{what}.{key}` must be a single value");
                }
            }
            Ok(map.clone())
        }
        Some(_) => bail!("`{what}` must be a mapping"),
    }
}

fn to_json(yaml: Yaml) -> Result<Value> {
    Ok(match yaml {
        Yaml::Null => Value::Null,
        Yaml::Boolean(b) => Value::Bool(b),
        Yaml::Integer(n) => Value::from(n),
        Yaml::Real(text) => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::String(text), Value::Number),
        Yaml::String(s) => Value::String(s),
        Yaml::Array(items) => Value::Array(items.into_iter().map(to_json).collect::<Result<_>>()?),
        Yaml::Hash(hash) => {
            let mut map = Map::new();
            for (key, value) in hash {
                let key = match key {
                    Yaml::String(s) => s,
                    Yaml::Integer(n) => n.to_string(),
                    Yaml::Boolean(b) => b.to_string(),
                    Yaml::Real(r) => r,
                    Yaml::Null => "null".into(),
                    _ => bail!("mapping keys must be plain values"),
                };
                map.insert(key, to_json(value)?);
            }
            Value::Object(map)
        }
        Yaml::Alias(_) => bail!("YAML aliases aren't supported"),
        Yaml::BadValue => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CI: &str = r#"
name: CI
on:
  push:
    branches: [main, 'release/**']
    paths-ignore: ['docs/**']
  pull_request:
env:
  CARGO_TERM_COLOR: always
jobs:
  test:
    name: Test (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, self-hosted]
        rust: [stable, nightly]
        exclude:
          - os: self-hosted
            rust: nightly
        include:
          - rust: nightly
            experimental: true
          - os: windows
            rust: stable
    steps:
      - uses: actions/checkout@v4
      - name: Test
        run: cargo test
        env:
          RUST_BACKTRACE: 1
  deploy:
    needs: test
    if: github.ref == 'refs/heads/main'
    runs-on: [self-hosted, linux]
    steps:
      - run: |
          echo deploying
          ./deploy.sh
"#;

    #[test]
    fn parses_a_workflow() {
        let wf = Workflow::parse(CI).unwrap();
        assert_eq!(wf.name.as_deref(), Some("CI"));
        assert_eq!(wf.env["CARGO_TERM_COLOR"], json!("always"));
        assert_eq!(wf.jobs.len(), 2);
        let (id, deploy) = &wf.jobs[1];
        assert_eq!(id, "deploy");
        assert_eq!(deploy.needs, ["test"]);
        assert_eq!(deploy.runs_on, json!(["self-hosted", "linux"]));
        assert_eq!(deploy.steps[0].display_name(), "Run echo deploying");
        let test = &wf.jobs[0].1;
        assert_eq!(test.steps[0].display_name(), "Run actions/checkout@v4");
        assert_eq!(test.steps[1].env["RUST_BACKTRACE"], json!(1));

        let combos = test.matrix.as_ref().unwrap().expand().unwrap();
        let shown: Vec<Value> = combos.into_iter().map(Value::Object).collect();
        assert_eq!(
            shown,
            vec![
                json!({"os": "ubuntu-latest", "rust": "stable"}),
                json!({"os": "ubuntu-latest", "rust": "nightly", "experimental": true}),
                json!({"os": "self-hosted", "rust": "stable"}),
                json!({"os": "windows", "rust": "stable"}),
            ]
        );
    }

    #[test]
    fn matches_events() {
        let wf = Workflow::parse(CI).unwrap();
        let push = |git_ref, changed: Option<&[String]>| {
            wf.triggered_by(&Event::Push { git_ref, changed })
        };
        assert!(push("refs/heads/main", None));
        assert!(push("refs/heads/release/1.0/rc", None));
        assert!(!push("refs/heads/feature", None));
        // Only branches are listed, so tags don't run it.
        assert!(!push("refs/tags/v1", None));
        let docs = vec!["docs/a.md".to_owned()];
        let code = vec!["docs/a.md".to_owned(), "src/main.rs".to_owned()];
        assert!(!push("refs/heads/main", Some(&docs)));
        assert!(push("refs/heads/main", Some(&code)));

        let pr = |action| {
            wf.triggered_by(&Event::PullRequest {
                action,
                base_branch: "anything",
                changed: None,
            })
        };
        assert!(pr("opened"));
        assert!(pr("synchronize"));
        assert!(!pr("closed"));

        let simple = Workflow::parse(
            "on: [push]\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: true\n",
        )
        .unwrap();
        assert!(simple.triggered_by(&Event::Push {
            git_ref: "refs/tags/v1",
            changed: None
        }));
        assert!(!simple.triggered_by(&Event::PullRequest {
            action: "opened",
            base_branch: "main",
            changed: None
        }));
    }

    #[test]
    fn explains_mistakes() {
        let err = |source: &str| format!("{:#}", Workflow::parse(source).unwrap_err());
        assert!(err("jobs: {}").contains("no `on`"));
        assert!(err("on: push\njobs:\n  a:\n    steps:\n      - run: x\n")
            .contains("`runs-on` is missing"));
        assert!(err(
            "on: push\njobs:\n  a:\n    runs-on: x\n    needs: b\n    steps:\n      - run: x\n"
        )
        .contains("needs `b`"));
        assert!(
            err("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - name: nothing\n")
                .contains("in step 1: a step needs `run` or `uses`")
        );
        let cycle = "on: push\njobs:\n  a:\n    runs-on: x\n    needs: b\n    steps: [{run: x}]\n  b:\n    runs-on: x\n    needs: a\n    steps: [{run: x}]\n";
        assert!(err(cycle).contains("cycle"));
        assert!(err("on: push\njobs: [\n").contains("invalid YAML"));
    }
}
