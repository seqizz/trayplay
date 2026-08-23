use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::jellyfin::{auth, Client, FileStore, TokenStore};

#[derive(Debug, Parser)]
#[command(name = "trayplay", version, about = "Systray Jellyfin music player")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Authenticate against Jellyfin and store the access token
    Login {
        /// Server URL; falls back to `server` in config.toml
        #[arg(long)]
        server: Option<String>,
        /// Username; falls back to `username` in config.toml
        #[arg(long)]
        username: Option<String>,
    },
    /// Discard the stored access token
    Logout,
    /// Print a batch of random tracks; verifies auth and queries without the UI
    DumpRandom {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

/// Runs a CLI subcommand on the given runtime. Returns Ok(()) when handled.
pub fn run(cmd: Command, cfg: &Config, rt: &tokio::runtime::Runtime) -> Result<()> {
    match cmd {
        Command::Login { server, username } => login(cfg, rt, server, username),
        Command::Logout => {
            FileStore::new()?.clear()?;
            println!("Stored credentials removed.");
            Ok(())
        }
        Command::DumpRandom { limit } => dump_random(rt, limit),
    }
}

fn login(
    cfg: &Config,
    rt: &tokio::runtime::Runtime,
    server: Option<String>,
    username: Option<String>,
) -> Result<()> {
    let server = server
        .or_else(|| cfg.server.clone())
        .context("no server given; pass --server or set `server` in config.toml")?;
    let username = match username.or_else(|| cfg.username.clone()) {
        Some(u) => u,
        None => prompt("Username: ")?,
    };
    let password = auth::prompt_password(&username)?;

    let mut client = Client::new(&server)?;
    let creds = rt.block_on(client.login(&username, &password))?;

    let store = FileStore::new()?;
    store.store(&creds)?;

    println!(
        "Logged in as {} on {}. Token stored (mode 0600).",
        creds.username, creds.server
    );
    Ok(())
}

fn dump_random(rt: &tokio::runtime::Runtime, limit: u32) -> Result<()> {
    let creds = FileStore::new()?
        .load()?
        .context("no stored credentials, run `trayplay login` first")?;
    let client = Client::authenticated(creds)?;

    let tracks = rt.block_on(client.random_tracks(limit))?;
    println!("{} tracks", tracks.len());
    for t in &tracks {
        let secs = t.duration().map(|d| d.as_secs()).unwrap_or(0);
        println!(
            "{}  {:>3}:{:02}  {} - {} [{}]",
            t.id,
            secs / 60,
            secs % 60,
            t.display_artist(),
            t.name,
            t.album.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
