//! Job logs, one file per step under `<data>/ci/logs/<job>/`.

use std::path::PathBuf;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::AppState;

/// More than this per step is dropped (with a note saying so).
const MAX_STEP_LOG: u64 = 8 * 1024 * 1024;
const TRUNCATED: &str = "\n[omaforge: the rest of this step's log was dropped; it's over 8 MB]\n";

fn dir(state: &AppState, job_id: i64) -> PathBuf {
    state.data.join("ci").join("logs").join(job_id.to_string())
}

fn path(state: &AppState, job_id: i64, step: usize) -> PathBuf {
    dir(state, job_id).join(format!("{step}.log"))
}

/// Add output to a step's log. Problems are logged, not returned: logs never fail a job.
pub async fn append(state: &AppState, job_id: i64, step: usize, text: &str) {
    if let Err(error) = try_append(state, job_id, step, text).await {
        tracing::warn!(job_id, step, "couldn't write the job log: {error:#}");
    }
}

async fn try_append(state: &AppState, job_id: i64, step: usize, text: &str) -> Result<()> {
    tokio::fs::create_dir_all(dir(state, job_id)).await?;
    let path = path(state, job_id, step);
    let size = tokio::fs::metadata(&path).await.map_or(0, |m| m.len());
    if size >= MAX_STEP_LOG {
        return Ok(());
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let room = (MAX_STEP_LOG - size) as usize;
    if text.len() <= room {
        file.write_all(text.as_bytes()).await?;
    } else {
        let mut cut = room.saturating_sub(TRUNCATED.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        file.write_all(&text.as_bytes()[..cut]).await?;
        file.write_all(TRUNCATED.as_bytes()).await?;
    }
    Ok(())
}

/// A step's log from byte `offset`, and the offset to continue from.
pub async fn read(
    state: &AppState,
    job_id: i64,
    step: usize,
    offset: u64,
) -> Result<(String, u64)> {
    let mut file = match tokio::fs::File::open(path(state, job_id, step)).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata().await?.len();
    let offset = offset.min(len);
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    // Stop before a partial UTF-8 character; the next read picks it up.
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => bytes.len(),
    };
    let text = String::from_utf8_lossy(&bytes[..valid]).into_owned();
    Ok((text, offset + valid as u64))
}

/// Delete the logs of these jobs.
pub async fn remove(state: &AppState, job_ids: &[i64]) {
    for &id in job_ids {
        let dir = dir(state, id);
        if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(job_id = id, "couldn't delete the job's logs: {error}");
            }
        }
    }
}

/// The whole job's log as one text, with a header per step.
pub async fn full(state: &AppState, job_id: i64, steps: &[crate::db::CiStep]) -> Result<String> {
    let mut out = String::new();
    for step in steps {
        let (text, _) = read(state, job_id, step.number as usize, 0).await?;
        out.push_str(&format!("##[group]{}\n", step.name));
        out.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("##[endgroup]\n");
    }
    Ok(out)
}
