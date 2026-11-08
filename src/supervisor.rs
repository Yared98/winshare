//! Supervisor de processo para reinício automático do screencast.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;
use crate::tray;

/// Gerenciador de backoff exponencial para reinício do processo filho supervisionado.
/// A espera começa em 2 s e dobra a cada falha (2, 4, 8, 16, 32, 60), com teto de 60 s.
/// Se o filho permanecer estável por >= 10 minutos (600 s), o atraso volta para o inicial (2 s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorBackoff {
    current_delay_secs: u64,
}

impl SupervisorBackoff {
    pub const INITIAL_DELAY_SECS: u64 = 2;
    pub const MAX_DELAY_SECS: u64 = 60;
    pub const STABLE_DURATION: Duration = Duration::from_secs(10 * 60);

    pub fn new() -> Self {
        Self {
            current_delay_secs: Self::INITIAL_DELAY_SECS,
        }
    }

    /// Retorna o atraso atual.
    pub fn current_delay(&self) -> Duration {
        Duration::from_secs(self.current_delay_secs)
    }

    /// Retorna o tempo de espera a ser aplicado com base no tempo de atividade (uptime) do filho
    /// e calcula o próximo atraso (dobrado até 60 s).
    pub fn next_delay(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE_DURATION {
            self.current_delay_secs = Self::INITIAL_DELAY_SECS;
        }
        let delay = Duration::from_secs(self.current_delay_secs);
        self.current_delay_secs = (self.current_delay_secs * 2).min(Self::MAX_DELAY_SECS);
        delay
    }
}

impl Default for SupervisorBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Filtra argumentos da linha de comando, removendo `--supervised`.
pub fn filter_child_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    args.into_iter()
        .filter(|arg| arg != "--supervised")
        .collect()
}

/// Executa o laço supervisor: lança o executável filho e o monitora.
/// Se o filho sair com código 0 (ex.: opção "Sair" do menu), encerra o supervisor com sucesso.
/// Se sair com código diferente de 0, aplica backoff exponencial (2, 4, 8 ... 60 s) e relança.
pub fn run_supervisor(exe_path: &Path, child_args: &[String], config: &Config) -> Result<()> {
    info!(
        exe = %exe_path.display(),
        args = ?child_args,
        "iniciando supervisor do winshare"
    );

    // Se já houver uma instância ativa executando, apenas abre o painel e sai com 0.
    if tray::is_instance_running() {
        info!("instância do winshare já em execução; abrindo o painel e finalizando supervisor");
        let control_url = format!("http://127.0.0.1:{}/control", config.control_port);
        tray::open_control_page(&control_url);
        return Ok(());
    }

    let mut backoff = SupervisorBackoff::new();

    loop {
        info!("lançando processo filho supervisionado");
        let start = Instant::now();
        let mut child = Command::new(exe_path)
            .args(child_args)
            .spawn()
            .with_context(|| format!("falha ao iniciar processo filho {}", exe_path.display()))?;

        let status = child
            .wait()
            .with_context(|| "falha ao aguardar encerramento do processo filho")?;
        let uptime = start.elapsed();

        if status.success() {
            info!("processo filho encerrou normalmente (código 0); finalizando supervisor");
            return Ok(());
        }

        let delay = backoff.next_delay(uptime);
        warn!(
            ?status,
            uptime_secs = uptime.as_secs(),
            delay_secs = delay.as_secs(),
            "processo filho encerrou de forma anormal; aguardando {} s antes de reiniciar",
            delay.as_secs()
        );
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supervisor_backoff_progression() {
        let mut backoff = SupervisorBackoff::new();
        let short_uptime = Duration::from_secs(5);

        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(4));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(8));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(16));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(32));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(60));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(60));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(60));
    }

    #[test]
    fn test_supervisor_backoff_reset_after_stable() {
        let mut backoff = SupervisorBackoff::new();
        let short_uptime = Duration::from_secs(10);

        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(4));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(8));

        // 10 minutos (600 s) estável: reinicia para 2 s
        let stable_uptime = Duration::from_secs(600);
        assert_eq!(backoff.next_delay(stable_uptime), Duration::from_secs(2));
        // Próxima falha curta continua a progressão a partir de 2 s
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(4));
    }

    #[test]
    fn test_supervisor_backoff_no_reset_before_stable() {
        let mut backoff = SupervisorBackoff::new();
        let short_uptime = Duration::from_secs(10);

        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(4));

        // 9 minutos e 59 segundos não é suficiente para zerar (precisa >= 10 min)
        let almost_stable = Duration::from_secs(599);
        assert_eq!(backoff.next_delay(almost_stable), Duration::from_secs(8));
        assert_eq!(backoff.next_delay(short_uptime), Duration::from_secs(16));
    }

    #[test]
    fn test_filter_child_args() {
        let args = vec![
            "--config".to_string(),
            "config.toml".to_string(),
            "--supervised".to_string(),
            "--console".to_string(),
        ];
        let filtered = filter_child_args(args);
        assert_eq!(
            filtered,
            vec!["--config".to_string(), "config.toml".to_string(), "--console".to_string()]
        );
    }
}
