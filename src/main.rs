//! `dln-node` — the binary.
//!
//! Three things it can be asked to do: `keygen`, `pubkey`, or run.
//!
//! ## It will not invent an identity
//!
//! It used to. An unparseable key fell through to `Keys::generate()`, so
//! a mistyped character brought the node up as a **different node** — one
//! no grant names and no client URI points at. Every request went
//! unanswered and it looked like a relay problem. The key was never
//! written, so it was different again on the next restart.
//!
//! Harmless while the only callers were tests that wrote the config and
//! read the pubkey back out of the same variable. Fatal the first time
//! somebody deploys it. Mission 29.1.

use anyhow::Result;
use nostr_sdk::prelude::*;
use serde::Deserialize;
use std::fs;
use std::sync::Arc;

use dln_node::lightning::{LdkService, LdkServiceConfig};

#[derive(Debug, Deserialize)]
struct Config {
    node: NodeConfig,
    nostr: NostrConfig,
    wallet: WalletConfig,
    bitcoind: Option<BitcoindConfig>,
    signer: Option<SignerConfig>,
}

#[derive(Debug, Deserialize)]
struct NodeConfig {
    network: String,
    listening_port: u16,
    data_dir: String,
    #[serde(default)]
    alias: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NostrConfig {
    relay: String,
    /// Where this node's identity lives. One file, holding one secret.
    ///
    /// A config file is reviewed, copied between hosts, diffed and pasted
    /// into tickets; ordinary handling of one leaks whatever is in it. So
    /// the key is named here and kept elsewhere.
    #[serde(default)]
    key_file: Option<String>,
    /// The identity, inline. **Deprecated** — see `key_file`.
    ///
    /// Kept working for one release because every scenario and harness in
    /// the estate writes it, and a change about operability should not be
    /// paid for by breaking every test that starts a node.
    #[serde(default)]
    private_key: Option<String>,
    /// Whose grants this node accepts. **Required, and an empty list
    /// answers nothing** — absent configuration fails closed rather than
    /// being read as "any owner", which is dln-node#1.
    #[serde(default)]
    owners: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WalletConfig {
    max_channel_size_sats: u64,
    min_channel_size_sats: u64,
    auto_accept_channels: bool,
}

#[derive(Debug, Deserialize)]
struct BitcoindConfig {
    rpc_host: String,
    rpc_port: u16,
    rpc_user: String,
    rpc_password: String,
}

#[derive(Debug, Deserialize)]
struct SignerConfig {
    transport: String,
    relay: Option<String>,
    nsec: Option<String>,
    signer_pubkey: Option<String>,
}


/// Where a config is looked for, in order, when none is named.
const CONFIG_PATHS: [&str; 2] = ["/etc/dln-node/config.toml", "config.toml"];
/// Where the identity lives when the config does not say.
const DEFAULT_KEY_FILE: &str = "/etc/dln-node/node.key";

fn usage() -> String {
    format!(
        "usage:
  dln-node [config.toml]     run the node
  dln-node keygen [config]   create the identity named by key_file
  dln-node pubkey [config]   print this node's public key

A config is looked for at {}, then {}, unless one is named.",
        CONFIG_PATHS[0], CONFIG_PATHS[1]
    )
}

/// Read the config, from a named path or the first default that exists.
///
/// **Names the path in every error.** `Failed to read config.toml` does
/// not say which one it looked for, and under systemd nobody can guess.
fn load_config(named: Option<&str>) -> Result<(Config, String)> {
    let path = match named {
        Some(p) => p.to_string(),
        None => CONFIG_PATHS
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .map(|p| p.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no config found at {} or {} — name one as an argument",
                    CONFIG_PATHS[0],
                    CONFIG_PATHS[1]
                )
            })?,
    };
    let contents = fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("could not read {path}: {e}"))?;
    let config: Config =
        toml::from_str(&contents).map_err(|e| anyhow::anyhow!("could not parse {path}: {e}"))?;
    Ok((config, path))
}

/// The path the identity should live at.
fn key_file_path(config: &Config) -> String {
    config.nostr.key_file.clone().unwrap_or_else(|| DEFAULT_KEY_FILE.to_string())
}

/// Load the identity, and **never invent one**.
///
/// Every failure here is fatal and names the file. A node that comes up
/// as somebody else is worse than a node that does not come up: the
/// second is obvious within seconds, and the first looks like a network
/// problem for as long as anybody is willing to keep looking.
fn load_keys(config: &Config, config_path: &str) -> Result<Keys> {
    if let Some(inline) = config.nostr.private_key.as_deref().filter(|k| !k.trim().is_empty()) {
        tracing::warn!(
            "nostr.private_key is deprecated and will be removed: move it to a \
             file and set nostr.key_file (see {config_path})"
        );
        return Keys::parse(inline).map_err(|e| {
            anyhow::anyhow!("nostr.private_key in {config_path} is not a secret key: {e}")
        });
    }

    let path = key_file_path(config);
    let raw = fs::read_to_string(&path).map_err(|e| {
        anyhow::anyhow!(
            "could not read the identity at {path}: {e}\n\
             create one with: dln-node keygen"
        )
    })?;
    check_key_mode(&path)?;
    Keys::parse(raw.trim())
        .map_err(|e| anyhow::anyhow!("{path} does not contain a secret key: {e}"))
}

/// Refuse a key file anyone else can read, as `ssh` does.
///
/// The failure this prevents is the one nobody notices until later.
#[cfg(unix)]
fn check_key_mode(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map_err(|e| anyhow::anyhow!("could not stat {path}: {e}"))?
        .permissions()
        .mode()
        & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "{path} is mode {mode:04o} — group or world readable. \
         Run: chmod 600 {path}"
    );
    Ok(())
}

#[cfg(not(unix))]
fn check_key_mode(_path: &str) -> Result<()> {
    Ok(())
}

/// Warn about a wide file this program did not create.
///
/// LDK owns `keys_seed` and the packaging owns `config.toml`, so the
/// honest amount of opinion to have about either is a line naming it.
#[cfg(unix)]
fn warn_if_wide(path: &str, what: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = fs::metadata(path) {
        let mode = md.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!("{what} at {path} is mode {mode:04o} — readable by others");
        }
    }
}

#[cfg(not(unix))]
fn warn_if_wide(_path: &str, _what: &str) {}

/// Write a new identity, and refuse to replace one.
///
/// Generation is an act somebody performs, not something that happens.
/// A key created because a program wanted one is a key nobody chose and
/// nobody wrote down.
fn keygen(config: &Config) -> Result<()> {
    let path = key_file_path(config);
    anyhow::ensure!(
        !std::path::Path::new(&path).exists(),
        "{path} already exists — refusing to replace an identity. \
         Move it aside first if that is really what you want."
    );
    if let Some(dir) = std::path::Path::new(&path).parent() {
        fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("could not create {}: {e}", dir.display()))?;
    }

    let keys = Keys::generate();
    fs::write(&path, format!("{}\n", keys.secret_key().to_secret_hex()))
        .map_err(|e| anyhow::anyhow!("could not write {path}: {e}"))?;
    set_600(&path)?;

    println!("{}", keys.public_key().to_hex());
    eprintln!("wrote {path}");
    eprintln!("publish your grant against the public key above");
    Ok(())
}

#[cfg(unix)]
fn set_600(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| anyhow::anyhow!("could not chmod 600 {path}: {e}"))
}

#[cfg(not(unix))]
fn set_600(_path: &str) -> Result<()> {
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, named) = match args.first().map(String::as_str) {
        Some("keygen") => ("keygen", args.get(1).map(String::as_str)),
        Some("pubkey") => ("pubkey", args.get(1).map(String::as_str)),
        Some("-h") | Some("--help") => {
            println!("{}", usage());
            return Ok(());
        }
        other => ("run", other),
    };

    let (config, config_path) = load_config(named)?;

    if command == "keygen" {
        return keygen(&config);
    }
    if command == "pubkey" {
        let keys = load_keys(&config, &config_path)?;
        println!("{}", keys.public_key().to_hex());
        return Ok(());
    }

    tracing::info!("config: {config_path}");
    tracing::info!(
        "network {}, port {}, data {}, relay {}",
        config.node.network,
        config.node.listening_port,
        config.node.data_dir,
        config.nostr.relay
    );
    tracing::info!(
        "channels {}..{} sats, auto-accept {}",
        config.wallet.min_channel_size_sats,
        config.wallet.max_channel_size_sats,
        config.wallet.auto_accept_channels
    );
    warn_if_wide(&config_path, "the config");

    // **The line that matters.** Which identity this node came up as
    // decides whether any grant reaches it, and it used to read exactly
    // like the five above.
    let keys = load_keys(&config, &config_path)?;
    tracing::info!("identity {}", keys.public_key().to_hex());
    warn_if_wide(
        &format!("{}/keys_seed", config.node.data_dir.trim_end_matches('/')),
        "LDK's seed",
    );

    let owners: Vec<nostr_sdk::prelude::PublicKey> = config
        .nostr
        .owners
        .iter()
        .map(|o| {
            nostr_sdk::prelude::PublicKey::parse(o).map_err(|e| {
                anyhow::anyhow!("nostr.owners in {config_path}: {o} is not a public key: {e}")
            })
        })
        .collect::<Result<_>>()?;
    if owners.is_empty() {
        eprintln!(
            "nostr.owners is empty: this node will accept no grants and \
             answer nothing. Set it to the owner's public key."
        );
    }
    tracing::info!("{} owner(s)", owners.len());

    let bitcoind = config.bitcoind.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "{config_path} has no [bitcoind] section: this node serves a \
             Lightning node and has nothing to answer with when there is none"
        )
    })?;
    let signer_transport = config
        .signer
        .as_ref()
        .map(|s| s.transport.as_str())
        .unwrap_or("embedded");
    let ldk_cfg = LdkServiceConfig {
        network: config.node.network.clone(),
        bitcoind_rpc_host: bitcoind.rpc_host.clone(),
        bitcoind_rpc_port: bitcoind.rpc_port,
        bitcoind_rpc_user: bitcoind.rpc_user.clone(),
        bitcoind_rpc_password: bitcoind.rpc_password.clone(),
        ldk_storage_dir: config.node.data_dir.clone(),
        ldk_listen_addr: Some(format!("0.0.0.0:{}", config.node.listening_port)),
        node_alias: config.node.alias.clone(),
        signer_transport: signer_transport.to_string(),
        signer_relay: config.signer.as_ref().and_then(|s| s.relay.clone()),
        signer_nsec: config.signer.as_ref().and_then(|s| s.nsec.clone()),
        signer_pubkey: config.signer.as_ref().and_then(|s| s.signer_pubkey.clone()),
    };
    // **Report, do not panic.** This is where a misconfigured node fails,
    // and the operator reading the journal gets one line rather than a
    // backtrace note about an environment variable they will never set.
    let ldk = LdkService::start_from_config(&ldk_cfg)
        .map_err(|e| anyhow::anyhow!("could not start the Lightning node: {e}"))?;

    tracing::info!("serving");
    let service = dln_node::service::run(
        dln_node::service::NodeConfig {
            keys,
            relays: vec![config.nostr.relay.clone()],
            owners,
            alias: config.node.alias.clone(),
        },
        ldk,
    );

    // **SIGTERM as well as SIGINT.** systemd sends the first on `stop`
    // and `restart`, and a service that only knows about Ctrl-C gets
    // killed rather than asked.
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("could not listen for SIGTERM: {e}"))?;

    #[cfg(unix)]
    tokio::select! {
        r = service => r.map_err(|e| anyhow::anyhow!("the service stopped: {e}"))?,
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT — stopping"),
        _ = term.recv() => tracing::info!("SIGTERM — stopping"),
    }

    #[cfg(not(unix))]
    tokio::select! {
        r = service => r.map_err(|e| anyhow::anyhow!("the service stopped: {e}"))?,
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT — stopping"),
    }

    Ok(())
}
