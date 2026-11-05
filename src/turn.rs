//! Servidores ICE (STUN + TURN) usados pelo servidor e enviados aos espectadores no `offer`.
//!
//! Fontes: STUN padrão, TURN fixo (`[turn]`) e TURN efêmero do Cloudflare Realtime
//! (`[turn_cloudflare]`, renovado a cada 6 h enquanto a transmissão está ativa).
//! Credenciais nunca vão para logs; o `api_token` só é usado na chamada à API.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};
use webrtc::peer_connection::RTCIceServer;

use crate::config::Config;

pub const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";
const CLOUDFLARE_API: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";
/// Validade pedida à Cloudflare para cada credencial (24 h).
const CLOUDFLARE_TTL_SECS: u64 = 86_400;
/// Intervalo de renovação enquanto a transmissão está ativa.
pub const CLOUDFLARE_REFRESH: Duration = Duration::from_secs(6 * 60 * 60);
const CLOUDFLARE_TIMEOUT: Duration = Duration::from_secs(10);

/// Um servidor ICE no formato do navegador (`RTCIceServer`).
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl fmt::Debug for IceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &self.username.as_ref().map(|_| "<redigido>"))
            .field("credential", &self.credential.as_ref().map(|_| "<redigido>"))
            .finish()
    }
}

/// Lista de ICE compartilhada pela transmissão; a renovação do Cloudflare troca o conteúdo.
pub type SharedIceServers = Arc<Mutex<Vec<IceServer>>>;

/// STUN, depois o TURN do Cloudflare (se houver) e por fim o TURN fixo da config.
pub fn build_ice_servers(config: &Config, cloudflare: Option<Vec<IceServer>>) -> Vec<IceServer> {
    let mut servers = vec![IceServer {
        urls: vec![DEFAULT_STUN.to_owned()],
        username: None,
        credential: None,
    }];
    servers.extend(cloudflare.unwrap_or_default());
    if let Some(turn) = &config.turn {
        servers.push(IceServer {
            urls: vec![turn.url.clone()],
            username: Some(turn.username.clone()),
            credential: Some(turn.credential.clone()),
        });
    }
    servers
}

/// Converte para o tipo do webrtc-rs. Campos ausentes viram string vazia.
pub fn to_rtc(servers: &[IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|server| RTCIceServer {
            urls: server.urls.clone(),
            username: server.username.clone().unwrap_or_default(),
            credential: server.credential.clone().unwrap_or_default(),
        })
        .collect()
}

/// Lista inicial da transmissão: busca o TURN do Cloudflare (se configurado) e monta a lista.
/// Falha na busca só gera aviso; segue com STUN/TURN estático.
pub async fn initial_ice_servers(config: &Config) -> Vec<IceServer> {
    let cloudflare = match &config.turn_cloudflare {
        Some(cf) => cloudflare_ice_servers(&cf.key_id, &cf.api_token).await,
        None => None,
    };
    build_ice_servers(config, cloudflare)
}

/// Busca credenciais do Cloudflare. Devolve `None` (e avisa) se a chamada falhar.
pub async fn cloudflare_ice_servers(key_id: &str, api_token: &str) -> Option<Vec<IceServer>> {
    match fetch_cloudflare(key_id, api_token).await {
        Ok(servers) => {
            info!(count = servers.len(), "credenciais TURN do Cloudflare obtidas");
            Some(servers)
        }
        Err(err) => {
            warn!(
                error = %format!("{err:#}"),
                "TURN do Cloudflare indisponível; seguindo com STUN/TURN estático"
            );
            None
        }
    }
}

async fn fetch_cloudflare(key_id: &str, api_token: &str) -> Result<Vec<IceServer>> {
    let client = reqwest::Client::builder()
        .timeout(CLOUDFLARE_TIMEOUT)
        .build()
        .context("não consegui criar o cliente HTTP")?;
    let url = format!("{CLOUDFLARE_API}/{key_id}/credentials/generate-ice-servers");
    let response = client
        .post(url)
        .bearer_auth(api_token)
        .json(&json!({ "ttl": CLOUDFLARE_TTL_SECS }))
        .send()
        .await
        .context("chamada à API de TURN do Cloudflare falhou")?;
    let status = response.status();
    if !status.is_success() {
        bail!("API de TURN do Cloudflare respondeu {status}");
    }
    let body = response
        .text()
        .await
        .context("não consegui ler a resposta do Cloudflare")?;
    parse_cloudflare_response(&body)
}

#[derive(Deserialize)]
struct CloudflareResponse {
    #[serde(rename = "iceServers")]
    ice_servers: IceServersField,
}

/// A API documenta uma lista; aceita também um objeto único, por segurança.
#[derive(Deserialize)]
#[serde(untagged)]
enum IceServersField {
    Many(Vec<CloudflareServer>),
    One(CloudflareServer),
}

#[derive(Deserialize)]
struct CloudflareServer {
    urls: Urls,
    username: Option<String>,
    credential: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Urls {
    One(String),
    Many(Vec<String>),
}

/// Lê `{"iceServers": [{urls, username, credential}, ...]}`.
/// O erro não repete o corpo da resposta, pois ele contém credenciais.
pub fn parse_cloudflare_response(body: &str) -> Result<Vec<IceServer>> {
    let parsed: CloudflareResponse = serde_json::from_str(body)
        .map_err(|_| anyhow!("resposta do Cloudflare fora do formato esperado"))?;
    let servers = match parsed.ice_servers {
        IceServersField::Many(list) => list,
        IceServersField::One(server) => vec![server],
    };
    if servers.is_empty() {
        bail!("resposta do Cloudflare sem iceServers");
    }
    Ok(servers
        .into_iter()
        .map(|server| IceServer {
            urls: match server.urls {
                Urls::One(url) => vec![url],
                Urls::Many(urls) => urls,
            },
            username: server.username,
            credential: server.credential,
        })
        .collect())
}

/// Renova o TURN do Cloudflare a cada 6 h até `stop` receber `true` (ou o sender sumir).
/// Falha na renovação mantém a lista anterior (as credenciais valem 24 h).
pub fn spawn_refresh(config: Config, ice: SharedIceServers, mut stop: watch::Receiver<bool>) {
    let Some(cf) = config.turn_cloudflare.clone() else {
        return;
    };
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + CLOUDFLARE_REFRESH;
        let mut ticker = tokio::time::interval_at(start, CLOUDFLARE_REFRESH);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                    continue;
                }
            }
            if let Some(servers) = cloudflare_ice_servers(&cf.key_id, &cf.api_token).await {
                *ice.lock().unwrap_or_else(PoisonError::into_inner) =
                    build_ice_servers(&config, Some(servers));
                info!("credenciais TURN do Cloudflare renovadas");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TurnCloudflareConfig, TurnConfig};

    const SAMPLE: &str = r#"{
        "iceServers": [
            {"urls": ["stun:stun.cloudflare.com:3478"]},
            {
                "urls": ["turn:turn.cloudflare.com:3478?transport=udp", "turns:turn.cloudflare.com:5349?transport=tcp"],
                "username": "g0-user-exemplo",
                "credential": "cred-exemplo"
            }
        ]
    }"#;

    #[test]
    fn parses_cloudflare_response() {
        let servers = parse_cloudflare_response(SAMPLE).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, vec!["stun:stun.cloudflare.com:3478"]);
        assert_eq!(servers[0].username, None);
        assert_eq!(servers[1].urls.len(), 2);
        assert_eq!(servers[1].username.as_deref(), Some("g0-user-exemplo"));
        assert_eq!(servers[1].credential.as_deref(), Some("cred-exemplo"));
    }

    #[test]
    fn parses_single_url_string_and_single_object() {
        let body = r#"{"iceServers": {"urls": "turn:t.example:3478", "username": "u", "credential": "c"}}"#;
        let servers = parse_cloudflare_response(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec!["turn:t.example:3478"]);
    }

    #[test]
    fn rejects_bad_or_empty_responses_without_echoing_body() {
        let err = parse_cloudflare_response(r#"{"iceServers": "segredo-ecoado"}"#).unwrap_err();
        assert!(!format!("{err:#}").contains("segredo-ecoado"));
        assert!(parse_cloudflare_response(r#"{"iceServers": []}"#).is_err());
        assert!(parse_cloudflare_response("não é json").is_err());
    }

    #[test]
    fn list_is_stun_then_cloudflare_then_static_turn() {
        let config = Config {
            turn: Some(TurnConfig {
                url: "turn:fixo.example:3478".to_owned(),
                username: "fixo".to_owned(),
                credential: "fixo-cred".to_owned(),
            }),
            turn_cloudflare: Some(TurnCloudflareConfig {
                key_id: "k".to_owned(),
                api_token: "t".to_owned(),
            }),
            ..Config::default()
        };
        let cloudflare = parse_cloudflare_response(SAMPLE).unwrap();
        let list = build_ice_servers(&config, Some(cloudflare));
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].urls, vec![DEFAULT_STUN]);
        assert_eq!(list[1].urls[0], "stun:stun.cloudflare.com:3478");
        assert_eq!(list[3].urls, vec!["turn:fixo.example:3478"]);
        assert_eq!(list[3].credential.as_deref(), Some("fixo-cred"));
    }

    #[test]
    fn list_without_cloudflare_is_stun_and_static_turn() {
        let plain = build_ice_servers(&Config::default(), None);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].urls, vec![DEFAULT_STUN]);
        assert!(plain[0].username.is_none());
    }

    #[test]
    fn converts_to_rtc_with_empty_strings_for_missing_credentials() {
        let list = build_ice_servers(&Config::default(), None);
        let rtc = to_rtc(&list);
        assert_eq!(rtc[0].urls, vec![DEFAULT_STUN]);
        assert_eq!(rtc[0].username, "");
        assert_eq!(rtc[0].credential, "");
    }

    #[test]
    fn debug_and_json_do_not_leak_needless_fields() {
        let server = parse_cloudflare_response(SAMPLE).unwrap().remove(1);
        let debug = format!("{server:?}");
        assert!(!debug.contains("cred-exemplo"));
        assert!(!debug.contains("g0-user-exemplo"));
        let stun = IceServer {
            urls: vec![DEFAULT_STUN.to_owned()],
            username: None,
            credential: None,
        };
        let value = serde_json::to_value(&stun).unwrap();
        assert!(value.get("username").is_none());
        assert!(value.get("credential").is_none());
    }
}
