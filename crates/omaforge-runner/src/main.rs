//! `omaforge-runner`: claims CI jobs from an omaforge server and runs them, on this machine
//! or in Docker containers.
//!
//! Each label the runner offers says where jobs with it run: `--label self-hosted=host`
//! runs them directly here, `--label ubuntu-latest=docker://ubuntu:24.04` in a container.
//! A job runs if the runner has every label in its `runs-on`.

mod client;
mod exec;
mod job;

use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Result};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "omaforge-runner",
    version,
    about = "Runs CI jobs for an omaforge server"
)]
struct Cli {
    /// The omaforge server's URL.
    #[arg(long, env = "OMAFORGE_RUNNER_SERVER")]
    server: String,
    /// The runner's token, from `omaforge admin create-runner` or the admin settings page.
    #[arg(long, env = "OMAFORGE_RUNNER_TOKEN", hide_env_values = true)]
    token: String,
    /// A label to accept, and where its jobs run: `NAME=host` or `NAME=docker://IMAGE`.
    /// Repeat for more. Without any, the runner accepts `ubuntu-latest`, `ubuntu-24.04`
    /// and `ubuntu-22.04` and runs them in matching Ubuntu containers.
    #[arg(long = "label", value_name = "NAME=WHERE")]
    labels: Vec<String>,
    /// Where jobs check out code and keep temporary files.
    #[arg(long, env = "OMAFORGE_RUNNER_WORK", default_value = "./runner-work")]
    work: PathBuf,
    /// Run one job, then exit.
    #[arg(long)]
    once: bool,
}

/// Where a label's jobs run.
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    Host,
    Docker(String),
}

#[derive(Debug, Clone)]
pub struct Label {
    pub name: String,
    pub place: Place,
}

fn parse_label(text: &str) -> Result<Label> {
    let Some((name, place)) = text.split_once('=') else {
        bail!("label {text:?} must say where its jobs run: {text}=host or {text}=docker://IMAGE");
    };
    let name = name.trim().to_lowercase();
    if name.is_empty() || name.contains(',') {
        bail!("invalid label name in {text:?}");
    }
    let place = match place.trim() {
        "host" => Place::Host,
        other => match other.strip_prefix("docker://") {
            Some(image) if !image.is_empty() && !image.starts_with('-') => {
                Place::Docker(image.to_owned())
            }
            _ => bail!("label {text:?} must end in =host or =docker://IMAGE"),
        },
    };
    Ok(Label { name, place })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let labels: Vec<Label> = if cli.labels.is_empty() {
        [
            ("ubuntu-latest", "ubuntu:24.04"),
            ("ubuntu-24.04", "ubuntu:24.04"),
            ("ubuntu-22.04", "ubuntu:22.04"),
        ]
        .into_iter()
        .map(|(name, image)| Label {
            name: name.into(),
            place: Place::Docker(image.into()),
        })
        .collect()
    } else {
        cli.labels
            .iter()
            .map(|l| parse_label(l))
            .collect::<Result<_>>()?
    };
    std::fs::create_dir_all(&cli.work)?;
    let work = std::fs::canonicalize(&cli.work)?;
    let server = cli.server.trim_end_matches('/').to_owned();
    let client = client::Runner::new(&server, &cli.token)?;
    tracing::info!(
        server,
        labels = %labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", "),
        "waiting for jobs"
    );

    let mut failures = 0u32;
    loop {
        match client.claim(&labels).await {
            Ok(Some(spec)) => {
                failures = 0;
                tracing::info!(job = spec.id, name = %spec.name, repo = %spec.repository, "running job");
                let conclusion = job::run(&client, spec, &labels, &work).await;
                tracing::info!(?conclusion, "job finished");
                if cli.once {
                    return Ok(());
                }
            }
            Ok(None) => failures = 0,
            Err(error) => {
                failures += 1;
                let wait = Duration::from_secs(u64::from(failures.min(6)) * 5);
                tracing::warn!(
                    "couldn't reach the server ({error:#}); retrying in {}s",
                    wait.as_secs()
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}
