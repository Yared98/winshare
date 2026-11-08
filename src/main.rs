#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
use screencast::server::{self, control_router, public_router, AppState};
use screencast::tray;
use screencast::tunnel;

/// Compartilha uma janela e o áudio do processo com espectadores no navegador (WebRTC).
#[derive(Debug, Parser, Clone)]
#[command(name = "screencast", version, about)]
struct Cli {
    /// Arquivo de configuração TOML
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Aloca ou anexa um console para depuração
    #[arg(long)]
    console: bool,

    /// Executa em modo supervisor (reinício automático do processo filho)
    #[arg(long)]
    supervised: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    #[cfg(windows)]
    if cli.console {
        attach_or_alloc_console();
    }

    let exe_path =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("screencast.exe"));
    let exe_dir = exe_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let log_dir = exe_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    cleanup_old_logs(&log_dir, Duration::from_secs(7 * 24 * 3600));

    let log_filename = if cli.supervised {
        "supervisor.log"
    } else {
        "screencast.log"
    };
    let file_appender = tracing_appender::rolling::daily(&log_dir, log_filename);
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

    let config = Config::load(&cli.config)?;

    // Modo supervisor: lança e monitora o processo filho sem a flag --supervised
    if cli.supervised {
        let raw_args: Vec<String> = std::env::args().skip(1).collect();
        let child_args = screencast::supervisor::filter_child_args(raw_args);
        return screencast::supervisor::run_supervisor(&exe_path, &child_args, &config);
    }

    // Instância única: mutex nomeado Local\winshare
    let _instance_guard = match tray::init_single_instance(config.control_port) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            info!("outra instância do winshare já está ativa; painel aberto no navegador e saindo com 0");
            return Ok(());
        }
        Err(err) => {
            warn!(?err, "falha ao inicializar verificação de instância única");
            None
        }
    };

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

    // Canal para encerramento limpo via opção "Sair" do menu da bandeja
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Inicia a thread do ícone na bandeja com o laço de mensagens Win32
    let mut tray_handle =
        match tray::spawn_tray(state.clone(), cli.config.clone(), shutdown_tx) {
            Ok(handle) => Some(handle),
            Err(err) => {
                warn!(?err, "não foi possível inicializar o ícone na bandeja");
                None
            }
        };

    tray::open_control_page(&control_url);

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.context("falha ao aguardar Ctrl+C")?;
            info!("encerrando por Ctrl+C");
        }
        _ = shutdown_rx.changed() => {
            info!("encerrando por solicitação do menu da bandeja");
        }
        result = public_task => warn!(?result, "servidor público parou"),
        result = control_task => warn!(?result, "servidor de controle parou"),
    }

    state.stop_live();
    if let Some(mut handle) = tray_handle.take() {
        handle.shutdown();
    }
    // O cloudflared (kill_on_drop) é derrubado quando `_tunnel` sai do escopo.
    Ok(())
}

/// Anexa o console do processo pai ou aloca um novo console em modo de depuração.
#[cfg(windows)]
fn attach_or_alloc_console() {
    unsafe {
        if windows::Win32::System::Console::AttachConsole(
            windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
        )
        .is_err()
        {
            let _ = windows::Win32::System::Console::AllocConsole();
        }
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
