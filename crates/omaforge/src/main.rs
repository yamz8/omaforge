mod auth;
mod db;
mod git_http;
mod git_service;
mod git_ssh;
mod import;
mod web;

use std::{
    io::BufRead,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
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
        /// URL people use to reach this server, for clone URLs (default: http://<listen>).
        #[arg(long, env = "OMAFORGE_PUBLIC_URL")]
        public_url: Option<String>,
        /// UI colors: "omarchy" (the active Omarchy theme, live), "builtin", or a path to
        /// an Omarchy colors.toml. Defaults to Omarchy's theme when available.
        #[arg(long, env = "OMAFORGE_THEME")]
        theme: Option<String>,
    },
    /// Administrative tasks.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Bring existing repositories into omaforge.
    #[command(subcommand)]
    Import(ImportCommand),
}

#[derive(Subcommand)]
enum ImportCommand {
    /// Import git repositories from folders on this machine. Each folder becomes a repo
    /// named after it, with its branches, tags and any branches only on `origin`.
    Local {
        /// Repository folders (working trees or bare repositories).
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// User who will own the imported repositories.
        #[arg(long)]
        owner: String,
        /// Make the repositories public (they are private by default).
        #[arg(long)]
        public: bool,
        /// Fetch new commits into repositories imported before, instead of skipping them.
        /// Branches only fast-forward; nothing is overwritten.
        #[arg(long)]
        update: bool,
        /// Show what would be imported without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
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
    /// Set a repository's description, given as owner/name.
    SetDescription { repo: String, description: String },
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
    pub site: Arc<web::Site>,
}

/// Names that can't be usernames because they are top-level pages.
const RESERVED_NAMES: &[&str] = &[
    "about",
    "admin",
    "api",
    "assets",
    "explore",
    "health",
    "help",
    "login",
    "logout",
    "new",
    "notifications",
    "org",
    "orgs",
    "settings",
    "signup",
    "static",
    "user",
    "users",
];

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
        Command::Serve {
            listen,
            ssh_listen,
            public_url,
            theme,
        } => {
            let http_url = public_url
                .unwrap_or_else(|| format!("http://{listen}"))
                .trim_end_matches('/')
                .to_owned();
            let site = web::Site {
                ssh: Some((url_host(&http_url), ssh_listen.port())),
                http_url,
                theme: web::Theme::from_arg(theme.as_deref())?,
            };
            tracing::info!(theme = ?site.theme, "UI theme");
            let state = AppState {
                site: Arc::new(site),
                ..state
            };
            serve(&cli.data, state, listen, ssh_listen).await
        }
        Command::Admin(command) => admin(&state, command).await,
        Command::Import(ImportCommand::Local {
            paths,
            owner,
            public,
            update,
            dry_run,
        }) => {
            let options = import::Options {
                owner,
                public,
                update,
                dry_run,
            };
            import::local(&state, &paths, &options).await
        }
    }
}

async fn open(data: &Path) -> Result<AppState> {
    let repos = data.join("repos");
    std::fs::create_dir_all(&repos).with_context(|| format!("creating {}", repos.display()))?;
    std::fs::create_dir_all(data.join("attachments"))?;
    let db = db::open(data).await?;
    let site = web::Site {
        http_url: "http://127.0.0.1:3000".into(),
        ssh: None,
        theme: web::Theme::Builtin,
    };
    Ok(AppState {
        db,
        repos,
        site: Arc::new(site),
    })
}

async fn serve(
    data: &Path,
    state: AppState,
    listen: SocketAddr,
    ssh_listen: SocketAddr,
) -> Result<()> {
    let host_key = git_ssh::load_or_create_host_key(data)?;
    let ssh = git_ssh::serve(state.clone(), ssh_listen, host_key);

    web::render::warm_up();
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(git_http::routes())
        .merge(web::routes())
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
            if RESERVED_NAMES.contains(&username.to_ascii_lowercase().as_str()) {
                bail!("{username:?} is reserved");
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
            let (owner, name) = split_repo(&repo)?;
            let owner = find_user(state, owner).await?;
            db::create_repo(&state.db, &state.repos, &owner, name, "", private).await?;
            println!("created {}/{name}", owner.username);
        }
        AdminCommand::SetDescription { repo, description } => {
            let (owner, name) = split_repo(&repo)?;
            let owner = find_user(state, owner).await?;
            if db::repo_by_path(&state.db, &owner.username, name)
                .await?
                .is_none()
            {
                bail!("no such repository {repo:?}");
            }
            db::set_repo_description(&state.db, owner.id, name, description.trim()).await?;
            println!("updated {}/{name}", owner.username);
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

fn split_repo(repo: &str) -> Result<(&str, &str)> {
    repo.split_once('/')
        .with_context(|| format!("expected owner/name, got {repo:?}"))
}

async fn find_user(state: &AppState, username: &str) -> Result<db::User> {
    db::user_by_username(&state.db, username)
        .await?
        .with_context(|| format!("no such user {username:?}"))
}

/// The host part of a URL like `https://forge.example.com:8443/path`.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    match authority.strip_prefix('[') {
        // [::1]:3000
        Some(v6) => format!("[{}]", v6.split(']').next().unwrap_or(v6)),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
            .to_owned(),
    }
}
