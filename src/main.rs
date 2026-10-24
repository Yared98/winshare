use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use screencast::config::{Config, TunnelMode};
use screencast::server::{self, AppState, control_router, public_router};
use screencast::tunnel;

/// Compartilha uma janela e o áudio do processo com espectadores no navegador (WebRTC).
#[derive(Debug, Parser)]
#[command(name = "screencast", version, about)]
struct Cli {
    /// Arquivo de configuração TOML
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let token = server::generate_token();

    let public = TcpListener::bind(config.public_addr())
        .await
        .with_context(|| {
            format!(
                "não consegui abrir a porta pública {}",
                config.public_addr()
            )
        })?;
    let control = TcpListener::bind(config.control_addr())
        .await
        .with_context(|| {
            format!(
                "não consegui abrir a porta de controle {}",
                config.control_addr()
            )
        })?;

    let mut notices = Vec::new();
    let _tunnel = match config.tunnel {
        TunnelMode::Managed => {
            let child = tunnel::spawn_managed(&config).await?;
            if child.is_none() {
                notices.push(tunnel::CLOUDFLARED_MISSING.to_owned());
            }
            child
        }
        TunnelMode::External => {
            info!(
                "tunnel externo: o roteador deve encaminhar {}/TCP para este PC",
                config.public_port
            );
            None
        }
        TunnelMode::None => None,
    };

    let state = Arc::new(AppState::new(config.clone(), token, notices));
    let control_url = format!("http://127.0.0.1:{}/control", config.control_port);
    info!("link do espectador: {}", state.viewer_link());
    info!("controle (somente neste PC): {control_url}");

    let public_task = tokio::spawn(
        axum::serve(
            public,
            public_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    let control_task = tokio::spawn(
        axum::serve(
            control,
            control_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );

    open_control_page(&control_url);

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.context("falha ao aguardar Ctrl+C")?;
            info!("encerrando");
        }
        result = public_task => warn!(?result, "servidor público parou"),
        result = control_task => warn!(?result, "servidor de controle parou"),
    }

    state.stop_live();
    // O cloudflared (kill_on_drop) é derrubado quando `_tunnel` sai do escopo.
    Ok(())
}

/// Abre o controle no navegador padrão. Falha silenciosa: o link também aparece no log.
fn open_control_page(url: &str) {
    if cfg!(windows) {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
}
