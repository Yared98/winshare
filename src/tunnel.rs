//! Tunnel `managed`: sobe `cloudflared` como processo filho, com `kill_on_drop`.
//! Se o binário não estiver no PATH, avisa e segue só com acesso local.

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::config::Config;

pub const CLOUDFLARED_MISSING: &str =
    "cloudflared não encontrado no PATH; o link público não vai funcionar (acesso só local)";

/// Inicia `cloudflared tunnel run` apontando para a porta pública local.
/// Devolve `None` (sem erro) quando o binário não existe.
pub async fn spawn_managed(config: &Config) -> Result<Option<Child>> {
    let name = config
        .tunnel_name
        .as_deref()
        .context("tunnel = \"managed\" exige tunnel_name")?;
    let origin = format!("http://127.0.0.1:{}", config.public_port);
    let spawned = Command::new("cloudflared")
        .args(["tunnel", "--no-autoupdate", "run", "--url", &origin, name])
        .kill_on_drop(true)
        .spawn();
    match spawned {
        Ok(child) => {
            info!(tunnel = name, origin = %origin, "cloudflared iniciado");
            Ok(Some(child))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warn!("{CLOUDFLARED_MISSING}");
            Ok(None)
        }
        Err(err) => Err(err).context("não consegui iniciar o cloudflared"),
    }
}
