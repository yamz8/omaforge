//! Running a step's command on this machine or in the job's container.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::mpsc,
};

/// Where the job keeps its checkout and temporary files, on this machine.
pub struct Dirs {
    pub workspace: PathBuf,
    pub temp: PathBuf,
}

const CONTAINER_WORKSPACE: &str = "/__w/workspace";
const CONTAINER_TEMP: &str = "/__w/_temp";

pub enum Executor {
    Host {
        /// Process groups started, to stop anything left running when the job ends.
        groups: Mutex<Vec<i32>>,
    },
    Docker {
        container: String,
    },
}

pub enum Outcome {
    Exited(i32),
    /// Stopped by a cancellation or timeout.
    Stopped,
}

impl Executor {
    pub fn host() -> Self {
        Executor::Host {
            groups: Mutex::new(Vec::new()),
        }
    }

    /// Start the job's container, pulling the image if needed.
    pub async fn docker(
        image: &str,
        name: &str,
        dirs: &Dirs,
        env: &BTreeMap<String, String>,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<Self> {
        let present = Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("running docker (is it installed, and may this user use it?)")?
            .success();
        if !present {
            log(&format!("Pulling {image}"));
            let mut pull = Command::new("docker");
            pull.args(["pull", "--quiet", image]);
            if run_logged(pull, log).await? != 0 {
                bail!("couldn't pull the image {image}");
            }
        }
        let mut command = Command::new("docker");
        command
            .args([
                "run",
                "--detach",
                "--name",
                name,
                "--label",
                "omaforge-runner=1",
            ])
            .arg("--volume")
            .arg(format!(
                "{}:{CONTAINER_WORKSPACE}",
                dirs.workspace.display()
            ))
            .arg("--volume")
            .arg(format!("{}:{CONTAINER_TEMP}", dirs.temp.display()))
            .args(["--workdir", CONTAINER_WORKSPACE]);
        for (key, value) in env {
            command.arg("--env").arg(key).env(key, value);
        }
        command.args(["--entrypoint", "tail", image, "-f", "/dev/null"]);
        let output = command.stdin(Stdio::null()).output().await?;
        if !output.status.success() {
            bail!(
                "couldn't start a container from {image}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        log(&format!("Started container {name} from {image}"));
        Ok(Executor::Docker {
            container: name.to_owned(),
        })
    }

    /// The workspace as steps see it.
    pub fn workspace(&self, dirs: &Dirs) -> String {
        match self {
            Executor::Host { .. } => dirs.workspace.display().to_string(),
            Executor::Docker { .. } => CONTAINER_WORKSPACE.into(),
        }
    }

    pub fn temp(&self, dirs: &Dirs) -> String {
        match self {
            Executor::Host { .. } => dirs.temp.display().to_string(),
            Executor::Docker { .. } => CONTAINER_TEMP.into(),
        }
    }

    /// Turn a path steps see into one on this machine (for files under the temp dir or
    /// workspace).
    pub fn host_path(&self, dirs: &Dirs, path: &str) -> PathBuf {
        match self {
            Executor::Host { .. } => PathBuf::from(path),
            Executor::Docker { .. } => {
                if let Some(rest) = path.strip_prefix(CONTAINER_TEMP) {
                    dirs.temp.join(rest.trim_start_matches('/'))
                } else if let Some(rest) = path.strip_prefix(CONTAINER_WORKSPACE) {
                    dirs.workspace.join(rest.trim_start_matches('/'))
                } else {
                    PathBuf::from(path)
                }
            }
        }
    }

    /// Run `argv` in `cwd` (as steps see it) with `env` plus `path_prefix` in front of
    /// `PATH`, passing each output line to `on_line`, until it exits or `stop` resolves.
    pub async fn run(
        &self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        path_prefix: &str,
        cwd: &str,
        mut on_line: impl FnMut(String),
        stop: impl Future<Output = ()>,
    ) -> Result<Outcome> {
        let mut command = match self {
            Executor::Host { .. } => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]).envs(env).current_dir(cwd);
                if !path_prefix.is_empty() {
                    let path = std::env::var("PATH").unwrap_or_default();
                    command.env("PATH", format!("{path_prefix}{path}"));
                }
                command.process_group(0);
                command
            }
            Executor::Docker { container } => {
                let mut command = Command::new("docker");
                command.args(["exec", "--workdir", cwd]);
                for (key, value) in env {
                    // `--env NAME` takes the value from docker's own environment, which
                    // keeps values out of the process list and allows newlines.
                    command.arg("--env").arg(key).env(key, value);
                }
                command
                    .arg("--env")
                    .arg("OMAFORGE_PATH_PREFIX")
                    .env("OMAFORGE_PATH_PREFIX", path_prefix);
                command.arg(container).args([
                    "sh",
                    "-c",
                    concat!(
                        "echo $$ > /__w/_temp/.step-pid; ",
                        "PATH=\"$OMAFORGE_PATH_PREFIX$PATH\"; export PATH; unset OMAFORGE_PATH_PREFIX; ",
                        "exec \"$@\""
                    ),
                    "sh",
                ]);
                command.args(argv);
                command
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                on_line(format!("{}: command not found", argv[0]));
                return Ok(Outcome::Exited(127));
            }
            Err(error) => return Err(error.into()),
        };
        if let (Executor::Host { groups }, Some(pid)) = (self, child.id()) {
            groups.lock().unwrap().push(pid as i32);
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        if let Some(out) = child.stdout.take() {
            tokio::spawn(read_lines(out, tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(read_lines(err, tx));
        }

        let mut stop = std::pin::pin!(stop);
        let mut stopped = false;
        let mut exit = None;
        // After the process exits, give its output a moment to arrive; something it
        // left running in the background may hold the pipes open.
        let drain = tokio::time::sleep(Duration::from_secs(3600));
        tokio::pin!(drain);
        loop {
            tokio::select! {
                line = rx.recv() => match line {
                    Some(line) => on_line(line),
                    None => break,
                },
                status = child.wait(), if exit.is_none() => {
                    exit = Some(status?);
                    drain.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
                }
                _ = &mut drain, if exit.is_some() => break,
                _ = &mut stop, if !stopped && exit.is_none() => {
                    stopped = true;
                    self.interrupt(child.id()).await;
                }
            }
        }
        let status = match exit {
            Some(status) => status,
            None => child.wait().await?,
        };
        if stopped {
            return Ok(Outcome::Stopped);
        }
        Ok(Outcome::Exited(status.code().unwrap_or(1)))
    }

    /// Stop a running step: politely, then by force.
    async fn interrupt(&self, pid: Option<u32>) {
        match self {
            Executor::Host { .. } => {
                if let Some(pid) = pid {
                    let group = pid as i32;
                    // SAFETY: signalling a process group we started.
                    unsafe { libc::kill(-group, libc::SIGTERM) };
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        unsafe { libc::kill(-group, libc::SIGKILL) };
                    });
                }
            }
            Executor::Docker { container } => {
                let _ = Command::new("docker")
                    .args([
                        "exec",
                        container,
                        "sh",
                        "-c",
                        "kill -TERM $(cat /__w/_temp/.step-pid) 2>/dev/null",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                let container = container.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = Command::new("docker")
                        .args(["kill", &container])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                });
            }
        }
    }

    /// Stop whatever the job left running and remove its container.
    pub async fn stop(&self, log: &(dyn Fn(&str) + Sync)) {
        match self {
            Executor::Host { groups } => {
                let groups = std::mem::take(&mut *groups.lock().unwrap());
                for group in groups {
                    // SAFETY: signalling process groups we started; gone ones are ignored.
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                }
            }
            Executor::Docker { container } => {
                // Files the container's root user made must stay removable.
                // SAFETY: getuid/getgid can't fail.
                let owner = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };
                let _ = Command::new("docker")
                    .args([
                        "exec",
                        container,
                        "chown",
                        "-R",
                        &owner,
                        CONTAINER_WORKSPACE,
                        CONTAINER_TEMP,
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                let removed = Command::new("docker")
                    .args(["rm", "--force", container])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|s| s.success());
                if removed {
                    log(&format!("Removed container {container}"));
                } else {
                    log(&format!("Couldn't remove container {container}"));
                }
            }
        }
    }
}

async fn read_lines(stream: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<String>) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\n', '\r']);
                if tx.send(text.to_owned()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Run a helper command (git, docker pull) on this machine, logging its output.
pub async fn run_logged(mut command: Command, log: &(dyn Fn(&str) + Sync)) -> Result<i32> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {:?}", command.as_std().get_program()))?;
    for line in String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
    {
        log(line);
    }
    Ok(output.status.code().unwrap_or(1))
}

pub fn remove_dir(path: &Path) {
    if path.exists() {
        if let Err(error) = std::fs::remove_dir_all(path) {
            tracing::warn!("couldn't remove {}: {error}", path.display());
        }
    }
}
