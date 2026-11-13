use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::warn;

/// Configurações de vídeo negociadas para a transmissão (taxa e intervalo de keyframes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    pub max_kbps: u32,
    pub min_kbps: u32,
    pub keyframe_interval: Duration,
}

impl VideoSettings {
    pub fn new(max_kbps: u32, min_kbps: u32, keyframe_interval: Duration) -> Self {
        Self {
            max_kbps,
            min_kbps,
            keyframe_interval,
        }
    }

    /// Taxa máxima em bits por segundo.
    pub fn max_bps(&self) -> u32 {
        self.max_kbps * 1000
    }

    /// Taxa mínima em bits por segundo.
    pub fn min_bps(&self) -> u32 {
        self.min_kbps * 1000
    }
}

impl From<u32> for VideoSettings {
    fn from(bps: u32) -> Self {
        let kbps = (bps / 1000).max(300);
        Self {
            max_kbps: kbps,
            min_kbps: 800.min(kbps),
            keyframe_interval: Duration::from_secs(5),
        }
    }
}

/// Modo do tunnel que expõe a porta pública à internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TunnelMode {
    /// O tunnel roda no roteador (aponta para a porta pública deste PC). O app não sobe cloudflared.
    #[default]
    External,
    /// O app sobe `cloudflared tunnel run` como processo filho.
    Managed,
    /// Sem tunnel: só acesso local/rede.
    None,
}

/// Servidor TURN opcional usado nas conexões ICE.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnConfig {
    pub url: String,
    pub username: String,
    pub credential: String,
}

/// TURN efêmero do Cloudflare Realtime (`[turn_cloudflare]`). O `api_token` nunca sai do servidor.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCloudflareConfig {
    /// Identificador da TURN key criada no painel (Realtime → TURN).
    pub key_id: String,
    /// Token de API com permissão para gerar credenciais TURN.
    pub api_token: String,
}

impl fmt::Debug for TurnCloudflareConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnCloudflareConfig")
            .field("key_id", &self.key_id)
            .field("api_token", &"<redigido>")
            .finish()
    }
}

/// Configuração carregada de `config.toml`. Campos ausentes usam o padrão.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Domínio público usado no link (ex.: winshare.exemplo.com.br).
    pub public_host: String,
    /// Porta pública (0.0.0.0): só /s/<token>, /viewer e o WebSocket do espectador.
    pub public_port: u16,
    /// Porta de controle (127.0.0.1): /control e /api/*.
    pub control_port: u16,
    pub tunnel: TunnelMode,
    /// Nome do tunnel do Cloudflare (obrigatório em `tunnel = "managed"`).
    pub tunnel_name: Option<String>,
    /// Senha opcional da sala.
    pub password: Option<String>,
    /// Limite de espectadores simultâneos.
    pub max_viewers: usize,
    /// Taxa-teto do vídeo em kbps (padrão 3000 = 3 Mbps).
    pub video_bitrate_kbps: u32,
    /// Taxa mínima do vídeo em kbps (padrão 800 = 800 kbps, validar < teto e >= 300).
    pub video_min_bitrate_kbps: u32,
    /// Intervalo entre keyframes forçados em segundos (padrão 5, faixa 1..=30).
    pub keyframe_interval_secs: u32,
    /// Faixa de portas UDP para as conexões ICE (encaminhar no roteador).
    pub udp_port_min: u16,
    pub udp_port_max: u16,
    pub turn: Option<TurnConfig>,
    /// TURN efêmero do Cloudflare Realtime, renovado a cada 6 h durante a transmissão.
    pub turn_cloudflare: Option<TurnCloudflareConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            public_host: "localhost".to_owned(),
            public_port: 8080,
            control_port: 7878,
            tunnel: TunnelMode::default(),
            tunnel_name: None,
            password: None,
            max_viewers: 10,
            video_bitrate_kbps: 3000,
            video_min_bitrate_kbps: 800,
            keyframe_interval_secs: 5,
            udp_port_min: 50000,
            udp_port_max: 50100,
            turn: None,
            turn_cloudflare: None,
        }
    }
}

impl Config {
    /// Configurações negociadas de vídeo baseadas nos valores da configuração.
    pub fn video_settings(&self) -> VideoSettings {
        VideoSettings {
            max_kbps: self.video_bitrate_kbps,
            min_kbps: self.video_min_bitrate_kbps,
            keyframe_interval: self.keyframe_interval(),
        }
    }

    /// Lê o arquivo TOML. Se o arquivo não existir, usa os padrões e avisa.
    pub fn load(path: &Path) -> Result<Self> {
        let config = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("não consegui ler {}", path.display()))?;
            toml::from_str::<Config>(&text)
                .with_context(|| format!("config inválida em {}", path.display()))?
        } else {
            warn!(path = %path.display(), "arquivo de configuração não encontrado; usando padrões");
            Config::default()
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.public_port == self.control_port {
            bail!("public_port e control_port precisam ser diferentes");
        }
        if self.udp_port_min == 0 || self.udp_port_min > self.udp_port_max {
            bail!(
                "faixa UDP inválida: {}-{}",
                self.udp_port_min,
                self.udp_port_max
            );
        }
        if self.max_viewers == 0 {
            bail!("max_viewers precisa ser pelo menos 1");
        }
        if !(500..=50_000).contains(&self.video_bitrate_kbps) {
            bail!("video_bitrate_kbps precisa estar entre 500 e 50000");
        }
        if self.video_min_bitrate_kbps < 300 || self.video_min_bitrate_kbps >= self.video_bitrate_kbps {
            bail!("video_min_bitrate_kbps precisa ser >= 300 e menor que video_bitrate_kbps");
        }
        if !(1..=30).contains(&self.keyframe_interval_secs) {
            bail!("keyframe_interval_secs precisa estar entre 1 e 30");
        }
        if self.tunnel == TunnelMode::Managed && self.tunnel_name.is_none() {
            bail!("tunnel = \"managed\" exige tunnel_name");
        }
        if self.password.as_deref() == Some("") {
            bail!("password não pode ser vazia (remova a chave para desativar a senha)");
        }
        if let Some(cf) = &self.turn_cloudflare {
            if cf.key_id.trim().is_empty() || cf.api_token.trim().is_empty() {
                bail!("turn_cloudflare exige key_id e api_token preenchidos");
            }
        }
        Ok(())
    }

    /// Taxa-teto do vídeo em bits por segundo.
    pub fn video_bitrate_bps(&self) -> u32 {
        self.video_bitrate_kbps * 1000
    }

    /// Taxa mínima do vídeo em bits por segundo.
    pub fn video_min_bitrate_bps(&self) -> u32 {
        self.video_min_bitrate_kbps * 1000
    }

    /// Intervalo entre keyframes forçados.
    pub fn keyframe_interval(&self) -> Duration {
        Duration::from_secs(self.keyframe_interval_secs as u64)
    }

    /// Endereço da porta pública (acessível pelo tunnel/roteador).
    pub fn public_addr(&self) -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], self.public_port))
    }

    /// Endereço da porta de controle (somente loopback).
    pub fn control_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.control_port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let c = Config::default();
        c.validate().unwrap();
        assert_eq!(c.tunnel, TunnelMode::External);
        assert_eq!(c.max_viewers, 10);
        assert_eq!(c.video_bitrate_kbps, 3000);
        assert_eq!(c.video_min_bitrate_kbps, 800);
        assert_eq!(c.keyframe_interval_secs, 5);
        assert_eq!(c.video_bitrate_bps(), 3_000_000);
        assert_eq!(c.video_min_bitrate_bps(), 800_000);
        assert_eq!(c.keyframe_interval(), Duration::from_secs(5));
    }

    #[test]
    fn validates_bitrate_and_keyframe_interval() {
        // Mínimo menor que 300 deve ser rejeitado
        let low_min = Config {
            video_min_bitrate_kbps: 299,
            ..Config::default()
        };
        assert!(low_min.validate().is_err());

        // Mínimo igual ou maior que teto deve ser rejeitado
        let equal = Config {
            video_bitrate_kbps: 3000,
            video_min_bitrate_kbps: 3000,
            ..Config::default()
        };
        assert!(equal.validate().is_err());

        let greater = Config {
            video_bitrate_kbps: 3000,
            video_min_bitrate_kbps: 3500,
            ..Config::default()
        };
        assert!(greater.validate().is_err());

        // Intervalo de keyframe fora de 1..=30 deve ser rejeitado
        let zero_kf = Config {
            keyframe_interval_secs: 0,
            ..Config::default()
        };
        assert!(zero_kf.validate().is_err());

        let high_kf = Config {
            keyframe_interval_secs: 31,
            ..Config::default()
        };
        assert!(high_kf.validate().is_err());

        // Limites válidos
        let valid_min = Config {
            video_min_bitrate_kbps: 300,
            keyframe_interval_secs: 1,
            ..Config::default()
        };
        assert!(valid_min.validate().is_ok());

        let valid_max = Config {
            keyframe_interval_secs: 30,
            ..Config::default()
        };
        assert!(valid_max.validate().is_ok());
    }

    #[test]
    fn parses_partial_toml_with_defaults() {
        let c: Config = toml::from_str(
            r#"
            public_host = "winshare.exemplo.com.br"
            tunnel = "managed"
            tunnel_name = "winshare"
            "#,
        )
        .unwrap();
        assert_eq!(c.public_host, "winshare.exemplo.com.br");
        assert_eq!(c.tunnel, TunnelMode::Managed);
        assert_eq!(c.public_port, 8080);
        c.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("porta_errada = 1").is_err());
    }

    #[test]
    fn rejects_managed_without_tunnel_name() {
        let c: Config = toml::from_str(r#"tunnel = "managed""#).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_same_ports_and_bad_udp_range() {
        let same = Config {
            control_port: 8080,
            ..Config::default()
        };
        assert!(same.validate().is_err());
        let range = Config {
            udp_port_min: 50100,
            udp_port_max: 50000,
            ..Config::default()
        };
        assert!(range.validate().is_err());
    }

    #[test]
    fn control_addr_is_loopback() {
        assert!(Config::default().control_addr().ip().is_loopback());
    }

    #[test]
    fn parses_turn_cloudflare_section() {
        let c: Config = toml::from_str(
            r#"
            [turn_cloudflare]
            key_id = "abc123"
            api_token = "segredo"
            "#,
        )
        .unwrap();
        let cf = c.turn_cloudflare.as_ref().unwrap();
        assert_eq!(cf.key_id, "abc123");
        assert_eq!(cf.api_token, "segredo");
        c.validate().unwrap();
    }

    #[test]
    fn rejects_empty_turn_cloudflare_fields() {
        let c: Config = toml::from_str(
            r#"
            [turn_cloudflare]
            key_id = "abc123"
            api_token = "  "
            "#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn turn_cloudflare_debug_hides_api_token() {
        let cf = TurnCloudflareConfig {
            key_id: "abc123".to_owned(),
            api_token: "segredo-do-token".to_owned(),
        };
        let text = format!("{cf:?}");
        assert!(text.contains("abc123"));
        assert!(!text.contains("segredo-do-token"));
    }

    #[test]
    fn video_settings_reflects_config() {
        let c = Config::default();
        let vs = c.video_settings();
        assert_eq!(vs.max_kbps, c.video_bitrate_kbps);
        assert_eq!(vs.min_kbps, c.video_min_bitrate_kbps);
        assert_eq!(vs.keyframe_interval, Duration::from_secs(5));
        assert_eq!(vs.max_bps(), 3_000_000);
        assert_eq!(vs.min_bps(), 800_000);
    }
}
