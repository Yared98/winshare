use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::warn;

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

/// Configuração carregada de `config.toml`. Campos ausentes usam o padrão.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Domínio público usado no link (ex.: winshare.yared.com.br).
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
    /// Taxa-alvo do vídeo em kbps (padrão 6000 = 6 Mbps).
    pub video_bitrate_kbps: u32,
    /// Faixa de portas UDP para as conexões ICE (encaminhar no roteador).
    pub udp_port_min: u16,
    pub udp_port_max: u16,
    pub turn: Option<TurnConfig>,
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
            video_bitrate_kbps: 6000,
            udp_port_min: 50000,
            udp_port_max: 50100,
            turn: None,
        }
    }
}

impl Config {
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
        if self.tunnel == TunnelMode::Managed && self.tunnel_name.is_none() {
            bail!("tunnel = \"managed\" exige tunnel_name");
        }
        if self.password.as_deref() == Some("") {
            bail!("password não pode ser vazia (remova a chave para desativar a senha)");
        }
        Ok(())
    }

    /// Taxa-alvo do vídeo em bits por segundo.
    pub fn video_bitrate_bps(&self) -> u32 {
        self.video_bitrate_kbps * 1000
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
}
