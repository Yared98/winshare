use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
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
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let log_dir = exe_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    cleanup_old_logs(&log_dir, Duration::from_secs(7 * 24 * 3600));

    let file_appender = tracing_appender::rolling::daily(&log_dir, "screencast.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
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

/// Apaga arquivos na pasta de logs modificados/criados há mais de `max_age`.
fn cleanup_old_logs(dir: &Path, max_age: Duration) {
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let file_time = meta.modified().or_else(|_| meta.created());
        if let Ok(time) = file_time {
            if let Ok(age) = now.duration_since(time) {
                if age > max_age {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}
