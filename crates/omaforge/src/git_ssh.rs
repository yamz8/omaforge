//! Git over SSH: a built-in SSH server that authenticates users by public key and runs
//! `git upload-pack` / `git receive-pack` for `ssh git@host owner/repo.git` requests.
//! There is no shell; anything other than those two commands is refused.

use std::{
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use russh::{
    keys::{
        ssh_key::{private::Ed25519Keypair, LineEnding},
        HashAlg, PrivateKey, PublicKey,
    },
    server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Server, Session},
    Channel, ChannelId, MethodKind, MethodSet,
};
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    db::{self, User},
    git_service::{self, Decision, Service},
    AppState,
};

/// Load the server's host key from the data directory, generating it on first run.
pub fn load_or_create_host_key(data: &Path) -> Result<PrivateKey> {
    let path = data.join("ssh_host_ed25519_key");
    if path.exists() {
        return russh::keys::load_secret_key(&path, None)
            .with_context(|| format!("loading SSH host key {}", path.display()));
    }

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("generating SSH host key: {e}"))?;
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(key.to_openssh(LineEnding::LF)?.as_bytes())?;
    tracing::info!(
        fingerprint = %key.public_key().fingerprint(HashAlg::Sha256),
        "generated SSH host key"
    );
    Ok(key)
}

/// Parse a user-supplied OpenSSH public key line, returning `(key_without_comment, fingerprint)`.
pub fn parse_public_key(line: &str) -> Result<(String, String)> {
    let mut key = PublicKey::from_openssh(line.trim()).context("not a valid OpenSSH public key")?;
    key.set_comment("");
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    Ok((key.to_openssh()?.trim().to_owned(), fingerprint))
}

pub async fn serve(state: AppState, listen: SocketAddr, host_key: PrivateKey) -> Result<()> {
    let config = Arc::new(Config {
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        keys: vec![host_key],
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(600)),
        ..Default::default()
    });
    tracing::info!(%listen, "SSH listening");
    let mut server = SshServer { state };
    server.run_on_address(config, listen).await?;
    Ok(())
}

struct SshServer {
    state: AppState,
}

impl Server for SshServer {
    type Handler = Connection;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Connection {
        Connection {
            state: self.state.clone(),
            peer,
            user: None,
            channels: HashMap::new(),
            git_protocol: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: anyhow::Error) {
        tracing::debug!(%error, "SSH session ended with an error");
    }
}

struct Connection {
    state: AppState,
    peer: Option<SocketAddr>,
    user: Option<User>,
    /// Session channels opened but not yet given a command.
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// `GIT_PROTOCOL` sent by the client per channel (for protocol v2).
    git_protocol: HashMap<ChannelId, String>,
}

impl Connection {
    async fn user_for_key(&self, key: &PublicKey) -> Result<Option<User>> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        db::user_by_ssh_fingerprint(&self.state.db, &fingerprint).await
    }
}

impl Handler for Connection {
    type Error = anyhow::Error;

    async fn auth_publickey_offered(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        Ok(match self.user_for_key(key).await? {
            Some(_) => Auth::Accept,
            None => Auth::reject(),
        })
    }

    // Called once the client has proven it holds the private key. The SSH username is
    // ignored (everyone connects as `git@`); the key decides who you are.
    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        let Some(user) = self.user_for_key(key).await? else {
            return Ok(Auth::reject());
        };
        db::touch_ssh_key(
            &self.state.db,
            &key.fingerprint(HashAlg::Sha256).to_string(),
        )
        .await?;
        tracing::debug!(user = %user.username, peer = ?self.peer, "SSH login");
        self.user = Some(user);
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        _session: &mut Session,
    ) -> Result<()> {
        if name == "GIT_PROTOCOL" {
            self.git_protocol.insert(channel, value.to_owned());
        }
        Ok(())
    }

    async fn shell_request(&mut self, id: ChannelId, session: &mut Session) -> Result<()> {
        session.channel_success(id)?;
        let username = self.user.as_ref().map_or("there", |u| u.username.as_str());
        let message = format!(
            "Hi {username}! You've successfully authenticated, but omaforge does not provide shell access.\n"
        );
        if let Some(channel) = self.channels.remove(&id) {
            tokio::spawn(finish(channel, message, 1));
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let Some(channel) = self.channels.remove(&id) else {
            session.channel_failure(id)?;
            return Ok(());
        };
        session.channel_success(id)?;

        let command = String::from_utf8_lossy(command);
        let Some((service, owner, repo)) = parse_command(&command) else {
            tokio::spawn(finish(
                channel,
                format!("omaforge: unsupported command: {command}\n"),
                1,
            ));
            return Ok(());
        };
        let decision =
            git_service::authorize(&self.state, self.user.as_ref(), owner, repo, service).await?;
        let path = match decision {
            Decision::Allowed(path) => path,
            Decision::ReadOnly => {
                let message = "omaforge: you do not have write access to this repository\n";
                tokio::spawn(finish(channel, message.into(), 1));
                return Ok(());
            }
            Decision::NeedsLogin | Decision::NotFound => {
                let message = format!("omaforge: repository '{owner}/{repo}' not found\n");
                tokio::spawn(finish(channel, message, 1));
                return Ok(());
            }
        };

        let protocol = self.git_protocol.remove(&id);
        tokio::spawn(async move {
            if let Err(error) = run_git(channel, service, path, protocol).await {
                tracing::warn!(%error, "{} over SSH failed", service.name());
            }
        });
        Ok(())
    }
}

/// Pipe the channel to and from a git process, then report its exit status.
async fn run_git(
    channel: Channel<Msg>,
    service: Service,
    path: PathBuf,
    protocol: Option<String>,
) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg(service.subcommand())
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(protocol) = protocol {
        command.env("GIT_PROTOCOL", protocol);
    }
    let mut child = command.spawn()?;
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(anyhow!("git process is missing a pipe"));
    };

    let (mut read_half, write_half) = channel.split();
    // The client only closes its side once it's done, which for a fetch may be after git
    // has already exited, so stdin is pumped separately and dropped when git finishes.
    let input = tokio::spawn(async move {
        let mut reader = read_half.make_reader();
        let _ = tokio::io::copy(&mut reader, &mut stdin).await;
    });

    let mut out = write_half.make_writer();
    let mut err = write_half.make_writer_ext(Some(1));
    let (copied_out, copied_err) = tokio::join!(
        tokio::io::copy(&mut stdout, &mut out),
        tokio::io::copy(&mut stderr, &mut err),
    );
    let status = child.wait().await?;
    input.abort();
    copied_out?;
    copied_err?;

    err.flush().await?;
    // Shutting down the stdout writer sends EOF.
    out.shutdown().await?;
    let code = status.code().unwrap_or(1);
    write_half
        .exit_status(u32::try_from(code).unwrap_or(1))
        .await?;
    write_half.close().await?;
    Ok(())
}

/// Write `message` to the client's stderr and end the channel with `code`.
async fn finish(channel: Channel<Msg>, message: String, code: u32) {
    let _ = channel.extended_data(1, message.as_bytes()).await;
    let _ = channel.exit_status(code).await;
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

/// Parse `git-upload-pack 'owner/repo.git'` (as sent by git) into its parts.
fn parse_command(command: &str) -> Option<(Service, &str, &str)> {
    let (program, argument) = command.trim().split_once(' ')?;
    let service = Service::from_name(program)?;
    let argument = argument.trim();
    let path = match argument.strip_prefix('\'') {
        Some(quoted) => quoted.strip_suffix('\'')?,
        None => argument,
    };
    let (owner, repo) = path.trim_start_matches('/').split_once('/')?;
    Some((service, owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_commands() {
        assert_eq!(
            parse_command("git-upload-pack 'alice/hello.git'"),
            Some((Service::UploadPack, "alice", "hello.git"))
        );
        assert_eq!(
            parse_command("git-receive-pack '/alice/hello'"),
            Some((Service::ReceivePack, "alice", "hello"))
        );
        assert_eq!(
            parse_command("git-upload-pack alice/hello.git"),
            Some((Service::UploadPack, "alice", "hello.git"))
        );
        // Anything else is not ours to run. Bad names are rejected later by authorize().
        assert_eq!(parse_command("ls -la"), None);
        assert_eq!(parse_command("git-upload-archive 'alice/hello'"), None);
        assert_eq!(parse_command("git-upload-pack 'alice/hello"), None);
        assert_eq!(parse_command("git-upload-pack 'hello'"), None);
    }
}
