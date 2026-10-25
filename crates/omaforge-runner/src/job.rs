//! Running one job: set up where it runs, run its steps in order, report as it goes.

use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};
use forge_ci::{
    expr::{self, Context, Status},
    protocol::{step_count, Conclusion, JobSpec},
    Step,
};
use serde_json::{json, Map, Value};
use tokio::process::Command;

use crate::{
    client::{JobClient, Runner},
    exec::{self, Dirs, Executor, Outcome},
    Label, Place,
};

const DEFAULT_TIMEOUT_MINUTES: f64 = 360.0;

/// Run a job and report it; returns how it ended.
pub async fn run(runner: &Runner, spec: JobSpec, labels: &[Label], work: &Path) -> Conclusion {
    let client = runner.job(&spec);
    let root = work.join(format!("job-{}", spec.id));
    let dirs = Dirs {
        workspace: root.join("workspace"),
        temp: root.join("temp"),
    };
    let mut job = JobRun {
        spec: &spec,
        client: &client,
        dirs: &dirs,
        env: BTreeMap::new(),
        path_prefix: String::new(),
        steps_ctx: Map::new(),
        outcome: Status::Success,
    };
    let (conclusion, outputs) = job.run(labels).await;
    exec::remove_dir(&root);
    client.finish(conclusion, outputs).await;
    conclusion
}

struct JobRun<'a> {
    spec: &'a JobSpec,
    client: &'a JobClient,
    dirs: &'a Dirs,
    /// Workflow and job env plus what steps added through `GITHUB_ENV`.
    env: BTreeMap<String, String>,
    /// Directories steps added through `GITHUB_PATH`, as a `PATH` prefix.
    path_prefix: String,
    /// The `steps` context.
    steps_ctx: Map<String, Value>,
    /// Whether the job has failed so far.
    outcome: Status,
}

impl JobRun<'_> {
    async fn run(&mut self, labels: &[Label]) -> (Conclusion, Map<String, Value>) {
        let total = step_count(&self.spec.job);
        let last = total - 1;
        let started = Instant::now();
        let timeout = Duration::from_secs_f64(
            self.spec
                .job
                .timeout_minutes
                .unwrap_or(DEFAULT_TIMEOUT_MINUTES)
                .max(0.0)
                * 60.0,
        );

        self.client.step_started(0).await;
        let executor = match self.set_up(labels).await {
            Ok(executor) => {
                self.client.step_finished(0, Conclusion::Success).await;
                executor
            }
            Err(error) => {
                self.client.log(0, &format!("##[error]{error:#}"));
                self.client.step_finished(0, Conclusion::Failure).await;
                for number in 1..total {
                    self.client.step_started(number).await;
                    self.client.step_finished(number, Conclusion::Skipped).await;
                }
                return (Conclusion::Failure, Map::new());
            }
        };

        let mut timed_out = false;
        for number in 1..last {
            let step = &self.spec.job.steps[number - 1];
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() && !timed_out {
                timed_out = true;
                self.outcome = Status::Cancelled;
                self.client.log(
                    number,
                    &format!(
                        "##[error]The job ran longer than its {} minute limit.",
                        timeout.as_secs() / 60
                    ),
                );
            }
            let conclusion = self.step(&executor, number, step, remaining).await;
            if conclusion == StepEnd::TimedOutJob {
                timed_out = true;
                self.outcome = Status::Cancelled;
                self.client.log(
                    number,
                    &format!(
                        "##[error]The job ran longer than its {} minute limit.",
                        timeout.as_secs() / 60
                    ),
                );
            }
        }

        self.client.step_started(last).await;
        let outputs = self.outputs(last);
        executor.stop(&|line| self.client.log(last, line)).await;
        self.client.step_finished(last, Conclusion::Success).await;

        let conclusion = if self.client.cancelled() {
            Conclusion::Cancelled
        } else if timed_out || self.outcome != Status::Success {
            Conclusion::Failure
        } else {
            Conclusion::Success
        };
        (conclusion, outputs)
    }

    /// Prepare directories, the job's environment, and where it runs.
    async fn set_up(&mut self, labels: &[Label]) -> Result<Executor> {
        let log = |line: &str| self.client.log(0, line);
        log(&format!("omaforge-runner {}", env!("CARGO_PKG_VERSION")));
        log(&format!(
            "Job: {} (job {}, run {})",
            self.spec.name, self.spec.id, self.spec.run_id
        ));
        std::fs::create_dir_all(&self.dirs.workspace)?;
        std::fs::create_dir_all(&self.dirs.temp)?;

        let image = match &self.spec.job.container {
            Some(container) => Some(
                self.interpolate_base(&container.image)
                    .context("in `container.image`")?,
            ),
            None => {
                let wanted = runs_on(&self.spec.job.runs_on, &self.base_context());
                let mut chosen = None;
                for name in &wanted {
                    if let Some(label) = labels.iter().find(|l| &l.name == name) {
                        if let Place::Docker(image) = &label.place {
                            chosen = Some(image.clone());
                            break;
                        }
                    }
                }
                chosen
            }
        };
        if self
            .spec
            .job
            .container
            .as_ref()
            .is_some_and(|c| c.options.is_some())
        {
            log("Note: `container.options` is ignored; a workflow can't change how its container runs.");
        }

        // The environment: workflow `env`, then the job's.
        let mut ctx = self.base_context();
        for (key, value) in self.spec.workflow_env.clone() {
            let value = self
                .evaluate(&value, &ctx)
                .with_context(|| format!("in `env.{key}`"))?;
            self.env.insert(key, value);
            ctx.set("env", env_value(&self.env));
        }
        for (key, value) in self.spec.job.env.clone() {
            let value = self
                .evaluate(&value, &ctx)
                .with_context(|| format!("in the job's `env.{key}`"))?;
            self.env.insert(key, value);
            ctx.set("env", env_value(&self.env));
        }

        let executor = match &image {
            Some(image) => {
                let mut container_env = BTreeMap::new();
                if let Some(container) = &self.spec.job.container {
                    for (key, value) in &container.env {
                        container_env.insert(key.clone(), self.evaluate(value, &ctx)?);
                    }
                }
                let name = format!("omaforge-job-{}-{}", self.spec.id, std::process::id());
                Executor::docker(image, &name, self.dirs, &container_env, &log).await?
            }
            None => {
                log("Running on this machine (no container)");
                Executor::host()
            }
        };
        let event = self
            .spec
            .contexts
            .get("github")
            .map(|g| g["event"].clone())
            .unwrap_or(json!({}));
        std::fs::write(
            self.dirs.temp.join("event.json"),
            serde_json::to_vec_pretty(&event)?,
        )?;
        Ok(executor)
    }

    /// Contexts every expression sees, before steps run.
    fn base_context(&self) -> Context {
        let mut ctx = Context::new();
        for (name, value) in &self.spec.contexts {
            ctx.set(name, value.clone());
        }
        ctx.set("env", env_value(&self.env));
        ctx.set(
            "runner",
            json!({
                "os": "Linux",
                "arch": arch(),
                "name": "omaforge-runner",
                "temp": self.dirs.temp.display().to_string(),
                "tool_cache": self.dirs.temp.join("tool-cache").display().to_string(),
            }),
        );
        ctx.status = self.outcome;
        ctx
    }

    fn step_context(&self, executor: &Executor) -> Context {
        let mut ctx = self.base_context();
        let mut github = ctx.values.get("github").cloned().unwrap_or(json!({}));
        github["workspace"] = json!(executor.workspace(self.dirs));
        ctx.set("github", github);
        ctx.set("steps", Value::Object(self.steps_ctx.clone()));
        ctx.set(
            "job",
            json!({ "status": match self.outcome { Status::Success => "success", Status::Failure => "failure", Status::Cancelled => "cancelled" } }),
        );
        if self.client.cancelled() {
            ctx.status = Status::Cancelled;
        }
        ctx
    }

    fn interpolate_base(&self, text: &str) -> Result<String> {
        expr::interpolate(text, &self.base_context())
    }

    fn evaluate(&self, value: &Value, ctx: &Context) -> Result<String> {
        Ok(match value {
            Value::String(s) => expr::interpolate(s, ctx)?,
            other => expr::to_string(other),
        })
    }

    async fn step(
        &mut self,
        executor: &Executor,
        number: usize,
        step: &Step,
        remaining: Duration,
    ) -> StepEnd {
        self.client.step_started(number).await;
        let ctx = self.step_context(executor);
        let condition = step.condition.as_deref().unwrap_or("success()");
        let run = match expr::condition(condition, &ctx) {
            Ok(run) => run,
            Err(error) => {
                self.client.log(
                    number,
                    &format!("##[error]The step's `if` is invalid: {error:#}"),
                );
                self.record(step, Conclusion::Failure, Conclusion::Failure, Map::new());
                self.outcome = Status::Failure;
                self.client.step_finished(number, Conclusion::Failure).await;
                return StepEnd::Done;
            }
        };
        if !run || (remaining.is_zero()) {
            self.record(step, Conclusion::Skipped, Conclusion::Skipped, Map::new());
            self.client.step_finished(number, Conclusion::Skipped).await;
            return StepEnd::Done;
        }

        let step_timeout = step
            .timeout_minutes
            .map(|m| Duration::from_secs_f64(m.max(0.0) * 60.0))
            .unwrap_or(remaining)
            .min(remaining);
        let job_limited = step_timeout == remaining;
        let result = self
            .execute(executor, number, step, &ctx, step_timeout)
            .await;
        let (outcome, outputs, end) = match result {
            Ok(Ran {
                success,
                outputs,
                timed_out,
            }) => {
                let end = if timed_out && job_limited {
                    StepEnd::TimedOutJob
                } else {
                    StepEnd::Done
                };
                if timed_out && !job_limited {
                    self.client.log(
                        number,
                        "##[error]The step ran longer than its `timeout-minutes`.",
                    );
                }
                (
                    if success {
                        Conclusion::Success
                    } else {
                        Conclusion::Failure
                    },
                    outputs,
                    end,
                )
            }
            Err(error) => {
                self.client.log(number, &format!("##[error]{error:#}"));
                (Conclusion::Failure, Map::new(), StepEnd::Done)
            }
        };
        let continue_on_error = match &step.continue_on_error {
            Value::Bool(b) => *b,
            Value::String(s) => expr::evaluate_value(s, &ctx)
                .map(|v| expr::truthy(&v))
                .unwrap_or(false),
            _ => false,
        };
        let conclusion = if outcome == Conclusion::Failure && continue_on_error {
            Conclusion::Success
        } else {
            outcome
        };
        if conclusion == Conclusion::Failure && self.outcome == Status::Success {
            self.outcome = Status::Failure;
        }
        self.record(step, outcome, conclusion, outputs);
        let reported = if self.client.cancelled() && outcome == Conclusion::Failure {
            Conclusion::Cancelled
        } else {
            conclusion
        };
        self.client.step_finished(number, reported).await;
        end
    }

    fn record(
        &mut self,
        step: &Step,
        outcome: Conclusion,
        conclusion: Conclusion,
        outputs: Map<String, Value>,
    ) {
        if let Some(id) = &step.id {
            self.steps_ctx.insert(
                id.clone(),
                json!({ "outcome": outcome.as_str(), "conclusion": conclusion.as_str(), "outputs": outputs }),
            );
        }
    }

    async fn execute(
        &mut self,
        executor: &Executor,
        number: usize,
        step: &Step,
        ctx: &Context,
        timeout: Duration,
    ) -> Result<Ran> {
        // The step's environment: the job's, then the step's own.
        let mut env = self.env.clone();
        let mut ctx = ctx.clone();
        for (key, value) in &step.env {
            let value = self
                .evaluate(value, &ctx)
                .with_context(|| format!("in the step's `env.{key}`"))?;
            env.insert(key.clone(), value);
            ctx.set("env", env_value(&env));
        }
        if let Some(uses) = &step.uses {
            return self.uses(executor, number, uses, step, &ctx, &env).await;
        }

        let script = expr::interpolate(step.run.as_deref().unwrap_or(""), &ctx)?;
        let shell = step
            .shell
            .clone()
            .or_else(|| self.spec.job.defaults.shell.clone())
            .or_else(|| self.spec.workflow_defaults.shell.clone());
        let working_directory = step
            .working_directory
            .clone()
            .or_else(|| self.spec.job.defaults.working_directory.clone())
            .or_else(|| self.spec.workflow_defaults.working_directory.clone());
        let (argv_template, extension) = shell_command(shell.as_deref(), executor)?;
        let file_name = format!("step-{number}.{extension}");
        std::fs::write(self.dirs.temp.join(&file_name), &script)?;
        let script_path = format!("{}/{file_name}", executor.temp(self.dirs));
        let argv: Vec<String> = argv_template
            .iter()
            .map(|a| a.replace("{0}", &script_path))
            .collect();

        // Files steps write to set outputs, env and PATH for later steps.
        let files = ["output", "env", "path", "summary"]
            .map(|kind| format!("{}/{kind}-{number}", executor.temp(self.dirs)));
        for file in &files {
            std::fs::write(executor.host_path(self.dirs, file), "")?;
        }
        let workspace = executor.workspace(self.dirs);
        let cwd = match working_directory {
            Some(dir) => {
                let dir = expr::interpolate(&dir, &ctx)?;
                if dir.starts_with('/') {
                    dir
                } else {
                    format!("{workspace}/{dir}")
                }
            }
            None => workspace.clone(),
        };
        env.extend(self.default_env(executor, &files));

        self.client.log(
            number,
            &format!(
                "##[group]Run {}",
                script.lines().next().unwrap_or("").trim()
            ),
        );
        for line in script.lines() {
            self.client.log(number, line);
        }
        self.client.log(number, "##[endgroup]");

        let mut set_outputs = Map::new();
        let client = self.client;
        let outcome = executor
            .run(
                &argv,
                &env,
                &self.path_prefix,
                &cwd,
                |line| handle_line(client, number, line, &mut set_outputs),
                async {
                    tokio::select! {
                        _ = client.wait_cancelled() => {},
                        _ = tokio::time::sleep(timeout) => {},
                    }
                },
            )
            .await?;

        let mut outputs = set_outputs;
        outputs.extend(read_env_file(&executor.host_path(self.dirs, &files[0]))?);
        for (key, value) in read_env_file(&executor.host_path(self.dirs, &files[1]))? {
            self.env.insert(key, expr::to_string(&value));
        }
        let added =
            std::fs::read_to_string(executor.host_path(self.dirs, &files[2])).unwrap_or_default();
        for dir in added.lines().map(str::trim).filter(|l| !l.is_empty()) {
            self.path_prefix = format!("{dir}:{}", self.path_prefix);
        }
        Ok(match outcome {
            Outcome::Exited(0) => Ran {
                success: true,
                outputs,
                timed_out: false,
            },
            Outcome::Exited(code) => {
                self.client.log(
                    number,
                    &format!("##[error]Process completed with exit code {code}."),
                );
                Ran {
                    success: false,
                    outputs,
                    timed_out: false,
                }
            }
            Outcome::Stopped if self.client.cancelled() => {
                self.client.log(number, "##[error]The job was cancelled.");
                Ran {
                    success: false,
                    outputs,
                    timed_out: false,
                }
            }
            Outcome::Stopped => Ran {
                success: false,
                outputs,
                timed_out: true,
            },
        })
    }

    /// The variables GitHub sets for every step.
    fn default_env(&self, executor: &Executor, files: &[String; 4]) -> BTreeMap<String, String> {
        let github = self
            .spec
            .contexts
            .get("github")
            .cloned()
            .unwrap_or(json!({}));
        let text = |key: &str| expr::to_string(&github[key]);
        let temp = executor.temp(self.dirs);
        let mut env = BTreeMap::new();
        let mut set = |k: &str, v: String| {
            env.insert(k.to_owned(), v);
        };
        set("CI", "true".into());
        set("GITHUB_ACTIONS", "true".into());
        set("OMAFORGE_ACTIONS", "true".into());
        set("GITHUB_WORKSPACE", executor.workspace(self.dirs));
        set("GITHUB_SHA", self.spec.sha.clone());
        set("GITHUB_REF", self.spec.git_ref.clone());
        for (var, key) in [
            ("GITHUB_REF_NAME", "ref_name"),
            ("GITHUB_REF_TYPE", "ref_type"),
            ("GITHUB_REPOSITORY", "repository"),
            ("GITHUB_REPOSITORY_OWNER", "repository_owner"),
            ("GITHUB_RUN_ID", "run_id"),
            ("GITHUB_RUN_NUMBER", "run_number"),
            ("GITHUB_RUN_ATTEMPT", "run_attempt"),
            ("GITHUB_JOB", "job"),
            ("GITHUB_ACTOR", "actor"),
            ("GITHUB_TRIGGERING_ACTOR", "triggering_actor"),
            ("GITHUB_EVENT_NAME", "event_name"),
            ("GITHUB_WORKFLOW", "workflow"),
            ("GITHUB_WORKFLOW_REF", "workflow_ref"),
            ("GITHUB_SERVER_URL", "server_url"),
            ("GITHUB_API_URL", "api_url"),
            ("GITHUB_GRAPHQL_URL", "graphql_url"),
            ("GITHUB_HEAD_REF", "head_ref"),
            ("GITHUB_BASE_REF", "base_ref"),
        ] {
            set(var, text(key));
        }
        set("GITHUB_EVENT_PATH", format!("{temp}/event.json"));
        set("GITHUB_OUTPUT", files[0].clone());
        set("GITHUB_ENV", files[1].clone());
        set("GITHUB_PATH", files[2].clone());
        set("GITHUB_STEP_SUMMARY", files[3].clone());
        set("RUNNER_OS", "Linux".into());
        set("RUNNER_ARCH", arch().into());
        set("RUNNER_NAME", "omaforge-runner".into());
        set("RUNNER_TEMP", temp);
        env
    }

    /// `uses:` steps. Only checking out the repository is built in so far.
    async fn uses(
        &mut self,
        executor: &Executor,
        number: usize,
        uses: &str,
        step: &Step,
        ctx: &Context,
        _env: &BTreeMap<String, String>,
    ) -> Result<Ran> {
        let action = uses.split('@').next().unwrap_or(uses).to_lowercase();
        if action != "actions/checkout" {
            bail!(
                "`uses: {uses}` isn't supported yet: omaforge-runner can't run actions. \
                 Only actions/checkout is built in; use `run:` steps for the rest."
            );
        }
        let with = |key: &str| -> Result<Option<String>> {
            step.with
                .get(key)
                .map(|v| match v {
                    Value::String(s) => expr::interpolate(s, ctx),
                    other => Ok(expr::to_string(other)),
                })
                .transpose()
        };
        if let Some(repository) = with("repository")? {
            if !repository.eq_ignore_ascii_case(&self.spec.repository) {
                bail!(
                    "actions/checkout can only check out this repository ({}), not {repository}",
                    self.spec.repository
                );
            }
        }
        let depth = match with("fetch-depth")?.as_deref() {
            None | Some("") => 1,
            Some(depth) => depth
                .parse::<u32>()
                .context("`fetch-depth` must be a number")?,
        };
        let rev = with("ref")?
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| self.spec.sha.clone());
        let dir = match with("path")? {
            Some(path) if !path.is_empty() => {
                if path.starts_with('/') || path.split('/').any(|p| p == "..") {
                    bail!("`path` must stay inside the workspace");
                }
                self.dirs.workspace.join(path)
            }
            _ => self.dirs.workspace.clone(),
        };
        std::fs::create_dir_all(&dir)?;
        let log = |line: &str| self.client.log(number, line);
        log(&format!("Checking out {} at {rev}", self.spec.repository));
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1");
            command
        };
        let header = format!("http.extraHeader=Authorization: Bearer {}", self.spec.token);
        let depth_arg = format!("--depth={depth}");
        let mut fetch_args = vec!["-c", header.as_str(), "fetch", "--no-tags", "--quiet"];
        if depth > 0 {
            fetch_args.push(&depth_arg);
        }
        fetch_args.extend(["origin", rev.as_str()]);
        for args in [
            vec!["init", "--quiet"],
            vec!["remote", "add", "origin", self.spec.clone_url.as_str()],
        ] {
            if exec::run_logged(git(&args), &log).await? != 0 {
                bail!("git {} failed", args[0]);
            }
        }
        if exec::run_logged(git(&fetch_args), &log).await? != 0 {
            bail!("couldn't fetch {rev}");
        }
        if exec::run_logged(
            git(&["checkout", "--quiet", "--force", "--detach", "FETCH_HEAD"]),
            &log,
        )
        .await?
            != 0
        {
            bail!("couldn't check out {rev}");
        }
        if let Some(branch) = self.spec.git_ref.strip_prefix("refs/heads/") {
            // Leave the branch checked out, as actions/checkout does for pushes.
            let _ = exec::run_logged(git(&["checkout", "--quiet", "-B", branch]), &log).await;
        }
        let _ = executor;
        Ok(Ran {
            success: true,
            outputs: Map::new(),
            timed_out: false,
        })
    }

    /// The job's `outputs`, evaluated now that its steps are done.
    fn outputs(&self, step: usize) -> Map<String, Value> {
        let mut ctx = self.base_context();
        ctx.set("steps", Value::Object(self.steps_ctx.clone()));
        let mut outputs = Map::new();
        for (key, value) in &self.spec.job.outputs {
            match self.evaluate(value, &ctx) {
                Ok(text) => {
                    outputs.insert(key.clone(), Value::String(text));
                }
                Err(error) => self
                    .client
                    .log(step, &format!("##[warning]Output {key}: {error:#}")),
            }
        }
        outputs
    }
}

struct Ran {
    success: bool,
    outputs: Map<String, Value>,
    timed_out: bool,
}

#[derive(PartialEq)]
enum StepEnd {
    Done,
    /// The job's own time limit ran out during this step.
    TimedOutJob,
}

fn env_value(env: &BTreeMap<String, String>) -> Value {
    Value::Object(
        env.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

fn runs_on(value: &Value, ctx: &Context) -> Vec<String> {
    let value = match value {
        Value::String(s) => expr::evaluate_value(s, ctx).unwrap_or(Value::Null),
        other => other.clone(),
    };
    match value {
        Value::Array(items) => items
            .iter()
            .map(|v| expr::to_string(v).to_lowercase())
            .collect(),
        other => vec![expr::to_string(&other).to_lowercase()],
    }
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "X64",
        "aarch64" => "ARM64",
        "x86" => "X86",
        "arm" => "ARM",
        other => other,
    }
}

/// The command for a `shell:` (with `{0}` for the script) and the script's extension.
fn shell_command(shell: Option<&str>, executor: &Executor) -> Result<(Vec<String>, &'static str)> {
    let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    Ok(match shell.map(str::trim) {
        None | Some("") => match executor {
            Executor::Host { .. } if which("bash") => (words("bash --noprofile --norc -eo pipefail {0}"), "sh"),
            Executor::Host { .. } => (words("sh -e {0}"), "sh"),
            // Containers may not have bash.
            Executor::Docker { .. } => (
                vec![
                    "sh".into(),
                    "-c".into(),
                    "if command -v bash >/dev/null 2>&1; then exec bash --noprofile --norc -eo pipefail \"$1\"; else exec sh -e \"$1\"; fi".into(),
                    "sh".into(),
                    "{0}".into(),
                ],
                "sh",
            ),
        },
        Some("bash") => (words("bash --noprofile --norc -eo pipefail {0}"), "sh"),
        Some("sh") => (words("sh -e {0}"), "sh"),
        Some("python") => (words("python {0}"), "py"),
        Some("pwsh") => (vec!["pwsh".into(), "-command".into(), ". '{0}'".into()], "ps1"),
        Some(custom) if custom.contains("{0}") => (words(custom), "sh"),
        Some(custom) => bail!("shell `{custom}` must include {{0}} where the script's path goes"),
    })
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Workflow commands in a step's output (`::add-mask::`, `::set-output::`), then log it.
fn handle_line(client: &JobClient, step: usize, line: String, outputs: &mut Map<String, Value>) {
    if let Some(command) = line.strip_prefix("::") {
        if let Some(secret) = command.strip_prefix("add-mask::") {
            client.mask(secret.trim());
            return;
        }
        if let Some(rest) = command.strip_prefix("set-output name=") {
            if let Some((name, value)) = rest.split_once("::") {
                outputs.insert(name.to_owned(), Value::String(value.to_owned()));
                return;
            }
        }
    }
    client.log(step, &line);
}

/// `NAME=value` lines, or `NAME<<DELIMITER` blocks, as in `GITHUB_OUTPUT` and `GITHUB_ENV`.
fn read_env_file(path: &Path) -> Result<Map<String, Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e.into()),
    };
    parse_env_file(&text)
}

fn parse_env_file(text: &str) -> Result<Map<String, Value>> {
    let mut values = Map::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((name, delimiter)) = line.split_once("<<") {
            let mut value = Vec::new();
            let mut closed = false;
            for next in lines.by_ref() {
                if next == delimiter {
                    closed = true;
                    break;
                }
                value.push(next);
            }
            if !closed {
                bail!("`{name}<<{delimiter}` is never closed by a line with just {delimiter}");
            }
            values.insert(name.to_owned(), Value::String(value.join("\n")));
        } else if let Some((name, value)) = line.split_once('=') {
            values.insert(name.to_owned(), Value::String(value.to_owned()));
        } else {
            bail!("invalid line {line:?}: expected NAME=value or NAME<<DELIMITER");
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_output_files() {
        let parsed = parse_env_file("a=1\nb=x=y\n\nnotes<<EOF\nline 1\nline 2\nEOF\nc=\n").unwrap();
        assert_eq!(
            Value::Object(parsed),
            json!({ "a": "1", "b": "x=y", "notes": "line 1\nline 2", "c": "" })
        );
        assert!(parse_env_file("x<<EOF\nnever closed\n").is_err());
        assert!(parse_env_file("no equals sign\n").is_err());
    }
}
