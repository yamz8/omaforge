mod auth;
mod db;
mod git_http;
mod git_service;
mod git_ssh;

use std::{
    io::BufRead,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use axum::{routing::get, Router};
use clap::{Parser, Subcommand};
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;

#[derive(Parser)]
#[command(name = "omaforge", version, about = "A self-hosted git forge")]
struct Cli {
    /// Directory holding repos, the database and attachments.
    #[arg(long, env = "OMAFORGE_DATA", default_value = "./data", global = true)]
    data: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the data directory and database.
    Init,
    /// Run the web, git and API server.
    Serve {
        #[arg(long, env = "OMAFORGE_LISTEN", default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// Address for git over SSH.
        #[arg(long, env = "OMAFORGE_SSH_LISTEN", default_value = "127.0.0.1:2222")]
        ssh_listen: SocketAddr,
    },
    /// Administrative tasks.
    #[command(subcommand)]
    Admin(AdminCommand),
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Create a user. Prompts for the password unless --password-stdin is given.
    CreateUser {
        username: String,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        admin: bool,
        /// Read the password from the first line of stdin.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Create a personal access token and print it.
    CreateToken { username: String, name: String },
    /// Create a repository, given as owner/name.
    CreateRepo {
        repo: String,
        #[arg(long)]
        private: bool,
    },
    /// Add an SSH public key for a user, read from --key-file or stdin.
    AddSshKey {
        username: String,
        name: String,
        /// Path to an OpenSSH public key, e.g. ~/.ssh/id_ed25519.pub.
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    /// Root directory of bare repositories.
    pub repos: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let state = open(&cli.data).await?;
    match cli.command {
        Command::Init => {
            tracing::info!(path = %cli.data.display(), "initialized data directory");
            Ok(())
        }
        Command::Serve { listen, ssh_listen } => serve(&cli.data, state, listen, ssh_listen).await,
        Command::Admin(command) => admin(&state, command).await,
    }
}

async fn open(data: &Path) -> Result<AppState> {
    let repos = data.join("repos");
    std::fs::create_dir_all(&repos).with_context(|| format!("creating {}", repos.display()))?;
    std::fs::create_dir_all(data.join("attachments"))?;
    let db = db::open(data).await?;
    Ok(AppState { db, repos })
}

async fn serve(
    data: &Path,
    state: AppState,
    listen: SocketAddr,
    ssh_listen: SocketAddr,
) -> Result<()> {
    let host_key = git_ssh::load_or_create_host_key(data)?;
    let ssh = git_ssh::serve(state.clone(), ssh_listen, host_key);

    let app = Router::new()
        .route("/", get(|| async { "omaforge" }))
        .route("/health", get(|| async { "ok" }))
        .merge(git_http::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "HTTP listening");
    let http = async { anyhow::Ok(axum::serve(listener, app).await?) };

    tokio::try_join!(http, ssh)?;
    Ok(())
}

async fn admin(state: &AppState, command: AdminCommand) -> Result<()> {
    match command {
        AdminCommand::CreateUser {
            username,
            email,
            admin,
            password_stdin,
        } => {
            if !forge_git::is_valid_name(&username) {
                bail!("invalid username {username:?}");
            }
            let password = if password_stdin {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            } else {
                rpassword::prompt_password("Password: ")?
            };
            if password.len() < 8 {
                bail!("password must be at least 8 characters");
            }
            let hash = auth::hash_password(&password)?;
            db::create_user(&state.db, &username, email.as_deref(), &hash, admin).await?;
            println!("created user {username}");
        }
        AdminCommand::CreateToken { username, name } => {
            let user = find_user(state, &username).await?;
            let (token, hash) = auth::new_token()?;
            db::create_token(&state.db, user.id, &name, &hash).await?;
            println!("{token}");
        }
        AdminCommand::CreateRepo { repo, private } => {
            let Some((owner, name)) = repo.split_once('/') else {
                bail!("expected owner/name, got {repo:?}");
            };
            let owner = find_user(state, owner).await?;
            db::create_repo(&state.db, &state.repos, &owner, name, private).await?;
            println!("created {}/{name}", owner.username);
        }
        AdminCommand::AddSshKey {
            username,
            name,
            key_file,
        } => {
            let user = find_user(state, &username).await?;
            let line = match key_file {
                Some(path) => std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
                None => std::io::read_to_string(std::io::stdin())?,
            };
            let (public_key, fingerprint) = git_ssh::parse_public_key(&line)?;
            db::add_ssh_key(&state.db, user.id, &name, &public_key, &fingerprint).await?;
            println!("added {fingerprint} for {}", user.username);
        }
    }
    Ok(())
}

async fn find_user(state: &AppState, username: &str) -> Result<db::User> {
    db::user_by_username(&state.db, username)
        .await?
        .with_context(|| format!("no such user {username:?}"))
}
